//! Protobuf messages of Prusa's camera signaling protocol, schema 4.4. Field numbers follow
//! Prusa's own browser webcam (camera-service-webcam.prusa3d.com); only the fields this camera
//! sends or reads are declared.
//!
//! Most messages carry the camera token, so never log them with `{:?}`.

use prost::{Enumeration, Message};

pub const SCHEMA_VERSION: &str = "4.4";

/// The single argument of a Socket.IO acknowledgement, in both directions.
pub mod ack {
    pub const OK: i64 = 0;
    pub const NOT_AUTHORIZED: i64 = 1;
    pub const DECODE_MESSAGE: i64 = 4;
    pub const INVALID_COMMAND: i64 = 101;
    pub const UNSUPPORTED_FEATURE: i64 = 102;
}

/// `client_trigger` error code for a stream the camera can't start.
pub const ERROR_WEBRTC_START_FAIL: i32 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum Toggle {
    Invalid = 0,
    Enable = 1,
    Disable = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum WebrtcMode {
    Unknown = 0,
    On = 1,
    Off = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum MessageType {
    Unknown = 0,
    Request = 1,
    Answer = 2,
    Offer = 3,
    Candidate = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum StreamStatus {
    Invalid = 0,
    Start = 1,
    Stop = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum ClientType {
    Unknown = 0,
    Camera = 1,
    Client = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum IcePolicy {
    Undefined = 0,
    All = 1,
    Relay = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum Scheme {
    Undefined = 0,
    Stun = 1,
    Turn = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum Transport {
    Undefined = 0,
    Udp = 1,
    Tcp = 2,
    Tls = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum VideoQuality {
    Invalid = 0,
    Sd = 1,
    Hd = 2,
    Fhd = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum ConnectionType {
    Undefined = 0,
    Host = 1,
    ServerReflexive = 2,
    PeerReflexive = 3,
    Relayed = 4,
    Unknown = 5,
    Fail = 6,
    P2p = 7,
}

#[derive(Clone, PartialEq, Message)]
pub struct CameraAuthentication {
    #[prost(string, tag = "1")]
    pub camera_token: String,
    #[prost(string, tag = "2")]
    pub fingerprint: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProtobufVersion {
    #[prost(string, tag = "1")]
    pub protobuf_schema_version: String,
    #[prost(string, tag = "2")]
    pub camera_token: String,
    #[prost(string, optional, tag = "3")]
    pub request_id: Option<String>,
    #[prost(string, tag = "4")]
    pub fingerprint: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Features {
    #[prost(string, tag = "2")]
    pub camera_token: String,
    #[prost(string, tag = "3")]
    pub fw_version: String,
    #[prost(string, tag = "4")]
    pub hw_version: String,
    #[prost(string, tag = "5")]
    pub protobuf_schema_version: String,
    /// A JSON array of feature names.
    #[prost(string, tag = "6")]
    pub features: String,
    #[prost(string, tag = "7")]
    pub fingerprint: String,
}

/// The server asking for information.
#[derive(Clone, PartialEq, Message)]
pub struct Trigger {
    #[prost(enumeration = "Toggle", tag = "1")]
    pub get_status: i32,
    #[prost(enumeration = "Toggle", tag = "2")]
    pub get_features: i32,
    #[prost(enumeration = "Toggle", tag = "12")]
    pub get_protocol_information: i32,
    #[prost(string, optional, tag = "13")]
    pub request_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Configuration {
    #[prost(message, optional, tag = "3")]
    pub control: Option<Control>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Control {
    #[prost(enumeration = "WebrtcMode", tag = "12")]
    pub webrtc_mode: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Status {
    #[prost(message, optional, tag = "1")]
    pub image: Option<Image>,
    #[prost(message, optional, tag = "5")]
    pub system: Option<System>,
    #[prost(string, tag = "8")]
    pub camera_token: String,
    #[prost(string, optional, tag = "10")]
    pub request_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Image {
    #[prost(message, optional, tag = "1")]
    pub current_resolution: Option<Resolution>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Resolution {
    #[prost(uint32, tag = "1")]
    pub width: u32,
    #[prost(uint32, tag = "2")]
    pub height: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct System {
    #[prost(string, tag = "1")]
    pub fw_version: String,
    #[prost(string, tag = "2")]
    pub hw_version: String,
    #[prost(string, tag = "3")]
    pub camera_name: String,
    #[prost(message, optional, tag = "11")]
    pub webrtc: Option<WebrtcState>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WebrtcState {
    #[prost(enumeration = "WebrtcMode", tag = "1")]
    pub mode: i32,
}

/// Every step of a stream: the viewer's request, the camera's offer, the viewer's answer and
/// both sides' ICE candidates.
#[derive(Clone, PartialEq, Message)]
pub struct Webrtc {
    #[prost(string, tag = "1")]
    pub camera_token: String,
    #[prost(string, tag = "2")]
    pub request_id: String,
    #[prost(string, tag = "3")]
    pub fingerprint: String,
    #[prost(message, optional, tag = "4")]
    pub sdp: Option<Sdp>,
    #[prost(enumeration = "MessageType", tag = "5")]
    pub message_type: i32,
    #[prost(enumeration = "StreamStatus", tag = "6")]
    pub webrtc_stream_status: i32,
    #[prost(enumeration = "ClientType", tag = "7")]
    pub client_type: i32,
    #[prost(message, optional, tag = "8")]
    pub ice_configuration: Option<IceConfiguration>,
    #[prost(message, optional, tag = "9")]
    pub video_settings: Option<VideoSettings>,
}

/// An SDP offer or answer in `candidate`, or an ICE candidate with its media id.
#[derive(Clone, PartialEq, Message)]
pub struct Sdp {
    #[prost(string, tag = "1")]
    pub candidate: String,
    #[prost(string, tag = "2")]
    pub mid: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct IceConfiguration {
    #[prost(message, repeated, tag = "1")]
    pub ice_servers: Vec<IceServer>,
    #[prost(enumeration = "IcePolicy", tag = "2")]
    pub ice_transport_policy: i32,
    #[prost(uint32, tag = "3")]
    pub ttl: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct IceServer {
    #[prost(message, repeated, tag = "1")]
    pub endpoints: Vec<Endpoint>,
    #[prost(string, tag = "2")]
    pub username: String,
    #[prost(string, tag = "3")]
    pub credential: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Endpoint {
    #[prost(enumeration = "Scheme", tag = "1")]
    pub scheme_type: i32,
    #[prost(string, tag = "2")]
    pub address: String,
    #[prost(uint32, tag = "3")]
    pub port: u32,
    #[prost(enumeration = "Transport", tag = "4")]
    pub transport_protocol: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct VideoSettings {
    #[prost(enumeration = "VideoQuality", tag = "1")]
    pub video_quality: i32,
    #[prost(uint32, tag = "2")]
    pub video_fps: u32,
    #[prost(uint32, tag = "3")]
    pub plan_priority: u32,
    /// Seconds a stream may last, on the connection types `apply_scope` names.
    #[prost(uint32, tag = "4")]
    pub ttl: u32,
    #[prost(enumeration = "ApplyScope", tag = "5")]
    pub apply_scope: i32,
}

/// Which connection types a stream's video settings and time limit apply to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Enumeration)]
#[repr(i32)]
pub enum ApplyScope {
    Unspecified = 0,
    Turn = 1,
    Stun = 2,
    P2p = 3,
    TurnAndStun = 4,
    Any = 5,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectionInfo {
    #[prost(string, tag = "1")]
    pub camera_token: String,
    #[prost(string, tag = "2")]
    pub request_id: String,
    #[prost(string, tag = "3")]
    pub fingerprint: String,
    #[prost(enumeration = "ConnectionType", tag = "4")]
    pub remote: i32,
    #[prost(enumeration = "ConnectionType", tag = "5")]
    pub local: i32,
    #[prost(enumeration = "ClientType", tag = "6")]
    pub client_type: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientTrigger {
    #[prost(string, tag = "1")]
    pub camera_token: String,
    #[prost(string, tag = "2")]
    pub request_id: String,
    #[prost(string, tag = "4")]
    pub fingerprint: String,
    #[prost(int32, tag = "6")]
    pub error_code: i32,
}

/// The ICE server URLs a browser would use for `server`, as Prusa's webcam builds them.
pub fn ice_urls(server: &IceServer) -> Vec<String> {
    server
        .endpoints
        .iter()
        .filter(|e| !e.address.is_empty() && e.port != 0)
        .filter_map(|e| {
            let tls = e.transport_protocol() == Transport::Tls;
            let url = match (e.scheme_type(), tls) {
                (Scheme::Stun, false) => return Some(format!("stun:{}:{}", e.address, e.port)),
                (Scheme::Stun, true) => return Some(format!("stuns:{}:{}", e.address, e.port)),
                (Scheme::Turn, false) => format!("turn:{}:{}", e.address, e.port),
                (Scheme::Turn, true) => format!("turns:{}:{}", e.address, e.port),
                (Scheme::Undefined, _) => return None,
            };
            let tcp = tls || e.transport_protocol() == Transport::Tcp;
            Some(format!(
                "{url}?transport={}",
                if tcp { "tcp" } else { "udp" }
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_prusas_client() {
        // camera_authentication: field 1 token, field 2 fingerprint, both strings.
        let bytes = CameraAuthentication {
            camera_token: "tok".into(),
            fingerprint: "fp".into(),
        }
        .encode_to_vec();
        assert_eq!(bytes, [0x0a, 3, b't', b'o', b'k', 0x12, 2, b'f', b'p']);
    }

    #[test]
    fn decodes_a_stream_request() {
        let request = Webrtc {
            request_id: "r1".into(),
            message_type: MessageType::Request as i32,
            ice_configuration: Some(IceConfiguration {
                ice_servers: vec![IceServer {
                    endpoints: vec![
                        Endpoint {
                            scheme_type: Scheme::Turn as i32,
                            address: "coturn.prusa3d.com".into(),
                            port: 3478,
                            transport_protocol: Transport::Udp as i32,
                        },
                        Endpoint {
                            scheme_type: Scheme::Turn as i32,
                            address: "coturn.prusa3d.com".into(),
                            port: 443,
                            transport_protocol: Transport::Tls as i32,
                        },
                    ],
                    username: "user".into(),
                    credential: "secret".into(),
                }],
                ice_transport_policy: IcePolicy::All as i32,
                ttl: 600,
            }),
            ..Default::default()
        }
        .encode_to_vec();
        let decoded = Webrtc::decode(bytes::Bytes::from(request)).unwrap();
        assert_eq!(decoded.message_type(), MessageType::Request);
        let ice = decoded.ice_configuration.unwrap();
        assert_eq!(
            ice_urls(&ice.ice_servers[0]),
            [
                "turn:coturn.prusa3d.com:3478?transport=udp",
                "turns:coturn.prusa3d.com:443?transport=tcp"
            ]
        );
    }
}
