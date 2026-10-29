//! Splits the back-to-back JPEG images that `rpicam-vid --codec mjpeg -o -` writes into frames.
//!
//! Each image is walked segment by segment instead of searching for the first FF D9, so bytes
//! inside headers can't be mistaken for the end-of-image marker.

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;

/// An image still incomplete at this size is treated as corrupt and skipped.
const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub struct Splitter {
    buf: Vec<u8>,
    /// The buffer starts with an image that was incomplete when last scanned.
    waiting: bool,
}

enum Scan {
    Complete(usize),
    Incomplete,
    Corrupt,
}

impl Splitter {
    /// Adds bytes from the stream and returns the frames they complete.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        // Back up one byte in case an end marker straddles two reads.
        let new_data = self.buf.len().saturating_sub(1);
        self.buf.extend_from_slice(data);
        // Only an end-of-image marker can complete a waiting image, so don't rescan it until
        // one arrives. Rescanning on every read made splitting quadratic in the frame size.
        if self.waiting
            && self.buf.len() <= MAX_FRAME
            && !self.buf[new_data..].windows(2).any(|w| w == [0xFF, EOI])
        {
            return Vec::new();
        }
        self.waiting = false;
        let mut frames = Vec::new();
        loop {
            match self.buf.windows(2).position(|w| w == [0xFF, SOI]) {
                Some(start) => drop(self.buf.drain(..start)),
                None => {
                    // Keep a trailing 0xFF: it may be the first half of the next start marker.
                    let keep = usize::from(self.buf.last() == Some(&0xFF));
                    self.buf.drain(..self.buf.len() - keep);
                    break;
                }
            }
            match image_len(&self.buf) {
                Scan::Complete(len) => frames.push(self.buf.drain(..len).collect()),
                Scan::Incomplete if self.buf.len() <= MAX_FRAME => {
                    self.waiting = true;
                    break;
                }
                // Skip this start marker and look for the next one.
                Scan::Incomplete | Scan::Corrupt => drop(self.buf.drain(..2)),
            }
        }
        frames
    }
}

/// Finds the length of the JPEG at the start of `buf`, which begins with a start-of-image marker.
fn image_len(buf: &[u8]) -> Scan {
    let mut i = 2;
    loop {
        let (Some(&byte), Some(&marker)) = (buf.get(i), buf.get(i + 1)) else {
            return Scan::Incomplete;
        };
        if byte != 0xFF {
            return Scan::Corrupt;
        }
        match marker {
            // Fill byte before a marker.
            0xFF => i += 1,
            EOI => return Scan::Complete(i + 2),
            // A new image started before this one ended.
            SOI => return Scan::Corrupt,
            // Markers without a length field.
            0x01 | 0xD0..=0xD7 => i += 2,
            _ => {
                let (Some(&hi), Some(&lo)) = (buf.get(i + 2), buf.get(i + 3)) else {
                    return Scan::Incomplete;
                };
                let segment_len = usize::from(u16::from_be_bytes([hi, lo]));
                if segment_len < 2 {
                    return Scan::Corrupt;
                }
                i += 2 + segment_len;
                if marker == SOS {
                    match skip_entropy_coded_data(buf, i) {
                        Some(next_marker) => i = next_marker,
                        None => return Scan::Incomplete,
                    }
                }
            }
        }
    }
}

/// Returns the position of the marker that ends the compressed data starting at `i`.
/// Inside that data, 0xFF is always followed by 0x00 (a stuffed byte) or a restart marker.
fn skip_entropy_coded_data(buf: &[u8], mut i: usize) -> Option<usize> {
    while i + 1 < buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        match buf[i + 1] {
            0x00 | 0xD0..=0xD7 => i += 2,
            0xFF => i += 1,
            _ => return Some(i),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: &[u8] = include_bytes!("../../testdata/frame.jpg");

    fn split_in_chunks(stream: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let mut splitter = Splitter::default();
        stream
            .chunks(chunk)
            .flat_map(|c| splitter.push(c))
            .collect()
    }

    #[test]
    fn splits_back_to_back_frames_at_any_chunk_size() {
        let stream = [FRAME, FRAME, FRAME].concat();
        for chunk in [1, 2, 3, 7, 100, 4096, stream.len()] {
            let frames = split_in_chunks(&stream, chunk);
            assert_eq!(frames.len(), 3, "chunk size {chunk}");
            assert!(frames.iter().all(|f| f == FRAME), "chunk size {chunk}");
        }
    }

    #[test]
    fn skips_garbage_and_truncated_frames() {
        let truncated = &FRAME[..FRAME.len() / 2];
        let stream = [
            b"noise\xFF\x00".as_slice(),
            truncated,
            FRAME,
            b"\xFF\xD9junk",
            FRAME,
        ]
        .concat();
        let frames = split_in_chunks(&stream, 5);
        assert_eq!(frames, vec![FRAME.to_vec(), FRAME.to_vec()]);
    }

    #[test]
    fn waits_for_the_rest_of_a_frame() {
        let mut splitter = Splitter::default();
        assert!(splitter.push(&FRAME[..FRAME.len() - 1]).is_empty());
        assert_eq!(
            splitter.push(&FRAME[FRAME.len() - 1..]),
            vec![FRAME.to_vec()]
        );
    }
}
