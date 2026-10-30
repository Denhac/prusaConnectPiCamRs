//! The camera side of Prusa's camera signaling (Socket.IO at camera-signaling.prusa3d.com), as
//! Prusa's browser webcam speaks it: authenticate with the camera token, answer the server's
//! requests for status and features, and run a WebRTC session when a viewer presses play.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use prost::Message;
use tokio::sync::mpsc;

use super::proto::{self, ClientType, MessageType, StreamStatus, Toggle, WebrtcMode, ack};
use super::session::{self, Command, Outgoing, Session};
use super::socketio::{self, Packet, Writer};
use crate::config::Token;
use crate::source::rpicam::Rpicam;

/// The origin Prusa's browser webcam connects from.
const ORIGIN: &str = "https://camera-service-webcam.prusa3d.com";
const USER_AGENT: &str = concat!("prusa-connect-camera/", env!("CARGO_PKG_VERSION"));
/// Prusa's webcam authenticates again this often on a long-lived connection.
const REAUTH_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
/// Prusa's webcam serves one viewer at a time.
const MAX_VIEWERS: usize = 1;
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(300);

/// A camera offered for live view.
pub struct Camera {
    pub id: String,
    pub name: String,
    pub token: Token,
    pub fingerprint: String,
    pub hardware: String,
    /// The snapshot resolution, reported as the camera's current one.
    pub resolution: (u32, u32),
    pub source: Rpicam,
}

/// Keeps the camera connected to signaling, reconnecting with backoff, forever.
pub async fn run(url: String, camera: Camera) {
    let mut delay = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let error = match connection(&url, &camera).await {
            Ok(()) => "disconnected".to_owned(),
            Err(e) => format!("{e:#}"),
        };
        if started.elapsed() > MAX_RECONNECT_DELAY {
            delay = Duration::from_secs(1);
        }
        log::warn!(
            "[{}] live view signaling: {error}; reconnecting in {}s",
            camera.id,
            delay.as_secs()
        );
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

async fn connection(url: &str, camera: &Camera) -> Result<()> {
    let (mut reader, writer) = socketio::connect(url, ORIGIN, USER_AGENT).await?;
    let (out, mut outgoing) = mpsc::channel(64);
    let mut signaling = Signaling {
        camera,
        writer,
        next_ack: 0,
        awaiting: HashMap::new(),
        mode: WebrtcMode::On,
        sessions: HashMap::new(),
        out,
        authenticated: false,
    };
    signaling.authenticate().await?;
    let mut reauth = tokio::time::interval_at(
        tokio::time::Instant::now() + REAUTH_INTERVAL,
        REAUTH_INTERVAL,
    );
    loop {
        tokio::select! {
            packet = reader.next() => signaling.handle(packet?).await?,
            Some(message) = outgoing.recv() => signaling.send(message).await?,
            _ = reauth.tick() => signaling.authenticate().await?,
        }
    }
}

struct Signaling<'a> {
    camera: &'a Camera,
    writer: Writer,
    next_ack: u64,
    /// Events awaiting the server's acknowledgement, by acknowledgement id.
    awaiting: HashMap<u64, &'static str>,
    mode: WebrtcMode,
    /// Running viewer sessions by request id; dropping a sender ends its session.
    sessions: HashMap<String, mpsc::Sender<Command>>,
    out: mpsc::Sender<Outgoing>,
    authenticated: bool,
}

impl Signaling<'_> {
    fn token(&self) -> String {
        self.camera.token.as_str().to_owned()
    }

    async fn emit(&mut self, name: &'static str, message: impl Message) -> Result<()> {
        let id = self.next_ack;
        self.next_ack += 1;
        self.awaiting.insert(id, name);
        self.writer.emit(name, message.encode_to_vec(), id).await
    }

    async fn authenticate(&mut self) -> Result<()> {
        let message = proto::CameraAuthentication {
            camera_token: self.token(),
            fingerprint: self.camera.fingerprint.clone(),
        };
        self.emit("camera_authentication", message).await
    }

    async fn handle(&mut self, packet: Packet) -> Result<()> {
        match packet {
            Packet::Ping => self.writer.pong().await,
            Packet::Ack { id, code } => self.acknowledged(id, code).await,
            Packet::Event { name, data, ack_id } => {
                let code = self.event(&name, &data).await?;
                match ack_id {
                    Some(id) => self.writer.ack(id, code).await,
                    None => Ok(()),
                }
            }
        }
    }

    async fn acknowledged(&mut self, id: u64, code: i64) -> Result<()> {
        let Some(name) = self.awaiting.remove(&id) else {
            return Ok(());
        };
        if name == "camera_authentication" {
            if code != ack::OK {
                bail!("Prusa Connect rejected the camera's credentials (code {code})");
            }
            if !self.authenticated {
                self.authenticated = true;
                log::info!("[{}] live view available in Prusa Connect", self.camera.id);
            }
            self.emit("protobuf_version", self.protobuf_version(None))
                .await?;
            return self.emit("features", self.features()).await;
        }
        if code == ack::NOT_AUTHORIZED {
            return self.authenticate().await;
        }
        if code != ack::OK {
            log::debug!(
                "[{}] signaling answered {name} with code {code}",
                self.camera.id
            );
        }
        Ok(())
    }

    /// Handles an event from the server and returns the code to acknowledge it with.
    async fn event(&mut self, name: &str, data: &[u8]) -> Result<i64> {
        match name {
            "trigger" => {
                let Ok(trigger) = proto::Trigger::decode(data) else {
                    return Ok(ack::DECODE_MESSAGE);
                };
                let enabled = |field: i32| field == Toggle::Enable as i32;
                let mut code = ack::UNSUPPORTED_FEATURE;
                if enabled(trigger.get_features) {
                    self.emit("features", self.features()).await?;
                    code = ack::OK;
                }
                if enabled(trigger.get_protocol_information) {
                    let message = self.protobuf_version(trigger.request_id.clone());
                    self.emit("protobuf_version", message).await?;
                    code = ack::OK;
                }
                if enabled(trigger.get_status) {
                    self.emit("status", self.status(trigger.request_id)).await?;
                    code = ack::OK;
                }
                Ok(code)
            }
            "configuration" => {
                let Ok(configuration) = proto::Configuration::decode(data) else {
                    return Ok(ack::DECODE_MESSAGE);
                };
                match configuration.control.map(|c| c.webrtc_mode()) {
                    Some(mode @ (WebrtcMode::On | WebrtcMode::Off)) => {
                        self.mode = mode;
                        if mode == WebrtcMode::Off {
                            self.sessions.clear();
                        }
                        Ok(ack::OK)
                    }
                    _ => Ok(ack::UNSUPPORTED_FEATURE),
                }
            }
            "webrtc" => {
                let Ok(message) = proto::Webrtc::decode(data) else {
                    return Ok(ack::DECODE_MESSAGE);
                };
                self.webrtc(message).await
            }
            other => {
                log::debug!("[{}] ignoring signaling event {other:?}", self.camera.id);
                Ok(ack::UNSUPPORTED_FEATURE)
            }
        }
    }

    async fn webrtc(&mut self, message: proto::Webrtc) -> Result<i64> {
        if message.webrtc_stream_status() == StreamStatus::Stop {
            self.sessions.clear();
            return Ok(ack::OK);
        }
        if message.request_id.is_empty() {
            return Ok(ack::INVALID_COMMAND);
        }
        let request_id = message.request_id.clone();
        let sdp = message.sdp.clone().unwrap_or_default();
        match message.message_type() {
            MessageType::Request => {
                self.start_session(message).await?;
                Ok(ack::OK)
            }
            MessageType::Answer => {
                self.forward(&request_id, Command::Answer(sdp.candidate))
                    .await;
                Ok(ack::OK)
            }
            MessageType::Candidate => {
                let command = Command::Candidate {
                    candidate: sdp.candidate,
                    mid: sdp.mid,
                };
                self.forward(&request_id, command).await;
                Ok(ack::OK)
            }
            _ => Ok(ack::INVALID_COMMAND),
        }
    }

    async fn start_session(&mut self, request: proto::Webrtc) -> Result<()> {
        let request_id = request.request_id.clone();
        let full = self.sessions.len() >= MAX_VIEWERS && !self.sessions.contains_key(&request_id);
        if self.mode == WebrtcMode::Off || full {
            log::info!(
                "[{}] declining a live view request: {}",
                self.camera.id,
                if full {
                    "another viewer is watching"
                } else {
                    "live view is off"
                }
            );
            return self.reject(request_id).await;
        }
        let (commands, receiver) = mpsc::channel(32);
        self.sessions.insert(request_id.clone(), commands);
        tokio::spawn(session::run(Session {
            camera_id: self.camera.id.clone(),
            request_id,
            request,
            camera: self.camera.source.clone(),
            out: self.out.clone(),
            commands: receiver,
        }));
        Ok(())
    }

    async fn forward(&self, request_id: &str, command: Command) {
        if let Some(session) = self.sessions.get(request_id) {
            let _ = session.send(command).await;
        }
    }

    async fn reject(&mut self, request_id: String) -> Result<()> {
        let message = proto::ClientTrigger {
            camera_token: self.token(),
            request_id,
            fingerprint: self.camera.fingerprint.clone(),
            error_code: proto::ERROR_WEBRTC_START_FAIL,
        };
        self.emit("client_trigger", message).await
    }

    /// Sends a session's message to the server.
    async fn send(&mut self, message: Outgoing) -> Result<()> {
        match message {
            Outgoing::Offer { request_id, sdp } => {
                let sdp = proto::Sdp {
                    candidate: sdp,
                    mid: String::new(),
                };
                self.emit(
                    "webrtc",
                    self.webrtc_message(request_id, MessageType::Offer, sdp),
                )
                .await
            }
            Outgoing::Candidate {
                request_id,
                candidate,
                mid,
            } => {
                let sdp = proto::Sdp { candidate, mid };
                self.emit(
                    "webrtc",
                    self.webrtc_message(request_id, MessageType::Candidate, sdp),
                )
                .await
            }
            Outgoing::Connected {
                request_id,
                local,
                remote,
            } => {
                let message = proto::ConnectionInfo {
                    camera_token: self.token(),
                    request_id,
                    fingerprint: self.camera.fingerprint.clone(),
                    remote: remote as i32,
                    local: local as i32,
                    client_type: ClientType::Camera as i32,
                };
                self.emit("webrtc_connection_info", message).await
            }
            Outgoing::Rejected { request_id } => self.reject(request_id).await,
            Outgoing::Closed { request_id } => {
                if self
                    .sessions
                    .get(&request_id)
                    .is_some_and(|s| s.is_closed())
                {
                    self.sessions.remove(&request_id);
                }
                Ok(())
            }
        }
    }

    fn webrtc_message(
        &self,
        request_id: String,
        kind: MessageType,
        sdp: proto::Sdp,
    ) -> proto::Webrtc {
        proto::Webrtc {
            camera_token: self.token(),
            request_id,
            fingerprint: self.camera.fingerprint.clone(),
            sdp: Some(sdp),
            message_type: kind as i32,
            client_type: ClientType::Camera as i32,
            ..Default::default()
        }
    }

    fn protobuf_version(&self, request_id: Option<String>) -> proto::ProtobufVersion {
        proto::ProtobufVersion {
            protobuf_schema_version: proto::SCHEMA_VERSION.into(),
            camera_token: self.token(),
            request_id,
            fingerprint: self.camera.fingerprint.clone(),
        }
    }

    fn features(&self) -> proto::Features {
        proto::Features {
            camera_token: self.token(),
            fw_version: env!("CARGO_PKG_VERSION").into(),
            hw_version: self.camera.hardware.clone(),
            protobuf_schema_version: proto::SCHEMA_VERSION.into(),
            features: serde_json::to_string(&super::FEATURES).expect("serializing feature names"),
            fingerprint: self.camera.fingerprint.clone(),
        }
    }

    fn status(&self, request_id: Option<String>) -> proto::Status {
        let (width, height) = self.camera.resolution;
        proto::Status {
            image: Some(proto::Image {
                current_resolution: Some(proto::Resolution { width, height }),
            }),
            system: Some(proto::System {
                fw_version: env!("CARGO_PKG_VERSION").into(),
                hw_version: self.camera.hardware.clone(),
                camera_name: self.camera.name.clone(),
                webrtc: Some(proto::WebrtcState {
                    mode: self.mode as i32,
                }),
            }),
            camera_token: self.token(),
            request_id,
        }
    }
}

/// The board's model, reported to Prusa Connect as the camera's hardware.
pub fn hardware() -> String {
    std::fs::read_to_string("/proc/device-tree/model")
        .map(|model| model.trim_end_matches('\0').trim().to_owned())
        .ok()
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| "Raspberry Pi".into())
}
