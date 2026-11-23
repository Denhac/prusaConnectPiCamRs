//! Live view in Prusa Connect. The camera registers as a Prusa "web camera" and speaks the same
//! signaling and WebRTC protocol as Prusa's browser webcam, streaming the camera's H.264 from the
//! video hub to each viewer who presses play, up to two at once. Snapshots pause while anyone,
//! here or on another output, watches.
//!
//! The protocol is undocumented; it follows Prusa's webcam app (camera-service-webcam.prusa3d.com,
//! version 0.3.6) and may change without notice.

mod proto;
mod session;
mod signaling;
mod socketio;

use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, watch};

use crate::supervise::{self, Service};

pub use signaling::{Camera, hardware};

/// How long stopping may take to close sessions and release their relay allocations.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Features a web camera with live view reports to Prusa Connect.
pub const FEATURES: [&str; 2] = ["SocketCom", "WebRtc"];

/// Live view for every camera that offers it, on a runtime of its own.
pub struct Live {
    service: Service,
    stop: watch::Sender<bool>,
}

/// Starts live view for `cameras`, staying connected to `signaling_url` and reconnecting
/// whenever a connection drops. A camera whose signaling panics is restarted on its own.
pub fn spawn(signaling_url: String, cameras: Vec<Camera>) -> Result<Live> {
    let (stop, stopping) = watch::channel(false);
    let service = supervise::spawn_runtime("live-view", 2, run(signaling_url, cameras, stopping))?;
    Ok(Live { service, stop })
}

impl Live {
    pub fn check(&self) -> Result<()> {
        self.service.check()
    }

    /// Closes every session, waiting at most [`SHUTDOWN_TIMEOUT`]. A session killed instead
    /// can't release its relay allocation, which would count against the viewer's quota until
    /// it expires.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
        self.service.wait(SHUTDOWN_TIMEOUT + Duration::from_secs(1));
    }
}

async fn run(signaling_url: String, cameras: Vec<Camera>, mut stopping: watch::Receiver<bool>) {
    // Each camera and session holds a sender, so `recv` ends when the last has finished.
    let (alive, mut all_finished) = mpsc::channel::<()>(1);
    for camera in cameras {
        let label = format!("{}/cloud live view", camera.id);
        let (url, stopping, alive) = (signaling_url.clone(), stopping.clone(), alive.clone());
        tokio::spawn(supervise::restart_on_panic(label, move || {
            signaling::run(url.clone(), camera.clone(), stopping.clone(), alive.clone())
        }));
    }
    drop(alive);
    let _ = stopping.wait_for(|stop| *stop).await;
    log::info!("stopping: closing live view sessions");
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, all_finished.recv()).await;
}
