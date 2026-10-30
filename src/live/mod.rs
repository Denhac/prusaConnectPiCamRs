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

use anyhow::{Context, Result};

pub use signaling::{Camera, hardware};

/// Features a web camera with live view reports to Prusa Connect.
pub const FEATURES: [&str; 2] = ["SocketCom", "WebRtc"];

/// Runs live view for `camera` on its own thread, staying connected to `signaling_url` and
/// reconnecting whenever the connection drops.
pub fn spawn(signaling_url: String, camera: Camera) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("live-{}", camera.id))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("live-runtime")
                .enable_all()
                .build()
                .expect("building the live view runtime");
            runtime.block_on(signaling::run(signaling_url, camera));
        })
        .context("starting the live view thread")
}
