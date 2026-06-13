// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Packet framing: every MySQL packet is a 3-byte little-endian payload length,
//! a 1-byte sequence id, then the payload. Payloads of 0xffffff bytes or more
//! are split across several frames; the receiver reassembles them.
//!
//! This module is sans-IO. [`encode_frame`] appends framed bytes to a buffer,
//! and [`FrameDecoder`] reassembles payloads from bytes fed to it by a
//! transport (a tokio socket in the server, an in-memory buffer in tests).

use crate::error::Result;

/// Length of a packet header: 3-byte length + 1-byte sequence id.
pub const HEADER_LEN: usize = 4;

/// Maximum payload carried by a single frame. A payload of exactly this size is
/// always followed by another frame (possibly empty) so the receiver knows the
/// logical packet continued.
pub const MAX_PAYLOAD: usize = 0xff_ffff;

/// Appends `payload` to `out` as one or more framed packets, starting at
/// sequence id `seq`, and returns the next sequence id to use.
pub fn encode_frame(out: &mut Vec<u8>, seq: u8, payload: &[u8]) -> u8 {
    let mut seq = seq;
    let mut chunks = payload.chunks(MAX_PAYLOAD);
    // `chunks` yields nothing for an empty payload, but we must still emit a
    // zero-length frame, so handle that explicitly.
    if payload.is_empty() {
        write_header(out, 0, seq);
        return seq.wrapping_add(1);
    }
    let mut last_was_full = false;
    for chunk in chunks.by_ref() {
        write_header(out, chunk.len(), seq);
        out.extend_from_slice(chunk);
        seq = seq.wrapping_add(1);
        last_was_full = chunk.len() == MAX_PAYLOAD;
    }
    // A trailing full-sized chunk needs an empty terminator frame.
    if last_was_full {
        write_header(out, 0, seq);
        seq = seq.wrapping_add(1);
    }
    seq
}

fn write_header(out: &mut Vec<u8>, len: usize, seq: u8) {
    let len = len as u32;
    out.extend_from_slice(&len.to_le_bytes()[..3]);
    out.push(seq);
}

/// Parses a 4-byte header into `(payload_len, sequence_id)`.
pub fn parse_header(header: &[u8; HEADER_LEN]) -> (usize, u8) {
    let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    (len, header[3])
}

/// A complete, reassembled logical packet.
pub struct Packet {
    /// Sequence id of the first frame of the packet.
    pub seq: u8,
    /// The reassembled payload.
    pub payload: Vec<u8>,
}

/// Reassembles logical packets from a byte stream fed in arbitrary chunks.
///
/// Feed bytes with [`FrameDecoder::extend`], then call [`FrameDecoder::next`]
/// in a loop until it returns `None` to drain all packets currently available.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    cursor: usize,
}

impl FrameDecoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends freshly received bytes to the internal buffer.
    pub fn extend(&mut self, bytes: &[u8]) {
        // Compact occasionally so the buffer does not grow without bound.
        if self.cursor > 0 && self.cursor == self.buf.len() {
            self.buf.clear();
            self.cursor = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Attempts to pull the next complete logical packet out of the buffer.
    ///
    /// Returns `Ok(None)` if more bytes are needed. Reassembles multi-frame
    /// payloads (those split at the 16 MiB boundary) transparently.
    pub fn next_packet(&mut self) -> Result<Option<Packet>> {
        let mut scan = self.cursor;
        let mut payload = Vec::new();
        let mut first_seq = None;
        loop {
            if self.buf.len() - scan < HEADER_LEN {
                return Ok(None);
            }
            let mut header = [0u8; HEADER_LEN];
            header.copy_from_slice(&self.buf[scan..scan + HEADER_LEN]);
            let (len, seq) = parse_header(&header);
            if self.buf.len() - scan - HEADER_LEN < len {
                return Ok(None);
            }
            let start = scan + HEADER_LEN;
            payload.extend_from_slice(&self.buf[start..start + len]);
            if first_seq.is_none() {
                first_seq = Some(seq);
            }
            scan = start + len;
            if len < MAX_PAYLOAD {
                // Final frame of this logical packet.
                self.cursor = scan;
                return Ok(Some(Packet {
                    seq: first_seq.expect("first_seq set after first frame"),
                    payload,
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_packet_roundtrips() {
        let mut out = Vec::new();
        let next = encode_frame(&mut out, 0, b"hello");
        assert_eq!(next, 1);

        let mut dec = FrameDecoder::new();
        dec.extend(&out);
        let pkt = dec.next_packet().unwrap().unwrap();
        assert_eq!(pkt.seq, 0);
        assert_eq!(pkt.payload, b"hello");
        assert!(dec.next_packet().unwrap().is_none());
    }

    #[test]
    fn split_feed_reassembles() {
        let mut out = Vec::new();
        encode_frame(&mut out, 7, b"abcdef");

        let mut dec = FrameDecoder::new();
        // Feed one byte at a time; the packet only becomes available at the end.
        for chunk in out.chunks(1) {
            assert!(dec.next_packet().unwrap().is_none());
            dec.extend(chunk);
        }
        let pkt = dec.next_packet().unwrap().unwrap();
        assert_eq!(pkt.seq, 7);
        assert_eq!(pkt.payload, b"abcdef");
    }

    #[test]
    fn multi_frame_payload_reassembles() {
        let payload = vec![0xabu8; MAX_PAYLOAD + 10];
        let mut out = Vec::new();
        encode_frame(&mut out, 0, &payload);

        let mut dec = FrameDecoder::new();
        dec.extend(&out);
        let pkt = dec.next_packet().unwrap().unwrap();
        assert_eq!(pkt.payload.len(), payload.len());
        assert_eq!(pkt.payload, payload);
    }
}
