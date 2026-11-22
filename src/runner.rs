//! Starts each camera's outputs, one upload loop apiece, and reports liveness to systemd.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sd_notify::NotifyState;

use crate::config::{CameraConfig, Config, SourceConfig};
use crate::hub::Hub;
use crate::outputs::connect_cloud::{self, live};
use crate::outputs::rtsp;
use crate::source::rpicam::Rpicam;
use crate::source::{self, CAPTURE_TIMEOUT, FrameSource};
use crate::status::Status;
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
}

fn open_source(camera: &CameraConfig) -> Result<OpenSource> {
    Ok(match &camera.source {
        SourceConfig::Rpicam(config) => {
            let rpicam = Rpicam::start(config)?;
            let hub = Hub::start(&camera.id, rpicam.clone(), config.live_sensor_size())?;
            OpenSource {
                frames: Arc::new(rpicam),
                hub: Some(hub),
            }
        }
        other => OpenSource {
            frames: source::open(other)?,
            hub: None,
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

pub fn run(config: &Config) -> Result<()> {
    let status = Status::from_env();
    let mut uploaders = Vec::new();
    let mut live_cameras = Vec::new();
    let mut hubs = Vec::new();
    for camera in &config.cameras {
        let source = open_source(camera)?;
        if let Some(hub) = &source.hub {
            hubs.push((camera.id.clone(), hub.clone()));
        }
        let outputs = outputs(config, camera, &source)?;
        for target in outputs.stills {
            let frames = Arc::clone(&source.frames);
            uploaders.push(Uploader::new(&camera.id, target, frames, status.clone()));
        }
        live_cameras.extend(outputs.live);
    }

    let live_thread = if live_cameras.is_empty() {
        None
    } else {
        Some(live::spawn(config.signaling_url.clone(), live_cameras)?)
    };
    let rtsp_thread = match &config.rtsp {
        Some(rtsp_config) => Some(rtsp::spawn(rtsp_config, hubs, status.clone())?),
        None => None,
    };
    let workers = uploaders
        .into_iter()
        .map(spawn)
        .collect::<Result<Vec<_>>>()?;

    let _ = sd_notify::notify(&[NotifyState::Ready]);
    let watchdog = sd_notify::watchdog_enabled();
    let tick = watchdog.map_or(Duration::from_secs(10), |timeout| timeout / 2);
    loop {
        thread::sleep(tick);
        // Exiting lets systemd restart everything, including rpicam-vid.
        for worker in &workers {
            if worker.handle.is_finished() {
                bail!("{}'s upload loop crashed", worker.label);
            }
            if worker.heartbeat.lock().unwrap().elapsed() > STALL_LIMIT {
                bail!("{}'s upload loop is stuck", worker.label);
            }
        }
        if live_thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            bail!("live view crashed");
        }
        if rtsp_thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            bail!("the RTSP server crashed");
        }
        if watchdog.is_some() {
            let _ = sd_notify::notify(&[NotifyState::Watchdog]);
        }
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
