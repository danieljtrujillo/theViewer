//! The clock, and the thread that decodes against it.
//!
//! Decoding happens on its own thread for one reason: a hardware video decode takes several
//! milliseconds and a slow one takes twenty, and neither is an acceptable pause in a UI that
//! redraws sixty times a second. The interface between the two is deliberately narrow, the UI
//! thread posts the presentation time it wants and picks up whatever has arrived since, and never
//! blocks on either.
//!
//! One request is outstanding at a time. That is not a throughput limit in practice, because the
//! decoder only has to keep up with real time, and it means a scrub that outruns the decoder
//! coalesces into "decode the newest position" instead of queueing up every position it passed
//! through.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use viewer_formats::{Frame, FrameSource};
use viewer_scene::Timeline;

enum Request {
    Frame(u64),
    Stop,
}

enum Reply {
    /// A new frame to show.
    Frame(Box<Frame>),
    /// The frame already on screen is still correct.
    Unchanged,
    Failed(String),
}

pub struct Player {
    requests: Sender<Request>,
    replies: Receiver<Reply>,
    worker: Option<JoinHandle<()>>,
    pub timeline: Timeline,
    /// Presentation time, in microseconds. The single source of truth for what should be visible.
    pub clock_us: u64,
    pub playing: bool,
    pub looping: bool,
    /// Set when a decode is outstanding, so the clock does not queue a second one behind it.
    inflight: bool,
    pub error: Option<String>,
    /// True once the source has run out of frames at the current position.
    pub at_end: bool,
}

impl Player {
    pub fn new(source: Box<dyn FrameSource>, timeline: Timeline) -> Player {
        let (requests, request_rx) = mpsc::channel::<Request>();
        let (reply_tx, replies) = mpsc::channel::<Reply>();

        let worker = std::thread::Builder::new()
            .name("decode".into())
            .spawn(move || {
                let mut source = source;
                while let Ok(request) = request_rx.recv() {
                    let Request::Frame(pts) = request else {
                        break;
                    };
                    // Coalesce: if the UI has already asked for a newer position while this one
                    // was waiting, decode that one instead. A fast scrub generates these by the
                    // dozen and decoding every one of them is wasted work nobody sees.
                    //
                    // A `while let Ok(Request::Frame(..))` here would be a deadlock: `try_recv`
                    // takes the message before the pattern is tested, so a `Stop` arriving mid
                    // scrub would be consumed and dropped, and the thread would park forever on
                    // the next `recv` while the app waited to join it.
                    let mut pts = pts;
                    let mut stopping = false;
                    loop {
                        match request_rx.try_recv() {
                            Ok(Request::Frame(newer)) => pts = newer,
                            Ok(Request::Stop) => {
                                stopping = true;
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    if stopping {
                        break;
                    }
                    let reply = match source.frame_at(pts) {
                        Ok(Some(frame)) => Reply::Frame(Box::new(frame)),
                        Ok(None) => Reply::Unchanged,
                        Err(e) => Reply::Failed(e.to_string()),
                    };
                    // A closed channel means the app is shutting down, which is not an error.
                    if reply_tx.send(reply).is_err() {
                        break;
                    }
                }
            })
            .ok();

        Player {
            requests,
            replies,
            worker,
            timeline,
            clock_us: 0,
            // Video and volumetric capture are meant to play. A picture with two frames is meant
            // to play as well, which is why animation lands here rather than in a still path.
            playing: true,
            looping: true,
            inflight: false,
            error: None,
            at_end: false,
        }
    }

    pub fn duration_us(&self) -> u64 {
        self.timeline.duration_us.max(1)
    }

    pub fn progress(&self) -> f32 {
        (self.clock_us as f64 / self.duration_us() as f64).clamp(0.0, 1.0) as f32
    }

    pub fn toggle(&mut self) {
        // Pressing play at the very end restarts rather than doing nothing, which is what every
        // other player does and what the gesture obviously means.
        if !self.playing && self.clock_us >= self.duration_us() {
            self.clock_us = 0;
        }
        self.playing = !self.playing;
    }

    pub fn seek_to(&mut self, pts_us: u64) {
        self.clock_us = pts_us.min(self.duration_us());
        self.at_end = false;
    }

    pub fn seek_fraction(&mut self, t: f32) {
        self.seek_to((t.clamp(0.0, 1.0) as f64 * self.duration_us() as f64) as u64);
    }

    /// Step by whole frames. Stepping implies pausing: nobody steps through a playing video.
    pub fn step(&mut self, frames: i32) {
        self.playing = false;
        let frame_us = self.frame_us();
        let delta = frame_us as i64 * frames as i64;
        let target = self.clock_us as i64 + delta;
        self.seek_to(target.clamp(0, self.duration_us() as i64) as u64);
    }

    fn frame_us(&self) -> u64 {
        if self.timeline.fps > 0.0 {
            (1_000_000.0 / self.timeline.fps) as u64
        } else {
            33_333
        }
    }

    pub fn current_frame(&self) -> u32 {
        self.timeline.frame_at(self.clock_us as f64 / 1_000_000.0)
    }

    /// Advance the clock and collect whatever the decoder has produced.
    ///
    /// `dt` is wall-clock seconds since the last call. Clamped, because a stalled frame, a
    /// window drag, a display sleep, would otherwise jump the clock forward by seconds and take
    /// a seek to recover from.
    pub fn advance(&mut self, dt: f32) -> Option<Frame> {
        if self.playing {
            let step = (dt.clamp(0.0, 0.25) as f64 * 1_000_000.0) as u64;
            let duration = self.duration_us();
            self.clock_us += step;
            if self.clock_us >= duration {
                if self.looping {
                    self.clock_us %= duration;
                    self.at_end = false;
                } else {
                    self.clock_us = duration;
                    self.playing = false;
                }
            }
        }

        let mut newest = None;
        loop {
            match self.replies.try_recv() {
                Ok(Reply::Frame(frame)) => {
                    self.inflight = false;
                    self.at_end = false;
                    self.error = None;
                    newest = Some(*frame);
                }
                Ok(Reply::Unchanged) => {
                    self.inflight = false;
                    self.at_end = true;
                }
                Ok(Reply::Failed(e)) => {
                    self.inflight = false;
                    self.error = Some(e);
                    // A decoder that has failed will keep failing; stopping the clock turns a
                    // storm of identical errors into one.
                    self.playing = false;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.inflight = false;
                    break;
                }
            }
        }

        if !self.inflight
            && self.error.is_none()
            && self.requests.send(Request::Frame(self.clock_us)).is_ok()
        {
            self.inflight = true;
        }
        newest
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Stop);
        // The worker may be mid-decode. Waiting for it keeps the decoder's own teardown ordered,
        // and a decode is milliseconds, not seconds.
        if let Some(worker) = self.worker.take() {
            // Draining first unblocks a worker parked on a full reply queue.
            while self.replies.recv_timeout(Duration::from_millis(0)).is_ok() {}
            let _ = worker.join();
        }
    }
}

/// Format a presentation time as a clock reading, for the one label the playback bar carries.
pub fn timecode(us: u64) -> String {
    let total = us / 1_000_000;
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use viewer_formats::ImportError;

    struct Fixed(Timeline);

    impl FrameSource for Fixed {
        fn timeline(&self) -> Timeline {
            self.0.clone()
        }
        fn frame_at(&mut self, _pts_us: u64) -> Result<Option<Frame>, ImportError> {
            Ok(None)
        }
    }

    fn player(fps: f32, duration_us: u64) -> Player {
        let timeline = Timeline {
            frame_count: (duration_us as f64 / 1e6 * fps as f64) as u32,
            fps,
            duration_us,
            seekable: true,
        };
        Player::new(Box::new(Fixed(timeline.clone())), timeline)
    }

    #[test]
    fn timecodes_read_as_clock_times() {
        assert_eq!(timecode(0), "0:00");
        assert_eq!(timecode(9_500_000), "0:09");
        assert_eq!(timecode(3_725_000_000), "1:02:05");
    }

    /// Run the clock for a while, one plausible redraw at a time.
    ///
    /// One big `advance` will not do it: the step is clamped so that a stalled frame cannot skip
    /// the clock forward, which means wall-clock time only passes in believable increments.
    fn run(p: &mut Player, seconds: f32) {
        let mut left = seconds;
        while left > 0.0 {
            let step = left.min(0.05);
            p.advance(step);
            left -= step;
        }
    }

    #[test]
    fn the_clock_wraps_when_looping_and_stops_when_not() {
        let mut p = player(30.0, 1_000_000);
        p.looping = true;
        run(&mut p, 1.5);
        assert!(p.playing);
        assert!(p.clock_us < 1_000_000, "{}", p.clock_us);

        let mut p = player(30.0, 1_000_000);
        p.looping = false;
        run(&mut p, 1.5);
        assert!(!p.playing, "playback must stop at the end when not looping");
        assert_eq!(p.clock_us, 1_000_000);
    }

    #[test]
    fn a_stalled_frame_does_not_jump_the_clock() {
        // Ten seconds of wall clock between two redraws -- a window drag, a laptop lid. Letting
        // that through skips most of a clip and forces a seek to recover.
        let mut p = player(30.0, 60_000_000);
        p.advance(10.0);
        assert!(p.clock_us <= 250_000, "{}", p.clock_us);
    }

    #[test]
    fn pressing_play_at_the_end_starts_again_from_the_beginning() {
        let mut p = player(30.0, 1_000_000);
        p.looping = false;
        run(&mut p, 2.0);
        assert!(!p.playing);
        p.toggle();
        assert!(p.playing);
        assert_eq!(p.clock_us, 0);
    }

    #[test]
    fn stepping_pauses_and_moves_by_exactly_one_frame() {
        let mut p = player(25.0, 10_000_000);
        p.seek_to(1_000_000);
        p.step(1);
        assert!(!p.playing);
        assert_eq!(p.clock_us, 1_040_000);
        p.step(-1);
        assert_eq!(p.clock_us, 1_000_000);
        // Stepping back past the start clamps rather than wrapping.
        p.seek_to(0);
        p.step(-5);
        assert_eq!(p.clock_us, 0);
    }

    #[test]
    fn seeking_by_fraction_covers_the_whole_timeline() {
        let mut p = player(30.0, 9_066_667);
        p.seek_fraction(0.0);
        assert_eq!(p.clock_us, 0);
        p.seek_fraction(1.0);
        assert_eq!(p.clock_us, 9_066_667);
        p.seek_fraction(0.5);
        assert!((p.progress() - 0.5).abs() < 1e-3);
        // Out-of-range input from a drag that left the widget must not escape the timeline.
        p.seek_fraction(-3.0);
        assert_eq!(p.clock_us, 0);
        p.seek_fraction(9.0);
        assert_eq!(p.clock_us, 9_066_667);
    }

    #[test]
    fn a_zero_length_timeline_does_not_divide_by_zero() {
        let mut p = player(0.0, 0);
        p.advance(0.016);
        assert!(p.progress().is_finite());
        p.step(3);
        assert!(p.clock_us <= 1);
    }
}
