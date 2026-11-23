//! One camera's live video, shared by every viewer. The hub is the camera's only video consumer:
//! it runs one H.264 stream for all its subscribers, whichever output they belong to, and knows
//! nothing about any of them beyond the caps they ask for.
//!
//! - The encoder runs at the lowest of the subscribers' caps, so no viewer is sent more than it
//!   asked for. A subscriber that needs less restarts it lower. Nothing restarts it higher while
//!   anyone watches, so the stream never flaps, and a restart only costs everyone about 1.3 s.
//! - A subscriber's queue stays open across restarts. It gets nothing until a keyframe, because
//!   the frames before one refer to frames it never got, and drops frames when it falls behind.
//! - When the last subscriber leaves, the stream lingers for [`LINGER`] before the camera goes
//!   back to stills, so a viewer who reconnects doesn't wait for two restarts.
//! - The newest keyframe is kept, for taking a still from the video later.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::source::h264::is_keyframe;
use crate::source::rpicam::{LiveFrames, LiveStream, Rpicam};

/// How long the stream outlives its last subscriber. Going back to stills and starting video again
/// costs about 8 s, while a viewer's reconnect takes a few.
const LINGER: Duration = Duration::from_secs(10);
/// Frames buffered for a subscriber that falls behind; beyond this, frames are dropped.
const QUEUE: usize = 30;
/// How often the hub looks at its subscribers when the camera sends nothing.
const TICK: Duration = Duration::from_millis(500);

/// The most a subscriber wants of the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub max_width: u32,
    pub max_fps: u32,
    /// Bits per second.
    pub max_bitrate: u32,
}

/// One access unit (a frame) of the stream.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Annex B, with start codes, and the parameter sets inline before each keyframe.
    pub data: Bytes,
    /// When the camera produced it, by the stream's own clock (see [`Clock`]). Viewers time
    /// their stream by this.
    pub captured: Instant,
    pub keyframe: bool,
}

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    label: String,
    camera: Rpicam,
    /// How long the stream outlives its last subscriber: [`LINGER`], or less in tests.
    linger: Duration,
    /// The size of the sensor mode video reads, which gives every stream its shape.
    sensor: (u32, u32),
    state: Mutex<State>,
    /// Signalled when a subscriber arrives or leaves.
    changed: Condvar,
    /// Cleared if the hub's thread ends, which only a bug can make it do.
    alive: AtomicBool,
}

#[derive(Default)]
struct State {
    subscribers: Vec<Subscriber>,
    next_id: u64,
    keyframe: Option<Frame>,
    /// What the encoder runs, while it runs.
    streaming: Option<LiveStream>,
}

struct Subscriber {
    id: u64,
    caps: Caps,
    frames: mpsc::Sender<Frame>,
    /// Set once it has been sent a keyframe.
    synced: bool,
}

/// A subscriber's frames. Dropping it unsubscribes.
pub struct Subscription {
    id: u64,
    pub frames: mpsc::Receiver<Frame>,
    inner: Arc<Inner>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        state.subscribers.retain(|s| s.id != self.id);
        self.inner.changed.notify_all();
    }
}

impl Hub {
    /// Starts the hub for `camera`, whose video reads a sensor mode of size `sensor`. `label`
    /// prefixes its log lines.
    pub fn start(label: &str, camera: Rpicam, sensor: (u32, u32)) -> Result<Self> {
        Self::start_with(label, camera, sensor, LINGER)
    }

    fn start_with(
        label: &str,
        camera: Rpicam,
        sensor: (u32, u32),
        linger: Duration,
    ) -> Result<Self> {
        let inner = Arc::new(Inner {
            label: label.to_owned(),
            camera,
            linger,
            sensor,
            state: Mutex::default(),
            changed: Condvar::new(),
            alive: AtomicBool::new(true),
        });
        let pump_inner = Arc::clone(&inner);
        thread::Builder::new()
            .name(format!("hub-{label}"))
            .spawn(move || {
                // Clears `alive` however the thread ends, panics included.
                struct Alive<'a>(&'a AtomicBool);
                impl Drop for Alive<'_> {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::SeqCst);
                    }
                }
                let _alive = Alive(&pump_inner.alive);
                pump(&pump_inner);
            })
            .context("starting the video hub")?;
        Ok(Self { inner })
    }

    /// Subscribes to the stream. The first frame is a keyframe, at most a second away when
    /// the stream is running, and about 1.3 s when it has to start or restart.
    pub fn subscribe(&self, caps: Caps) -> Subscription {
        let (sender, frames) = mpsc::channel(QUEUE);
        let mut state = self.inner.state.lock().unwrap();
        state.next_id += 1;
        let id = state.next_id;
        state.subscribers.push(Subscriber {
            id,
            caps,
            frames: sender,
            synced: false,
        });
        self.inner.changed.notify_all();
        Subscription {
            id,
            frames,
            inner: Arc::clone(&self.inner),
        }
    }

    /// What the encoder runs, while video runs, and how many subscribers watch it.
    pub fn stream(&self) -> (Option<LiveStream>, usize) {
        let state = self.inner.state.lock().unwrap();
        (state.streaming, state.subscribers.len())
    }

    /// Whether the hub's thread is still running. If it isn't, nobody gets video any more.
    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::SeqCst)
    }

    /// The newest keyframe, while video runs. Unused until stills are taken from video.
    #[allow(dead_code)]
    pub fn keyframe(&self) -> Option<Frame> {
        self.inner.state.lock().unwrap().keyframe.clone()
    }
}

/// What the hub does next.
#[derive(Debug, PartialEq)]
enum Action {
    /// Nothing runs and nobody watches: sleep until somebody subscribes.
    Wait,
    /// Start or restart the encoder with these settings.
    Run(LiveStream),
    /// Leave the encoder as it is.
    Keep,
    /// Stop the stream, and go back to stills.
    Stop,
}

/// Decides what to do about the stream. `running` is what the encoder runs, `idle_for` how long
/// it has run with no subscribers, and `linger` how long that may last.
fn decide(
    caps: &[Caps],
    running: Option<LiveStream>,
    idle_for: Option<Duration>,
    linger: Duration,
    sensor: (u32, u32),
) -> Action {
    match (caps.is_empty(), running) {
        (true, None) => Action::Wait,
        (true, Some(_)) if idle_for.is_some_and(|idle| idle >= linger) => Action::Stop,
        (true, Some(_)) => Action::Keep,
        (false, None) => Action::Run(choose(caps, sensor)),
        // The first subscriber after everyone left gets what it asks for, up or down.
        (false, Some(running)) if idle_for.is_some() => {
            let wanted = choose(caps, sensor);
            if wanted == running {
                Action::Keep
            } else {
                Action::Run(wanted)
            }
        }
        (false, Some(running)) if caps.iter().all(|c| fits(running, *c)) => Action::Keep,
        // Somebody needs less: go lower, never higher than what runs now.
        (false, Some(running)) => {
            let wanted = choose(caps, sensor);
            Action::Run(LiveStream {
                width: wanted.width.min(running.width),
                height: wanted.height.min(running.height),
                framerate: wanted.framerate.min(running.framerate),
                bitrate: wanted.bitrate.min(running.bitrate),
            })
        }
    }
}

fn fits(stream: LiveStream, caps: Caps) -> bool {
    stream.width <= caps.max_width
        && stream.framerate <= caps.max_fps
        && stream.bitrate <= caps.max_bitrate
}

/// The stream that gives every subscriber no more than it asked for: the lowest of their caps,
/// in the shape of the sensor mode so nothing is cropped, and never wider than the mode itself.
fn choose(caps: &[Caps], (sensor_width, sensor_height): (u32, u32)) -> LiveStream {
    let lowest = |f: fn(&Caps) -> u32| caps.iter().map(f).min().unwrap_or(u32::MAX);
    let width = lowest(|c| c.max_width).min(sensor_width);
    let height = u64::from(width) * u64::from(sensor_height) / u64::from(sensor_width);
    LiveStream {
        // The H.264 encoder needs even dimensions.
        width: width & !1,
        height: height as u32 & !1,
        framerate: lowest(|c| c.max_fps).max(1),
        bitrate: lowest(|c| c.max_bitrate),
    }
}

/// Gives frames evenly spaced times. The camera produces frames at a steady rate, but they're
/// read from a pipe, and two can arrive in one read; stamped by arrival, they could share a time,
/// which players reject. So each frame's time is the stream's start plus its count of frame
/// intervals, set back to the arrival time whenever the two drift more than [`Clock::SLACK`]
/// apart, as after dropped frames or a stall.
struct Clock {
    /// When the first frame counted from arrived; none before the stream's first frame.
    start: Option<Instant>,
    frames: u32,
    interval: Duration,
}

impl Clock {
    const SLACK: Duration = Duration::from_millis(500);

    fn new(framerate: u32) -> Self {
        Self {
            start: None,
            frames: 0,
            interval: Duration::from_secs(1) / framerate.max(1),
        }
    }

    fn stamp(&mut self, arrived: Instant) -> Instant {
        if let Some(start) = self.start {
            let expected = start + self.interval * self.frames;
            let drift = if arrived > expected {
                arrived - expected
            } else {
                expected - arrived
            };
            if drift <= Self::SLACK {
                self.frames += 1;
                return expected;
            }
        }
        self.start = Some(arrived);
        self.frames = 1;
        arrived
    }
}

/// Runs the encoder for the subscribers, and hands each frame to every one of them.
fn pump(inner: &Inner) {
    let mut running: Option<(LiveStream, LiveFrames)> = None;
    let mut clock = Clock::new(1);
    let mut idle_since: Option<Instant> = None;
    loop {
        let action = {
            let mut state = inner.state.lock().unwrap();
            if state.subscribers.is_empty() {
                idle_since.get_or_insert_with(Instant::now);
            }
            let caps: Vec<Caps> = state.subscribers.iter().map(|s| s.caps).collect();
            let action = decide(
                &caps,
                running.as_ref().map(|(stream, _)| *stream),
                idle_since.map(|since| since.elapsed()),
                inner.linger,
                inner.sensor,
            );
            if action == Action::Wait {
                drop(
                    inner
                        .changed
                        .wait_while(state, |s| s.subscribers.is_empty())
                        .unwrap(),
                );
                idle_since = None;
                continue;
            }
            if !caps.is_empty() {
                idle_since = None;
            }
            if action == Action::Stop {
                state.keyframe = None;
                state.streaming = None;
            }
            action
        };
        match action {
            Action::Wait => unreachable!("handled above"),
            Action::Run(stream) => {
                let viewers = inner.state.lock().unwrap().subscribers.len();
                log::info!(
                    "[{}] video: {}x{} at {} fps, {} kbit/s, for {viewers} viewer(s)",
                    inner.label,
                    stream.width,
                    stream.height,
                    stream.framerate,
                    stream.bitrate / 1000
                );
                // Replaces the stream that ran before, if any. The new stream's first frame
                // starts its clock.
                running = Some((stream, inner.camera.start_live(stream)));
                inner.state.lock().unwrap().streaming = Some(stream);
                clock = Clock::new(stream.framerate);
            }
            Action::Stop => {
                if let Some((_, frames)) = running.take() {
                    inner.camera.stop_live(frames.id);
                }
                log::info!("[{}] video stopped; back to stills", inner.label);
                continue;
            }
            Action::Keep => {}
        }
        let Some((_, live)) = &running else {
            continue;
        };
        match live.frames.recv_timeout(TICK) {
            Ok(frame) => hand_out(inner, frame.data, clock.stamp(frame.captured)),
            Err(RecvTimeoutError::Timeout) => {}
            // The camera dropped this stream; the next round starts another.
            Err(RecvTimeoutError::Disconnected) => {
                running = None;
                inner.state.lock().unwrap().streaming = None;
            }
        }
    }
}

fn hand_out(inner: &Inner, data: Bytes, captured: Instant) {
    let frame = Frame {
        keyframe: is_keyframe(&data),
        data,
        captured,
    };
    let mut state = inner.state.lock().unwrap();
    if frame.keyframe {
        state.keyframe = Some(frame.clone());
    }
    state.subscribers.retain_mut(|subscriber| {
        subscriber.synced |= frame.keyframe;
        if !subscriber.synced {
            return !subscriber.frames.is_closed();
        }
        // One that falls behind loses frames until it catches up; one that's gone is dropped.
        !matches!(
            subscriber.frames.try_send(frame.clone()),
            Err(TrySendError::Closed(_))
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE_2: (u32, u32) = (1640, 1232);
    const MODULE_3: (u32, u32) = (2304, 1296);

    const SD: Caps = Caps {
        max_width: 640,
        max_fps: 12,
        max_bitrate: 900_000,
    };
    const HD: Caps = Caps {
        max_width: 1280,
        max_fps: 12,
        max_bitrate: 2_500_000,
    };
    const FHD: Caps = Caps {
        max_width: 1920,
        max_fps: 24,
        max_bitrate: 6_500_000,
    };

    fn size(caps: Caps, sensor: (u32, u32)) -> (u32, u32) {
        let stream = choose(&[caps], sensor);
        (stream.width, stream.height)
    }

    #[test]
    fn spaces_frames_evenly_and_follows_long_gaps() {
        let mut clock = Clock::new(10);
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        // The first frame starts the clock. Frames read late, or two in one read, still get
        // 100 ms apart.
        assert_eq!(clock.stamp(at(0)), at(0));
        assert_eq!(clock.stamp(at(180)), at(100));
        assert_eq!(clock.stamp(at(181)), at(200));
        assert_eq!(clock.stamp(at(300)), at(300));
        // After a stall, time follows the camera again.
        assert_eq!(clock.stamp(at(2000)), at(2000));
        assert_eq!(clock.stamp(at(2110)), at(2100));
    }

    #[test]
    fn streams_take_the_shape_of_the_sensor_mode() {
        assert_eq!(size(SD, MODULE_2), (640, 480));
        assert_eq!(size(HD, MODULE_2), (1280, 960));
        assert_eq!(size(FHD, MODULE_2), (1640, 1232));
        assert_eq!(size(SD, MODULE_3), (640, 360));
        assert_eq!(size(HD, MODULE_3), (1280, 720));
        assert_eq!(size(FHD, MODULE_3), (1920, 1080));
    }

    #[test]
    fn gives_nobody_more_than_they_asked_for() {
        let low_fps_hd = Caps { max_fps: 10, ..HD };
        assert_eq!(
            choose(&[FHD, low_fps_hd], MODULE_3),
            LiveStream {
                width: 1280,
                height: 720,
                framerate: 10,
                bitrate: 2_500_000
            }
        );
    }

    #[test]
    fn restarts_only_to_go_lower_while_anyone_watches() {
        let hd = choose(&[HD], MODULE_3);
        let sd = choose(&[SD], MODULE_3);
        // The first viewer starts it at once.
        assert_eq!(decide(&[HD], None, None, LINGER, MODULE_3), Action::Run(hd));
        // A viewer who wants as much or more joins as it is.
        assert_eq!(
            decide(&[HD, FHD], Some(hd), None, LINGER, MODULE_3),
            Action::Keep
        );
        // One who wants less brings it down.
        assert_eq!(
            decide(&[HD, SD], Some(hd), None, LINGER, MODULE_3),
            Action::Run(sd)
        );
        // When that viewer leaves, it stays down.
        assert_eq!(
            decide(&[HD], Some(sd), None, LINGER, MODULE_3),
            Action::Keep
        );
        // Lowering one setting never raises another past what runs.
        let slow = LiveStream { framerate: 5, ..hd };
        assert_eq!(
            decide(&[FHD, SD], Some(slow), None, LINGER, MODULE_3),
            Action::Run(LiveStream { framerate: 5, ..sd })
        );
    }

    #[test]
    fn lingers_before_going_back_to_stills() {
        let hd = choose(&[HD], MODULE_3);
        assert_eq!(decide(&[], None, None, LINGER, MODULE_3), Action::Wait);
        let soon = Some(Duration::from_secs(3));
        assert_eq!(decide(&[], Some(hd), soon, LINGER, MODULE_3), Action::Keep);
        assert_eq!(
            decide(&[], Some(hd), Some(LINGER), LINGER, MODULE_3),
            Action::Stop
        );
        // A viewer who comes back in time gets what it asks for, up or down.
        assert_eq!(
            decide(&[HD], Some(hd), soon, LINGER, MODULE_3),
            Action::Keep
        );
        let fhd = choose(&[FHD], MODULE_3);
        assert_eq!(
            decide(&[FHD], Some(hd), soon, LINGER, MODULE_3),
            Action::Run(fhd)
        );
    }

    /// A stand-in for rpicam-vid that logs its arguments and, when asked for H.264, writes a
    /// stream of made-up NAL units: a keyframe (SPS, PPS, IDR slice) then three P frames, over
    /// and over, 20 ms apart. Asked for MJPEG, it just waits.
    const FAKE_CAMERA: &str = r#"#!/bin/sh
echo "$*" >> "$0.log"
case "$*" in
  *h264*) ;;
  *) exec sleep 600 ;;
esac
while :; do
  printf '\000\000\000\001\047\102\200\051\252\000\000\000\001\050\316\074\200'
  printf '\000\000\000\001\045\210\204\000\021'
  sleep 0.02
  for p in 1 2 3; do
    printf '\000\000\000\001\041\232\002\003'
    sleep 0.02
  done
done
"#;

    /// Waits up to 5 s for the next frame.
    fn next(subscription: &mut Subscription) -> Frame {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(frame) = subscription.frames.try_recv() {
                return frame;
            }
            assert!(Instant::now() < deadline, "no frame within 5 s");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits up to 5 s for a keyframe sent after this call.
    fn next_keyframe(subscription: &mut Subscription) -> Frame {
        while subscription.frames.try_recv().is_ok() {}
        loop {
            let frame = next(subscription);
            if frame.keyframe {
                return frame;
            }
        }
    }

    #[test]
    fn shares_one_stream_and_restarts_only_lower() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("pcc-hub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-rpicam-vid");
        std::fs::write(&script, FAKE_CAMERA).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.join("fake-rpicam-vid.log");
        let invocations = || std::fs::read_to_string(&log).unwrap_or_default();
        let widths = || -> Vec<String> {
            invocations()
                .lines()
                .map(|line| {
                    let codec = if line.contains("h264") {
                        "h264"
                    } else {
                        "mjpeg"
                    };
                    let width = line
                        .split("--width ")
                        .nth(1)
                        .unwrap_or("")
                        .split(' ')
                        .next();
                    format!("{codec} {}", width.unwrap_or(""))
                })
                .collect()
        };

        let config = crate::config::RpicamConfig {
            command: script.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let camera = Rpicam::start(&config).unwrap();
        let hub = Hub::start_with("test", camera, MODULE_3, Duration::from_millis(300)).unwrap();

        // The first viewer starts the stream, and its first frame is a keyframe.
        let mut first = hub.subscribe(HD);
        assert!(next(&mut first).keyframe);
        assert!(hub.keyframe().is_some());

        // A second viewer who wants less brings the stream down for both. The first keeps its
        // queue across the restart, and both go on getting frames.
        let mut second = hub.subscribe(SD);
        assert!(next(&mut second).keyframe);
        next_keyframe(&mut first);
        next(&mut second);

        // When the second leaves, the stream stays down rather than restarting higher.
        drop(second);
        thread::sleep(Duration::from_millis(200));
        next(&mut first);

        // When the last leaves, the camera goes back to stills once the linger is over.
        drop(first);
        thread::sleep(Duration::from_millis(1000));
        // (The camera may not have started on stills yet when the first viewer arrived.)
        let runs: Vec<String> = widths()
            .into_iter()
            .skip_while(|run| run.starts_with("mjpeg"))
            .collect();
        assert_eq!(
            runs,
            ["h264 1280", "h264 640", "mjpeg 2048"],
            "{}",
            invocations()
        );
        assert!(hub.keyframe().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
