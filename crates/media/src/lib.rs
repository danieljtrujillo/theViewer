//! Video decoding through whatever the platform already has.
//!
//! This crate ships no codecs and never will. On Windows it drives **Media Foundation**, which is
//! hardware accelerated on every GPU made this decade and answers for whatever the machine has
//! installed — H.264 and HEVC always, AV1 and VP9 when the corresponding Store extension is
//! present. Bundling a software decoder instead would add tens of megabytes, lose the hardware
//! path, and drag in patent obligations, all to decode files the operating system can already
//! decode.
//!
//! The consequence, stated plainly: **what plays depends on the machine.** A file that opens here
//! may not open on a fresh Windows install, and the error says so rather than pretending the file
//! is broken.

#![deny(unsafe_code)]

use std::fmt;
use std::path::Path;

#[cfg(windows)]
mod mf;
#[cfg(windows)]
mod mft;
#[cfg(windows)]
mod wic;

/// A compressed video format, named rather than described by a platform GUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Av1,
    Vp9,
    H264,
    Hevc,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::Av1 => "AV1",
            Codec::Vp9 => "VP9",
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
        }
    }
}

/// What a decoder could work out about a stream before decoding any of it.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub duration_us: u64,
    /// Human-readable codec name, for the inspector. Empty when the platform will not say.
    pub codec: String,
}

impl VideoInfo {
    pub fn frame_count(&self) -> u32 {
        if self.fps <= 0.0 {
            return 0;
        }
        ((self.duration_us as f64 / 1_000_000.0) * self.fps as f64).round() as u32
    }
}

/// One decoded frame, in the RGBA8 the renderer uploads.
pub struct VideoFrame {
    pub pts_us: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug)]
pub enum MediaError {
    /// This build has no decoder for this platform at all.
    Unsupported,
    /// The platform has no decoder installed for this file's codec. Recoverable by the user, so
    /// it is worth distinguishing from a broken file.
    NoDecoder(String),
    /// The file opened but carries no video track.
    NoVideoTrack,
    Platform(String),
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::Unsupported => {
                write!(f, "video playback needs Windows Media Foundation")
            }
            MediaError::NoDecoder(codec) => write!(
                f,
                "no decoder installed for {codec}; install the matching Video Extension from the \
                 Microsoft Store"
            ),
            MediaError::NoVideoTrack => write!(f, "the file carries no video track"),
            MediaError::Platform(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MediaError {}

/// A video file, opened and ready to decode.
///
/// Decoding is stateful and sequential by nature, so this is used from one thread at a time. The
/// player gives it its own.
pub struct Video {
    #[cfg(windows)]
    inner: mf::Reader,
    info: VideoInfo,
}

impl Video {
    pub fn open(path: &Path) -> Result<Video, MediaError> {
        #[cfg(windows)]
        {
            let (inner, info) = mf::Reader::open(path)?;
            Ok(Video { inner, info })
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err(MediaError::Unsupported)
        }
    }

    pub fn info(&self) -> &VideoInfo {
        &self.info
    }

    /// Decode the next frame in presentation order. `None` at the end of the stream.
    pub fn next_frame(&mut self) -> Result<Option<VideoFrame>, MediaError> {
        #[cfg(windows)]
        {
            self.inner.next_frame()
        }
        #[cfg(not(windows))]
        {
            Err(MediaError::Unsupported)
        }
    }

    /// Move to the keyframe at or before `pts_us`. The next frame decoded starts from there, so
    /// the caller may have to decode forward a little to reach the frame it actually wants.
    pub fn seek(&mut self, pts_us: u64) -> Result<(), MediaError> {
        #[cfg(windows)]
        {
            self.inner.seek(pts_us)
        }
        #[cfg(not(windows))]
        {
            let _ = pts_us;
            Err(MediaError::Unsupported)
        }
    }
}

/// One decoded still picture, in the RGBA8 the renderer uploads.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Which platform decoder produced it, for the inspector.
    pub decoder: String,
}

/// Decode a still image through the platform, for formats this build carries no decoder for.
///
/// HEIF and AVIF are the reason it exists — both need a video codec, and Windows already hosts
/// one — but it is also the escape hatch for camera RAW, JPEG XR, and whatever the platform
/// learns to read next.
pub fn decode_image(path: &Path) -> Result<Picture, MediaError> {
    #[cfg(windows)]
    {
        let image = wic::decode(path)?;
        Ok(Picture {
            width: image.width,
            height: image.height,
            rgba: image.rgba,
            decoder: image.decoder,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(MediaError::Unsupported)
    }
}

/// Whether the platform has a decoder for this file, without decoding it.
pub fn can_decode_image(path: &Path) -> bool {
    #[cfg(windows)]
    {
        wic::can_decode(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// A decoder for a bare stream of coded frames, with no container around them.
///
/// This is what `.ares` needs: its texture track is AV1 or VP9 frames stored in the container's
/// own chunks, so there is nothing to demux and the decoder has to be fed directly.
pub struct StreamDecoder {
    #[cfg(windows)]
    inner: mft::Transform,
}

impl StreamDecoder {
    pub fn new(codec: Codec, width: u32, height: u32) -> Result<StreamDecoder, MediaError> {
        #[cfg(windows)]
        {
            Ok(StreamDecoder {
                inner: mft::Transform::new(codec, width, height)?,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (codec, width, height);
            Err(MediaError::Unsupported)
        }
    }

    /// Which decoder the system supplied, for the inspector.
    pub fn name(&self) -> String {
        #[cfg(windows)]
        {
            self.inner.name().to_string()
        }
        #[cfg(not(windows))]
        {
            String::new()
        }
    }

    /// Feed one coded frame. `Ok(None)` means the decoder is still buffering, which is normal for
    /// the first frames of a stream and is not an error.
    pub fn decode(
        &mut self,
        data: &[u8],
        key: bool,
        pts_us: u64,
    ) -> Result<Option<VideoFrame>, MediaError> {
        #[cfg(windows)]
        {
            self.inner.decode(data, key, pts_us)
        }
        #[cfg(not(windows))]
        {
            let _ = (data, key, pts_us);
            Err(MediaError::Unsupported)
        }
    }

    /// Throw away everything buffered, so decoding can restart from a keyframe.
    ///
    /// Needed on any backward seek: an inter-coded stream cannot be entered part way through, and
    /// feeding a decoder frames out of order without telling it produces smeared garbage rather
    /// than an error.
    pub fn flush(&mut self) {
        #[cfg(windows)]
        {
            self.inner.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_count_follows_from_duration_and_rate() {
        let info = VideoInfo {
            width: 1920,
            height: 1080,
            fps: 30.0,
            duration_us: 9_066_667,
            codec: "H.264".into(),
        };
        assert_eq!(info.frame_count(), 272);
    }

    #[test]
    fn a_stream_with_no_declared_rate_reports_no_frames_rather_than_dividing_by_zero() {
        let info = VideoInfo {
            width: 640,
            height: 480,
            fps: 0.0,
            duration_us: 5_000_000,
            codec: String::new(),
        };
        assert_eq!(info.frame_count(), 0);
    }

    #[test]
    fn a_missing_codec_reads_as_something_the_user_can_act_on() {
        let message = MediaError::NoDecoder("AV1".into()).to_string();
        assert!(message.contains("AV1"));
        assert!(message.contains("Store"), "the user needs to know what to do");
    }
}
