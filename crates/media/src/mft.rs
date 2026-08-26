//! Decoding a bare stream of coded frames, with no container around them.
//!
//! `.ares` stores its texture as raw AV1 or VP9 frames inside its own chunks, so there is nothing
//! for a source reader to demux — the decoder has to be driven directly. That is what an
//! `IMFTransform` is: one input, one output, feed it frames and pull pictures out.
//!
//! What is *available* here is not this crate's decision. Windows ships H.264 and HEVC decoders;
//! VP9 and AV1 arrive as Store extensions ("VP9 Video Extensions", "AV1 Video Extension") and are
//! frequently absent. Asking for one that is not installed has to produce an explanation the user
//! can act on, not a failure code, which is why `MediaError::NoDecoder` names the codec.

#![allow(unsafe_code)]

use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::*;

use crate::{Codec, MediaError, VideoFrame};

/// Only synchronous software transforms are enumerated.
///
/// A hardware MFT is asynchronous: it signals readiness through `IMFMediaEventGenerator` and has
/// to be pumped from an event loop. For a texture track measured in hundreds of kilobytes a frame
/// that machinery buys nothing, and getting it subtly wrong costs frames that never arrive.
const ENUM_FLAGS: MFT_ENUM_FLAG = MFT_ENUM_FLAG(
    MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_LOCALMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
);

pub struct Transform {
    transform: IMFTransform,
    width: u32,
    height: u32,
    /// Output subtype actually negotiated. NV12 is what almost every decoder offers.
    output: GUID,
    stride: i32,
    /// Whether the transform hands back its own sample, or expects one to be supplied.
    ///
    /// Software decoders overwhelmingly expect one, and passing a null sample to those is a
    /// silent no-op: `ProcessOutput` succeeds, produces nothing, and the picture never arrives.
    provides_samples: bool,
    /// Bytes the transform wants in a supplied output buffer.
    output_size: u32,
    /// Which decoder the system handed us, for the inspector. Worth showing: whether a texture
    /// came back from AMD's hardware path or Microsoft's software one changes what to suspect
    /// when it does not.
    name: String,
}

// Created and used from one thread only; the player gives each file its own.
unsafe impl Send for Transform {}

impl Transform {
    pub fn new(codec: Codec, width: u32, height: u32) -> Result<Transform, MediaError> {
        super::mf::ensure_started()?;
        let subtype = subtype_for(codec);

        let (transform, name) = unsafe { find_decoder(subtype) }
            .ok_or_else(|| MediaError::NoDecoder(codec.label().to_string()))?;

        unsafe {
            // Ask for one picture per frame instead of a pipeline's worth.
            //
            // Without this a decoder is entitled to buffer several frames before emitting the
            // first, which for a texture track being scrubbed frame by frame means it emits
            // nothing at all. It is not an optimization; it is what makes the transform usable
            // outside a playback pipeline.
            if let Ok(attributes) = transform.GetAttributes() {
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            let input = MFCreateMediaType().map_err(platform)?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(platform)?;
            input.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(platform)?;
            // Frame size is packed as two 32-bit values in one 64-bit attribute, width first.
            input
                .SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    ((width as u64) << 32) | height as u64,
                )
                .map_err(platform)?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(platform)?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(|_| MediaError::NoDecoder(codec.label().to_string()))?;
        }

        // Take the first output type the decoder offers rather than demanding one: a decoder that
        // only speaks NV12 is the norm, and asking it for RGB32 fails outright.
        let (output, stride) = unsafe { negotiate_output(&transform, width)? };
        let (provides_samples, output_size) = unsafe { output_allocation(&transform, width, height) };

        unsafe {
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }

        Ok(Transform {
            transform,
            width,
            height,
            output,
            stride,
            provides_samples,
            output_size,
            name,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Discard buffered state so decoding can restart from a keyframe.
    pub fn flush(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }

    /// Feed one coded frame and take whatever picture comes out.
    ///
    /// Decoders are allowed to buffer: a frame in does not have to mean a frame out, and the
    /// first few calls of a stream routinely return nothing. `Ok(None)` means exactly that, and
    /// is not an error.
    pub fn decode(
        &mut self,
        data: &[u8],
        key: bool,
        pts_us: u64,
    ) -> Result<Option<VideoFrame>, MediaError> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32).map_err(platform)?;
            let mut target: *mut u8 = std::ptr::null_mut();
            let mut capacity = 0u32;
            buffer
                .Lock(&mut target, Some(&mut capacity), None)
                .map_err(platform)?;
            // SAFETY: `Lock` guarantees `capacity` writable bytes, and the buffer was just
            // created with exactly `data.len()` of them.
            std::slice::from_raw_parts_mut(target, data.len()).copy_from_slice(data);
            buffer.Unlock().map_err(platform)?;
            buffer
                .SetCurrentLength(data.len() as u32)
                .map_err(platform)?;

            let sample = MFCreateSample().map_err(platform)?;
            sample.AddBuffer(&buffer).map_err(platform)?;
            // 100-nanosecond units, as everywhere else in Media Foundation.
            sample
                .SetSampleTime((pts_us * 10) as i64)
                .map_err(platform)?;
            if key {
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }

            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => {}
                // The decoder is holding a picture it wants collected first. Draining and
                // retrying once is the documented response.
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    let pending = self.pull()?;
                    self.transform
                        .ProcessInput(0, &sample, 0)
                        .map_err(platform)?;
                    if pending.is_some() {
                        return Ok(pending);
                    }
                }
                Err(e) => return Err(platform(e)),
            }
        }
        self.pull()
    }

    /// Take one decoded picture, if the transform has one ready.
    fn pull(&mut self) -> Result<Option<VideoFrame>, MediaError> {
        unsafe {
            let mut status = 0u32;
            // A transform that does not allocate its own samples needs one supplied, sized to
            // what `GetOutputStreamInfo` asked for. Handing it nothing instead is not an error and
            // not a picture — it simply returns success having done nothing, which is the least
            // helpful failure mode in the whole API.
            let supplied = if self.provides_samples {
                None
            } else {
                let buffer = MFCreateMemoryBuffer(self.output_size).map_err(platform)?;
                let sample = MFCreateSample().map_err(platform)?;
                sample.AddBuffer(&buffer).map_err(platform)?;
                Some(sample)
            };

            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(supplied),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];

            match self
                .transform
                .ProcessOutput(0, &mut buffers, &mut status)
            {
                Ok(()) => {}
                // Nothing ready yet: the decoder wants more input first.
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                    let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pEvents);
                    return Ok(None);
                }
                // The decoder changed its mind about the output format, which VP9 does at the
                // first keyframe once it has read the real frame size.
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                    let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pEvents);
                    let (output, stride) = negotiate_output(&self.transform, self.width)?;
                    self.output = output;
                    self.stride = stride;
                    let (provides, size) =
                        output_allocation(&self.transform, self.width, self.height);
                    self.provides_samples = provides;
                    self.output_size = size;
                    return Ok(None);
                }
                Err(e) => {
                    let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                    let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pEvents);
                    return Err(platform(e));
                }
            }

            let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
            let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pEvents);
            let Some(sample) = sample else {
                return Ok(None);
            };

            let mut timestamp = 0i64;
            let _ = sample.GetSampleTime().map(|t| timestamp = t);

            let buffer = sample.ConvertToContiguousBuffer().map_err(platform)?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut length = 0u32;
            buffer
                .Lock(&mut data, None, Some(&mut length))
                .map_err(platform)?;
            // SAFETY: valid for `length` bytes until the matching `Unlock` below.
            let locked = std::slice::from_raw_parts(data, length as usize);
            let rgba = self.to_rgba(locked);
            let _ = buffer.Unlock();

            Ok(Some(VideoFrame {
                pts_us: (timestamp.max(0) / 10) as u64,
                width: self.width,
                height: self.height,
                rgba,
            }))
        }
    }

    fn to_rgba(&self, data: &[u8]) -> Vec<u8> {
        let width = self.width as usize;
        let height = self.height as usize;
        if self.output == MFVideoFormat_NV12 {
            nv12_to_rgba(data, width, height, self.stride.unsigned_abs() as usize)
        } else {
            super::mf::copy_bgra(data, width, height, self.stride)
        }
    }
}

impl Drop for Transform {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// Choose an output type and report its stride.
///
/// # Safety
/// The transform must have an input type already set, which is what makes its output list real.
unsafe fn negotiate_output(
    transform: &IMFTransform,
    width: u32,
) -> Result<(GUID, i32), MediaError> {
    let mut index = 0u32;
    // Prefer NV12, which is what a hardware-derived decoder produces natively; fall back to the
    // first type on offer rather than failing, since a software decoder may list something else.
    let mut fallback: Option<(IMFMediaType, GUID)> = None;
    while let Ok(candidate) = transform.GetOutputAvailableType(0, index) {
        let subtype = candidate.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
        if subtype == MFVideoFormat_NV12 {
            transform
                .SetOutputType(0, &candidate, 0)
                .map_err(platform)?;
            let stride = candidate
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|v| v as i32)
                .unwrap_or(width as i32);
            return Ok((subtype, stride));
        }
        if fallback.is_none() {
            fallback = Some((candidate, subtype));
        }
        index += 1;
        if index > 64 {
            break;
        }
    }

    let (candidate, subtype) = fallback.ok_or(MediaError::NoVideoTrack)?;
    transform
        .SetOutputType(0, &candidate, 0)
        .map_err(platform)?;
    let stride = candidate
        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
        .map(|v| v as i32)
        .unwrap_or((width * 4) as i32);
    Ok((subtype, stride))
}

/// Whether the transform allocates its own output samples, and how big a supplied one must be.
///
/// # Safety
/// The transform must have both types set, which is what makes its stream info meaningful.
unsafe fn output_allocation(transform: &IMFTransform, width: u32, height: u32) -> (bool, u32) {
    // A generous fallback: NV12 is 1.5 bytes per pixel, and an RGB32 output is 4. Sizing for the
    // larger costs nothing and avoids a decoder rejecting the buffer it was handed.
    let fallback = width.saturating_mul(height).saturating_mul(4).max(4096);
    match transform.GetOutputStreamInfo(0) {
        Ok(info) => {
            let provides = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;
            (provides, info.cbSize.max(fallback))
        }
        Err(_) => (false, fallback),
    }
}

/// Find and instantiate a decoder for one compressed subtype.
///
/// # Safety
/// Media Foundation must be started, which `Transform::new` does first.
unsafe fn find_decoder(subtype: GUID) -> Option<(IMFTransform, String)> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        ENUM_FLAGS,
        Some(&input),
        None,
        &mut activates,
        &mut count,
    )
    .ok()?;
    if activates.is_null() || count == 0 {
        return None;
    }

    // SAFETY: `MFTEnumEx` returns a CoTaskMem array of `count` interface pointers, which the
    // caller owns and must free. Each entry is taken exactly once.
    let list = std::slice::from_raw_parts_mut(activates, count as usize);
    let mut result = None;
    for slot in list.iter_mut() {
        let Some(activate) = slot.take() else {
            continue;
        };
        if result.is_none() {
            let name = friendly_name(&activate);
            result = activate
                .ActivateObject::<IMFTransform>()
                .ok()
                .map(|t| (t, name));
        }
    }
    windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
    result
}

/// The decoder's display name, or an empty string if it declines to give one.
///
/// # Safety
/// `activate` must be a live activation object from `MFTEnumEx`.
unsafe fn friendly_name(activate: &IMFActivate) -> String {
    let mut text = windows::core::PWSTR::null();
    let mut length = 0u32;
    if activate
        .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut length)
        .is_err()
        || text.is_null()
    {
        return String::new();
    }
    let name = text.to_string().unwrap_or_default();
    windows::Win32::System::Com::CoTaskMemFree(Some(text.as_ptr() as *const _));
    name
}

fn subtype_for(codec: Codec) -> GUID {
    match codec {
        Codec::Av1 => MFVideoFormat_AV1,
        Codec::Vp9 => MFVideoFormat_VP90,
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
    }
}

fn platform(e: windows::core::Error) -> MediaError {
    MediaError::Platform(e.message().to_string())
}

/// NV12 to RGBA: a full-resolution luma plane, then a half-resolution interleaved chroma plane.
///
/// BT.709 limited range, which is what every decoder on this path produces at the sizes involved.
/// Getting the range wrong is not subtle — it washes blacks out to grey — but getting the *matrix*
/// wrong shifts skin tones in a way that is easy to miss, which is why the coefficients are
/// spelled out rather than approximated.
///
/// Fixed point rather than floating: a 2048x2048 texture is four million pixels, and this runs
/// once per frame of a volumetric capture. The coefficients are the usual 8.8 forms of
/// `1.164, 1.793, 0.213, 0.533, 2.112`.
pub fn nv12_to_rgba(data: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    const Y: i32 = 298;
    const R_V: i32 = 459;
    const G_U: i32 = 55;
    const G_V: i32 = 136;
    const B_U: i32 = 541;

    let stride = if stride == 0 { width } else { stride };
    let mut out = vec![0u8; width * height * 4];
    if width == 0 || height == 0 {
        return out;
    }
    let chroma_base = stride * height;

    for row in 0..height {
        // Rows are sliced once rather than indexed per pixel: at four million pixels the bounds
        // check is a measurable part of the total.
        let luma = data
            .get(row * stride..row * stride + width)
            .unwrap_or_default();
        let chroma_start = chroma_base + (row / 2) * stride;
        let chroma = data
            .get(chroma_start..chroma_start + width.div_ceil(2) * 2)
            .unwrap_or_default();
        let target = &mut out[row * width * 4..(row + 1) * width * 4];

        for (x, pixel) in target.chunks_exact_mut(4).enumerate() {
            // A short buffer reads as black rather than as an out-of-range panic: a decoder that
            // hands back less than it promised should cost a frame, not the process.
            let y = luma.get(x).copied().unwrap_or(16) as i32 - 16;
            let u = chroma.get((x / 2) * 2).copied().unwrap_or(128) as i32 - 128;
            let v = chroma.get((x / 2) * 2 + 1).copied().unwrap_or(128) as i32 - 128;

            let base = Y * y + 128;
            pixel[0] = ((base + R_V * v) >> 8).clamp(0, 255) as u8;
            pixel[1] = ((base - G_U * u - G_V * v) >> 8).clamp(0, 255) as u8;
            pixel[2] = ((base + B_U * u) >> 8).clamp(0, 255) as u8;
            pixel[3] = 255;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an NV12 buffer of one flat colour.
    fn flat(width: usize, height: usize, y: u8, cb: u8, cr: u8) -> Vec<u8> {
        let mut v = vec![y; width * height];
        v.extend(
            std::iter::repeat([cb, cr])
                .take(width * height / 4)
                .flatten(),
        );
        v
    }

    #[test]
    fn neutral_chroma_produces_grey_across_the_luma_range() {
        for (luma, expected) in [(16u8, 0u8), (235, 255)] {
            let out = nv12_to_rgba(&flat(4, 4, luma, 128, 128), 4, 4, 4);
            for px in out.chunks_exact(4) {
                assert!(
                    (px[0] as i32 - expected as i32).abs() <= 2
                        && px[0] == px[1]
                        && px[1] == px[2],
                    "luma {luma} gave {px:?}"
                );
                assert_eq!(px[3], 255);
            }
        }
    }

    #[test]
    fn the_colour_matrix_puts_red_green_and_blue_where_they_belong() {
        // Limited-range BT.709 primaries. A swapped Cb/Cr shows up here immediately as red and
        // blue trading places.
        let red = nv12_to_rgba(&flat(2, 2, 63, 102, 240), 2, 2, 2);
        assert!(red[0] > 200 && red[1] < 80 && red[2] < 80, "{:?}", &red[..4]);

        let blue = nv12_to_rgba(&flat(2, 2, 32, 240, 118), 2, 2, 2);
        assert!(blue[2] > 200 && blue[0] < 80, "{:?}", &blue[..4]);
    }

    #[test]
    fn a_padded_stride_is_honoured_rather_than_assumed_to_be_the_width() {
        // A decoder's stride is routinely rounded up. Reading at the width instead skews every
        // row and produces the diagonal smear everybody recognizes.
        let (w, h, stride) = (4usize, 4usize, 8usize);
        let mut data = vec![16u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                data[y * stride + x] = if y == 0 { 235 } else { 16 };
            }
        }
        data.extend(vec![128u8; stride * h / 2]);
        let out = nv12_to_rgba(&data, w, h, stride);
        for x in 0..w {
            assert!(out[x * 4] > 200, "the first row should be white");
            assert!(out[(w + x) * 4] < 40, "the second row should be black");
        }
    }

    #[test]
    fn a_short_buffer_produces_black_rather_than_reading_past_the_end() {
        let out = nv12_to_rgba(&[], 8, 8, 8);
        assert_eq!(out.len(), 8 * 8 * 4);
        assert!(out.chunks_exact(4).all(|p| p[0] < 4 && p[3] == 255));
    }
}
