//! A minimal Socket.IO client (protocol 5 over Engine.IO 4, WebSocket transport only), enough
//! for Prusa's camera signaling: every event carries one binary attachment, and both sides
//! acknowledge events with a number.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub enum Packet {
    /// An event from the server; reply with [`Writer::ack`] when `ack_id` is set.
    Event {
        name: String,
        data: Vec<u8>,
        ack_id: Option<u64>,
    },
    /// The server's acknowledgement of an event this client emitted.
    Ack { id: u64, code: i64 },
    /// The server's heartbeat; reply with [`Writer::pong`].
    Ping,
}

pub struct Reader {
    stream: SplitStream<Socket>,
    /// Silence longer than this means the connection is dead (ping interval + ping timeout).
    timeout: Duration,
    last_seen: Instant,
    parser: Parser,
}

/// Turns Engine.IO text packets and binary attachments into [`Packet`]s.
#[derive(Default)]
struct Parser {
    /// A binary event's header, waiting for its attachment.
    pending: Option<(String, Option<u64>)>,
}

pub struct Writer {
    sink: SplitSink<Socket, Message>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Handshake {
    ping_interval: u64,
    ping_timeout: u64,
}

/// Connects to the default namespace of the Socket.IO server at `base_url` (https://…).
pub async fn connect(base_url: &str, origin: &str, user_agent: &str) -> Result<(Reader, Writer)> {
    let host = base_url.trim_end_matches('/');
    let url = match host.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        _ => bail!("signaling URL {base_url:?} must start with https:// or http://"),
    };
    let mut request =
        format!("{url}/socket.io/?EIO=4&transport=websocket").into_client_request()?;
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_str(origin)?);
    request
        .headers_mut()
        .insert("User-Agent", HeaderValue::from_str(user_agent)?);
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("connecting to {host}"))?;
    let (sink, stream) = socket.split();
    let mut reader = Reader {
        stream,
        timeout: Duration::from_secs(60),
        last_seen: Instant::now(),
        parser: Parser::default(),
    };
    let mut writer = Writer { sink };

    let open = reader.next_text().await?;
    let handshake: Handshake = serde_json::from_str(
        open.strip_prefix('0')
            .context("expected an Engine.IO open packet")?,
    )?;
    reader.timeout = Duration::from_millis(handshake.ping_interval + handshake.ping_timeout);

    writer.send_text("40".into()).await?;
    let reply = reader.next_text().await?;
    if let Some(error) = reply.strip_prefix("44") {
        bail!("the signaling server refused the connection: {error}");
    }
    ensure!(
        reply.starts_with("40"),
        "unexpected Socket.IO connect reply {reply:?}"
    );
    Ok((reader, writer))
}

impl Reader {
    /// The next packet from the server. Cancel-safe: it can be dropped and called again.
    pub async fn next(&mut self) -> Result<Packet> {
        loop {
            let message = self.next_message().await?;
            match message {
                Message::Text(text) => {
                    if let Some(packet) = self.parser.text(&text)? {
                        return Ok(packet);
                    }
                }
                Message::Binary(data) => return self.parser.attachment(data.to_vec()),
                Message::Close(frame) => {
                    bail!("the signaling server closed the connection: {frame:?}")
                }
                _ => {}
            }
        }
    }

    async fn next_text(&mut self) -> Result<String> {
        match self.next_message().await? {
            Message::Text(text) => Ok(text.to_string()),
            other => bail!("expected a text frame, got {other:?}"),
        }
    }

    async fn next_message(&mut self) -> Result<Message> {
        let remaining = self.timeout.saturating_sub(self.last_seen.elapsed());
        let message = tokio::time::timeout(remaining, self.stream.next())
            .await
            .context("the signaling server stopped responding")?
            .context("the signaling connection closed")??;
        self.last_seen = Instant::now();
        Ok(message)
    }
}

impl Parser {
    /// Parses an Engine.IO text packet. Returns `None` for packets with nothing to report yet,
    /// such as a binary event whose attachment follows.
    fn text(&mut self, text: &str) -> Result<Option<Packet>> {
        match text.as_bytes().first() {
            Some(b'2') => Ok(Some(Packet::Ping)),
            Some(b'1') => bail!("the signaling server closed the session"),
            Some(b'4') => self.parse_socketio(&text[1..]),
            _ => Ok(None),
        }
    }

    fn parse_socketio(&mut self, packet: &str) -> Result<Option<Packet>> {
        let kind = packet
            .as_bytes()
            .first()
            .copied()
            .context("empty Socket.IO packet")?;
        let mut rest = &packet[1..];
        let mut attachments = 0;
        if matches!(kind, b'5' | b'6') {
            let (count, after) = rest.split_once('-').context("malformed binary packet")?;
            attachments = count.parse::<usize>()?;
            rest = after;
        }
        if rest.starts_with('/') {
            rest = rest.split_once(',').map_or("", |(_, after)| after);
        }
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let id = (digits > 0)
            .then(|| rest[..digits].parse::<u64>())
            .transpose()?;
        let args: Vec<serde_json::Value> = serde_json::from_str(&rest[digits..])?;
        match kind {
            b'2' | b'5' => {
                let name = args
                    .first()
                    .and_then(|v| v.as_str())
                    .context("event without a name")?;
                ensure!(
                    attachments <= 1,
                    "events with {attachments} attachments aren't supported"
                );
                if attachments == 1 {
                    self.pending = Some((name.to_owned(), id));
                    return Ok(None);
                }
                Ok(Some(Packet::Event {
                    name: name.to_owned(),
                    data: Vec::new(),
                    ack_id: id,
                }))
            }
            b'3' | b'6' => {
                let id = id.context("acknowledgement without an id")?;
                let code = args.first().and_then(|v| v.as_i64()).unwrap_or(-1);
                Ok(Some(Packet::Ack { id, code }))
            }
            b'4' => bail!("Socket.IO connect error: {args:?}"),
            b'1' => bail!("the signaling server disconnected this client"),
            _ => Ok(None),
        }
    }

    fn attachment(&mut self, data: Vec<u8>) -> Result<Packet> {
        let (name, ack_id) = self
            .pending
            .take()
            .context("unexpected binary attachment")?;
        Ok(Packet::Event { name, data, ack_id })
    }
}

impl Writer {
    /// Emits `name` with `data` as its binary attachment, asking for acknowledgement `ack_id`.
    pub async fn emit(&mut self, name: &str, data: Vec<u8>, ack_id: u64) -> Result<()> {
        let header = format!(
            "451-{ack_id}[{},{{\"_placeholder\":true,\"num\":0}}]",
            serde_json::to_string(name)?
        );
        self.send_text(header.into()).await?;
        self.sink.send(Message::Binary(data.into())).await?;
        Ok(())
    }

    /// Acknowledges the server's event `id` with `code`.
    pub async fn ack(&mut self, id: u64, code: i64) -> Result<()> {
        self.send_text(format!("43{id}[{code}]").into()).await
    }

    pub async fn pong(&mut self) -> Result<()> {
        self.send_text("3".into()).await
    }

    async fn send_text(&mut self, text: Utf8Bytes) -> Result<()> {
        self.sink.send(Message::Text(text)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe(result: Result<Option<Packet>>) -> String {
        match result {
            Ok(Some(Packet::Ack { id, code })) => format!("ack {id} {code}"),
            Ok(Some(Packet::Ping)) => "ping".into(),
            Ok(Some(Packet::Event { name, data, ack_id })) => {
                format!("event {name} {ack_id:?} {} bytes", data.len())
            }
            Ok(None) => "pending".into(),
            Err(e) => format!("error {e}"),
        }
    }

    #[test]
    fn parses_signaling_packets() {
        let mut parser = Parser::default();
        let mut text = |t: &str| describe(parser.text(t));
        assert_eq!(text("2"), "ping");
        assert_eq!(text("430[0]"), "ack 0 0");
        assert_eq!(text("4317[102]"), "ack 17 102");
        assert_eq!(
            text(r#"42["camera_ready"]"#),
            "event camera_ready None 0 bytes"
        );
        assert!(text(r#"44{"message":"nope"}"#).starts_with("error"));
    }

    #[test]
    fn joins_binary_events_with_their_attachment() {
        let mut parser = Parser::default();
        let header = parser.text(r#"451-5["trigger",{"_placeholder":true,"num":0}]"#);
        assert_eq!(describe(header), "pending");
        assert_eq!(
            describe(parser.attachment(vec![1, 2, 3]).map(Some)),
            "event trigger Some(5) 3 bytes"
        );
        assert!(parser.attachment(vec![1]).is_err());
    }
}
