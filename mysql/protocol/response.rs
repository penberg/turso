// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Generic server responses: OK, ERR, and EOF packets.

use crate::constants::{status, EOF_HEADER, ERR_HEADER, OK_HEADER};
use crate::packet::PacketWriter;

/// A successful command response that carries no result set.
#[derive(Debug, Clone)]
pub struct OkPacket {
    pub affected_rows: u64,
    pub last_insert_id: u64,
    pub status_flags: u16,
    pub warnings: u16,
    pub info: String,
}

impl Default for OkPacket {
    fn default() -> Self {
        Self {
            affected_rows: 0,
            last_insert_id: 0,
            status_flags: status::SERVER_STATUS_AUTOCOMMIT,
            warnings: 0,
            info: String::new(),
        }
    }
}

impl OkPacket {
    /// An OK packet reporting the number of rows a statement changed.
    pub fn with_affected_rows(affected_rows: u64, last_insert_id: u64) -> Self {
        Self {
            affected_rows,
            last_insert_id,
            ..Default::default()
        }
    }

    /// Serializes the packet into a payload (assumes `CLIENT_PROTOCOL_41`).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = PacketWriter::with_capacity(16 + self.info.len());
        w.u8(OK_HEADER);
        w.lenenc_u64(self.affected_rows);
        w.lenenc_u64(self.last_insert_id);
        w.u16(self.status_flags);
        w.u16(self.warnings);
        // Human-readable info string runs to the end of the packet.
        w.bytes(self.info.as_bytes());
        w.into_bytes()
    }
}

/// An error response.
#[derive(Debug, Clone)]
pub struct ErrPacket {
    /// MySQL error code (e.g. 1064 for a syntax error).
    pub code: u16,
    /// Five-character SQLSTATE, e.g. `"42000"`.
    pub sql_state: [u8; 5],
    /// Human-readable error message.
    pub message: String,
}

impl ErrPacket {
    /// A generic error. Uses SQLSTATE `HY000` (general error) unless a more
    /// specific state is supplied via [`ErrPacket::with_state`].
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            sql_state: *b"HY000",
            message: message.into(),
        }
    }

    /// Overrides the SQLSTATE.
    pub fn with_state(mut self, state: [u8; 5]) -> Self {
        self.sql_state = state;
        self
    }

    /// Serializes the packet into a payload (assumes `CLIENT_PROTOCOL_41`).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = PacketWriter::with_capacity(9 + self.message.len());
        w.u8(ERR_HEADER);
        w.u16(self.code);
        w.u8(b'#'); // SQLSTATE marker
        w.bytes(&self.sql_state);
        w.bytes(self.message.as_bytes());
        w.into_bytes()
    }
}

/// An end-of-data marker used to terminate a column list or a result set when
/// `CLIENT_DEPRECATE_EOF` was not negotiated.
#[derive(Debug, Clone, Default)]
pub struct EofPacket {
    pub warnings: u16,
    pub status_flags: u16,
}

impl EofPacket {
    /// An EOF packet carrying the given server status flags.
    pub fn new(status_flags: u16) -> Self {
        Self {
            warnings: 0,
            status_flags,
        }
    }

    /// Serializes the packet into a payload (assumes `CLIENT_PROTOCOL_41`).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = PacketWriter::with_capacity(5);
        w.u8(EOF_HEADER);
        w.u16(self.warnings);
        w.u16(self.status_flags);
        w.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_packet_has_zero_header() {
        let bytes = OkPacket::default().encode();
        assert_eq!(bytes[0], OK_HEADER);
    }

    #[test]
    fn err_packet_carries_code_and_message() {
        let bytes = ErrPacket::new(1064, "boom").encode();
        assert_eq!(bytes[0], ERR_HEADER);
        assert_eq!(u16::from_le_bytes([bytes[1], bytes[2]]), 1064);
        assert_eq!(bytes[3], b'#');
        assert_eq!(&bytes[4..9], b"HY000");
        assert_eq!(&bytes[9..], b"boom");
    }
}
