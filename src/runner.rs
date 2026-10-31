//! Runs one upload loop per camera and reports liveness to systemd.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sd_notify::NotifyState;

use crate::config::{CameraConfig, Config, SourceConfig};
use crate::connect::{Client, ConnectError, REQUEST_TIMEOUT};
use crate::fingerprint;
use crate::live;
use crate::source::rpicam::Rpicam;
use crate::source::{self, CAPTURE_TIMEOUT, FrameSource, Paused};

const MAX_BACKOFF: Duration = Duration::from_secs(300);
const PRINTER_TIMEOUT: Duration = Duration::from_secs(2);
/// A camera loop that hasn't come round in this long is stuck: the longest backoff plus a
/// capture, an upload, and some slack.
const STALL_LIMIT: Duration = Duration::from_secs(
    MAX_BACKOFF.as_secs() + CAPTURE_TIMEOUT.as_secs() + REQUEST_TIMEOUT.as_secs() + 60,
);

struct Camera {
    id: String,
    name: Option<String>,
    interval: Duration,
    printer_check: Option<String>,
    client: Client,
    source: Box<dyn FrameSource>,
    /// Set when the camera offers live view; taken by [`run`] to start it.
    live: Option<live::Camera>,
    live_enabled: bool,
}

impl Camera {
    fn open(config: &Config, camera: &CameraConfig) -> Result<Self> {
        let token = camera.load_token()?;
        let fingerprint = fingerprint::load_or_create(&config.state_dir, &camera.id)?;
        let (source, live): (Box<dyn FrameSource>, _) = match &camera.source {
            SourceConfig::Rpicam(rpicam_config) => {
                let rpicam = Rpicam::start(rpicam_config)?;
                let live = camera.live.then(|| live::Camera {
                    id: camera.id.clone(),
                    name: camera.name.clone().unwrap_or_else(|| camera.id.clone()),
                    token: token.clone(),
                    fingerprint: fingerprint.clone(),
                    hardware: live::hardware(),
                    resolution: (rpicam_config.width, rpicam_config.height),
                    sensor_size: rpicam_config.live_sensor_size(),
                    source: rpicam.clone(),
                });
                (Box::new(rpicam), live)
            }
            other => (source::open(other)?, None),
        };
        Ok(Self {
            id: camera.id.clone(),
            name: camera.name.clone(),
            interval: camera.interval(),
            printer_check: camera.printer_check.clone(),
            client: Client::new(&config.connect_url, token, fingerprint),
            source,
            live_enabled: live.is_some(),
            live,
        })
    }

    /// Captures a frame and uploads it. `sent` is set to when the upload began, so the next
    /// upload is paced from there rather than from a capture that may have waited for the
    /// camera to settle.
    fn upload(&self, sent: &mut Instant) -> Result<u16> {
        let jpeg = self.source.capture(CAPTURE_TIMEOUT)?;
        *sent = Instant::now();
        let status = self.client.upload_snapshot(&jpeg)?;
        log::debug!(
            "[{}] uploaded {} bytes in {} ms (HTTP {status})",
            self.id,
            jpeg.len(),
            sent.elapsed().as_millis()
        );
        Ok(status)
    }

    /// Sets the camera's name in Prusa Connect, and declares live view when it's offered.
    /// Called after a successful upload, so the camera is registered by then.
    fn apply_info(&self) {
        let live = self.live_enabled;
        let Some(name) = self.name.as_deref().or(live.then_some(self.id.as_str())) else {
            return;
        };
        match self.client.set_info(name, live) {
            Ok(_) => log::info!("[{}] camera name set to {name:?}", self.id),
            Err(e) => log::warn!("[{}] couldn't update the camera's details: {e}", self.id),
        }
    }

    fn run_loop(&self, heartbeat: &Mutex<Instant>) {
        let mut failures = 0;
        let mut uploading = false;
        let mut printer_up = true;
        loop {
            *heartbeat.lock().unwrap() = Instant::now();
            let mut started = Instant::now();

            if let Some(addr) = &self.printer_check {
                let up = reachable(addr);
                if up != printer_up {
                    if up {
                        log::info!("[{}] printer at {addr} is back; resuming uploads", self.id);
                    } else {
                        log::info!(
                            "[{}] printer at {addr} is unreachable; pausing uploads",
                            self.id
                        );
                    }
                    printer_up = up;
                }
            }

            let delay = if !printer_up {
                self.interval
            } else {
                match self.upload(&mut started) {
                    Ok(status) => {
                        if !uploading {
                            log::info!(
                                "[{}] uploading every {}s (HTTP {status})",
                                self.id,
                                self.interval.as_secs()
                            );
                            self.apply_info();
                        }
                        uploading = true;
                        failures = 0;
                        self.interval
                    }
                    Err(e) if e.is::<Paused>() => {
                        log::debug!("[{}] {e}", self.id);
                        self.interval
                    }
                    Err(e) => {
                        uploading = false;
                        failures += 1;
                        let auth = e
                            .downcast_ref::<ConnectError>()
                            .is_some_and(ConnectError::is_auth);
                        let delay = if auth {
                            MAX_BACKOFF
                        } else {
                            backoff(self.interval, failures)
                        };
                        log::warn!("[{}] {e:#}; retrying in {}s", self.id, delay.as_secs());
                        delay
                    }
                }
            };
            thread::sleep(delay.saturating_sub(started.elapsed()));
        }
    }
}

/// Doubles the wait after each consecutive failure, starting from the normal interval.
fn backoff(interval: Duration, failures: u32) -> Duration {
    interval
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)))
        .min(MAX_BACKOFF)
}

fn reachable(addr: &str) -> bool {
    let Ok(addrs) = addr.to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, PRINTER_TIMEOUT).is_ok())
}

pub fn run(config: &Config) -> Result<()> {
    let mut cameras: Vec<Camera> = config
        .cameras
        .iter()
        .map(|c| Camera::open(config, c))
        .collect::<Result<_>>()?;

    let mut live_threads = Vec::new();
    for camera in &mut cameras {
        if let Some(live_camera) = camera.live.take() {
            let handle = live::spawn(config.signaling_url.clone(), live_camera)?;
            live_threads.push((camera.id.clone(), handle));
        }
    }

    let mut workers = Vec::new();
    for camera in cameras {
        let heartbeat = Arc::new(Mutex::new(Instant::now()));
        let thread_heartbeat = Arc::clone(&heartbeat);
        let id = camera.id.clone();
        let handle = thread::Builder::new()
            .name(format!("camera-{id}"))
            .spawn(move || camera.run_loop(&thread_heartbeat))
            .context("starting a camera thread")?;
        workers.push((id, heartbeat, handle));
    }

    let _ = sd_notify::notify(&[NotifyState::Ready]);
    let watchdog = sd_notify::watchdog_enabled();
    let tick = watchdog.map_or(Duration::from_secs(10), |timeout| timeout / 2);
    loop {
        thread::sleep(tick);
        // Exiting lets systemd restart everything, including rpicam-vid.
        for (id, heartbeat, handle) in &workers {
            if handle.is_finished() {
                bail!("camera {id}'s upload loop crashed");
            }
            if heartbeat.lock().unwrap().elapsed() > STALL_LIMIT {
                bail!("camera {id}'s upload loop is stuck");
            }
        }
        if let Some((id, _)) = live_threads.iter().find(|(_, handle)| handle.is_finished()) {
            bail!("camera {id}'s live view crashed");
        }
        if watchdog.is_some() {
            let _ = sd_notify::notify(&[NotifyState::Watchdog]);
        }
    }
}

pub fn test(config: &Config, camera: &CameraConfig) -> Result<()> {
    let camera = Camera::open(config, camera)?;
    let jpeg = camera.source.capture(CAPTURE_TIMEOUT)?;
    let status = camera.client.upload_snapshot(&jpeg)?;
    println!(
        "Uploaded a {} byte snapshot from camera {} with fingerprint {}; Prusa Connect answered HTTP {status}.",
        jpeg.len(),
        camera.id,
        camera.client.fingerprint()
    );
    camera.apply_info();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_limit() {
        let interval = Duration::from_secs(10);
        let delays: Vec<u64> = (1..=7).map(|n| backoff(interval, n).as_secs()).collect();
        assert_eq!(delays, [10, 20, 40, 80, 160, 300, 300]);
        assert_eq!(backoff(interval, u32::MAX), MAX_BACKOFF);
    }
}
