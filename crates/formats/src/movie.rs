//! Video files.
//!
//! The scene a video produces is one [`Screen`] and one dynamic image; everything that moves comes
//! through the [`FrameSource`]. That is the same shape an animated GIF produces, and the same
//! shape a 360 photo produces, which is why one viewport can present all three without knowing
//! which it has.
//!
//! Decoding is the platform's job — see `viewer-media`. What is decided here is *presentation*:
//! whether the file is a panorama, how its eyes are packed, and which frame belongs on screen at a
//! given instant.

use std::path::Path;

use viewer_io::VideoKind;
use viewer_media::{MediaError, Video};
use viewer_scene::{Image, Node, Payload, Scene, Screen, ScreenProjection, StereoLayout, Timeline};

use crate::spherical::{self, Spherical};
use crate::{Frame, FrameSource, ImportError, Opened};

/// How far the clock may run ahead of the last decoded frame before seeking rather than decoding
/// forward.
///
/// Decoding forward through a second of video is quicker than a seek plus the decode back up to
/// the keyframe; much past that and the seek wins. This is also what makes a scrub feel immediate
/// instead of fast-forwarding through the whole file.
const SEEK_THRESHOLD_US: u64 = 1_000_000;

pub fn import(path: &Path, bytes: &[u8], kind: VideoKind) -> Result<Opened, ImportError> {
    let format = viewer_io::FormatId::Video(kind);
    let video = Video::open(path).map_err(|e| media_error(&format, e))?;
    let info = video.info().clone();

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    // Container metadata first, then the name, then the shape. Only MP4-family files carry the
    // spherical boxes; for the rest the name and the aspect ratio are all there is.
    let spherical = container_spherical(bytes, kind)
        .unwrap_or_else(|| spherical::detect(None, file_name, info.width, info.height));

    let mut scene = Scene::new();
    // A black poster frame: the first real frame lands within a few milliseconds, and starting
    // from a blank texture of the right size avoids a one-frame flash at a different aspect.
    let poster = Image {
        name: "frame".into(),
        width: info.width.max(1),
        height: info.height.max(1),
        rgba: vec![0; info.width.max(1) as usize * info.height.max(1) as usize * 4],
    };
    let image = scene.push_dynamic_image(poster);
    scene.push_root(Node::with_payload(
        "video",
        Payload::Screen(Screen {
            image,
            projection: spherical.projection,
            stereo: spherical.stereo,
            width: info.width,
            height: info.height,
        }),
    ));

    let timeline = Timeline {
        frame_count: info.frame_count(),
        fps: info.fps,
        duration_us: info.duration_us,
        seekable: true,
    };
    scene.timeline = Some(timeline.clone());

    scene.set_meta("format", kind.label());
    if !info.codec.is_empty() {
        scene.set_meta("codec", &info.codec);
    }
    scene.set_meta("pixels", format!("{} x {}", info.width, info.height));
    if info.fps > 0.0 {
        scene.set_meta("frame rate", format!("{:.3} fps", info.fps));
    }
    scene.set_meta("duration", format_duration(info.duration_us));
    if spherical.is_panorama() || spherical.stereo != StereoLayout::Mono {
        scene.set_meta("projection", projection_label(spherical));
        scene.set_meta("projection source", spherical.evidence.label());
    }

    Ok(Opened {
        scene,
        frames: Some(Box::new(VideoSource {
            video,
            format,
            timeline,
            ahead: None,
            served_pts: None,
            index: 0,
            ended: false,
        })),
    })
}

/// Spherical metadata carried by the container, for the containers that carry it.
fn container_spherical(bytes: &[u8], kind: VideoKind) -> Option<Spherical> {
    match kind {
        VideoKind::Mp4 | VideoKind::QuickTime => crate::isobmff::spherical(bytes),
        // Matroska carries the same information in a `Projection` element. Not read yet: it needs
        // an EBML walker, and the file name plus the 2:1 shape covers the common case.
        _ => None,
    }
}

fn projection_label(s: Spherical) -> String {
    let stereo = match s.stereo {
        StereoLayout::Mono => "",
        StereoLayout::OverUnder => ", stereo over-under",
        StereoLayout::SideBySide => ", stereo side-by-side",
    };
    match s.projection {
        ScreenProjection::Flat => format!("flat{stereo}"),
        ScreenProjection::Equirect { horizontal_fov } => {
            format!("equirectangular {horizontal_fov:.0}\u{b0}{stereo}")
        }
    }
}

pub fn format_duration(us: u64) -> String {
    let total = us / 1_000_000;
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn media_error(format: &viewer_io::FormatId, e: MediaError) -> ImportError {
    ImportError::Malformed {
        format: format.clone(),
        reason: e.to_string(),
    }
}

/// Pulls frames out of the platform decoder on demand.
///
/// The one decoded frame held in `ahead` is what makes this work against a clock. Without it there
/// is no way to answer "is the frame on screen still the right one?" without decoding a frame and
/// then having nowhere to put it — and at 60 Hz against 30 fps content, that question is asked
/// twice for every frame that actually changes.
struct VideoSource {
    video: Video,
    format: viewer_io::FormatId,
    timeline: Timeline,
    ahead: Option<viewer_media::VideoFrame>,
    served_pts: Option<u64>,
    index: u32,
    ended: bool,
}

impl VideoSource {
    fn fill(&mut self) -> Result<(), ImportError> {
        if self.ahead.is_none() && !self.ended {
            match self
                .video
                .next_frame()
                .map_err(|e| media_error(&self.format, e))?
            {
                Some(f) => self.ahead = Some(f),
                None => self.ended = true,
            }
        }
        Ok(())
    }

    fn needs_seek(&self, pts_us: u64) -> bool {
        match self.served_pts {
            // Nothing served yet: only worth seeking if the caller opened part way in.
            None => pts_us > SEEK_THRESHOLD_US,
            // Backwards is always a seek -- including the wrap at the end of a loop.
            Some(last) => pts_us < last || pts_us > last + SEEK_THRESHOLD_US,
        }
    }
}

impl FrameSource for VideoSource {
    fn timeline(&self) -> Timeline {
        self.timeline.clone()
    }

    fn frame_at(&mut self, pts_us: u64) -> Result<Option<Frame>, ImportError> {
        if self.needs_seek(pts_us) {
            self.video
                .seek(pts_us)
                .map_err(|e| media_error(&self.format, e))?;
            self.ahead = None;
            self.served_pts = None;
            self.ended = false;
        }

        loop {
            self.fill()?;
            let Some(candidate) = &self.ahead else {
                return Ok(None); // end of stream: hold the last frame
            };
            if self.served_pts.is_some() && candidate.pts_us > pts_us {
                return Ok(None); // the frame already on screen is still the right one
            }

            let frame = self.ahead.take().expect("just checked");
            self.served_pts = Some(frame.pts_us);

            // If the next frame is also already due, this one is late and gets dropped rather
            // than shown. That is what keeps a stall from turning into permanent drift.
            self.fill()?;
            if self.ahead.as_ref().is_some_and(|next| next.pts_us <= pts_us) {
                continue;
            }

            self.index = self.index.saturating_add(1);
            return Ok(Some(Frame {
                index: self.index,
                pts_us: frame.pts_us,
                image: Some(Image {
                    name: "frame".into(),
                    width: frame.width,
                    height: frame.height,
                    rgba: frame.rgba,
                }),
                mesh: None,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_clock_times() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(9_066_667), "0:09");
        assert_eq!(format_duration(95_000_000), "1:35");
        assert_eq!(format_duration(3_725_000_000), "1:02:05");
    }

    /// The seek decision, exercised without a decoder behind it.
    fn should_seek(served: Option<u64>, target: u64) -> bool {
        match served {
            None => target > SEEK_THRESHOLD_US,
            Some(last) => target < last || target > last + SEEK_THRESHOLD_US,
        }
    }

    #[test]
    fn ordinary_forward_playback_never_seeks() {
        let mut served = Some(0u64);
        for step in 0..300 {
            let target = step * 33_333;
            assert!(!should_seek(served, target), "seeked at {target}");
            served = Some(target);
        }
    }

    #[test]
    fn a_loop_back_to_the_start_seeks() {
        // The wrap at the end of a loop is a jump backwards, and decoding forward cannot serve it.
        assert!(should_seek(Some(9_000_000), 0));
    }

    #[test]
    fn a_scrub_across_the_timeline_seeks_but_a_short_stall_does_not() {
        assert!(should_seek(Some(1_000_000), 400_000_000));
        // Dropping half a second of frames -- an alt-tab, a garbage collection -- is cheaper to
        // decode through than to seek.
        assert!(!should_seek(Some(1_000_000), 1_500_000));
    }

    #[test]
    fn opening_at_the_start_does_not_seek_but_opening_part_way_in_does() {
        assert!(!should_seek(None, 0));
        assert!(should_seek(None, 30_000_000));
    }
}
