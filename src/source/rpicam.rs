//! Keeps one rpicam-vid process streaming MJPEG into a pipe and remembers its newest frame.
//! Unlike starting rpicam-still for each snapshot, the camera keeps running, so exposure and
//! white balance stay settled between uploads.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::FrameSource;
use super::jpeg::Splitter;
use crate::config::RpicamConfig;

/// Frames skipped after rpicam-vid starts, while auto exposure and white balance settle.
const SETTLE_FRAMES: u64 = 3;
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

pub struct Rpicam {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    new_frame: Condvar,
}

#[derive(Default)]
struct State {
    /// Frames received since this source was created.
    seq: u64,
    /// Frames received from the current rpicam-vid process.
    since_start: u64,
    latest: Option<Arc<[u8]>>,
    /// Why rpicam-vid last stopped, reported when no frames arrive.
    last_error: Option<String>,
}

impl Rpicam {
    pub fn start(config: &RpicamConfig) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            new_frame: Condvar::new(),
        });
        let command = config.command.clone();
        let args = args(config);
        let thread_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name(format!("rpicam-{}", config.index))
            .spawn(move || supervise(&command, &args, &thread_shared))
            .context("starting the rpicam-vid thread")?;
        Ok(Self { shared })
    }
}

impl FrameSource for Rpicam {
    fn capture(&self, timeout: Duration) -> Result<Arc<[u8]>> {
        let state = self.shared.state.lock().unwrap();
        let after = state.seq;
        let (state, wait) = self
            .shared
            .new_frame
            .wait_timeout_while(state, timeout, |s| {
                s.seq <= after || s.since_start <= SETTLE_FRAMES
            })
            .unwrap();
        if wait.timed_out() {
            match &state.last_error {
                Some(error) => bail!("no frame from rpicam-vid within {timeout:?} ({error})"),
                None => bail!("no frame from rpicam-vid within {timeout:?}"),
            }
        }
        Ok(Arc::clone(state.latest.as_ref().expect("a frame arrived")))
    }
}

fn args(config: &RpicamConfig) -> Vec<String> {
    #[rustfmt::skip]
    let mut args: Vec<String> = [
        "--camera", &config.index.to_string(),
        "--timeout", "0",
        "--nopreview",
        // Level 1 prints a line per frame, which would flood the journal.
        "--verbose", "0",
        "--codec", "mjpeg",
        "--width", &config.width.to_string(),
        "--height", &config.height.to_string(),
        "--framerate", &config.framerate.to_string(),
        "--quality", &config.quality.to_string(),
        "--rotation", &config.rotation.to_string(),
        // Write each frame out whole instead of leaving its tail in stdio's buffer.
        "--flush",
    ]
    .map(String::from)
    .into();
    if config.hflip {
        args.push("--hflip".into());
    }
    if config.vflip {
        args.push("--vflip".into());
    }
    if let Some(position) = config.lens_position {
        args.extend(["--autofocus-mode".into(), "manual".into()]);
        args.extend(["--lens-position".into(), position.to_string()]);
    }
    args.extend(config.extra_args.iter().cloned());
    args.extend(["--output".into(), "-".into()]);
    args
}

/// Runs rpicam-vid for as long as the program runs, restarting it when it stops.
fn supervise(command: &str, args: &[String], shared: &Shared) {
    let mut delay = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let error = match stream(command, args, shared) {
            Ok(status) => format!("{command} stopped ({status})"),
            Err(e) => format!("{e:#}"),
        };
        log::warn!("{error}; restarting it in {delay:?}");
        {
            let mut state = shared.state.lock().unwrap();
            state.since_start = 0;
            state.last_error = Some(error);
        }
        thread::sleep(delay);
        // A long run means the camera was healthy, so start backing off from scratch.
        delay = if started.elapsed() > Duration::from_secs(60) {
            Duration::from_secs(1)
        } else {
            (delay * 2).min(MAX_RESTART_DELAY)
        };
    }
}

fn stream(command: &str, args: &[String], shared: &Shared) -> Result<ExitStatus> {
    let mut cmd = Command::new(command);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped());
    // Have the kernel stop rpicam-vid when the thread that started it ends, so it never keeps
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

    let stdout = child.stdout.take().expect("stdout is piped");
    let result = read_frames(stdout, shared);
    // Usually a no-op: the stream ends because rpicam-vid exited.
    let _ = child.kill();
    let status = child
        .wait()
        .with_context(|| format!("waiting for {command}"))?;
    result.map(|()| status)
}

fn read_frames(mut stdout: impl Read, shared: &Shared) -> Result<()> {
    let mut splitter = Splitter::default();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = stdout.read(&mut buf).context("reading from rpicam-vid")?;
        if n == 0 {
            return Ok(());
        }
        for frame in splitter.push(&buf[..n]) {
            let mut state = shared.state.lock().unwrap();
            state.seq += 1;
            state.since_start += 1;
            state.latest = Some(frame.into());
            shared.new_frame.notify_all();
        }
    }
}

pub fn list_cameras() -> Result<()> {
    let status = Command::new("rpicam-vid")
        .arg("--list-cameras")
        .status()
        .context(
            "running rpicam-vid (on Raspberry Pi OS Lite: sudo apt install rpicam-apps-lite)",
        )?;
    if !status.success() {
        bail!("rpicam-vid --list-cameras failed ({status})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_rpicam_vid_arguments() {
        let config = RpicamConfig {
            index: 1,
            rotation: 180,
            lens_position: Some(3.0),
            extra_args: vec!["--denoise".into(), "cdn_off".into()],
            ..RpicamConfig::default()
        };
        let args = args(&config).join(" ");
        assert!(args.starts_with("--camera 1 --timeout 0 "));
        assert!(args.contains(
            "--codec mjpeg --width 2048 --height 1536 --framerate 1 --quality 50 --rotation 180"
        ));
        assert!(
            args.ends_with(
                "--autofocus-mode manual --lens-position 3 --denoise cdn_off --output -"
            )
        );
        assert!(!args.contains("--hflip"));
    }
}
