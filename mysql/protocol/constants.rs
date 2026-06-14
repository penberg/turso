// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Protocol constants: capability flags, status flags, commands, column types.
//!
//! These mirror the values defined by the MySQL client/server protocol and the
//! MariaDB protocol documentation. Only the subset needed by Turso is listed,
//! but the module is intended to grow as more of the protocol is supported.

/// Capability flags negotiated in the handshake. A client and server agree on
/// the intersection of the flags each advertises.
pub mod capabilities {
    pub const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
    pub const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
    pub const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
    pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
    pub const CLIENT_NO_SCHEMA: u32 = 0x0000_0010;
    pub const CLIENT_COMPRESS: u32 = 0x0000_0020;
    pub const CLIENT_LOCAL_FILES: u32 = 0x0000_0080;
    pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
    pub const CLIENT_INTERACTIVE: u32 = 0x0000_0400;
    pub const CLIENT_SSL: u32 = 0x0000_0800;
    pub const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
    pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
    pub const CLIENT_MULTI_STATEMENTS: u32 = 0x0001_0000;
    pub const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;
    pub const CLIENT_PS_MULTI_RESULTS: u32 = 0x0004_0000;
    pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
    pub const CLIENT_CONNECT_ATTRS: u32 = 0x0010_0000;
    pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
    pub const CLIENT_DEPRECATE_EOF: u32 = 0x0100_0000;
}

/// Server status flags reported in OK and EOF packets.
pub mod status {
    pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
    pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
    pub const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;
}

/// Text-protocol command bytes (the first byte of a command packet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CommandByte {
    Quit = 0x01,
    InitDb = 0x02,
    Query = 0x03,
    FieldList = 0x04,
    Ping = 0x0e,
    StmtPrepare = 0x16,
    StmtExecute = 0x17,
    StmtClose = 0x19,
}

impl CommandByte {
    /// Returns the command for a leading byte, if it is one we model explicitly.
    pub fn from_u8(byte: u8) -> Option<Self> {
        Some(match byte {
            0x01 => Self::Quit,
            0x02 => Self::InitDb,
            0x03 => Self::Query,
            0x04 => Self::FieldList,
            0x0e => Self::Ping,
            0x16 => Self::StmtPrepare,
            0x17 => Self::StmtExecute,
            0x19 => Self::StmtClose,
            _ => return None,
        })
    }
}

/// MySQL column types as sent in column-definition packets, matching the
/// `enum_field_types` values from the server protocol.
pub mod column_type {
    pub const MYSQL_TYPE_DECIMAL: u8 = 0x00;
    pub const MYSQL_TYPE_TINY: u8 = 0x01;
    pub const MYSQL_TYPE_SHORT: u8 = 0x02;
    pub const MYSQL_TYPE_LONG: u8 = 0x03;
    pub const MYSQL_TYPE_FLOAT: u8 = 0x04;
    pub const MYSQL_TYPE_DOUBLE: u8 = 0x05;
    pub const MYSQL_TYPE_NULL: u8 = 0x06;
    pub const MYSQL_TYPE_TIMESTAMP: u8 = 0x07;
    pub const MYSQL_TYPE_LONGLONG: u8 = 0x08;
    pub const MYSQL_TYPE_INT24: u8 = 0x09;
    pub const MYSQL_TYPE_DATE: u8 = 0x0a;
    pub const MYSQL_TYPE_TIME: u8 = 0x0b;
    pub const MYSQL_TYPE_DATETIME: u8 = 0x0c;
    pub const MYSQL_TYPE_YEAR: u8 = 0x0d;
    pub const MYSQL_TYPE_NEWDECIMAL: u8 = 0xf6;
    pub const MYSQL_TYPE_JSON: u8 = 0xf5;
    pub const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;
    pub const MYSQL_TYPE_STRING: u8 = 0xfe;
    pub const MYSQL_TYPE_BLOB: u8 = 0xfc;
}

/// Column-definition flags (the `flags` field of `ColumnDefinition41`). They
/// describe nullability, key membership, and signedness.
pub mod column_flags {
    pub const NOT_NULL_FLAG: u16 = 0x0001;
    pub const PRI_KEY_FLAG: u16 = 0x0002;
    pub const UNIQUE_KEY_FLAG: u16 = 0x0004;
    pub const MULTIPLE_KEY_FLAG: u16 = 0x0008;
    pub const BLOB_FLAG: u16 = 0x0010;
    pub const UNSIGNED_FLAG: u16 = 0x0020;
    pub const ZEROFILL_FLAG: u16 = 0x0040;
    pub const BINARY_FLAG: u16 = 0x0080;
    pub const ENUM_FLAG: u16 = 0x0100;
    pub const AUTO_INCREMENT_FLAG: u16 = 0x0200;
    pub const TIMESTAMP_FLAG: u16 = 0x0400;
    pub const SET_FLAG: u16 = 0x0800;
    pub const NUM_FLAG: u16 = 0x8000;
}

/// Common collation identifiers used in the charset byte. `utf8mb4_general_ci`
/// is a safe default that every modern client understands; `binary` (id 63) is
/// used for blob/binary columns.
pub mod collation {
    pub const UTF8MB4_GENERAL_CI: u8 = 45;
    pub const BINARY: u8 = 63;
}

/// The single byte that marks a SQL NULL value inside a text result row.
pub const NULL_COLUMN_MARKER: u8 = 0xfb;

/// First byte of an OK packet.
pub const OK_HEADER: u8 = 0x00;
/// First byte of an EOF packet (also the lenenc 0xfe sentinel; disambiguated by
/// packet length, which is < 9 bytes for a real EOF packet).
pub const EOF_HEADER: u8 = 0xfe;
/// First byte of an ERR packet.
pub const ERR_HEADER: u8 = 0xff;

/// The auth plugin Turso advertises. We do not yet verify credentials, but
/// `mysql_native_password` is understood by every client and keeps the
/// handshake on the well-trodden path.
pub const DEFAULT_AUTH_PLUGIN: &str = "mysql_native_password";
