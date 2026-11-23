//! Starts each camera's outputs, watches that they keep running, reports liveness to systemd, and
//! stops everything in order: on a stop signal, or when a part has failed for good.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sd_notify::NotifyState;

use crate::config::{CameraConfig, Config, SourceConfig};
use crate::hub::Hub;
use crate::outputs::connect_cloud::{self, live};
use crate::outputs::{api, rtsp};
use crate::source::rpicam::Rpicam;
use crate::source::{self, CAPTURE_TIMEOUT, FrameSource};
use crate::status::Status;
use crate::supervise::{self, Service};
use crate::uploader::{MAX_BACKOFF, REQUEST_TIMEOUT, StillTarget, Uploader};

/// An upload loop that hasn't come round in this long is stuck: the longest backoff plus a
/// capture, an upload, and some slack.
const STALL_LIMIT: Duration = Duration::from_secs(
    MAX_BACKOFF.as_secs() + CAPTURE_TIMEOUT.as_secs() + REQUEST_TIMEOUT.as_secs() + 60,
);

/// A camera's stills and video, shared by its outputs.
struct OpenSource {
    frames: Arc<dyn FrameSource>,
    /// Live video, from a camera that has it.
    hub: Option<Hub>,
    /// The rpicam-vid process, which has to be stopped before the program ends.
    rpicam: Option<Rpicam>,
}

fn open_source(camera: &CameraConfig) -> Result<OpenSource> {
    Ok(match &camera.source {
        SourceConfig::Rpicam(config) => {
            let rpicam = Rpicam::start(config)?;
            let hub = Hub::start(&camera.id, rpicam.clone(), config.live_sensor_size())?;
            OpenSource {
                frames: Arc::new(rpicam.clone()),
                hub: Some(hub),
                rpicam: Some(rpicam),
            }
        }
        other => OpenSource {
            frames: source::open(other)?,
            hub: None,
            rpicam: None,
        },
    })
}

/// A camera's outputs, ready to start.
struct Outputs {
    stills: Vec<Box<dyn StillTarget>>,
    /// Live view in Prusa Connect, when the camera offers it.
    live: Option<live::Camera>,
}

/// Sets up a camera's outputs: Prusa Connect unless `cloud = false`, then each `[[output]]`.
fn outputs(config: &Config, camera: &CameraConfig, source: &OpenSource) -> Result<Outputs> {
    let mut outputs = Outputs {
        stills: Vec::new(),
        live: None,
    };
    if camera.cloud {
        let (cloud, live) = connect_cloud::open(config, camera, source.hub.as_ref())?;
        outputs.stills.push(Box::new(cloud));
        outputs.live = live;
    }
    for output in config.outputs_for(&camera.id) {
        outputs.stills.push(output.open(&camera.id)?);
    }
    Ok(outputs)
}

/// An upload loop's thread and the heartbeat the watchdog checks.
struct Worker {
    label: String,
    heartbeat: Arc<Mutex<Instant>>,
    handle: thread::JoinHandle<()>,
}

fn spawn(uploader: Uploader) -> Result<Worker> {
    let heartbeat = Arc::new(Mutex::new(Instant::now()));
    let thread_heartbeat = Arc::clone(&heartbeat);
    let label = uploader.label.clone();
    let handle = thread::Builder::new()
        .name(format!("upload-{label}"))
        .spawn(move || uploader.run(&thread_heartbeat))
        .context("starting an upload thread")?;
    Ok(Worker {
        label,
        heartbeat,
        handle,
    })
}

/// How long rpicam-vid may take to exit when the program stops.
const CAMERA_SHUTDOWN: Duration = Duration::from_secs(3);

/// Everything running, which [`check`] watches and [`shutdown`] stops.
struct Running {
    workers: Vec<Worker>,
    hubs: Vec<(String, Hub)>,
    live: Option<live::Live>,
    rtsp: Option<Service>,
    api: Option<Service>,
    cameras: Vec<Rpicam>,
}

pub fn run(config: &Config) -> Result<()> {
    let stop = supervise::stop_signals()?;
    let status = Status::from_env();
    let mut uploaders = Vec::new();
    let mut live_cameras = Vec::new();
    let mut hubs = Vec::new();
    let mut cameras = Vec::new();
    let mut api_cameras = Vec::new();
    for camera in &config.cameras {
        let source = open_source(camera)?;
        if let Some(hub) = &source.hub {
            hubs.push((camera.id.clone(), hub.clone()));
        }
        api_cameras.push(api::Camera {
            id: camera.id.clone(),
            name: camera.name.clone(),
            stills: Arc::clone(&source.frames),
            hub: source.hub.clone(),
        });
        cameras.extend(source.rpicam.clone());
        let outputs = outputs(config, camera, &source)?;
        for target in outputs.stills {
            let frames = Arc::clone(&source.frames);
            uploaders.push(Uploader::new(&camera.id, target, frames, status.clone()));
        }
        live_cameras.extend(outputs.live);
    }

    let live = if live_cameras.is_empty() {
        None
    } else {
        Some(live::spawn(config.signaling_url.clone(), live_cameras)?)
    };
    let rtsp = match &config.rtsp {
        Some(rtsp_config) => Some(rtsp::spawn(rtsp_config, hubs.clone(), status.clone())?),
        None => None,
    };
    let api = match &config.api {
        Some(api_config) => {
            let rtsp_port = config
                .rtsp
                .as_ref()
                .and_then(|r| r.listen.parse::<std::net::SocketAddr>().ok())
                .map(|a| a.port());
            Some(api::spawn(
                api_config,
                api_cameras,
                rtsp_port,
                status.clone(),
            )?)
        }
        None => None,
    };
    let workers = uploaders
        .into_iter()
        .map(spawn)
        .collect::<Result<Vec<_>>>()?;
    let running = Running {
        workers,
        hubs,
        live,
        rtsp,
        api,
        cameras,
    };

    let _ = sd_notify::notify(&[NotifyState::Ready]);
    let watchdog = sd_notify::watchdog_enabled();
    let tick = watchdog.map_or(Duration::from_secs(10), |timeout| timeout / 2);
    loop {
        match stop.recv_timeout(tick) {
            Ok(()) => {
                log::info!("stopping");
                shutdown(&running);
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        // A part that failed for good ends the program, and systemd starts it again.
        if let Err(e) = check(&running) {
            log::error!("{e:#}; stopping, to be restarted");
            shutdown(&running);
            return Err(e);
        }
        if watchdog.is_some() {
            let _ = sd_notify::notify(&[NotifyState::Watchdog]);
        }
    }
}

/// Fails if any part has stopped or stopped making progress. Upload loops and async tasks that
/// panic restart on their own, so what's left here means something is wrong beyond that.
fn check(running: &Running) -> Result<()> {
    for worker in &running.workers {
        if worker.handle.is_finished() {
            bail!("{}'s upload loop ended", worker.label);
        }
        if worker.heartbeat.lock().unwrap().elapsed() > STALL_LIMIT {
            bail!("{}'s upload loop is stuck", worker.label);
        }
    }
    for (id, hub) in &running.hubs {
        if !hub.is_alive() {
            bail!("camera {id}'s video hub stopped");
        }
    }
    if let Some(live) = &running.live {
        live.check()?;
    }
    if let Some(rtsp) = &running.rtsp {
        rtsp.check()?;
    }
    if let Some(api) = &running.api {
        api.check()?;
    }
    Ok(())
}

/// Closes live view sessions, so their relay allocations are released, then stops rpicam-vid,
/// so it lets go of the camera rather than being killed with the program.
fn shutdown(running: &Running) {
    let _ = sd_notify::notify(&[NotifyState::Stopping]);
    if let Some(live) = &running.live {
        live.stop();
    }
    for camera in &running.cameras {
        camera.shutdown(CAMERA_SHUTDOWN);
    }
}

/// Captures one frame and sends it to each of the camera's outputs, printing what each answered.
pub fn test(config: &Config, camera: &CameraConfig) -> Result<()> {
    let source = open_source(camera)?;
    let outputs = outputs(config, camera, &source)?;
    let jpeg = source.frames.capture(CAPTURE_TIMEOUT)?;
    let mut failed = 0;
    for target in &outputs.stills {
        let label = format!("{}/{}", camera.id, target.name());
        target.start(&label);
        match target.send(&jpeg) {
            Ok(status) => {
                println!(
                    "Uploaded a {} byte snapshot to {label}; the server answered HTTP {status}.",
                    jpeg.len()
                );
                target.on_uploading(&label);
            }
            Err(e) => {
                println!("{label}: {e}");
                failed += 1;
            }
        }
    }
    ensure!(failed == 0, "{failed} upload(s) failed");
    Ok(())
}
