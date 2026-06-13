// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Errors produced while decoding or encoding MySQL protocol packets.

use thiserror::Error;

/// An error encountered while parsing or serializing a MySQL wire packet.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// The buffer ended before a complete field could be read.
    #[error("unexpected end of packet: needed {needed} more byte(s) at offset {offset}")]
    UnexpectedEof { offset: usize, needed: usize },

    /// A field held a value that the protocol does not permit here.
    #[error("malformed packet: {0}")]
    Malformed(String),

    /// A length-encoded integer used the reserved 0xff prefix.
    #[error("invalid length-encoded integer prefix 0x{0:02x}")]
    InvalidLengthEncoding(u8),

    /// A field that must be valid UTF-8 was not.
    #[error("invalid UTF-8 in {field}")]
    InvalidUtf8 { field: &'static str },
}

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, ProtocolError>;
