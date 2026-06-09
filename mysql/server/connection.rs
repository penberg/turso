// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! The synchronous per-connection state machine: handshake, then a loop that
//! reads command packets and replies. All wire bytes go through the sans-IO
//! [`turso_mysql_protocol`] crate; this file only does socket I/O and bridges
//! commands onto the [`turso_core`] engine.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tracing::debug;
use turso_core::{Connection, LimboError, Value};
use turso_mysql_parser::{ast, ParseError};
use turso_mysql_protocol::constants::status::SERVER_STATUS_AUTOCOMMIT;
use turso_mysql_protocol::{
    encode_column_count, encode_frame, encode_text_row, ColumnDefinition, Command, EofPacket,
    ErrPacket, FrameDecoder, HandshakeResponse41, HandshakeV10, OkPacket,
};

use crate::session::{self, SessionResponse};
use crate::show::{self, ShowOutcome};
use crate::Server;

/// MySQL error code for "general error" — adequate for the proof of concept.
const ER_ERROR_GENERAL: u16 = 1105;
/// `ER_PARSE_ERROR`: a SQL syntax error.
const ER_PARSE_ERROR: u16 = 1064;
/// `ER_NOT_SUPPORTED_YET`: a statement the front-end recognizes but cannot run.
const ER_NOT_SUPPORTED_YET: u16 = 1235;
/// `ER_NO_SUCH_TABLE`: a statement referenced a table that does not exist.
const ER_NO_SUCH_TABLE: u16 = 1146;

/// Wraps the blocking socket with a frame decoder and a write buffer.
struct Wire {
    stream: TcpStream,
    decoder: FrameDecoder,
    read_buf: [u8; 16 * 1024],
}

impl Wire {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            decoder: FrameDecoder::new(),
            read_buf: [0u8; 16 * 1024],
        }
    }

    /// Reads the next complete logical packet, or `None` on clean EOF.
    fn read_packet(&mut self) -> io::Result<Option<turso_mysql_protocol::Packet>> {
        loop {
            if let Some(pkt) = self
                .decoder
                .next_packet()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            {
                return Ok(Some(pkt));
            }
            let n = self.stream.read(&mut self.read_buf)?;
            if n == 0 {
                return Ok(None);
            }
            let chunk = self.read_buf[..n].to_vec();
            self.decoder.extend(&chunk);
        }
    }

    /// Writes already-framed bytes to the socket.
    fn write_frames(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)?;
        self.stream.flush()
    }
}

/// Drives one client connection to completion. Returns `Ok(())` on a clean
/// disconnect; transport errors propagate.
pub fn handle(server: Arc<Server>, stream: TcpStream, connection_id: u32) -> anyhow::Result<()> {
    let mut wire = Wire::new(stream);
    let conn = server.db.connect()?;

    perform_handshake(&mut wire, connection_id)?;

    loop {
        let Some(packet) = wire.read_packet()? else {
            // Client closed the socket without a COM_QUIT.
            return Ok(());
        };
        let command = Command::decode(&packet.payload)?;
        debug!(connection_id, ?command, "command");
        // Replies to a command start one sequence id after the command packet.
        let seq = packet.seq.wrapping_add(1);
        match command {
            Command::Quit => return Ok(()),
            Command::Ping => {
                send_packet(&mut wire, seq, &OkPacket::default().encode())?;
            }
            Command::InitDb(_db) => {
                // Turso has a single schema; acknowledge the switch.
                send_packet(&mut wire, seq, &OkPacket::default().encode())?;
            }
            Command::Query(sql) => {
                let response = run_query(&conn, &sql, seq);
                wire.write_frames(&response)?;
            }
            Command::Unsupported(cmd) => {
                let err = ErrPacket::new(ER_ERROR_GENERAL, format!("unsupported command: {cmd:?}"));
                send_packet(&mut wire, seq, &err.encode())?;
            }
            Command::Unknown(byte) => {
                let err = ErrPacket::new(
                    ER_ERROR_GENERAL,
                    format!("unknown command byte 0x{byte:02x}"),
                );
                send_packet(&mut wire, seq, &err.encode())?;
            }
        }
    }
}

/// Sends the greeting, reads the client's response, and acknowledges it. We do
/// not yet verify credentials — any login is accepted.
fn perform_handshake(wire: &mut Wire, connection_id: u32) -> anyhow::Result<()> {
    let scramble = scramble_for(connection_id);
    let greeting = HandshakeV10::new("8.0.0-turso", connection_id, scramble);
    send_packet(wire, 0, &greeting.encode())?;

    let Some(response_packet) = wire.read_packet()? else {
        anyhow::bail!("client disconnected during handshake");
    };
    let response = HandshakeResponse41::decode(&response_packet.payload)?;
    debug!(connection_id, user = %response.username, "handshake response");

    // Acknowledge authentication. The response packet's sequence id is 1, so the
    // OK continues at 2.
    let seq = response_packet.seq.wrapping_add(1);
    send_packet(wire, seq, &OkPacket::default().encode())?;
    Ok(())
}

/// Frames `payload` with `seq` and writes it.
fn send_packet(wire: &mut Wire, seq: u8, payload: &[u8]) -> io::Result<()> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    encode_frame(&mut out, seq, payload);
    wire.write_frames(&out)
}

/// Handles a `COM_QUERY`: parse it with the MySQL front-end parser, then run it.
///
/// Every query goes through [`turso_mysql_parser`] first. Statements the parser
/// does not support are rejected here with a MySQL error packet — they never
/// reach the engine. A successfully parsed statement is handed to the engine as
/// an AST via [`Connection::prepare_stmt`], with no round trip through SQL text.
fn run_query(conn: &Arc<Connection>, sql: &str, first_seq: u8) -> Vec<u8> {
    debug!(%sql, "COM_QUERY");

    // Client libraries probe the connection with session/introspection queries
    // (`SELECT @@max_allowed_packet`, `SET ...`) before running real SQL. Answer
    // those here so the parser only ever sees user statements.
    if let Some(response) = session::try_handle(sql) {
        return encode_session_response(first_seq, response);
    }

    // `SHOW [FULL] COLUMNS FROM tbl` is answered from the schema here; the AST
    // has no `SHOW`, so it cannot go through the parser. Every other `SHOW`
    // falls through and is rejected by the parser as unsupported.
    if let Some(outcome) = show::try_handle(conn, sql) {
        return match outcome {
            Ok(ShowOutcome::Columns(result)) => encode_columns_result(first_seq, result),
            Ok(ShowOutcome::NoSuchTable(name)) => no_such_table_response(first_seq, &name),
            Err(e) => error_response(first_seq, &e),
        };
    }

    let stmt = match turso_mysql_parser::parse(sql) {
        Ok(stmt) => stmt,
        Err(e) => return parse_error_response(first_seq, &e),
    };
    debug!(%sql, "parsed by mysql front-end");
    execute_stmt(conn, stmt, first_seq)
}

/// Frames a [`SessionResponse`] (the reply to a connection/introspection query).
fn encode_session_response(first_seq: u8, response: SessionResponse) -> Vec<u8> {
    let mut out = Vec::new();
    match response {
        SessionResponse::Ok => {
            encode_frame(&mut out, first_seq, &OkPacket::default().encode());
        }
        SessionResponse::Row { columns, values } => {
            let mut seq = encode_frame(
                &mut out,
                first_seq,
                &encode_column_count(columns.len() as u64),
            );
            for name in &columns {
                seq = encode_frame(
                    &mut out,
                    seq,
                    &ColumnDefinition::text(name.clone()).encode(),
                );
            }
            seq = encode_frame(
                &mut out,
                seq,
                &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
            );
            let cells = values
                .into_iter()
                .map(|v| v.map(String::into_bytes))
                .collect::<Vec<_>>();
            seq = encode_frame(&mut out, seq, &encode_text_row(cells));
            encode_frame(
                &mut out,
                seq,
                &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
            );
        }
    }
    out
}

/// Frames a synthesized result set (e.g. the reply to `SHOW COLUMNS`): a column
/// header for each name, then one text row per data row.
fn encode_columns_result(first_seq: u8, result: show::ColumnsResult) -> Vec<u8> {
    let mut out = Vec::new();
    let mut seq = encode_frame(
        &mut out,
        first_seq,
        &encode_column_count(result.columns.len() as u64),
    );
    for name in &result.columns {
        seq = encode_frame(
            &mut out,
            seq,
            &ColumnDefinition::text((*name).to_string()).encode(),
        );
    }
    seq = encode_frame(
        &mut out,
        seq,
        &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
    );
    for row in result.rows {
        let cells = row
            .into_iter()
            .map(|v| v.map(String::into_bytes))
            .collect::<Vec<_>>();
        seq = encode_frame(&mut out, seq, &encode_text_row(cells));
    }
    encode_frame(
        &mut out,
        seq,
        &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
    );
    out
}

/// Builds a MySQL ERR packet for a reference to a table that does not exist.
fn no_such_table_response(first_seq: u8, table: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let err = ErrPacket::new(ER_NO_SUCH_TABLE, format!("Table '{table}' doesn't exist"))
        .with_state(*b"42S02");
    encode_frame(&mut out, first_seq, &err.encode());
    out
}

/// Builds a MySQL ERR packet for a front-end parse failure.
fn parse_error_response(first_seq: u8, error: &ParseError) -> Vec<u8> {
    let code = match error {
        ParseError::Unsupported(_) => ER_NOT_SUPPORTED_YET,
        _ => ER_PARSE_ERROR,
    };
    let mut out = Vec::new();
    let err = ErrPacket::new(code, error.to_string()).with_state(*b"42000");
    encode_frame(&mut out, first_seq, &err.encode());
    out
}

/// Prepares and executes a parsed statement, returning the full framed response.
///
/// The AST is handed straight to the engine via [`Connection::prepare_stmt`] —
/// no round trip through SQL text. The response is built entirely in memory so
/// that, if execution fails partway through, we can discard a partial result
/// set and reply with a clean ERR packet instead of a truncated stream.
fn execute_stmt(conn: &Arc<Connection>, stmt: ast::Stmt, first_seq: u8) -> Vec<u8> {
    let mut statement = match conn.prepare_stmt(stmt) {
        Ok(statement) => statement,
        Err(e) => return error_response(first_seq, &e),
    };

    let num_columns = statement.num_columns();
    if num_columns == 0 {
        // No result set: run for side effects and reply with an OK packet.
        let mut out = Vec::new();
        match statement.run_with_row_callback(|_| Ok(())) {
            Ok(()) => {
                let ok = OkPacket::with_affected_rows(
                    conn.changes().max(0) as u64,
                    conn.last_insert_rowid().max(0) as u64,
                );
                encode_frame(&mut out, first_seq, &ok.encode());
                out
            }
            Err(e) => error_response(first_seq, &e),
        }
    } else {
        match encode_result_set(&mut statement, num_columns, first_seq) {
            Ok(bytes) => bytes,
            Err(e) => error_response(first_seq, &e),
        }
    }
}

/// Encodes a complete text result set into a freshly framed buffer, starting at
/// sequence id `seq`. Returns an error if stepping the statement fails, in which
/// case the caller discards the buffer.
fn encode_result_set(
    stmt: &mut turso_core::Statement,
    num_columns: usize,
    seq: u8,
) -> Result<Vec<u8>, LimboError> {
    let mut out = Vec::new();
    let mut seq = encode_frame(&mut out, seq, &encode_column_count(num_columns as u64));

    for i in 0..num_columns {
        let name = stmt.get_column_name(i).to_string();
        seq = encode_frame(&mut out, seq, &ColumnDefinition::text(name).encode());
    }
    // Marker between the column definitions and the rows.
    seq = encode_frame(
        &mut out,
        seq,
        &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
    );

    // Rows are framed as they are produced. `seq` is threaded through the
    // closure via a cell so each row gets the next sequence id.
    let mut seq_cell = seq;
    stmt.run_with_row_callback(|row| {
        let cells = row.get_values().map(value_to_text).collect::<Vec<_>>();
        seq_cell = encode_frame(&mut out, seq_cell, &encode_text_row(cells));
        Ok(())
    })?;

    encode_frame(
        &mut out,
        seq_cell,
        &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
    );
    Ok(out)
}

/// Renders a single value as text-protocol bytes, or `None` for SQL NULL.
fn value_to_text(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Null => None,
        Value::Text(text) => Some(text.as_str().as_bytes().to_vec()),
        Value::Blob(blob) => Some(blob.clone()),
        other => Some(format!("{other}").into_bytes()),
    }
}

/// Builds a single ERR-packet response for a failed query.
fn error_response(seq: u8, error: &LimboError) -> Vec<u8> {
    let mut out = Vec::new();
    let err = ErrPacket::new(ER_ERROR_GENERAL, error.to_string());
    encode_frame(&mut out, seq, &err.encode());
    out
}

/// Produces a deterministic 20-byte auth scramble for a connection. Credentials
/// are not verified yet, so the exact bytes do not matter; they only need to be
/// well-formed for clients that compute a response.
fn scramble_for(connection_id: u32) -> [u8; 20] {
    let mut scramble = [0u8; 20];
    for (i, byte) in scramble.iter_mut().enumerate() {
        *byte = (connection_id.wrapping_add(i as u32) as u8).wrapping_add(1) | 0x01;
    }
    scramble
}
