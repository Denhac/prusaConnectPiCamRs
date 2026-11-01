//! Live view in Prusa Connect. The camera registers as a Prusa "web camera" and speaks the same
//! signaling and WebRTC protocol as Prusa's browser webcam, streaming H.264 from rpicam-vid to a
//! viewer who presses play. Snapshots pause while someone watches.
//!
//! The protocol is undocumented; it follows Prusa's webcam app (camera-service-webcam.prusa3d.com,
//! version 0.3.6) and may change without notice.

mod proto;
mod session;
mod signaling;
mod socketio;

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use sd_notify::NotifyState;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

pub use signaling::{Camera, hardware};

/// How long stopping may take to close sessions and release their relay allocations.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Features a web camera with live view reports to Prusa Connect.
pub const FEATURES: [&str; 2] = ["SocketCom", "WebRtc"];

/// Runs live view for `cameras` on one thread, staying connected to `signaling_url` and
/// reconnecting whenever a connection drops. From here on, SIGTERM and SIGINT end the program, once
/// sessions have closed: a killed session can't release its relay allocation, which would count
/// against the viewer's quota until it expires.
pub fn spawn(signaling_url: String, cameras: Vec<Camera>) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("live-view".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("live-runtime")
                .enable_all()
                .build()
                .expect("building the live view runtime");
            runtime.block_on(run(signaling_url, cameras));
        })
        .context("starting the live view thread")
}

async fn run(signaling_url: String, cameras: Vec<Camera>) {
    let (stop, stopping) = watch::channel(false);
    // Each camera and session holds a sender, so `recv` ends when the last has finished.
    let (alive, mut all_finished) = mpsc::channel::<()>(1);
    for camera in cameras {
        tokio::spawn(signaling::run(
            signaling_url.clone(),
            camera,
            stopping.clone(),
            alive.clone(),
        ));
    }
    drop(alive);

    let (Ok(mut terminate), Ok(mut interrupt)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        log::warn!("can't handle stop signals; relay allocations won't be released on stop");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    log::info!("stopping: closing live view sessions");
    let _ = sd_notify::notify(&[NotifyState::Stopping]);
    let _ = stop.send(true);
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, all_finished.recv()).await;
    std::process::exit(0);
}
