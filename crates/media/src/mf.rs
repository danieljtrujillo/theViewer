//! Windows Media Foundation, driven through `IMFSourceReader`.
//!
//! The source reader is the right level for a viewer: it demuxes, decodes and — with
//! `ENABLE_ADVANCED_VIDEO_PROCESSING` — colour-converts and scales, all through whatever hardware
//! path the machine has. The alternative, assembling a topology by hand, buys control this app has
//! no use for.
//!
//! Two things here regularly catch people out, and both are handled explicitly below:
//!
//! 1. **RGB32 is bottom-up.** Media Foundation reports it with a *negative* `MF_MT_DEFAULT_STRIDE`
//!    because the format is GDI-compatible, which means the first row in the buffer is the bottom
//!    row of the picture. Ignoring the sign gives a vertically mirrored video.
//! 2. **A read can legitimately return nothing.** `ReadSample` signals format changes and stream
//!    ticks with a null sample and no error. Treating that as end-of-stream truncates playback at
//!    the first discontinuity.

#![allow(unsafe_code)]

use std::path::Path;

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VT_I8, VT_UI8};

use crate::{MediaError, VideoFrame, VideoInfo};

/// Media Foundation is process-wide and must be started before anything else touches it.
///
/// Started once per thread that uses it rather than once per process: `MFStartup` is reference
/// counted, and the player gives each file its own decode thread, which also needs COM
/// initialized in that thread's apartment.
pub(crate) fn ensure_started() -> Result<(), MediaError> {
    thread_local! {
        static STARTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    STARTED.with(|s| {
        if s.get() {
            return Ok(());
        }
        unsafe {
            // Multithreaded apartment: the source reader is free-threaded, and an STA here would
            // need a message pump this thread does not have.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(platform)?;
        }
        s.set(true);
        Ok(())
    })
}

fn platform(e: windows::core::Error) -> MediaError {
    MediaError::Platform(e.message().to_string())
}

const FIRST_VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
const MEDIA_SOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;

pub struct Reader {
    reader: IMFSourceReader,
    width: u32,
    height: u32,
    /// Signed: negative means the picture is stored bottom-up.
    stride: i32,
    /// Set once the stream has ended, so a player that keeps asking gets `None` rather than a
    /// stream of errors.
    finished: bool,
}

// The source reader is an ordinary COM object with no thread affinity once created in an MTA, and
// this type is only ever touched from the one thread that owns it.
unsafe impl Send for Reader {}

impl Reader {
    pub fn open(path: &Path) -> Result<(Reader, VideoInfo), MediaError> {
        ensure_started()?;

        let url = HSTRING::from(path.as_os_str());
        let reader = unsafe {
            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 2).map_err(platform)?;
            let attributes = attributes.ok_or(MediaError::Unsupported)?;
            // Lets the reader insert a colour converter and scaler, which is what makes asking
            // for plain RGB32 out of an H.264 stream work at all.
            attributes
                .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                .map_err(platform)?;
            MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), &attributes).map_err(platform)?
        };

        unsafe {
            reader
                .SetStreamSelection(ALL_STREAMS, false)
                .map_err(platform)?;
            reader
                .SetStreamSelection(FIRST_VIDEO_STREAM, true)
                .map_err(|_| MediaError::NoVideoTrack)?;
        }

        // The codec of the *native* type, read before the output type is overwritten. This is the
        // only chance to name what the file actually contains, which is what makes a missing
        // decoder reportable as "no AV1 decoder" instead of a bare failure code.
        let codec = unsafe {
            reader
                .GetNativeMediaType(FIRST_VIDEO_STREAM, 0)
                .ok()
                .and_then(|t| t.GetGUID(&MF_MT_SUBTYPE).ok())
                .map(codec_name)
                .unwrap_or_default()
        };

        unsafe {
            let output = MFCreateMediaType().map_err(platform)?;
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(platform)?;
            output
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(platform)?;
            reader
                .SetCurrentMediaType(FIRST_VIDEO_STREAM, None, &output)
                // Failing here almost always means the machine cannot decode the compressed
                // stream at all, rather than that RGB32 is unavailable.
                .map_err(|_| MediaError::NoDecoder(codec.clone()))?;
        }

        let current = unsafe {
            reader
                .GetCurrentMediaType(FIRST_VIDEO_STREAM)
                .map_err(platform)?
        };

        // MF_MT_FRAME_SIZE and MF_MT_FRAME_RATE are pairs of 32-bit values packed into one 64-bit
        // attribute, high word first. Reading the u64 directly avoids two more imported helpers.
        let (width, height) = unsafe {
            let packed = current.GetUINT64(&MF_MT_FRAME_SIZE).map_err(platform)?;
            ((packed >> 32) as u32, packed as u32)
        };
        if width == 0 || height == 0 {
            return Err(MediaError::NoVideoTrack);
        }

        let fps = unsafe {
            current
                .GetUINT64(&MF_MT_FRAME_RATE)
                .ok()
                .map(|packed| {
                    let numerator = (packed >> 32) as u32;
                    let denominator = packed as u32;
                    if denominator == 0 {
                        0.0
                    } else {
                        numerator as f32 / denominator as f32
                    }
                })
                .unwrap_or(0.0)
        };

        // A declared stride can be absent; the fallback assumes a tightly packed top-down image,
        // which is what the reader produces when it does not say otherwise.
        let stride = unsafe {
            current
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|v| v as i32)
                .unwrap_or((width * 4) as i32)
        };

        let duration_us = unsafe {
            reader
                .GetPresentationAttribute(MEDIA_SOURCE, &MF_PD_DURATION)
                .ok()
                .and_then(|v| read_u64(&v))
                // Presentation timestamps are in 100-nanosecond units throughout Media Foundation.
                .map(|hundred_nanos| hundred_nanos / 10)
                .unwrap_or(0)
        };

        Ok((
            Reader {
                reader,
                width,
                height,
                stride,
                finished: false,
            },
            VideoInfo {
                width,
                height,
                fps,
                duration_us,
                codec,
            },
        ))
    }

    pub fn seek(&mut self, pts_us: u64) -> Result<(), MediaError> {
        // A null time-format GUID means "the default", which for every source that matters is
        // 100-nanosecond presentation time.
        let position = i64_propvariant((pts_us * 10) as i64);
        unsafe {
            self.reader
                .SetCurrentPosition(&GUID::zeroed(), &position)
                .map_err(platform)?;
        }
        self.finished = false;
        Ok(())
    }

    pub fn next_frame(&mut self) -> Result<Option<VideoFrame>, MediaError> {
        if self.finished {
            return Ok(None);
        }
        // A read can come back empty without being the end: a format change, a gap, a stream tick.
        // Retrying a bounded number of times gets past those without risking a spin.
        for _ in 0..64 {
            let mut flags = 0u32;
            let mut timestamp = 0i64;
            let mut sample: Option<IMFSample> = None;
            unsafe {
                self.reader
                    .ReadSample(
                        FIRST_VIDEO_STREAM,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .map_err(platform)?;
            }

            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                self.finished = true;
                return Ok(None);
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                self.refresh_layout();
            }
            let Some(sample) = sample else {
                continue;
            };
            return Ok(Some(self.convert(&sample, timestamp)?));
        }
        // Sixty-four consecutive empty reads is not a stream, it is a fault.
        self.finished = true;
        Ok(None)
    }

    /// Re-read the frame geometry after the reader reports a mid-stream format change. Adaptive
    /// streams and some captures genuinely change resolution part way through.
    fn refresh_layout(&mut self) {
        unsafe {
            let Ok(current) = self.reader.GetCurrentMediaType(FIRST_VIDEO_STREAM) else {
                return;
            };
            if let Ok(packed) = current.GetUINT64(&MF_MT_FRAME_SIZE) {
                let (w, h) = ((packed >> 32) as u32, packed as u32);
                if w > 0 && h > 0 {
                    self.width = w;
                    self.height = h;
                }
            }
            self.stride = current
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|v| v as i32)
                .unwrap_or((self.width * 4) as i32);
        }
    }

    fn convert(&self, sample: &IMFSample, timestamp: i64) -> Result<VideoFrame, MediaError> {
        let buffer = unsafe { sample.ConvertToContiguousBuffer().map_err(platform)? };

        let mut data: *mut u8 = std::ptr::null_mut();
        let mut length = 0u32;
        unsafe {
            buffer
                .Lock(&mut data, None, Some(&mut length))
                .map_err(platform)?;
        }

        // SAFETY: `Lock` hands back a pointer to `length` readable bytes that stay valid until
        // the matching `Unlock` below, and nothing between the two touches the buffer.
        let locked = unsafe { std::slice::from_raw_parts(data, length as usize) };
        let result = copy_bgra(
            locked,
            self.width as usize,
            self.height as usize,
            self.stride,
        );

        unsafe {
            let _ = buffer.Unlock();
        }

        Ok(VideoFrame {
            pts_us: (timestamp.max(0) / 10) as u64,
            width: self.width,
            height: self.height,
            rgba: result,
        })
    }

}

/// Copy a BGRA frame out as RGBA, undoing the bottom-up layout if there is one.
///
/// Free function rather than a method so it can be tested without conjuring a COM object: this is
/// the part with the arithmetic in it, and the part where a mistake shows up as an upside-down or
/// blue-tinted video rather than as a crash.
pub(crate) fn copy_bgra(src: &[u8], width: usize, height: usize, stride: i32) -> Vec<u8> {
    let row_bytes = width * 4;
    let mut out = vec![0u8; row_bytes * height];
    let step = stride.unsigned_abs() as usize;
    if step == 0 || width == 0 {
        return out;
    }
    let bottom_up = stride < 0;

    for y in 0..height {
        // With a negative stride the buffer holds the picture upside down, so the row that
        // belongs at the top comes from the far end.
        let source_row = if bottom_up { height - 1 - y } else { y };
        let start = source_row * step;
        let Some(row) = src.get(start..start + row_bytes) else {
            continue;
        };
        let dst = &mut out[y * row_bytes..(y + 1) * row_bytes];
        // RGB32 is B, G, R, X in memory. The fourth byte is undefined for opaque video, so it is
        // written rather than copied -- copying it makes the whole frame transparent.
        for (d, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
        }
    }
    out
}

/// Build the `VT_I8` PROPVARIANT that `SetCurrentPosition` wants.
///
/// Hand-built because `PROPVARIANT` in the bindings is the raw C union with no convenience
/// conversions. `VT_I8` owns nothing heap-allocated, so this needs no matching clear.
fn i64_propvariant(value: i64) -> PROPVARIANT {
    let mut pv = PROPVARIANT::default();
    // SAFETY: setting the discriminant and the matching arm together is exactly the invariant a
    // PROPVARIANT has. VT_I8 selects `hVal`.
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = value;
    }
    pv
}

/// Read an unsigned 64-bit value out of a PROPVARIANT, if that is what it holds.
///
/// `MF_PD_DURATION` is documented as `VT_UI8`, but sources are not uniformly careful about it, so
/// a signed value is accepted too rather than silently reporting a duration of zero.
fn read_u64(pv: &PROPVARIANT) -> Option<u64> {
    // SAFETY: the arm is chosen by the discriminant, which is the only way this union may be read.
    unsafe {
        let inner = &*pv.Anonymous.Anonymous;
        match inner.vt {
            VT_UI8 => Some(inner.Anonymous.uhVal),
            VT_I8 => u64::try_from(inner.Anonymous.hVal).ok(),
            _ => None,
        }
    }
}

/// Name the compressed format from its subtype GUID.
///
/// The first four bytes of a Media Foundation video subtype are its FourCC, which covers every
/// codec worth naming and keeps this from being a table of sixty GUIDs.
fn codec_name(subtype: GUID) -> String {
    let fourcc = subtype.data1.to_le_bytes();
    match &fourcc {
        b"H264" | b"AVC1" => "H.264".into(),
        b"HEVC" | b"HVC1" | b"HEVS" => "HEVC".into(),
        b"AV01" => "AV1".into(),
        b"VP90" => "VP9".into(),
        b"VP80" => "VP8".into(),
        b"MPG2" => "MPEG-2".into(),
        b"MP4V" | b"MP4S" => "MPEG-4".into(),
        b"WVC1" => "VC-1".into(),
        b"WMV1" | b"WMV2" | b"WMV3" => "Windows Media Video".into(),
        b"MJPG" => "Motion JPEG".into(),
        _ => {
            if fourcc.iter().all(|b| b.is_ascii_graphic()) {
                String::from_utf8_lossy(&fourcc).into_owned()
            } else {
                String::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subtype(fourcc: &[u8; 4]) -> GUID {
        GUID::from_values(u32::from_le_bytes(*fourcc), 0, 0x0010, [0; 8])
    }

    #[test]
    fn codec_names_come_out_of_the_fourcc_in_the_guid() {
        assert_eq!(codec_name(subtype(b"H264")), "H.264");
        assert_eq!(codec_name(subtype(b"AV01")), "AV1");
        assert_eq!(codec_name(subtype(b"VP90")), "VP9");
        // Something unregistered but printable is still worth showing.
        assert_eq!(codec_name(subtype(b"XYZW")), "XYZW");
        // ...and something that is not text at all is better left blank than shown as mojibake.
        assert_eq!(codec_name(GUID::from_values(1, 0, 0, [0; 8])), "");
    }

    /// Two rows, two pixels each, BGRX. First row in memory is blue, second is red.
    const TWO_ROWS: [u8; 16] = [
        255, 0, 0, 0, 255, 0, 0, 0, //
        0, 0, 255, 0, 0, 0, 255, 0,
    ];

    #[test]
    fn a_propvariant_round_trips_through_its_discriminant() {
        let pv = i64_propvariant(90_666_670);
        assert_eq!(read_u64(&pv), Some(90_666_670));
        // A negative position is not a duration, and must not wrap around into a huge one.
        assert_eq!(read_u64(&i64_propvariant(-1)), None);
        // An empty variant carries no value rather than a zero.
        assert_eq!(read_u64(&PROPVARIANT::default()), None);
    }

    #[test]
    fn a_bottom_up_buffer_is_flipped_and_a_top_down_one_is_not() {
        let out = copy_bgra(&TWO_ROWS, 2, 2, 8);
        assert_eq!(&out[0..4], &[0, 0, 255, 255], "top row should be blue");
        assert_eq!(&out[8..12], &[255, 0, 0, 255], "bottom row should be red");

        // The negative stride Media Foundation reports for RGB32. Getting this wrong mirrors
        // every video vertically, which is the single most common Media Foundation bug.
        let out = copy_bgra(&TWO_ROWS, 2, 2, -8);
        assert_eq!(&out[0..4], &[255, 0, 0, 255], "flipped: top row should be red");
        assert_eq!(&out[8..12], &[0, 0, 255, 255]);
    }

    #[test]
    fn the_alpha_byte_is_written_not_copied() {
        // RGB32's fourth byte is padding and is frequently zero. Copying it makes the frame
        // fully transparent, which renders as nothing at all.
        let out = copy_bgra(&TWO_ROWS, 2, 2, 8);
        assert!(out.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn a_short_buffer_produces_black_rows_rather_than_reading_past_the_end() {
        let out = copy_bgra(&[255u8; 8], 2, 4, 8);
        assert_eq!(out.len(), 2 * 4 * 4);
        assert_eq!(&out[8..], &vec![0u8; 24][..]);
    }

    #[test]
    fn padded_rows_are_read_at_their_declared_stride() {
        // A 2-pixel-wide frame in a buffer with 4 pixels of stride: the padding must be skipped,
        // not shown.
        let mut src = vec![0u8; 32];
        src[0..8].copy_from_slice(&TWO_ROWS[0..8]);
        src[16..24].copy_from_slice(&TWO_ROWS[8..16]);
        let out = copy_bgra(&src, 2, 2, 16);
        assert_eq!(&out[0..4], &[0, 0, 255, 255]);
        assert_eq!(&out[8..12], &[255, 0, 0, 255]);
    }
}
