// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Cursor-based readers and writers for the primitive encodings used by the
//! MySQL protocol: fixed-width little-endian integers, length-encoded integers,
//! and the various string framings (null-terminated, length-encoded, EOF).
//!
//! Everything here operates on plain byte slices and `Vec<u8>`. There is no I/O:
//! the caller is responsible for framing packets onto a transport.

use crate::error::{ProtocolError, Result};

/// A forward-only cursor over the payload of a single MySQL packet.
pub struct PacketReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> PacketReader<'a> {
    /// Creates a reader over a packet payload (header already stripped).
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Number of bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Returns true if the whole payload has been consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn ensure(&self, needed: usize) -> Result<()> {
        if self.remaining() < needed {
            return Err(ProtocolError::UnexpectedEof {
                offset: self.pos,
                needed: needed - self.remaining(),
            });
        }
        Ok(())
    }

    /// Reads a single byte.
    pub fn u8(&mut self) -> Result<u8> {
        self.ensure(1)?;
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    /// Reads a 2-byte little-endian integer.
    pub fn u16(&mut self) -> Result<u16> {
        self.ensure(2)?;
        let v = u16::from_le_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    /// Reads a 3-byte little-endian integer (used for packet lengths).
    pub fn u24(&mut self) -> Result<u32> {
        self.ensure(3)?;
        let v = u32::from_le_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            0,
        ]);
        self.pos += 3;
        Ok(v)
    }

    /// Reads a 4-byte little-endian integer.
    pub fn u32(&mut self) -> Result<u32> {
        self.ensure(4)?;
        let v = u32::from_le_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    /// Reads a length-encoded integer.
    pub fn lenenc_u64(&mut self) -> Result<u64> {
        let first = self.u8()?;
        match first {
            0xfb => Ok(0), // NULL sentinel; callers that care handle it separately.
            0xfc => Ok(self.u16()? as u64),
            0xfd => Ok(self.u24()? as u64),
            0xfe => {
                self.ensure(8)?;
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&self.buf[self.pos..self.pos + 8]);
                self.pos += 8;
                Ok(u64::from_le_bytes(bytes))
            }
            0xff => Err(ProtocolError::InvalidLengthEncoding(0xff)),
            n => Ok(n as u64),
        }
    }

    /// Borrows `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.ensure(n)?;
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Skips `n` bytes (e.g. reserved/filler fields).
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.ensure(n)?;
        self.pos += n;
        Ok(())
    }

    /// Borrows bytes up to (and consuming) the next NUL terminator.
    pub fn null_terminated(&mut self) -> Result<&'a [u8]> {
        let start = self.pos;
        while self.pos < self.buf.len() {
            if self.buf[self.pos] == 0 {
                let slice = &self.buf[start..self.pos];
                self.pos += 1; // consume the NUL
                return Ok(slice);
            }
            self.pos += 1;
        }
        Err(ProtocolError::UnexpectedEof {
            offset: start,
            needed: 1,
        })
    }

    /// Borrows a length-encoded byte string.
    pub fn lenenc_bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.lenenc_u64()? as usize;
        self.bytes(len)
    }

    /// Borrows all remaining bytes (an "EOF string").
    pub fn rest(&mut self) -> &'a [u8] {
        let slice = &self.buf[self.pos..];
        self.pos = self.buf.len();
        slice
    }
}

/// Decodes a slice as UTF-8, attributing failures to a named field.
pub fn utf8(field: &'static str, bytes: &[u8]) -> Result<String> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| ProtocolError::InvalidUtf8 { field })
}

/// An append-only builder for a single MySQL packet payload.
#[derive(Default)]
pub struct PacketWriter {
    buf: Vec<u8>,
}

impl PacketWriter {
    /// Creates an empty writer.
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Creates a writer with reserved capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Number of bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns true if nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u24(&mut self, v: u32) -> &mut Self {
        let b = v.to_le_bytes();
        self.buf.extend_from_slice(&b[..3]);
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Writes a length-encoded integer.
    pub fn lenenc_u64(&mut self, v: u64) -> &mut Self {
        if v < 0xfb {
            self.u8(v as u8);
        } else if v <= 0xffff {
            self.u8(0xfc).u16(v as u16);
        } else if v <= 0xff_ffff {
            self.u8(0xfd).u24(v as u32);
        } else {
            self.u8(0xfe);
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// Writes raw bytes followed by a NUL terminator.
    pub fn null_terminated(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self.buf.push(0);
        self
    }

    /// Writes a length-encoded byte string.
    pub fn lenenc_bytes(&mut self, b: &[u8]) -> &mut Self {
        self.lenenc_u64(b.len() as u64).bytes(b)
    }

    /// Writes `n` zero bytes.
    pub fn fill(&mut self, n: usize) -> &mut Self {
        self.buf.resize(self.buf.len() + n, 0);
        self
    }

    /// Consumes the writer, yielding the payload bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Borrows the payload bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenenc_int_roundtrips() {
        for v in [
            0u64,
            1,
            250,
            251,
            0xffff,
            0x1_0000,
            0xff_ffff,
            0x100_0000,
            u64::MAX,
        ] {
            let mut w = PacketWriter::new();
            w.lenenc_u64(v);
            let bytes = w.into_bytes();
            let mut r = PacketReader::new(&bytes);
            assert_eq!(r.lenenc_u64().unwrap(), v, "value {v}");
            assert!(r.is_empty());
        }
    }

    #[test]
    fn null_terminated_roundtrips() {
        let mut w = PacketWriter::new();
        w.null_terminated(b"root").u8(0x42);
        let bytes = w.into_bytes();
        let mut r = PacketReader::new(&bytes);
        assert_eq!(r.null_terminated().unwrap(), b"root");
        assert_eq!(r.u8().unwrap(), 0x42);
    }

    #[test]
    fn reading_past_end_errors() {
        let bytes = [1u8, 2];
        let mut r = PacketReader::new(&bytes);
        assert!(r.u32().is_err());
    }
}
