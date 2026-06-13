// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! `turso-mysql-protocol` is a sans-IO implementation of the MySQL client/server
//! wire protocol.
//!
//! "Sans-IO" means this crate never touches a socket, a runtime, or a clock. It
//! only turns bytes into typed packets and typed packets back into bytes. That
//! keeps it reusable everywhere Turso needs to speak MySQL — a tokio-based
//! standalone server, an embedded listener, or internal cloud services — without
//! dragging an async runtime into the dependency graph.
//!
//! # Layout
//!
//! * [`frame`] — packet framing (length + sequence id) and reassembly.
//! * [`packet`] — primitive integer/string encodings and the reader/writer.
//! * [`handshake`] — the connection-phase greeting and response.
//! * [`command`] — parsing client command packets.
//! * [`response`] — OK / ERR / EOF packets.
//! * [`resultset`] — column definitions and text rows.
//! * [`constants`] — capability flags, status flags, command and type codes.
//!
//! # Example: assembling a result set
//!
//! ```
//! use turso_mysql_protocol::frame::encode_frame;
//! use turso_mysql_protocol::resultset::{encode_column_count, encode_text_row, ColumnDefinition};
//! use turso_mysql_protocol::response::EofPacket;
//! use turso_mysql_protocol::constants::status::SERVER_STATUS_AUTOCOMMIT;
//!
//! let mut out = Vec::new();
//! // Sequence ids continue from the command packet the client sent (seq 0),
//! // so the first response packet is seq 1.
//! let mut seq = 1;
//! seq = encode_frame(&mut out, seq, &encode_column_count(1));
//! seq = encode_frame(&mut out, seq, &ColumnDefinition::text("greeting").encode());
//! seq = encode_frame(&mut out, seq, &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode());
//! seq = encode_frame(&mut out, seq, &encode_text_row([Some(b"hello")]));
//! let _ = encode_frame(&mut out, seq, &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode());
//! assert!(!out.is_empty());
//! ```

pub mod command;
pub mod constants;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod packet;
pub mod response;
pub mod resultset;

pub use command::Command;
pub use error::{ProtocolError, Result};
pub use frame::{encode_frame, FrameDecoder, Packet};
pub use handshake::{HandshakeResponse41, HandshakeV10};
pub use response::{EofPacket, ErrPacket, OkPacket};
pub use resultset::{encode_column_count, encode_text_row, ColumnDefinition};
