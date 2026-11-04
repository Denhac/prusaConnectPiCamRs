//! Supervises a camera subprocess streaming MJPEG snapshots or H.264 live video. It normally
//! streams MJPEG and remembers the newest frame; while someone watches live view it streams H.264
//! for them instead, and snapshots pause. Unlike starting a process for each snapshot, the
//! camera keeps running, so exposure and white balance stay settled between uploads.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TrySendError};

use super::h264::AccessUnits;
use super::jpeg::Splitter;
use super::{FrameSource, Paused};
use super::{rpicam, usb};
use crate::config::SourceConfig;

/// Frames skipped after camera process starts, while auto exposure and white balance settle.
const SETTLE_FRAMES: u64 = 3;
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);
/// How long camera process may run without producing a frame before it's restarted. It can stay
/// alive without output, for example retrying a camera that times out or with a stuck encoder.
const MIN_STALL_LIMIT: Duration = Duration::from_secs(30);
const STALL_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Live frames buffered for a viewer that falls behind; beyond this, frames are dropped.
const LIVE_QUEUE: usize = 30;

/// An H.264 stream for live view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveStream {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    /// Bits per second.
    pub bitrate: u32,
}

#[derive(Clone)]
pub struct CameraSource {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    new_frame: Condvar,
}

#[derive(Default)]
struct State {
    /// Snapshot frames received since this source was created.
    seq: u64,
    /// Snapshot frames received from the current camera process.
    since_start: u64,
    latest: Option<Arc<[u8]>>,
    /// Why camera process last stopped, reported when no frames arrive.
    last_error: Option<String>,
    /// Set while someone watches live view.
    live: Option<Live>,
    /// Bumped on every switch between snapshots and live view.
    generation: u64,
    /// The running camera process.
    pid: Option<u32>,
    /// When the running process started or last produced a frame.
    last_frame: Option<Instant>,
    /// How long the running process may go without producing a frame.
    stall_limit: Duration,
}

struct Live {
    /// The generation that started this stream.
    id: u64,
    stream: LiveStream,
    frames: mpsc::Sender<Bytes>,
}

/// A live stream's frames, one access unit each, and the id that stops it.
pub struct LiveFrames {
    pub id: u64,
    pub frames: mpsc::Receiver<Bytes>,
}

impl CameraSource {
    pub fn start(config: &SourceConfig) -> Result<Self> {
        if matches!(config, SourceConfig::File(_)) {
            bail!("a file source is not a streaming camera");
        }
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            new_frame: Condvar::new(),
        });
        let config = config.clone();
        let thread_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name(format!("capture-{}", source_name(&config)))
            .spawn(move || supervise(&config, &thread_shared))
            .context("starting the camera process thread")?;
        let thread_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("capture-watchdog".into())
            .spawn(move || restart_when_stalled(&thread_shared))
            .context("starting the camera process watchdog thread")?;
        Ok(Self { shared })
    }

    /// Switches the camera to H.264 for a live viewer, replacing any stream already running.
    /// Snapshots pause until [`CameraSource::stop_live`] or until the frames are dropped.
    pub fn start_live(&self, stream: LiveStream) -> LiveFrames {
        let (sender, frames) = mpsc::channel(LIVE_QUEUE);
        let mut state = self.shared.state.lock().unwrap();
        let id = self.shared.switch(&mut state, Some((stream, sender)));
        LiveFrames { id, frames }
    }

    /// Ends live stream `id` and resumes snapshots, unless another stream replaced it already.
    pub fn stop_live(&self, id: u64) {
        let mut state = self.shared.state.lock().unwrap();
        if state.live.as_ref().is_some_and(|live| live.id == id) {
            self.shared.switch(&mut state, None);
        }
    }
}

impl Shared {
    /// Changes what camera process streams and returns the new generation. The running process is
    /// stopped and the supervisor starts it again in the new mode.
    fn switch(&self, state: &mut State, live: Option<(LiveStream, mpsc::Sender<Bytes>)>) -> u64 {
        state.generation += 1;
        let id = state.generation;
        state.live = live.map(|(stream, frames)| Live { id, stream, frames });
        if let Some(pid) = state.pid {
            // SAFETY: kill only sends a signal. `pid` is our child, cleared under the state lock
            // before the child is reaped, so it can't refer to a reused process id.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
        self.new_frame.notify_all();
        id
    }
}

impl FrameSource for CameraSource {
    fn capture(&self, timeout: Duration) -> Result<Arc<[u8]>> {
        let state = self.shared.state.lock().unwrap();
        let after = state.seq;
        let (state, wait) = self
            .shared
            .new_frame
            .wait_timeout_while(state, timeout, |s| {
                s.live.is_none() && (s.seq <= after || s.since_start <= SETTLE_FRAMES)
            })
            .unwrap();
        if state.live.is_some() {
            return Err(Paused.into());
        }
        if wait.timed_out() {
            match &state.last_error {
                Some(error) => bail!("no frame from camera process within {timeout:?} ({error})"),
                None => bail!("no frame from camera process within {timeout:?}"),
            }
        }
        Ok(Arc::clone(state.latest.as_ref().expect("a frame arrived")))
    }
}

/// How long a process streaming at `framerate` may go without a frame. Long enough for very low
/// frame rates, and for camera process to start and produce its first frame.
fn stall_limit(framerate: f32) -> Duration {
    MIN_STALL_LIMIT.max(Duration::from_secs_f32(3.0 / framerate))
}

/// Kills a camera process that's still running but has stopped producing frames. [`supervise`] then
/// starts a new one, which rebuilds everything a process owns: camera state, the encoder and
/// its device handles. A hardware fault still needs a person; the log at least says so.
fn restart_when_stalled(shared: &Shared) {
    loop {
        thread::sleep(STALL_CHECK_INTERVAL);
        let mut state = shared.state.lock().unwrap();
        let (Some(pid), Some(last_frame)) = (state.pid, state.last_frame) else {
            continue;
        };
        if last_frame.elapsed() > state.stall_limit {
            let error = format!(
                "camera process is running but sent no frames for {}s; check the camera and its cable",
                state.stall_limit.as_secs()
            );
            log::warn!("{error}");
            state.last_error = Some(error);
            state.last_frame = Some(Instant::now());
            // SAFETY: kill only sends a signal. `pid` is our child, cleared under the state lock
            // before the child is reaped, so it can't refer to a reused process id. SIGKILL
            // because a stalled process may not get as far as handling SIGTERM.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// Runs camera process for as long as the program runs, restarting it when it stops or when the
/// camera switches between snapshots and live view.
fn supervise(config: &SourceConfig, shared: &Shared) {
    let mut delay = Duration::from_secs(1);
    loop {
        let (generation, args, frames, limit) = {
            let state = shared.state.lock().unwrap();
            match &state.live {
                Some(live) => (
                    state.generation,
                    live_args(config, live.stream),
                    Some(live.frames.clone()),
                    stall_limit(live.stream.framerate as f32),
                ),
                None => (
                    state.generation,
                    snapshot_args(config),
                    None,
                    stall_limit(framerate(config)),
                ),
            }
        };
        let started = Instant::now();
        let result = stream(command(config), &args, shared, generation, frames, limit);
        let switched = {
            let mut state = shared.state.lock().unwrap();
            state.since_start = 0;
            state.generation != generation
        };
        if switched {
            delay = Duration::from_secs(1);
            continue;
        }
        let error = match result {
            Ok(status) => format!("{} stopped ({status})", command(config)),
            Err(e) => format!("{e:#}"),
        };
        log::warn!("{error}; restarting it in {delay:?}");
        shared.state.lock().unwrap().last_error = Some(error);
        thread::sleep(delay);
        // A long run means the camera was healthy, so start backing off from scratch.
        delay = if started.elapsed() > Duration::from_secs(60) {
            Duration::from_secs(1)
        } else {
            (delay * 2).min(MAX_RESTART_DELAY)
        };
    }
}

fn stream(
    command: &str,
    args: &[String],
    shared: &Shared,
    generation: u64,
    live: Option<mpsc::Sender<Bytes>>,
    stall_limit: Duration,
) -> Result<ExitStatus> {
    let mut cmd = Command::new(command);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped());
    // Have the kernel stop camera process when the thread that started it ends, so it never keeps
    // holding the camera after this program exits or is killed.
    // SAFETY: prctl is async-signal-safe and touches no memory shared with the parent.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {command}"))?;
    log::info!("started {command} {}", args.join(" "));
    {
        let mut state = shared.state.lock().unwrap();
        state.pid = Some(child.id());
        state.last_frame = Some(Instant::now());
        state.stall_limit = stall_limit;
        // The mode changed while this process was starting, so it's already out of date.
        if state.generation != generation {
            let _ = child.kill();
        }
    }

    let stdout = child.stdout.take().expect("stdout is piped");
    let result = match live {
        Some(frames) => read_live(stdout, shared, &frames),
        None => read_snapshots(stdout, shared),
    };
    shared.state.lock().unwrap().pid = None;
    // Usually a no-op: the stream ends because camera process exited.
    let _ = child.kill();
    let status = child
        .wait()
        .with_context(|| format!("waiting for {command}"))?;
    result.map(|()| status)
}

fn read_snapshots(mut stdout: impl Read, shared: &Shared) -> Result<()> {
    let mut splitter = Splitter::default();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = stdout
            .read(&mut buf)
            .context("reading from camera process")?;
        if n == 0 {
            return Ok(());
        }
        for frame in splitter.push(&buf[..n]) {
            let mut state = shared.state.lock().unwrap();
            state.seq += 1;
            state.since_start += 1;
            state.latest = Some(frame.into());
            state.last_frame = Some(Instant::now());
            state.last_error = None;
            shared.new_frame.notify_all();
        }
    }
}

fn read_live(mut stdout: impl Read, shared: &Shared, frames: &mpsc::Sender<Bytes>) -> Result<()> {
    let mut units = AccessUnits::default();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = stdout
            .read(&mut buf)
            .context("reading from camera process")?;
        if n == 0 {
            return Ok(());
        }
        for unit in units.push(&buf[..n]) {
            shared.state.lock().unwrap().last_frame = Some(Instant::now());
            match frames.try_send(Bytes::from(unit)) {
                // A viewer that falls behind loses frames; the next keyframe recovers it.
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Closed(_)) => {
                    // The viewer is gone without stopping live view; go back to snapshots,
                    // unless another viewer's stream has taken over.
                    let mut state = shared.state.lock().unwrap();
                    if state
                        .live
                        .as_ref()
                        .is_some_and(|l| l.frames.same_channel(frames))
                    {
                        shared.switch(&mut state, None);
                    }
                    return Ok(());
                }
            }
        }
    }
}

fn command(config: &SourceConfig) -> &str {
    match config {
        SourceConfig::Rpicam(c) => &c.command,
        SourceConfig::Usb(c) => &c.command,
        SourceConfig::File(_) => unreachable!("not a streaming camera"),
    }
}

fn source_name(config: &SourceConfig) -> String {
    match config {
        SourceConfig::Rpicam(c) => format!("csi-{}", c.index),
        SourceConfig::Usb(c) => format!("usb-{}", c.device),
        SourceConfig::File(_) => unreachable!("not a streaming camera"),
    }
}

fn framerate(config: &SourceConfig) -> f32 {
    match config {
        SourceConfig::Rpicam(c) => c.framerate,
        SourceConfig::Usb(c) => c.framerate as f32,
        SourceConfig::File(_) => unreachable!("not a streaming camera"),
    }
}

fn snapshot_args(config: &SourceConfig) -> Vec<String> {
    match config {
        SourceConfig::Rpicam(c) => rpicam::snapshot_args(c),
        SourceConfig::Usb(c) => usb::snapshot_args(c),
        SourceConfig::File(_) => unreachable!("not a streaming camera"),
    }
}

fn live_args(config: &SourceConfig, stream: LiveStream) -> Vec<String> {
    match config {
        SourceConfig::Rpicam(c) => rpicam::live_args(c, stream),
        SourceConfig::Usb(c) => usb::live_args(c, stream),
        SourceConfig::File(_) => unreachable!("not a streaming camera"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_several_frames_before_calling_a_stall() {
        assert_eq!(stall_limit(1.0), MIN_STALL_LIMIT);
        assert_eq!(stall_limit(12.0), MIN_STALL_LIMIT);
        assert_eq!(stall_limit(0.05), Duration::from_secs(60));
    }
}
