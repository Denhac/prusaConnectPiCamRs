//! H.264 over RTP (RFC 6184, packetization mode 1), framed for RTSP's interleaved TCP transport,
//! and the SDP that describes it.

use std::net::IpAddr;
use std::time::Instant;

use crate::source::h264::nals;

pub const PAYLOAD_TYPE: u8 = 96;
const CLOCK_RATE: f64 = 90_000.0;
/// The largest RTP packet, small enough for a relay to pass it on to WebRTC unchanged.
const MAX_PACKET: usize = 1400;
const HEADER: usize = 12;
const FU_A: u8 = 28;

/// The session description DESCRIBE answers with. It names no parameter sets: they come inline
/// before every keyframe, and would go stale when the encoder restarts with other settings.
pub fn sdp(local: IpAddr, session: u64) -> String {
    let family = if local.is_ipv4() { "IP4" } else { "IP6" };
    format!(
        "v=0\r\n\
         o=- {session} 1 IN {family} {local}\r\n\
         s=prusa-connect-camera\r\n\
         c=IN {family} {local}\r\n\
         t=0 0\r\n\
         a=control:*\r\n\
         m=video 0 RTP/AVP {PAYLOAD_TYPE}\r\n\
         a=rtpmap:{PAYLOAD_TYPE} H264/90000\r\n\
         a=fmtp:{PAYLOAD_TYPE} packetization-mode=1\r\n\
         a=control:trackID=0\r\n"
    )
}

/// One session's RTP stream: its SSRC, and sequence numbers and timestamps that carry on across
/// encoder restarts.
pub struct Packetizer {
    channel: u8,
    ssrc: u32,
    sequence: u16,
    base: u32,
    first: Option<Instant>,
}

impl Packetizer {
    /// Starts at random values, as RFC 3550 asks.
    pub fn new(channel: u8) -> Self {
        let random = uuid::Uuid::new_v4().as_u128();
        Self {
            channel,
            ssrc: random as u32,
            sequence: (random >> 32) as u16,
            base: (random >> 48) as u32,
            first: None,
        }
    }

    pub fn sequence(&self) -> u16 {
        self.sequence
    }

    /// The RTP timestamp of the first frame.
    pub fn base(&self) -> u32 {
        self.base
    }

    /// Appends one access unit's RTP packets to `out`, each framed for interleaved TCP
    /// (`$`, channel, length). The last packet carries the marker bit.
    pub fn packetize(&mut self, unit: &[u8], captured: Instant, out: &mut Vec<u8>) {
        let first = *self.first.get_or_insert(captured);
        let elapsed = captured.saturating_duration_since(first).as_secs_f64();
        let timestamp = self
            .base
            .wrapping_add((elapsed * CLOCK_RATE).round() as u64 as u32);
        let nals = nals(unit);
        let last = nals.len().saturating_sub(1);
        for (i, nal) in nals.iter().enumerate() {
            let marker = i == last;
            if nal.len() <= MAX_PACKET - HEADER {
                self.packet(out, timestamp, marker, &[], nal);
                continue;
            }
            // FU-A: the NAL header's type moves to the FU header, with start and end bits.
            let indicator = (nal[0] & 0xe0) | FU_A;
            let kind = nal[0] & 0x1f;
            let chunks: Vec<&[u8]> = nal[1..].chunks(MAX_PACKET - HEADER - 2).collect();
            for (j, chunk) in chunks.iter().enumerate() {
                let end = j == chunks.len() - 1;
                let fu_header = kind | if j == 0 { 0x80 } else { 0 } | if end { 0x40 } else { 0 };
                self.packet(
                    out,
                    timestamp,
                    marker && end,
                    &[indicator, fu_header],
                    chunk,
                );
            }
        }
    }

    fn packet(
        &mut self,
        out: &mut Vec<u8>,
        timestamp: u32,
        marker: bool,
        lead: &[u8],
        body: &[u8],
    ) {
        let length = (HEADER + lead.len() + body.len()) as u16;
        out.extend_from_slice(&[b'$', self.channel]);
        out.extend_from_slice(&length.to_be_bytes());
        out.push(0x80); // version 2, no padding, extension or CSRCs
        out.push(PAYLOAD_TYPE | if marker { 0x80 } else { 0 });
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(lead);
        out.extend_from_slice(body);
        self.sequence = self.sequence.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    /// Each packet's sequence number, timestamp and marker bit.
    type Headers = Vec<(u16, u32, bool)>;

    /// Undoes the framing and the FU-A fragmentation, as a receiver does.
    fn depacketize(stream: &[u8]) -> (Vec<Vec<u8>>, Headers) {
        let mut nals = Vec::new();
        let mut headers = Vec::new();
        let mut rest = stream;
        while !rest.is_empty() {
            assert_eq!(rest[0], b'$');
            let length = u16::from_be_bytes([rest[2], rest[3]]) as usize;
            let packet = &rest[4..4 + length];
            rest = &rest[4 + length..];
            assert!(packet.len() <= MAX_PACKET);
            assert_eq!(packet[0], 0x80);
            assert_eq!(packet[1] & 0x7f, PAYLOAD_TYPE);
            let sequence = u16::from_be_bytes([packet[2], packet[3]]);
            let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
            headers.push((sequence, timestamp, packet[1] & 0x80 != 0));
            let payload = &packet[HEADER..];
            if payload[0] & 0x1f == FU_A {
                if payload[1] & 0x80 != 0 {
                    nals.push(vec![(payload[0] & 0xe0) | (payload[1] & 0x1f)]);
                }
                nals.last_mut().unwrap().extend_from_slice(&payload[2..]);
            } else {
                nals.push(payload.to_vec());
            }
        }
        (nals, headers)
    }

    #[test]
    fn packetizes_small_and_large_nal_units() {
        let sps = [0x27, 0x42, 0x80, 0x29];
        let idr: Vec<u8> = [0x25]
            .into_iter()
            .chain((0..5000).map(|i| (i % 251) as u8 + 1))
            .collect();
        let unit = [&[0, 0, 0, 1][..], &sps, &[0, 0, 0, 1], &idr].concat();
        let mut packetizer = Packetizer::new(0);
        let start = Instant::now();
        let mut out = Vec::new();
        packetizer.packetize(&unit, start, &mut out);
        let (nals, headers) = depacketize(&out);
        assert_eq!(nals, [sps.to_vec(), idr]);
        // One packet for the SPS, then the IDR in fragments; only the last has the marker.
        assert!(headers.len() > 4);
        assert!(headers.iter().all(|&(_, ts, _)| ts == packetizer.base()));
        let markers: Vec<bool> = headers.iter().map(|&(_, _, m)| m).collect();
        assert_eq!(markers.iter().filter(|&&m| m).count(), 1);
        assert!(markers[markers.len() - 1]);
        // Sequence numbers count up across packets and frames, and time follows the capture.
        let next = packetizer.sequence();
        let mut out = Vec::new();
        packetizer.packetize(
            &[0, 0, 0, 1, 0x21, 0x9a],
            start + Duration::from_millis(100),
            &mut out,
        );
        let (_, headers) = depacketize(&out);
        assert_eq!(headers[0].0, next);
        assert_eq!(headers[0].1, packetizer.base().wrapping_add(9000));
    }

    #[test]
    fn describes_the_track_the_server_reads() {
        let sdp = sdp("10.0.0.36".parse().unwrap(), 7);
        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"));
        assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"));
        // The server's client reads the track's control URL from the media section.
        assert!(sdp.ends_with("a=control:trackID=0\r\n"));
        assert!(!sdp.contains("sprop"));
    }
}
