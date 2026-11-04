//! Supervises a camera subprocess streaming MJPEG snapshots or H.264 live video. It normally
//! streams MJPEG and remembers the newest frame; while someone watches live view it streams H.264
//! for them instead, and snapshots pause. Unlike starting a process for each snapshot, the
//! camera keeps running, so exposure and white balance stay settled between uploads.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
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
const LIVE_QUEUE: usize = 1;

/// An H.264 stream for live view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveStream {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    /// Bits per second.
    pub bitrate: u32,
}

/// Encoded media with its actual production time, independent of the requested frame rate.
pub struct LiveFrame {
    pub data: Bytes,
    pub produced_at: Instant,
    /// When this JPEG entered the capture reader; absent for direct CSI/raw streams.
    pub captured_at: Option<Instant>,
    pub keyframe: bool,
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
    pids: Vec<u32>,
    /// When the running process started or last produced a frame.
    last_frame: Option<Instant>,
    /// How long the running process may go without producing a frame.
    stall_limit: Duration,
}

struct Live {
    /// The generation that started this stream.
    id: u64,
    stream: LiveStream,
    frames: mpsc::Sender<LiveFrame>,
}

/// A live stream's frames, one access unit each, and the id that stops it.
pub struct LiveFrames {
    pub id: u64,
    pub frames: mpsc::Receiver<LiveFrame>,
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
    fn switch(
        &self,
        state: &mut State,
        live: Option<(LiveStream, mpsc::Sender<LiveFrame>)>,
    ) -> u64 {
        state.generation += 1;
        let id = state.generation;
        state.live = live.map(|(stream, frames)| Live { id, stream, frames });
        for &pid in &state.pids {
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
        let Some(last_frame) = state.last_frame else {
            continue;
        };
        if !state.pids.is_empty() && last_frame.elapsed() > state.stall_limit {
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
            for &pid in &state.pids {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
    }
}

/// Runs camera process for as long as the program runs, restarting it when it stops or when the
/// camera switches between snapshots and live view.
fn supervise(config: &SourceConfig, shared: &Shared) {
    let mut delay = Duration::from_secs(1);
    loop {
        let (generation, args, frames, limit, live_stream) = {
            let state = shared.state.lock().unwrap();
            match &state.live {
                Some(live) => (
                    state.generation,
                    live_args(config, live.stream),
                    Some(live.frames.clone()),
                    stall_limit(live.stream.framerate as f32),
                    Some(live.stream),
                ),
                None => (
                    state.generation,
                    snapshot_args(config),
                    None,
                    stall_limit(framerate(config)),
                    None,
                ),
            }
        };
        let started = Instant::now();
        let result = match (config, live_stream, frames) {
            (SourceConfig::Usb(c), Some(live), Some(frames)) if c.input_format == "mjpeg" => {
                stream_usb_live(c, live, shared, generation, &frames, limit)
            }
            (_, _, frames) => stream(command(config), &args, shared, generation, frames, limit),
        };
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
    live: Option<mpsc::Sender<LiveFrame>>,
    stall_limit: Duration,
) -> Result<ExitStatus> {
    let mut child = spawn_child(
        command,
        args,
        Stdio::null(),
        shared,
        generation,
        stall_limit,
    )?;
    let stdout = child
        .child
        .as_mut()
        .unwrap()
        .stdout
        .take()
        .expect("stdout is piped");
    let result = match live {
        Some(frames) => read_live(stdout, shared, &frames),
        None => read_snapshots(stdout, shared),
    };
    let status = child.finish()?;
    result.map(|()| status)
}

/// Keeps every child registered until it is killed and about to be reaped. Both members of
/// the USB capture/encoder pipeline must stop on mode changes, stalls and error paths.
struct CameraChild<'a> {
    child: Option<Child>,
    shared: &'a Shared,
}

impl CameraChild<'_> {
    fn kill(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
    }

    fn finish(&mut self) -> Result<ExitStatus> {
        let mut child = self.child.take().expect("child not yet reaped");
        self.shared
            .state
            .lock()
            .unwrap()
            .pids
            .retain(|&pid| pid != child.id());
        let _ = child.kill();
        child.wait().context("waiting for camera subprocess")
    }
}

impl Drop for CameraChild<'_> {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.finish();
        }
    }
}

fn spawn_child<'a>(
    command: &str,
    args: &[String],
    stdin: Stdio,
    shared: &'a Shared,
    generation: u64,
    stall_limit: Duration,
) -> Result<CameraChild<'a>> {
    let mut cmd = Command::new(command);
    cmd.args(args).stdin(stdin).stdout(Stdio::piped());
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
        state.pids.push(child.id());
        state.last_frame = Some(Instant::now());
        state.stall_limit = stall_limit;
        // The mode changed while this process was starting, so it's already out of date.
        if state.generation != generation {
            let _ = child.kill();
        }
    }

    Ok(CameraChild {
        child: Some(child),
        shared,
    })
}

/// Capture compressed JPEGs continuously, replacing the previous one. Encoder backpressure
/// blocks only the feeder, never webcam draining. The next encode always gets the newest JPEG.
fn stream_usb_live(
    config: &crate::config::UsbConfig,
    live: LiveStream,
    shared: &Shared,
    generation: u64,
    frames: &mpsc::Sender<LiveFrame>,
    limit: Duration,
) -> Result<ExitStatus> {
    let mut capture = spawn_child(
        &config.command,
        &usb::snapshot_args(config),
        Stdio::null(),
        shared,
        generation,
        limit,
    )?;
    let mut encoder = spawn_child(
        &config.command,
        &usb::pipe_live_args(config, live),
        Stdio::piped(),
        shared,
        generation,
        limit,
    )?;
    let input = capture.child.as_mut().unwrap().stdout.take().unwrap();
    let output = encoder.child.as_mut().unwrap().stdout.take().unwrap();
    let stdin = encoder.child.as_mut().unwrap().stdin.take().unwrap();
    let latest = LatestJpeg::default();
    // Software encoding is covered by the bounded-window integration test. Broadcom
    // drivers need their normal buffering; track timestamps without gating their input.
    let flight = EncoderFlight::new(config.live_encoder == crate::config::UsbEncoder::Software);
    let result = thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let result = drain_jpegs(input, &latest);
            latest.close();
            result
        });
        let writer =
            scope.spawn(|| feed_jpegs_with_flight(stdin, &latest, live.framerate, Some(&flight)));
        let result = read_live_with_flight(output, shared, frames, Some(&flight));
        // Unblock both pipe workers before joining them, including a stalled stdin write.
        latest.close();
        flight.close();
        capture.kill();
        encoder.kill();
        let read_result = reader.join().expect("JPEG reader panicked");
        let _ = writer.join().expect("JPEG feeder panicked");
        result.and(read_result)
    });
    let _ = capture.finish();
    let status = encoder.finish()?;
    result.map(|()| status)
}

#[derive(Default)]
struct LatestJpeg {
    state: Mutex<JpegState>,
    changed: Condvar,
}

#[derive(Default)]
struct JpegState {
    seq: u64,
    latest: Option<CapturedJpeg>,
    closed: bool,
}

#[derive(Clone)]
struct CapturedJpeg {
    data: Vec<u8>,
    captured_at: Instant,
}

/// Track submitted timestamps and optionally bound read-ahead for software encoding.
struct EncoderFlight {
    state: Mutex<FlightState>,
    changed: Condvar,
    bounded: bool,
}

impl Default for EncoderFlight {
    fn default() -> Self {
        Self::new(true)
    }
}

struct FlightState {
    pending: VecDeque<Instant>,
    closed: bool,
    completed: u64,
    limit: usize,
}

impl Default for FlightState {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            closed: false,
            completed: 0,
            limit: 3,
        }
    }
}

impl EncoderFlight {
    fn new(bounded: bool) -> Self {
        Self {
            state: Mutex::default(),
            changed: Condvar::new(),
            bounded,
        }
    }

    fn wait_for_slot(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return false;
            }
            if !self.bounded || state.pending.len() < state.limit {
                return true;
            }
            let (next, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(2), |s| {
                    s.pending.len() >= s.limit && !s.closed
                })
                .unwrap();
            state = next;
            // Some hardware encoders need one extra picture to prime. Never grow beyond
            // four, or grow the window again to compensate for a slow running encoder.
            if timeout.timed_out() && !state.closed && state.completed == 0 && state.limit == 3 {
                state.limit = 4;
                log::warn!("live encoder needs a fourth startup frame; bounded window is now 4");
            }
        }
    }

    fn submitted(&self, captured_at: Instant) {
        self.state.lock().unwrap().pending.push_back(captured_at);
    }

    fn completed(&self) -> Option<Instant> {
        let mut state = self.state.lock().unwrap();
        let time = state.pending.pop_front();
        state.completed += 1;
        self.changed.notify_one();
        time
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.changed.notify_all();
    }
}

impl LatestJpeg {
    fn push(&self, frame: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        state.seq += 1;
        state.latest = Some(CapturedJpeg {
            data: frame,
            captured_at: Instant::now(),
        });
        self.changed.notify_one();
    }

    fn after(&self, seq: u64) -> Option<(u64, CapturedJpeg)> {
        let state = self.state.lock().unwrap();
        let state = self
            .changed
            .wait_while(state, |s| s.seq <= seq && !s.closed)
            .unwrap();
        if state.closed {
            return None;
        }
        Some((state.seq, state.latest.as_ref().unwrap().clone()))
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.changed.notify_all();
    }
}

fn drain_jpegs(mut input: impl Read, latest: &LatestJpeg) -> Result<()> {
    let mut splitter = Splitter::default();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = input.read(&mut buf).context("reading USB MJPEG capture")?;
        if n == 0 {
            return Ok(());
        }
        for frame in splitter.push(&buf[..n]) {
            latest.push(frame);
        }
    }
}

#[cfg(test)]
fn feed_jpegs(output: impl Write, latest: &LatestJpeg, fps: u32) -> Result<()> {
    feed_jpegs_with_flight(output, latest, fps, None)
}

fn feed_jpegs_with_flight(
    mut output: impl Write,
    latest: &LatestJpeg,
    fps: u32,
    flight: Option<&EncoderFlight>,
) -> Result<()> {
    let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let mut seq = 0;
    let mut next = Instant::now();
    loop {
        // Wake on close as well as pacing. Never carry a selected frame across the wait.
        let state = latest.state.lock().unwrap();
        let (state, _) = latest
            .changed
            .wait_timeout_while(state, next.saturating_duration_since(Instant::now()), |s| {
                !s.closed
            })
            .unwrap();
        if state.closed {
            return Ok(());
        }
        drop(state);
        if let Some(flight) = flight
            && !flight.wait_for_slot()
        {
            return Ok(());
        }
        let Some((new_seq, jpeg)) = latest.after(seq) else {
            return Ok(());
        };
        seq = new_seq;
        let write_started = Instant::now();
        if let Some(flight) = flight {
            flight.submitted(jpeg.captured_at);
        }
        output
            .write_all(&jpeg.data)
            .context("feeding newest JPEG to H.264 encoder")?;
        // Time blocked by encoder backpressure already counts toward the pacing interval.
        next = write_started + interval;
    }
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

fn read_live(stdout: impl Read, shared: &Shared, frames: &mpsc::Sender<LiveFrame>) -> Result<()> {
    read_live_with_flight(stdout, shared, frames, None)
}

fn read_live_with_flight(
    mut stdout: impl Read,
    shared: &Shared,
    frames: &mpsc::Sender<LiveFrame>,
    flight: Option<&EncoderFlight>,
) -> Result<()> {
    let mut units = AccessUnits::default();
    let mut buf = vec![0; 64 * 1024];
    let mut recovering = false;
    loop {
        let n = stdout
            .read(&mut buf)
            .context("reading from camera process")?;
        if n == 0 {
            return Ok(());
        }
        for unit in units.push(&buf[..n]) {
            let captured_at = flight.and_then(EncoderFlight::completed);
            shared.state.lock().unwrap().last_frame = Some(Instant::now());
            let keyframe = is_keyframe(&unit);
            if recovering && !keyframe {
                continue;
            }
            match frames.try_send(LiveFrame {
                data: Bytes::from(unit),
                produced_at: Instant::now(),
                captured_at,
                keyframe,
            }) {
                Ok(()) => recovering = false,
                // Never send dependent frames after losing one; recover at an IDR.
                Err(TrySendError::Full(_)) => recovering = true,
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

fn is_keyframe(unit: &[u8]) -> bool {
    unit.windows(4)
        .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 5)
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
    fn hardware_timing_tracks_fifo_without_gating_encoder_startup() {
        let flight = EncoderFlight::new(false);
        let start = Instant::now();
        for n in 0..32 {
            assert!(flight.wait_for_slot());
            flight.submitted(start + Duration::from_millis(n));
        }
        for n in 0..32 {
            assert_eq!(flight.completed(), Some(start + Duration::from_millis(n)));
        }
        assert!(flight.state.lock().unwrap().pending.is_empty());
        flight.close();
        assert!(!flight.wait_for_slot());
    }

    #[test]
    fn encoder_feedback_caps_read_ahead_and_selects_recent_capture() {
        use std::sync::mpsc as sync_mpsc;
        struct Pipe(sync_mpsc::Sender<Vec<u8>>);
        impl Write for Pipe {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let latest = Arc::new(LatestJpeg::default());
        let flight = Arc::new(EncoderFlight::default());
        let (sent, observed) = sync_mpsc::channel();
        let worker_latest = Arc::clone(&latest);
        let worker_flight = Arc::clone(&flight);
        let worker = thread::spawn(move || {
            feed_jpegs_with_flight(Pipe(sent), &worker_latest, 1000, Some(&worker_flight))
        });
        // The pipe accepts unlimited data; only encoder acknowledgements limit read-ahead.
        for n in 1..=3 {
            latest.push(vec![n]);
            assert_eq!(
                observed.recv_timeout(Duration::from_secs(1)).unwrap(),
                vec![n]
            );
        }
        for n in 4..=100 {
            latest.push(vec![n]);
        }
        assert!(observed.recv_timeout(Duration::from_millis(30)).is_err());
        assert!(flight.completed().is_some());
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(1)).unwrap(),
            vec![100]
        );
        assert_eq!(flight.state.lock().unwrap().pending.len(), 3);
        latest.close();
        flight.close();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn encoder_startup_has_only_one_bounded_extra_slot() {
        let flight = EncoderFlight::default();
        for _ in 0..3 {
            flight.submitted(Instant::now());
        }
        assert!(flight.wait_for_slot());
        assert_eq!(flight.state.lock().unwrap().limit, 4);
        flight.submitted(Instant::now());
        flight.close();
        assert!(!flight.wait_for_slot());
        assert_eq!(flight.state.lock().unwrap().pending.len(), 4);
    }

    #[tokio::test]
    #[ignore = "requires ffmpeg and local executable fixtures; run with cargo test usb_live_pipeline -- --ignored"]
    async fn usb_live_pipeline_streams_and_reaps_both_children() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("prusa-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)?;
        let script = dir.join("ffmpeg-test");
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$*" in
  *v4l2*) exec ffmpeg -hide_banner -loglevel error -re -f lavfi -i testsrc2=size=640x480:rate=30 -c:v mjpeg -threads 1 -f mjpeg pipe:1 ;;
  *) exec ffmpeg -re "$@" ;;
esac
"#,
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
        let config = crate::config::UsbConfig {
            command: script.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            new_frame: Condvar::new(),
        });
        let (sender, mut frames) = mpsc::channel(LIVE_QUEUE);
        let worker_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            stream_usb_live(
                &config,
                LiveStream {
                    width: 640,
                    height: 480,
                    framerate: 12,
                    bitrate: 900_000,
                },
                &worker_shared,
                0,
                &sender,
                MIN_STALL_LIMIT,
            )
        });
        let result = async {
            for _ in 0..4 {
                let frame = tokio::time::timeout(Duration::from_secs(5), frames.recv())
                    .await?
                    .context("pipeline ended before four frames")?;
                assert!(frame.keyframe);
                assert!(frame.produced_at.elapsed() < Duration::from_millis(500));
                let captured = frame.captured_at.expect("pipeline preserves capture time");
                assert!(
                    frame.produced_at.saturating_duration_since(captured) < Duration::from_secs(2)
                );
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        drop(frames);
        {
            let mut state = shared.state.lock().unwrap();
            state.generation += 1;
            for &pid in &state.pids {
                // SAFETY: registered child PIDs cannot be reaped while this lock is held.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
        let _ = worker.join().expect("pipeline worker panicked");
        assert!(shared.state.lock().unwrap().pids.is_empty());
        std::fs::remove_dir_all(&dir)?;
        result
    }

    #[test]
    fn slow_encoder_gets_newest_jpeg_instead_of_a_backlog() {
        use std::sync::mpsc;
        struct SlowWriter {
            sent: mpsc::Sender<Vec<u8>>,
            resume: mpsc::Receiver<()>,
        }
        impl Write for SlowWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.sent.send(bytes.to_vec()).unwrap();
                self.resume.recv().unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let latest = Arc::new(LatestJpeg::default());
        latest.push(vec![1]);
        let (sent, observed) = mpsc::channel();
        let (resume, stalled) = mpsc::channel();
        let worker_latest = Arc::clone(&latest);
        let worker = thread::spawn(move || {
            feed_jpegs(
                SlowWriter {
                    sent,
                    resume: stalled,
                },
                &worker_latest,
                1000,
            )
        });
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec![1]
        );
        // Camera drains another hundred frames while encoder input is blocked.
        for n in 2..=100 {
            latest.push(vec![n]);
        }
        resume.send(()).unwrap();
        assert_eq!(
            observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec![100]
        );
        latest.close();
        resume.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert!(latest.after(0).is_none());
    }

    #[test]
    fn identifies_keyframes_for_safe_video_dropping() {
        assert!(is_keyframe(&[0, 0, 0, 1, 0x65, 0x88]));
        assert!(is_keyframe(&[0, 0, 1, 0x25, 0x88]));
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 0x88]));
    }

    #[test]
    fn allows_several_frames_before_calling_a_stall() {
        assert_eq!(stall_limit(1.0), MIN_STALL_LIMIT);
        assert_eq!(stall_limit(12.0), MIN_STALL_LIMIT);
        assert_eq!(stall_limit(0.05), Duration::from_secs(60));
    }
}
