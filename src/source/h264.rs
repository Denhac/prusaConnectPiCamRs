//! Splits the H.264 byte stream (Annex B) that `rpicam-vid --codec h264 -o -` writes into
//! access units, one per frame, ready to send over WebRTC.

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NAL_SEI: u8 = 6;
const NAL_SPS: u8 = 7;
const NAL_AUD: u8 = 9;

/// A buffered access unit larger than this is treated as corrupt and dropped.
const MAX_ACCESS_UNIT: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub struct AccessUnits {
    buf: Vec<u8>,
    current: Vec<u8>,
    has_picture: bool,
}

impl AccessUnits {
    /// Adds bytes from the stream and returns the access units they complete. A unit is only
    /// known to be complete once the next one starts, so each frame waits for the header of the
    /// next frame's first NAL unit.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut units = Vec::new();
        let Some(first) = find_start_code(&self.buf, 0) else {
            // Keep a possible partial start code; drop anything before it.
            let keep = self.buf.len().min(2);
            self.buf.drain(..self.buf.len() - keep);
            return units;
        };
        let mut start = first;
        while let Some(next) = find_start_code(&self.buf, start + 3) {
            let nal = trim_trailing_zeros(&self.buf[start + 3..next]).to_vec();
            if let Some(unit) = self.add_nal(nal) {
                units.push(unit);
            }
            start = next;
        }
        // The incomplete NAL unit's header already shows whether the current frame is done.
        if let Some(unit) = self.finish_if(starts_access_unit(&self.buf[start + 3..])) {
            units.push(unit);
        }
        self.buf.drain(..start);
        units
    }

    fn add_nal(&mut self, mut nal: Vec<u8>) -> Option<Vec<u8>> {
        let nal_type = *nal.first()? & 0x1f;
        let finished = self.finish_if(starts_access_unit(&nal));
        if nal_type == NAL_SPS {
            patch_sps(&mut nal);
        }
        if self.current.len() + nal.len() > MAX_ACCESS_UNIT {
            self.current.clear();
            self.has_picture = false;
        }
        self.current.extend_from_slice(&START_CODE);
        self.current.extend_from_slice(&nal);
        self.has_picture |= is_slice(nal_type);
        finished
    }

    /// Takes the current access unit if `next_starts_unit` and it holds a picture.
    fn finish_if(&mut self, next_starts_unit: bool) -> Option<Vec<u8>> {
        if !(self.has_picture && next_starts_unit) {
            return None;
        }
        self.has_picture = false;
        Some(std::mem::take(&mut self.current))
    }
}

fn is_slice(nal_type: u8) -> bool {
    matches!(nal_type, 1 | 5)
}

/// Whether `nal` begins a new access unit: parameter sets, SEI and delimiters precede a frame's
/// first slice, and a slice whose `first_mb_in_slice` is 0 starts a new picture.
fn starts_access_unit(nal: &[u8]) -> bool {
    let Some(header) = nal.first() else {
        return false;
    };
    let nal_type = header & 0x1f;
    if (NAL_SEI..=NAL_AUD).contains(&nal_type) {
        return true;
    }
    // first_mb_in_slice is ue(v): a leading 1 bit encodes 0.
    is_slice(nal_type) && nal.get(1).is_some_and(|b| b & 0x80 != 0)
}

/// Rewrites the SPS to constrained baseline level 3.1 (`42e01f`), the profile offered in SDP.
/// Prusa's answerer rejects other profiles, and rpicam-vid's baseline stream decodes the same.
fn patch_sps(nal: &mut [u8]) {
    if let Some(header) = nal.get_mut(1..4) {
        header.copy_from_slice(&[0x42, 0xe0, 0x1f]);
    }
}

fn find_start_code(buf: &[u8], from: usize) -> Option<usize> {
    buf.get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|i| i + from)
}

/// Zeros before a start code belong to it (`00 00 00 01`), not to the preceding NAL unit.
fn trim_trailing_zeros(nal: &[u8]) -> &[u8] {
    let end = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &nal[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(bytes: &[u8]) -> Vec<u8> {
        [&START_CODE[..], bytes].concat()
    }

    /// Two frames: an IDR with parameter sets, then a P frame.
    fn stream() -> Vec<u8> {
        [
            nal(&[0x27, 0x42, 0x80, 0x29, 0xaa]), // SPS, baseline level 4.1
            nal(&[0x28, 0xce, 0x3c, 0x80]),       // PPS
            nal(&[0x25, 0x88, 0x84, 0x00, 0x11]), // IDR slice, first_mb 0
            nal(&[0x21, 0x9a, 0x02, 0x03]),       // P slice, first_mb 0
            nal(&[0x27, 0x42, 0x80, 0x29, 0xbb]), // next SPS starts the third frame
        ]
        .concat()
    }

    #[test]
    fn groups_nal_units_into_frames_and_patches_sps() {
        for chunk in [1, 2, 5, 1000] {
            let mut splitter = AccessUnits::default();
            let units: Vec<_> = stream()
                .chunks(chunk)
                .flat_map(|c| splitter.push(c))
                .collect();
            assert_eq!(units.len(), 2, "chunk size {chunk}");
            assert_eq!(
                units[0],
                [
                    nal(&[0x27, 0x42, 0xe0, 0x1f, 0xaa]),
                    nal(&[0x28, 0xce, 0x3c, 0x80]),
                    nal(&[0x25, 0x88, 0x84, 0x00, 0x11]),
                ]
                .concat()
            );
            assert_eq!(units[1], nal(&[0x21, 0x9a, 0x02, 0x03]));
        }
    }

    #[test]
    fn keeps_slices_of_one_picture_together() {
        let mut splitter = AccessUnits::default();
        let data = [
            nal(&[0x21, 0x9a, 0x01]), // first slice of a picture
            nal(&[0x21, 0x40, 0x02]), // second slice: first_mb != 0
            nal(&[0x21, 0x9a, 0x03]), // next picture
        ]
        .concat();
        let units = splitter.push(&data);
        assert_eq!(
            units,
            vec![[nal(&[0x21, 0x9a, 0x01]), nal(&[0x21, 0x40, 0x02])].concat()]
        );
    }
}
