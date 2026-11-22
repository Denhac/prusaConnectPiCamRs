//! Live video for anything that reads RTSP: the prusa-connect server's generic camera (as its
//! `stream_url`), VLC, ffmpeg, or a relay such as go2rtc or MediaMTX. One listener serves each
//! camera at `rtsp://<pi>:8554/<camera id>` from the camera's video hub, so every session shares
//! one encoder with the other outputs' viewers. It knows nothing about Prusa.
//!
//! RTP travels inside the RTSP connection (interleaved TCP): no UDP ports to open, and the one
//! transport the prusa-connect server's client speaks. There is no password: anyone on a private
//! network may watch, and clients from the internet are refused, in case the port is ever
//! forwarded. The video isn't encrypted.

mod rtp;

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::hub::{Caps, Hub, Subscription};
use crate::net::is_private;
use crate::status::{State, Status};
use rtp::Packetizer;

/// Keys the server's status, and prefixes its log lines.
const LABEL: &str = "rtsp";
/// What `Session: …;timeout=` announces. Clients send a keepalive every half of it; without the
/// announcement the prusa-connect server's client would send one every 2.5 s.
const SESSION_TIMEOUT: u64 = 60;
/// A connection that sends nothing for this long is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(2 * SESSION_TIMEOUT);
/// A client that takes this long to accept a write is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Frames (and replies) queued for a slow client before frames are dropped.
const OUT_QUEUE: usize = 64;
const MAX_LINE: u64 = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_BODY: u64 = 64 * 1024;
const PUBLIC: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER";

/// `[rtsp]`, in the config file or a drop-in.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The address and port to listen on.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Sessions streaming at once. Each is a copy of the video on the Pi's network.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// The most a session asks of the shared stream.
    #[serde(default = "default_max_width")]
    pub max_width: u32,
    #[serde(default = "default_max_fps")]
    pub max_fps: u32,
    /// Bits per second.
    #[serde(default = "default_max_bitrate")]
    pub max_bitrate: u32,
}

fn default_listen() -> String {
    "0.0.0.0:8554".into()
}

fn default_max_sessions() -> usize {
    4
}

fn default_max_width() -> u32 {
    1280
}

fn default_max_fps() -> u32 {
    12
}

fn default_max_bitrate() -> u32 {
    2_500_000
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        self.listen
            .parse::<SocketAddr>()
            .context("rtsp: listen must be address:port, e.g. \"0.0.0.0:8554\"")?;
        ensure!(
            self.max_sessions >= 1,
            "rtsp: max_sessions must be at least 1"
        );
        ensure!(
            self.max_width >= 64 && self.max_fps >= 1 && self.max_bitrate >= 100_000,
            "rtsp: max_width, max_fps and max_bitrate are too low to make a stream"
        );
        Ok(())
    }
}

/// Starts the server on a thread of its own, for the cameras with video.
pub fn spawn(
    config: &Config,
    cameras: Vec<(String, Hub)>,
    status: Status,
) -> Result<thread::JoinHandle<()>> {
    let server = Arc::new(Server {
        listen: config.listen.parse()?,
        cameras: cameras.into_iter().collect(),
        caps: Caps {
            max_width: config.max_width,
            max_fps: config.max_fps,
            max_bitrate: config.max_bitrate,
        },
        max_sessions: config.max_sessions,
        watching: AtomicUsize::new(0),
        refused: Mutex::default(),
        status,
    });
    server.status.register(LABEL);
    thread::Builder::new()
        .name("rtsp".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("rtsp-runtime")
                .enable_all()
                .build()
                .expect("building the RTSP runtime");
            runtime.block_on(server.run());
        })
        .context("starting the RTSP server")
}

struct Server {
    listen: SocketAddr,
    cameras: HashMap<String, Hub>,
    caps: Caps,
    max_sessions: usize,
    watching: AtomicUsize,
    /// Addresses already logged as refused, so a scanner doesn't flood the log.
    refused: Mutex<HashSet<IpAddr>>,
    status: Status,
}

impl Server {
    async fn run(self: Arc<Self>) {
        let listener = loop {
            match TcpListener::bind(self.listen).await {
                Ok(listener) => break listener,
                Err(e) => {
                    let message = format!("can't listen on {}: {e}", self.listen);
                    log::warn!("[{LABEL}] {message}; trying again in 30s");
                    self.status.set(LABEL, State::Retrying, message);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        };
        let mut paths: Vec<&str> = self.cameras.keys().map(String::as_str).collect();
        paths.sort_unstable();
        log::info!(
            "[{LABEL}] serving rtsp://{}/{}",
            self.listen,
            paths.join(", /")
        );
        self.report();
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    log::warn!("[{LABEL}] accepting a connection failed: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if !is_private(peer.ip()) {
                if self.refused.lock().unwrap().insert(peer.ip()) {
                    log::warn!(
                        "[{LABEL}] refused {}: only private networks may watch",
                        peer.ip()
                    );
                }
                continue;
            }
            let _ = stream.set_nodelay(true);
            tokio::spawn(Arc::clone(&self).connection(stream, peer));
        }
    }

    fn report(&self) {
        let watching = self.watching.load(Ordering::SeqCst);
        let message = format!("listening on {}, {watching} watching", self.listen);
        self.status.set(LABEL, State::Ok, message);
    }

    async fn connection(self: Arc<Self>, stream: TcpStream, peer: SocketAddr) {
        let local = stream.local_addr().map_or(self.listen.ip(), |a| a.ip());
        let (reader, writer) = stream.into_split();
        let (out, outgoing) = mpsc::channel(OUT_QUEUE);
        let mut writer_task = tokio::spawn(write_out(writer, outgoing));
        let mut reader = BufReader::new(reader);
        let mut connection = Connection {
            server: self,
            peer,
            local,
            session: None,
            out,
        };
        log::debug!("[{LABEL}] {} connected", peer.ip());
        loop {
            let message = tokio::select! {
                message = timeout(IDLE_TIMEOUT, read_message(&mut reader)) => message,
                _ = &mut writer_task => break,
            };
            let request = match message {
                Ok(Ok(Some(Incoming::Request(request)))) => request,
                // RTCP from the client, which this server doesn't act on.
                Ok(Ok(Some(Incoming::Interleaved))) => continue,
                Ok(Ok(None)) => break,
                Ok(Err(e)) => {
                    log::debug!("[{LABEL}] {}: {e}", peer.ip());
                    break;
                }
                Err(_) => {
                    log::debug!("[{LABEL}] {} sent nothing for a while", peer.ip());
                    break;
                }
            };
            let reply = connection.handle(&request);
            if connection.out.send(reply).await.is_err() {
                break;
            }
        }
        drop(connection);
        writer_task.abort();
        log::debug!("[{LABEL}] {} disconnected", peer.ip());
    }
}

/// Writes replies and RTP to the client, one whole message at a time.
async fn write_out(mut writer: OwnedWriteHalf, mut outgoing: mpsc::Receiver<Bytes>) {
    while let Some(bytes) = outgoing.recv().await {
        if !matches!(
            timeout(WRITE_TIMEOUT, writer.write_all(&bytes)).await,
            Ok(Ok(()))
        ) {
            return;
        }
    }
}

struct Connection {
    server: Arc<Server>,
    peer: SocketAddr,
    /// This end's address, for the SDP.
    local: IpAddr,
    session: Option<Session>,
    out: mpsc::Sender<Bytes>,
}

struct Session {
    id: String,
    camera: String,
    /// The interleaved channel RTP goes out on.
    channel: u8,
    /// The track URL from SETUP, which RTP-Info names.
    track: String,
    media: Option<JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(media) = self.media.take() {
            media.abort();
        }
    }
}

impl Connection {
    fn handle(&mut self, request: &Request) -> Bytes {
        let cseq = request.header("cseq").unwrap_or("0").to_owned();
        let reply = Reply::new(&cseq);
        let method = request.method.as_str();
        if method == "OPTIONS" {
            return reply.header("Public", PUBLIC).render();
        }
        match method {
            "DESCRIBE" => self.describe(reply, &request.uri),
            "SETUP" => self.setup(reply, request),
            "PLAY" => self.play(reply, request),
            "PAUSE" => {
                if let Some(media) = self.session.as_mut().and_then(|s| s.media.take()) {
                    media.abort();
                }
                self.with_session(reply).render()
            }
            "TEARDOWN" => {
                let reply = self.with_session(reply);
                self.session = None;
                reply.render()
            }
            "GET_PARAMETER" | "SET_PARAMETER" => self.with_session(reply).render(),
            _ => reply.status(501).render(),
        }
    }

    fn with_session(&self, reply: Reply) -> Reply {
        match &self.session {
            Some(session) => reply.header("Session", &session.id),
            None => reply,
        }
    }

    fn describe(&self, reply: Reply, uri: &str) -> Bytes {
        let (camera, _) = route(uri);
        if !self.server.cameras.contains_key(camera) {
            return reply.status(404).render();
        }
        let session = uuid::Uuid::new_v4().as_u64_pair().0 >> 1;
        reply
            .header("Content-Type", "application/sdp")
            .body(rtp::sdp(self.local, session))
            .render()
    }

    fn setup(&mut self, reply: Reply, request: &Request) -> Bytes {
        let (camera, _) = route(&request.uri);
        if !self.server.cameras.contains_key(camera) {
            return reply.status(404).render();
        }
        let transport = request.header("transport").unwrap_or("");
        if !transport.to_ascii_uppercase().contains("RTP/AVP/TCP") {
            // Only interleaved TCP: UDP would need ports opened through every firewall.
            return reply.status(461).render();
        }
        let channel = interleaved_channel(transport);
        let id = format!("{:016x}", uuid::Uuid::new_v4().as_u64_pair().0);
        let timeout = format!("{id};timeout={SESSION_TIMEOUT}");
        self.session = Some(Session {
            id,
            camera: camera.to_owned(),
            channel,
            track: request.uri.clone(),
            media: None,
        });
        let transport = format!(
            "RTP/AVP/TCP;unicast;interleaved={channel}-{}",
            channel.saturating_add(1)
        );
        reply
            .header("Transport", &transport)
            .header("Session", &timeout)
            .render()
    }

    fn play(&mut self, reply: Reply, request: &Request) -> Bytes {
        let server = Arc::clone(&self.server);
        let peer = self.peer.ip();
        let out = self.out.clone();
        let Some(session) = self.session.as_mut() else {
            return reply.status(455).render();
        };
        let named = request
            .header("session")
            .map(|s| s.split(';').next().unwrap_or(""));
        if named.is_some_and(|id| id.trim() != session.id) {
            return reply.status(454).render();
        }
        let reply = reply
            .header("Session", &session.id)
            .header("Range", "npt=0.000-");
        if session.media.is_some() {
            return reply.render();
        }
        if server.watching.load(Ordering::SeqCst) >= server.max_sessions {
            log::warn!(
                "[{LABEL}] {peer} can't watch: {} sessions already are",
                server.max_sessions
            );
            return reply.status(453).render();
        }
        let hub = &server.cameras[&session.camera];
        let packetizer = Packetizer::new(session.channel);
        let rtp_info = format!(
            "url={};seq={};rtptime={}",
            session.track,
            packetizer.sequence(),
            packetizer.base()
        );
        let watcher = Watcher::new(Arc::clone(&server), &session.camera, peer);
        session.media = Some(tokio::spawn(stream(
            hub.subscribe(server.caps),
            packetizer,
            out,
            watcher,
        )));
        reply.header("RTP-Info", &rtp_info).render()
    }
}

/// Counts a session that streams, and logs its start and end.
struct Watcher {
    server: Arc<Server>,
    label: String,
    peer: IpAddr,
}

impl Watcher {
    fn new(server: Arc<Server>, camera: &str, peer: IpAddr) -> Self {
        let watching = server.watching.fetch_add(1, Ordering::SeqCst) + 1;
        let label = format!("{camera}/{LABEL}");
        log::info!("[{label}] {peer} started watching; RTSP sessions: {watching}");
        server.report();
        Self {
            server,
            label,
            peer,
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let watching = self.server.watching.fetch_sub(1, Ordering::SeqCst) - 1;
        log::info!(
            "[{}] {} stopped watching; RTSP sessions: {watching}",
            self.label,
            self.peer
        );
        self.server.report();
    }
}

/// Sends a subscription's frames as RTP until the session ends. A client that falls behind
/// loses frames until the next keyframe, since the frames between refer to the ones it lost.
async fn stream(
    mut subscription: Subscription,
    mut packetizer: Packetizer,
    out: mpsc::Sender<Bytes>,
    _watcher: Watcher,
) {
    let mut synced = true;
    while let Some(frame) = subscription.frames.recv().await {
        synced |= frame.keyframe;
        if !synced {
            continue;
        }
        let mut packets = Vec::with_capacity(frame.data.len() + frame.data.len() / 64 + 64);
        packetizer.packetize(&frame.data, frame.captured, &mut packets);
        match out.try_send(packets.into()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => synced = false,
            Err(TrySendError::Closed(_)) => return,
        }
    }
}

/// The interleaved channel a SETUP's `Transport` asks for (`interleaved=0-1`), 0 by default.
fn interleaved_channel(transport: &str) -> u8 {
    transport
        .split(';')
        .find_map(|part| part.trim().strip_prefix("interleaved="))
        .and_then(|channels| channels.split('-').next()?.trim().parse().ok())
        .unwrap_or(0)
}

/// The camera a request URI names, and the rest of its path: `rtsp://pi:8554/cam0/trackID=0`
/// is `("cam0", "trackID=0")`. The host is ignored, since clients spell it in many ways.
fn route(uri: &str) -> (&str, &str) {
    let path = match uri.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map_or("", |(_, path)| path),
        None => uri.trim_start_matches('/'),
    };
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    path.split_once('/').unwrap_or((path, ""))
}

struct Reply {
    status: u16,
    cseq: String,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn new(cseq: &str) -> Self {
        Self {
            status: 200,
            cseq: cseq.to_owned(),
            headers: Vec::new(),
            body: String::new(),
        }
    }

    fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    fn body(mut self, body: String) -> Self {
        self.body = body;
        self
    }

    fn render(self) -> Bytes {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            453 => "Not Enough Bandwidth",
            454 => "Session Not Found",
            455 => "Method Not Valid in This State",
            461 => "Unsupported Transport",
            501 => "Not Implemented",
            _ => "Error",
        };
        let mut text = format!(
            "RTSP/1.0 {} {reason}\r\nCSeq: {}\r\nServer: prusa-connect-camera/{}\r\n",
            self.status,
            self.cseq,
            env!("CARGO_PKG_VERSION")
        );
        for (name, value) in &self.headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str(&format!(
            "Content-Length: {}\r\n\r\n{}",
            self.body.len(),
            self.body
        ));
        text.into()
    }
}

enum Incoming {
    Request(Request),
    /// An interleaved packet from the client, already read and discarded.
    Interleaved,
}

struct Request {
    method: String,
    uri: String,
    /// Lower-cased names.
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Reads one request, or one interleaved packet, or `None` at the end of the connection.
async fn read_message(reader: &mut BufReader<OwnedReadHalf>) -> io::Result<Option<Incoming>> {
    let first = match reader.fill_buf().await?.first() {
        Some(&byte) => byte,
        None => return Ok(None),
    };
    if first == b'$' {
        let mut header = [0; 4];
        reader.read_exact(&mut header).await?;
        let length = u16::from_be_bytes([header[2], header[3]]);
        tokio::io::copy(
            &mut (&mut *reader).take(length.into()),
            &mut tokio::io::sink(),
        )
        .await?;
        return Ok(Some(Incoming::Interleaved));
    }
    // Some clients send blank lines between requests.
    let mut line = read_line(reader).await?;
    while line.is_empty() {
        if reader.fill_buf().await?.is_empty() {
            return Ok(None);
        }
        line = read_line(reader).await?;
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(uri), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("the request line is malformed"));
    };
    if !version.starts_with("RTSP/") {
        return Err(invalid("not an RTSP request"));
    }
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader).await?;
        if line.is_empty() {
            break;
        }
        if headers.len() == MAX_HEADERS {
            return Err(invalid("too many headers"));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let request = Request {
        method: method.to_ascii_uppercase(),
        uri: uri.to_owned(),
        headers,
    };
    let length: u64 = request
        .header("content-length")
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(invalid("the body is too large"));
    }
    tokio::io::copy(&mut (&mut *reader).take(length), &mut tokio::io::sink()).await?;
    Ok(Some(Incoming::Request(request)))
}

/// Reads one line, without its line ending.
async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> io::Result<String> {
    let mut line = Vec::new();
    (&mut *reader)
        .take(MAX_LINE)
        .read_until(b'\n', &mut line)
        .await?;
    if !line.ends_with(b"\n") {
        return Err(invalid(
            "a line is too long, or the connection ended mid-line",
        ));
    }
    let line = String::from_utf8(line).map_err(|_| invalid("a line isn't UTF-8"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_by_path() {
        assert_eq!(route("rtsp://10.0.0.36:8554/cam0"), ("cam0", ""));
        assert_eq!(route("rtsp://pi/cam0/"), ("cam0", ""));
        assert_eq!(
            route("rtsp://pi:8554/cam0/trackID=0"),
            ("cam0", "trackID=0")
        );
        assert_eq!(route("rtsp://pi:8554/cam0?x=1"), ("cam0", ""));
        assert_eq!(route("/cam0/trackID=0"), ("cam0", "trackID=0"));
        assert_eq!(route("rtsp://pi:8554"), ("", ""));
    }

    #[test]
    fn reads_the_interleaved_channel() {
        assert_eq!(
            interleaved_channel("RTP/AVP/TCP;unicast;interleaved=0-1"),
            0
        );
        assert_eq!(
            interleaved_channel("RTP/AVP/TCP;interleaved=2-3;unicast"),
            2
        );
        assert_eq!(interleaved_channel("RTP/AVP/TCP;unicast"), 0);
    }

    #[test]
    fn checks_the_config() {
        let config = |listen: &str| Config {
            listen: listen.into(),
            max_sessions: 4,
            max_width: 1280,
            max_fps: 12,
            max_bitrate: 2_500_000,
        };
        assert!(config("0.0.0.0:8554").validate().is_ok());
        assert!(config("[::]:8554").validate().is_ok());
        assert!(config("0.0.0.0").validate().is_err());
        assert!(config("pi.local:8554").validate().is_err());
    }
}
