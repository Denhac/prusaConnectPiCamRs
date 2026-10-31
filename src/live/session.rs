//! One live viewer: a WebRTC connection streaming the camera's H.264 to Prusa Connect, set up
//! the way Prusa's browser webcam does it. The camera makes the offer, both sides trickle ICE
//! candidates through signaling, and a "ping-pong" data channel keeps the session alive.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use rtc::ice::mdns::MulticastDnsMode;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::statistics::stats::ice_candidate_pair::RTCStatsIceCandidatePairState;
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceCandidateType, RTCIceServer,
    RTCIceTransportPolicy, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSessionDescription, RTCStatsReportEntry, Registry, SettingEngineBuilder, StatsSelector,
    register_default_interceptors,
};
use webrtc::rtp_transceiver::RtpSender;

use super::proto::{self, ApplyScope, ConnectionType, IcePolicy, VideoQuality};
use crate::source::rpicam::{LiveStream, Rpicam};

const PAYLOAD_TYPE: u8 = 102;
/// The profile Prusa Connect's viewers accept; the camera's SPS is patched to match.
const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
/// Frame rate when the viewer doesn't ask for one, as in Prusa's webcam.
const DEFAULT_FPS: u32 = 12;
/// Frame rate for relayed streams without video settings, as in Prusa's webcam.
const RELAY_FPS: u32 = 10;
/// How long a disconnected viewer has to come back.
const RECONNECT_GRACE: Duration = Duration::from_secs(30);

/// Messages a session sends through signaling.
pub enum Outgoing {
    Offer {
        request_id: String,
        sdp: String,
    },
    Candidate {
        request_id: String,
        candidate: String,
        mid: String,
    },
    Connected {
        request_id: String,
        local: ConnectionType,
        remote: ConnectionType,
    },
    /// The stream couldn't start.
    Rejected {
        request_id: String,
    },
    Closed {
        request_id: String,
    },
}

/// What signaling forwards to a running session.
pub enum Command {
    Answer(String),
    Candidate { candidate: String, mid: String },
}

pub struct Session {
    pub camera_id: String,
    pub request_id: String,
    pub request: proto::Webrtc,
    pub camera: Rpicam,
    /// The size of the sensor mode live view reads, which gives its streams their shape.
    pub sensor_size: (u32, u32),
    pub out: mpsc::Sender<Outgoing>,
    /// Closed by signaling to end the session.
    pub commands: mpsc::Receiver<Command>,
}

/// Runs a session until the viewer leaves. The camera goes back to snapshots when the session's
/// stream stops.
pub async fn run(mut session: Session) {
    let request_id = session.request_id.clone();
    let id = session.camera_id.clone();
    let out = session.out.clone();
    let mut connected = false;
    match serve(&mut session, &mut connected).await {
        Ok(reason) => log::info!("[{id}] live view ended: {reason}"),
        Err(e) => {
            log::warn!("[{id}] live view failed: {e:#}");
            if !connected {
                let _ = out
                    .send(Outgoing::Rejected {
                        request_id: request_id.clone(),
                    })
                    .await;
            }
        }
    }
    let _ = out.send(Outgoing::Closed { request_id }).await;
}

/// Hands the camera back to snapshots when a session ends, however it ends.
struct LiveGuard {
    camera: Rpicam,
    id: u64,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.camera.stop_live(self.id);
    }
}

/// Stops a spawned task when dropped.
struct TaskGuard(tokio::task::JoinHandle<Result<()>>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The stream to encode for a request: the widths and bitrates of Prusa's webcam presets, with
/// the sensor mode's shape so nothing is cropped, and never wider than the mode itself.
pub fn stream_for(
    settings: Option<&proto::VideoSettings>,
    relay_only: bool,
    (sensor_width, sensor_height): (u32, u32),
) -> LiveStream {
    let (quality, framerate) = match settings {
        Some(s) => (s.video_quality(), Some(s.video_fps).filter(|&fps| fps > 0)),
        None if relay_only => (VideoQuality::Sd, Some(RELAY_FPS)),
        None => (VideoQuality::Hd, None),
    };
    let (width, bitrate) = match quality {
        VideoQuality::Sd => (640, 900_000),
        VideoQuality::Fhd => (1920, 6_500_000),
        VideoQuality::Hd | VideoQuality::Invalid => (1280, 2_500_000),
    };
    let width = width.min(sensor_width);
    let height = u64::from(width) * u64::from(sensor_height) / u64::from(sensor_width);
    LiveStream {
        // The H.264 encoder needs even dimensions.
        width: width & !1,
        height: height as u32 & !1,
        framerate: framerate.unwrap_or(DEFAULT_FPS),
        bitrate,
    }
}

async fn serve(s: &mut Session, connected: &mut bool) -> Result<&'static str> {
    let ice = s.request.ice_configuration.clone().unwrap_or_default();
    let video = s.request.video_settings.clone();
    let relay_only = ice.ice_transport_policy() == IcePolicy::Relay;
    let stream = stream_for(video.as_ref(), relay_only, s.sensor_size);
    log::info!(
        "[{}] live view requested: {}x{} at {} fps, {} kbit/s{}",
        s.camera_id,
        stream.width,
        stream.height,
        stream.framerate,
        stream.bitrate / 1000,
        if relay_only { ", relayed" } else { "" }
    );

    let codec = RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: H264_FMTP.to_owned(),
        rtcp_feedback: vec![
            RTCPFeedback {
                typ: "nack".into(),
                parameter: String::new(),
            },
            RTCPFeedback {
                typ: "nack".into(),
                parameter: "pli".into(),
            },
        ],
    };
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: codec.clone(),
            payload_type: PAYLOAD_TYPE,
        },
        RtpCodecKind::Video,
    )?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
    let configuration = RTCConfigurationBuilder::new()
        .with_ice_servers(ice_servers(&ice))
        .with_ice_transport_policy(if relay_only {
            RTCIceTransportPolicy::Relay
        } else {
            RTCIceTransportPolicy::All
        })
        .build();
    // Browsers hide their LAN addresses behind mDNS names; resolve them for local viewers.
    let settings = SettingEngineBuilder::new()
        .with_multicast_dns_mode(MulticastDnsMode::QueryOnly)
        .with_multicast_dns_timeout(Some(Duration::from_secs(5)))
        .build();

    let (states, mut state_changes) = mpsc::channel(16);
    let candidates = Arc::new(Mutex::new(Candidates::default()));
    let handler = Arc::new(Handler {
        request_id: s.request_id.clone(),
        out: s.out.clone(),
        states,
        candidates: Arc::clone(&candidates),
    });
    let addresses = host_addresses()?;
    log::debug!("[{}] host candidates on {addresses:?}", s.camera_id);
    let pc = PeerConnectionBuilder::new()
        .with_configuration(configuration)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .with_handler(handler)
        .with_runtime(webrtc::runtime::default_runtime().context("no WebRTC runtime")?)
        .with_udp_addrs(
            addresses
                .into_iter()
                .map(|ip| SocketAddr::new(ip, 0))
                .collect(),
        )
        .build()
        .await?;

    let result = drive(
        s,
        &pc,
        &mut state_changes,
        stream,
        video.as_ref(),
        &candidates,
        connected,
    )
    .await;
    let _ = pc.close().await;
    result
}

async fn drive(
    s: &mut Session,
    pc: &impl PeerConnection,
    state_changes: &mut mpsc::Receiver<RTCPeerConnectionState>,
    stream: LiveStream,
    video: Option<&proto::VideoSettings>,
    candidates: &Mutex<Candidates>,
    connected: &mut bool,
) -> Result<&'static str> {
    let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
    let track = Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "prusa-connect-camera".into(),
            "video".into(),
            "video".into(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90_000,
                    sdp_fmtp_line: H264_FMTP.to_owned(),
                    ..Default::default()
                },
                ..Default::default()
            }],
        ),
    )?);
    let sender = pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await?;
    let channel = pc.create_data_channel("ping-pong", None).await?;
    tokio::spawn(ping_pong(channel));

    let offer = pc.create_offer(None).await?;
    pc.set_local_description(offer.clone()).await?;
    s.out
        .send(Outgoing::Offer {
            request_id: s.request_id.clone(),
            sdp: offer.sdp,
        })
        .await?;

    // Locals drop in reverse order, so when the session ends the sending stops first, then the
    // camera goes back to snapshots.
    let mut _live: Option<LiveGuard> = None;
    let mut media: Option<TaskGuard> = None;
    let mut ttl_deadline: Option<tokio::time::Instant> = None;
    let mut disconnected_deadline: Option<tokio::time::Instant> = None;
    loop {
        let deadline = [ttl_deadline, disconnected_deadline]
            .into_iter()
            .flatten()
            .min();
        tokio::select! {
            command = s.commands.recv() => match command {
                Some(Command::Answer(sdp)) => {
                    pc.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
                }
                Some(Command::Candidate { candidate, mid }) => {
                    candidates.lock().unwrap().viewer.push(describe_candidate(&candidate));
                    if let Err(e) = pc.add_ice_candidate(candidate_init(&candidate, &mid)).await {
                        log::debug!("[{}] ignoring viewer ICE candidate: {e}", s.camera_id);
                    }
                }
                None => return Ok("stopped by Prusa Connect"),
            },
            Some(state) = state_changes.recv() => match state {
                RTCPeerConnectionState::Connected => {
                    *connected = true;
                    disconnected_deadline = None;
                    if media.is_none() {
                        let live = s.camera.start_live(stream);
                        _live = Some(LiveGuard { camera: s.camera.clone(), id: live.id });
                        media = Some(TaskGuard(tokio::spawn(send_frames(
                            Arc::clone(&track),
                            Arc::clone(&sender),
                            live.frames,
                            stream.framerate,
                        ))));
                        let (local, remote) = connection_types(pc).await;
                        let kind = connection_kind(local, remote);
                        log::info!(
                            "[{}] live view connected ({kind}); snapshots paused",
                            s.camera_id
                        );
                        s.out
                            .send(Outgoing::Connected {
                                request_id: s.request_id.clone(),
                                local,
                                remote,
                            })
                            .await?;
                        let ttl = video
                            .map(|v| v.ttl)
                            .filter(|&ttl| ttl > 0)
                            .or(s.request.ice_configuration.as_ref().map(|i| i.ttl))
                            .filter(|&ttl| ttl > 0);
                        if let Some(ttl) = ttl
                            && scope_applies(video.map(|v| v.apply_scope()), kind)
                        {
                            ttl_deadline = Some(
                                tokio::time::Instant::now() + Duration::from_secs(ttl.into()),
                            );
                        }
                    }
                }
                RTCPeerConnectionState::Disconnected => {
                    disconnected_deadline.get_or_insert(tokio::time::Instant::now() + RECONNECT_GRACE);
                }
                RTCPeerConnectionState::Failed => {
                    bail!("the connection to the viewer failed ({})", candidates.lock().unwrap());
                }
                RTCPeerConnectionState::Closed => return Ok("the viewer closed the connection"),
                _ => {}
            },
            result = async { (&mut media.as_mut().unwrap().0).await }, if media.is_some() => {
                result??;
                return Ok("the camera stopped streaming");
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                return Ok(if ttl_deadline.is_some_and(|t| t <= tokio::time::Instant::now()) {
                    "time limit reached"
                } else {
                    "the viewer disconnected"
                });
            }
        }
    }
}

/// Packetizes the camera's access units onto the video track.
async fn send_frames(
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    mut frames: mpsc::Receiver<Bytes>,
    framerate: u32,
) -> Result<()> {
    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|codec| codec.payload_type)
        .context("the viewer accepted no video codec")?;
    let ssrc = *track
        .ssrcs()
        .await
        .first()
        .context("the video track has no SSRC")?;
    let duration = Duration::from_secs(1) / framerate.max(1);
    while let Some(data) = frames.recv().await {
        track
            .sample_writer(ssrc, payload_type)
            .write_sample(&Sample {
                data,
                duration,
                ..Sample::new(Instant::now())
            })
            .await?;
    }
    Ok(())
}

/// Answers the viewer's keep-alive messages, as Prusa's webcam does.
async fn ping_pong(channel: Arc<dyn DataChannel>) {
    while let Some(event) = channel.poll().await {
        match event {
            DataChannelEvent::OnOpen | DataChannelEvent::OnMessage(_) => {
                let _ = channel.send_text("Ping").await;
            }
            DataChannelEvent::OnClose => break,
            _ => {}
        }
    }
}

struct Handler {
    request_id: String,
    out: mpsc::Sender<Outgoing>,
    states: mpsc::Sender<RTCPeerConnectionState>,
    candidates: Arc<Mutex<Candidates>>,
}

/// Both sides' ICE candidates, to explain a connection that fails.
#[derive(Default)]
struct Candidates {
    camera: Vec<String>,
    viewer: Vec<String>,
}

impl std::fmt::Display for Candidates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = |c: &[String]| {
            if c.is_empty() {
                "none".to_owned()
            } else {
                c.join(", ")
            }
        };
        write!(
            f,
            "camera candidates: {}; viewer candidates: {}",
            list(&self.camera),
            list(&self.viewer)
        )
    }
}

/// Shortens an ICE candidate to its type and address, e.g. "srflx 203.0.113.7".
fn describe_candidate(candidate: &str) -> String {
    let fields: Vec<&str> = candidate.split_whitespace().collect();
    let typ = fields
        .iter()
        .position(|&f| f == "typ")
        .and_then(|i| fields.get(i + 1));
    match (typ, fields.get(4)) {
        (Some(typ), Some(address)) => format!("{typ} {address}"),
        _ => "unparsed".to_owned(),
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let Ok(init) = event.candidate.to_json() else {
            return;
        };
        if init.candidate.is_empty() {
            return;
        }
        self.candidates
            .lock()
            .unwrap()
            .camera
            .push(describe_candidate(&init.candidate));
        // Prusa's webcam sends candidates in SDP attribute form.
        let candidate = if init.candidate.starts_with("a=") {
            init.candidate
        } else {
            format!("a={}", init.candidate)
        };
        let _ = self
            .out
            .send(Outgoing::Candidate {
                request_id: self.request_id.clone(),
                candidate,
                mid: init.sdp_mid.unwrap_or_default(),
            })
            .await;
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.states.send(state).await;
    }
}

fn ice_servers(ice: &proto::IceConfiguration) -> Vec<RTCIceServer> {
    let servers: Vec<_> = ice
        .ice_servers
        .iter()
        .map(|server| RTCIceServer {
            urls: proto::ice_urls(server),
            username: server.username.clone(),
            credential: server.credential.clone(),
        })
        .filter(|server| !server.urls.is_empty())
        .collect();
    if servers.is_empty() {
        // Prusa's webcam falls back to Google's public STUN servers.
        return vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".into()],
            ..Default::default()
        }];
    }
    servers
}

/// The viewer's candidates arrive in SDP attribute form (`a=candidate:…`).
fn candidate_init(candidate: &str, mid: &str) -> RTCIceCandidateInit {
    RTCIceCandidateInit {
        candidate: candidate.strip_prefix("a=").unwrap_or(candidate).to_owned(),
        sdp_mid: (!mid.is_empty()).then(|| mid.to_owned()),
        sdp_mline_index: mid.is_empty().then_some(0),
        ..Default::default()
    }
}

/// The IPv4 addresses of every interface that is up, for host ICE candidates. Besides the LAN,
/// this includes VPNs such as Tailscale, so a viewer on the same tailnet can connect directly.
fn host_addresses() -> Result<Vec<IpAddr>> {
    let mut addresses = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that is only read here and then
    // released with freeifaddrs. Each ifa_addr is checked for null and for AF_INET before it is
    // read as a sockaddr_in.
    unsafe {
        if libc::getifaddrs(&mut list) == 0 {
            let mut entry = list;
            while let Some(ifa) = entry.as_ref() {
                let up = ifa.ifa_flags & libc::IFF_UP as u32 != 0;
                let loopback = ifa.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
                if up
                    && !loopback
                    && !ifa.ifa_addr.is_null()
                    && i32::from((*ifa.ifa_addr).sa_family) == libc::AF_INET
                {
                    let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    addresses.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                        addr.sin_addr.s_addr,
                    ))));
                }
                entry = ifa.ifa_next;
            }
            libc::freeifaddrs(list);
        }
    }
    if addresses.is_empty() {
        addresses.push(local_ip()?);
    }
    Ok(addresses)
}

/// The address of the interface that routes to the internet.
fn local_ip() -> Result<IpAddr> {
    // Connecting a UDP socket sends nothing; it only selects the outgoing interface.
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("8.8.8.8:53")?;
    Ok(socket.local_addr()?.ip())
}

/// The candidate types of the connection's selected pair.
async fn connection_types(pc: &impl PeerConnection) -> (ConnectionType, ConnectionType) {
    let report = pc.get_stats(Instant::now(), StatsSelector::None).await;
    let pairs = || {
        report.iter().filter_map(|entry| match entry {
            RTCStatsReportEntry::IceCandidatePair(pair) => Some(pair),
            _ => None,
        })
    };
    // Like Prusa's webcam: the transport's selected pair, else a nominated or succeeded one.
    let pair = report
        .iter()
        .find_map(|entry| match entry {
            RTCStatsReportEntry::Transport(t) => match report.get(&t.selected_candidate_pair_id) {
                Some(RTCStatsReportEntry::IceCandidatePair(pair)) => Some(pair),
                _ => None,
            },
            _ => None,
        })
        .or_else(|| pairs().find(|pair| pair.nominated))
        .or_else(|| pairs().find(|pair| pair.state == RTCStatsIceCandidatePairState::Succeeded));
    let Some(pair) = pair else {
        log::debug!("no candidate pair among {} stats entries", report.len());
        return (ConnectionType::Unknown, ConnectionType::Unknown);
    };
    // A pair holds its candidates' bare ids, but webrtc-rs keys the candidates themselves with a
    // prefix, so try both.
    let candidate_type = |id: &str, prefix: &str| match report
        .get(id)
        .or_else(|| report.get(&format!("{prefix}{id}")))
    {
        Some(RTCStatsReportEntry::LocalCandidate(c) | RTCStatsReportEntry::RemoteCandidate(c)) => {
            match c.candidate_type {
                RTCIceCandidateType::Host => ConnectionType::Host,
                RTCIceCandidateType::Srflx => ConnectionType::ServerReflexive,
                RTCIceCandidateType::Prflx => ConnectionType::PeerReflexive,
                RTCIceCandidateType::Relay => ConnectionType::Relayed,
                _ => ConnectionType::Unknown,
            }
        }
        _ => ConnectionType::Unknown,
    };
    (
        candidate_type(&pair.local_candidate_id, "RTCLocalIceCandidate_"),
        candidate_type(&pair.remote_candidate_id, "RTCRemoteIceCandidate_"),
    )
}

/// Summarizes a connection the way Prusa's webcam does: relayed, through STUN, or direct.
fn connection_kind(local: ConnectionType, remote: ConnectionType) -> &'static str {
    let either = |t| local == t || remote == t;
    if either(ConnectionType::Fail) {
        "failed"
    } else if either(ConnectionType::Relayed) {
        "turn"
    } else if either(ConnectionType::ServerReflexive) {
        "stun"
    } else if either(ConnectionType::Unknown) {
        "unknown"
    } else {
        "p2p"
    }
}

fn scope_applies(scope: Option<ApplyScope>, kind: &str) -> bool {
    match (scope, kind) {
        (_, "unknown") => true,
        (Some(ApplyScope::Turn), kind) => kind == "turn",
        (Some(ApplyScope::Stun), kind) => kind == "stun",
        (Some(ApplyScope::P2p), kind) => kind == "p2p",
        (Some(ApplyScope::TurnAndStun), kind) => kind == "turn" || kind == "stun",
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE_2: (u32, u32) = (1640, 1232);
    const MODULE_3: (u32, u32) = (2304, 1296);

    fn size(quality: VideoQuality, sensor: (u32, u32)) -> (u32, u32) {
        let settings = proto::VideoSettings {
            video_quality: quality as i32,
            ..Default::default()
        };
        let stream = stream_for(Some(&settings), false, sensor);
        (stream.width, stream.height)
    }

    #[test]
    fn picks_stream_settings() {
        let fhd = proto::VideoSettings {
            video_quality: VideoQuality::Fhd as i32,
            video_fps: 24,
            ..Default::default()
        };
        assert_eq!(
            stream_for(Some(&fhd), false, MODULE_2),
            LiveStream {
                width: 1640,
                height: 1232,
                framerate: 24,
                bitrate: 6_500_000
            }
        );
        assert_eq!(stream_for(None, false, MODULE_2).framerate, DEFAULT_FPS);
        assert_eq!(
            stream_for(None, true, MODULE_2),
            LiveStream {
                width: 640,
                height: 480,
                framerate: 10,
                bitrate: 900_000
            }
        );
    }

    #[test]
    fn streams_take_the_shape_of_the_sensor_mode() {
        assert_eq!(size(VideoQuality::Sd, MODULE_2), (640, 480));
        assert_eq!(size(VideoQuality::Hd, MODULE_2), (1280, 960));
        assert_eq!(size(VideoQuality::Fhd, MODULE_2), (1640, 1232));
        assert_eq!(size(VideoQuality::Sd, MODULE_3), (640, 360));
        assert_eq!(size(VideoQuality::Hd, MODULE_3), (1280, 720));
        assert_eq!(size(VideoQuality::Fhd, MODULE_3), (1920, 1080));
    }

    #[test]
    fn classifies_connections_like_prusas_webcam() {
        use ConnectionType::*;
        assert_eq!(connection_kind(Host, Host), "p2p");
        assert_eq!(connection_kind(ServerReflexive, Host), "stun");
        assert_eq!(connection_kind(Host, Relayed), "turn");
        assert!(scope_applies(Some(ApplyScope::Turn), "turn"));
        assert!(!scope_applies(Some(ApplyScope::Turn), "p2p"));
        assert!(scope_applies(None, "p2p"));
    }

    #[test]
    fn describes_candidates() {
        let srflx =
            "candidate:2 1 udp 1686052607 203.0.113.7 61000 typ srflx raddr 10.0.0.2 rport 51000";
        assert_eq!(describe_candidate(srflx), "srflx 203.0.113.7");
        assert_eq!(
            describe_candidate("a=candidate:1 1 udp 1 abc.local 9 typ host"),
            "host abc.local"
        );
        assert_eq!(describe_candidate("garbage"), "unparsed");
    }

    #[test]
    fn reads_viewer_candidates() {
        let init = candidate_init(
            "a=candidate:1 1 udp 2122260223 10.0.0.2 51000 typ host",
            "0",
        );
        assert_eq!(
            init.candidate,
            "candidate:1 1 udp 2122260223 10.0.0.2 51000 typ host"
        );
        assert_eq!(init.sdp_mid.as_deref(), Some("0"));
        assert_eq!(candidate_init("candidate:x", "").sdp_mline_index, Some(0));
    }
}
