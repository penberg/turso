// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Result-set packets: the column count, per-column definitions, and text-protocol rows.
//!
//! A text result set is a sequence of packets:
//! 1. a single length-encoded column count,
//! 2. one [`ColumnDefinition`] per column,
//! 3. an EOF packet (when `CLIENT_DEPRECATE_EOF` is not negotiated),
//! 4. one text row per result row,
//! 5. a terminating EOF (or OK) packet.
//!
//! This module produces the payloads for steps 1, 2, and 4; the caller frames
//! them and interleaves the EOF/OK markers.

use crate::constants::{collation, column_type, NULL_COLUMN_MARKER};
use crate::packet::PacketWriter;

/// A column-definition packet (`Protocol::ColumnDefinition41`).
#[derive(Debug, Clone)]
pub struct ColumnDefinition {
    /// Column name as seen by the client.
    pub name: String,
    /// Originating table name (may be empty).
    pub table: String,
    /// MySQL column type (see [`crate::constants::column_type`]).
    pub column_type: u8,
    /// Collation id.
    pub charset: u8,
    /// Declared maximum length in bytes.
    pub column_length: u32,
    /// Column flags (nullability, key info, ...).
    pub flags: u16,
    /// Number of decimal digits, or `0x1f` for non-numeric.
    pub decimals: u8,
}

impl ColumnDefinition {
    /// A text column with the given name, typed as a variable-length string.
    ///
    /// The text protocol renders every value as a string, so this is a safe
    /// default when the precise SQL type is not known ahead of execution.
    pub fn text(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            table: String::new(),
            column_type: column_type::MYSQL_TYPE_VAR_STRING,
            charset: collation::UTF8MB4_GENERAL_CI,
            column_length: 1024,
            flags: 0,
            decimals: 0x1f,
        }
    }

    /// Serializes the column definition into a payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = PacketWriter::with_capacity(64);
        w.lenenc_bytes(b"def"); // catalog (always "def")
        w.lenenc_bytes(b""); // schema
        w.lenenc_bytes(self.table.as_bytes()); // table (virtual)
        w.lenenc_bytes(self.table.as_bytes()); // org_table
        w.lenenc_bytes(self.name.as_bytes()); // name
        w.lenenc_bytes(self.name.as_bytes()); // org_name
        w.lenenc_u64(0x0c); // length of the fixed-length fields that follow
        w.u16(self.charset as u16);
        w.u32(self.column_length);
        w.u8(self.column_type);
        w.u16(self.flags);
        w.u8(self.decimals);
        w.fill(2); // reserved
        w.into_bytes()
    }
}

/// Encodes the column-count packet that opens a result set.
pub fn encode_column_count(count: u64) -> Vec<u8> {
    let mut w = PacketWriter::with_capacity(4);
    w.lenenc_u64(count);
    w.into_bytes()
}

/// Encodes one text-protocol result row. Each value is `Some(bytes)` for its
/// textual representation, or `None` for SQL NULL.
pub fn encode_text_row<I, B>(values: I) -> Vec<u8>
where
    I: IntoIterator<Item = Option<B>>,
    B: AsRef<[u8]>,
{
    let mut w = PacketWriter::with_capacity(64);
    for value in values {
        match value {
            Some(bytes) => {
                w.lenenc_bytes(bytes.as_ref());
            }
            None => {
                w.u8(NULL_COLUMN_MARKER);
            }
        }
    }
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::PacketReader;

    #[test]
    fn text_row_encodes_null_and_values() {
        let row = encode_text_row(vec![Some(b"hello".as_ref()), None, Some(b"42".as_ref())]);
        let mut r = PacketReader::new(&row);
        assert_eq!(r.lenenc_bytes().unwrap(), b"hello");
        assert_eq!(r.u8().unwrap(), NULL_COLUMN_MARKER);
        assert_eq!(r.lenenc_bytes().unwrap(), b"42");
        assert!(r.is_empty());
    }

    #[test]
    fn column_count_roundtrips() {
        let bytes = encode_column_count(3);
        let mut r = PacketReader::new(&bytes);
        assert_eq!(r.lenenc_u64().unwrap(), 3);
    }
}
