// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! The synchronous per-connection state machine: handshake, then a loop that
//! reads command packets and replies. All wire bytes go through the sans-IO
//! [`turso_mysql_protocol`] crate; this file only does socket I/O and bridges
//! commands onto the [`turso_core`] engine.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::num::NonZero;
use std::sync::Arc;

use tracing::debug;
use turso_core::{Connection, LimboError, Statement, Value};
use turso_mysql_parser::{ast, ParseError};
use turso_mysql_protocol::constants::status::SERVER_STATUS_AUTOCOMMIT;
use turso_mysql_protocol::{
    encode_binary_row, encode_column_count, encode_frame, encode_text_row, ColumnDefinition,
    Command, EofPacket, ErrPacket, FrameDecoder, HandshakeResponse41, HandshakeV10, OkPacket,
    StmtExecute, StmtPrepareOk,
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

/// A prepared statement held open for binary execution, along with the column
/// metadata computed once at prepare time.
struct Prepared {
    statement: Statement,
    /// Typed result-column definitions, reused for every execution.
    columns: Vec<ColumnDefinition>,
    /// Number of `?` parameters the statement binds.
    num_params: usize,
}

/// The prepared statements opened on one connection, keyed by statement id.
#[derive(Default)]
struct StatementStore {
    next_id: u32,
    map: HashMap<u32, Prepared>,
}

impl StatementStore {
    /// Stores a freshly prepared statement and returns its new id.
    fn insert(&mut self, prepared: Prepared) -> u32 {
        self.next_id += 1;
        let id = self.next_id;
        self.map.insert(id, prepared);
        id
    }

    fn get_mut(&mut self, id: u32) -> Option<&mut Prepared> {
        self.map.get_mut(&id)
    }

    fn close(&mut self, id: u32) {
        self.map.remove(&id);
    }
}

/// Drives one client connection to completion. Returns `Ok(())` on a clean
/// disconnect; transport errors propagate.
pub fn handle(server: Arc<Server>, stream: TcpStream, connection_id: u32) -> anyhow::Result<()> {
    let mut wire = Wire::new(stream);
    let conn = server.db.connect()?;
    let mut statements = StatementStore::default();
    // The unlimited row count remembered from the last `SQL_CALC_FOUND_ROWS`
    // query, answered by a subsequent `SELECT FOUND_ROWS()`.
    let mut found_rows: u64 = 0;
    // The session SQL mode set via `SET sql_mode = '...'`, returned by
    // `SELECT @@SESSION.sql_mode`. Empty until the client sets it.
    let mut sql_mode = String::new();

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
                let response = run_query(&conn, &sql, seq, &mut found_rows, &mut sql_mode);
                wire.write_frames(&response)?;
            }
            Command::StmtPrepare(sql) => {
                let response = prepare_stmt(&conn, &mut statements, &sql, seq);
                wire.write_frames(&response)?;
            }
            Command::StmtExecute(exec) => {
                let response = execute_stmt_binary(&conn, &mut statements, &exec, seq);
                wire.write_frames(&response)?;
            }
            Command::StmtReset(id) => {
                let response = reset_stmt(&mut statements, id, seq);
                wire.write_frames(&response)?;
            }
            Command::StmtClose(id) => {
                // COM_STMT_CLOSE expects no reply; just drop the statement.
                statements.close(id);
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
fn run_query(
    conn: &Arc<Connection>,
    sql: &str,
    first_seq: u8,
    found_rows: &mut u64,
    sql_mode: &mut String,
) -> Vec<u8> {
    debug!(%sql, "COM_QUERY");

    // `SELECT FOUND_ROWS()` reports the count remembered from the last
    // `SQL_CALC_FOUND_ROWS` query (see below). It is answered here because the
    // engine has no such function.
    if is_found_rows_query(sql) {
        return encode_session_response(
            first_seq,
            SessionResponse::Row {
                columns: vec!["FOUND_ROWS()".to_string()],
                values: vec![Some(found_rows.to_string())],
            },
        );
    }

    // `SET [SESSION|GLOBAL] sql_mode = '...'` stores the session SQL mode, and
    // `SELECT @@[SESSION.|GLOBAL.]sql_mode` returns it. WordPress reads, sets,
    // and re-reads it (`wpdb::set_sql_mode`); without this round trip it gets a
    // constant value and `explode()` on it fails. MySQL's default value and
    // mode normalization/reordering are not modeled — the value is stored and
    // returned verbatim.
    if let Some(value) = parse_set_sql_mode(sql) {
        *sql_mode = value;
        let mut out = Vec::new();
        encode_frame(&mut out, first_seq, &OkPacket::default().encode());
        return out;
    }
    if is_select_sql_mode(sql) {
        return encode_session_response(
            first_seq,
            SessionResponse::Row {
                columns: vec!["@@SESSION.sql_mode".to_string()],
                values: vec![Some(sql_mode.clone())],
            },
        );
    }

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

    let mut stmts = match turso_mysql_parser::parse_all(sql) {
        Ok(stmts) => stmts,
        Err(e) => return parse_error_response(first_seq, &e),
    };
    debug!(%sql, "parsed by mysql front-end");

    // More than one statement comes only from a multi-table `DROP TABLE a, b`,
    // which the front-end expands into one `DROP TABLE` per table (the engine has
    // no multi-table drop). Run each for side effects and reply with a single OK;
    // a failure on any table returns its error.
    if stmts.len() != 1 {
        for stmt in stmts {
            if let Err(e) = run_for_side_effects(conn, stmt) {
                return error_response(first_seq, &e);
            }
        }
        let mut out = Vec::new();
        encode_frame(&mut out, first_seq, &OkPacket::default().encode());
        return out;
    }
    let stmt = stmts.pop().expect("exactly one statement");

    // `SELECT SQL_CALC_FOUND_ROWS ... LIMIT n` returns the limited rows, but
    // also sets `FOUND_ROWS()` to the count the query would return without its
    // LIMIT. The parser strips the modifier, so detect it from the SQL text and,
    // for a SELECT, re-run the query with the LIMIT removed to count the rows.
    if has_sql_calc_found_rows(sql) {
        if let ast::Stmt::Select(select) = &stmt {
            let mut unlimited = select.clone();
            unlimited.limit = None;
            if let Some(count) = count_rows(conn, ast::Stmt::Select(unlimited)) {
                *found_rows = count;
            }
        }
    }

    execute_stmt(conn, stmt, first_seq)
}

/// Extracts the value from a `SET [SESSION|GLOBAL] sql_mode = '...'` statement
/// (also the `SET @@[session.]sql_mode = '...'` spelling), or `None` if `sql` is
/// not such a statement. Only the single-assignment quoted form WordPress emits
/// is recognized; the quotes are stripped and `''` escapes are unescaped.
fn parse_set_sql_mode(sql: &str) -> Option<String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut rest = trimmed.strip_prefix("SET ").or_else(|| {
        // Case-insensitive `SET` prefix.
        trimmed
            .get(..4)
            .filter(|p| p.eq_ignore_ascii_case("SET "))
            .map(|_| &trimmed[4..])
    })?;
    rest = rest.trim_start();
    // Optional SESSION / GLOBAL / @@session. / @@global. scope before the name.
    for scope in ["SESSION ", "GLOBAL ", "@@session.", "@@global.", "@@"] {
        if rest.len() >= scope.len() && rest[..scope.len()].eq_ignore_ascii_case(scope) {
            rest = rest[scope.len()..].trim_start();
            break;
        }
    }
    let after = rest
        .get(.."sql_mode".len())
        .filter(|p| p.eq_ignore_ascii_case("sql_mode"))
        .map(|_| rest["sql_mode".len()..].trim_start())?;
    let value = after.strip_prefix('=')?.trim_start();
    // The value must be a single-quoted string; anything else (DEFAULT, a bare
    // identifier) is not modeled.
    let inner = value.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("''", "'"))
}

/// Whether `sql` is a standalone `SELECT @@[SESSION.|GLOBAL.]sql_mode` query,
/// ignoring case, surrounding whitespace, and a trailing semicolon.
fn is_select_sql_mode(sql: &str) -> bool {
    let normalized = sql
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    matches!(
        normalized.to_ascii_uppercase().as_str(),
        "SELECT @@SQL_MODE" | "SELECT @@SESSION.SQL_MODE" | "SELECT @@GLOBAL.SQL_MODE"
    )
}

/// Whether `sql` is a `SELECT FOUND_ROWS()` query (the only form WordPress
/// emits), ignoring case, surrounding whitespace, and a trailing semicolon.
fn is_found_rows_query(sql: &str) -> bool {
    let trimmed = sql.trim().trim_end_matches(';');
    let normalized = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.eq_ignore_ascii_case("SELECT FOUND_ROWS()")
        || normalized.eq_ignore_ascii_case("SELECT FOUND_ROWS ()")
}

/// Whether `sql` is a `SELECT` whose first item is the `SQL_CALC_FOUND_ROWS`
/// modifier. The modifier always appears immediately after `SELECT`.
fn has_sql_calc_found_rows(sql: &str) -> bool {
    let s = sql.trim_start();
    if s.len() < 6 || !s[..6].eq_ignore_ascii_case("SELECT") {
        return false;
    }
    let after = s[6..].trim_start();
    const MODIFIER: &str = "SQL_CALC_FOUND_ROWS";
    after.len() >= MODIFIER.len()
        && after[..MODIFIER.len()].eq_ignore_ascii_case(MODIFIER)
        // The next character must not extend the identifier.
        && after[MODIFIER.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
}

/// Prepares and steps a statement to completion, returning the number of rows
/// it produced (or `None` if preparing or stepping failed). Used to compute the
/// `SQL_CALC_FOUND_ROWS` count without sending a result to the client.
fn count_rows(conn: &Arc<Connection>, stmt: ast::Stmt) -> Option<u64> {
    let mut statement = conn.prepare_stmt(stmt).ok()?;
    let mut count: u64 = 0;
    statement
        .run_with_row_callback(|_| {
            count += 1;
            Ok(())
        })
        .ok()?;
    Some(count)
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
    // MySQL treats `COMMIT` / `ROLLBACK` with no transaction in progress as a
    // silent no-op, whereas the engine errors ("no transaction is active").
    // Clients — notably the WordPress test harness, which brackets every test
    // class with a bare `COMMIT` — depend on the no-op behavior, so answer with
    // a clean OK packet before the statement ever reaches the engine. When a
    // transaction *is* active (`get_auto_commit()` is false), fall through and
    // let the engine commit or roll it back as usual.
    if matches!(stmt, ast::Stmt::Commit { .. } | ast::Stmt::Rollback { .. })
        && conn.get_auto_commit()
    {
        let mut out = Vec::new();
        encode_frame(&mut out, first_seq, &OkPacket::default().encode());
        return out;
    }

    // MySQL has no nested transactions: `START TRANSACTION` / `BEGIN` issued
    // while one is already in progress implicitly commits it before starting
    // the new one. The engine instead errors ("cannot start a transaction
    // within a transaction"), so perform that implicit commit here when a
    // transaction is active (`get_auto_commit()` is false) and then fall
    // through to start the new one.
    if matches!(stmt, ast::Stmt::Begin { .. }) && !conn.get_auto_commit() {
        if let Err(e) = run_for_side_effects(conn, ast::Stmt::Commit { name: None }) {
            return error_response(first_seq, &e);
        }
    }

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

/// Prepares and runs a statement purely for its side effects, discarding any
/// rows. Used for statements the front-end issues on the client's behalf, such
/// as the implicit `COMMIT` before a nested `START TRANSACTION`.
fn run_for_side_effects(conn: &Arc<Connection>, stmt: ast::Stmt) -> Result<(), LimboError> {
    let mut statement = conn.prepare_stmt(stmt)?;
    statement.run_with_row_callback(|_| Ok(()))
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
        let column = crate::types::column_definition(stmt, i);
        seq = encode_frame(&mut out, seq, &column.encode());
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

/// Handles `COM_STMT_PREPARE`: parse and prepare the statement, retain it for
/// later execution, and reply with the prepare-OK header followed by the
/// parameter and column definition packets.
fn prepare_stmt(
    conn: &Arc<Connection>,
    statements: &mut StatementStore,
    sql: &str,
    first_seq: u8,
) -> Vec<u8> {
    debug!(%sql, "COM_STMT_PREPARE");
    let ast = match turso_mysql_parser::parse(sql) {
        Ok(ast) => ast,
        Err(e) => return parse_error_response(first_seq, &e),
    };
    let statement = match conn.prepare_stmt(ast) {
        Ok(statement) => statement,
        Err(e) => return error_response(first_seq, &e),
    };

    let num_params = statement.parameters_count();
    let num_columns = statement.num_columns();
    let columns: Vec<ColumnDefinition> = (0..num_columns)
        .map(|i| crate::types::column_definition(&statement, i))
        .collect();

    let mut out = Vec::new();
    let id = statements.insert(Prepared {
        statement,
        columns,
        num_params,
    });
    let prepared = statements.get_mut(id).expect("just inserted");

    let ok = StmtPrepareOk {
        statement_id: id,
        num_columns: num_columns as u16,
        num_params: num_params as u16,
        warning_count: 0,
    };
    let mut seq = encode_frame(&mut out, first_seq, &ok.encode());

    // Parameter definitions carry no useful type (a placeholder has none until
    // it is bound), so a generic string column stands in for each.
    if num_params > 0 {
        for _ in 0..num_params {
            seq = encode_frame(&mut out, seq, &ColumnDefinition::text("?").encode());
        }
        seq = encode_frame(
            &mut out,
            seq,
            &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
        );
    }
    if num_columns > 0 {
        for column in &prepared.columns {
            seq = encode_frame(&mut out, seq, &column.encode());
        }
        encode_frame(
            &mut out,
            seq,
            &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
        );
    }
    out
}

/// Handles `COM_STMT_EXECUTE`: decode and bind the parameters, run the
/// statement, and reply with a binary result set (or an OK packet for a
/// statement that returns no rows).
fn execute_stmt_binary(
    conn: &Arc<Connection>,
    statements: &mut StatementStore,
    exec: &StmtExecute,
    first_seq: u8,
) -> Vec<u8> {
    let Some(prepared) = statements.get_mut(exec.statement_id) else {
        return unknown_statement_response(first_seq, exec.statement_id);
    };

    let params = match exec.parse_params(prepared.num_params) {
        Ok(params) => params,
        Err(e) => return error_response(first_seq, &LimboError::InternalError(e.to_string())),
    };

    // Each execution starts from a clean slate: reset the program and rebind the
    // freshly supplied parameters.
    if let Err(e) = prepared.statement.reset() {
        return error_response(first_seq, &e);
    }
    prepared.statement.clear_bindings();
    for (i, value) in params.into_iter().enumerate() {
        let index = NonZero::new(i + 1).expect("parameter index is >= 1");
        if let Err(e) = prepared
            .statement
            .bind_at(index, crate::types::binary_to_value(value))
        {
            return error_response(first_seq, &e);
        }
    }

    if prepared.columns.is_empty() {
        // No result set: run for side effects and reply with an OK packet.
        let mut out = Vec::new();
        match prepared.statement.run_with_row_callback(|_| Ok(())) {
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
        match encode_binary_result_set(prepared, first_seq) {
            Ok(bytes) => bytes,
            Err(e) => error_response(first_seq, &e),
        }
    }
}

/// Encodes a binary result set: the column count, the typed column definitions,
/// and one binary row per result row. Mirrors [`encode_result_set`] but uses the
/// binary row format, so the column type codes drive how each value is encoded.
fn encode_binary_result_set(prepared: &mut Prepared, seq: u8) -> Result<Vec<u8>, LimboError> {
    let mut out = Vec::new();
    let num_columns = prepared.columns.len();
    let mut seq = encode_frame(&mut out, seq, &encode_column_count(num_columns as u64));
    for column in &prepared.columns {
        seq = encode_frame(&mut out, seq, &column.encode());
    }
    seq = encode_frame(
        &mut out,
        seq,
        &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
    );

    let type_codes: Vec<u8> = prepared.columns.iter().map(|c| c.column_type).collect();
    let mut seq_cell = seq;
    prepared.statement.run_with_row_callback(|row| {
        let cells: Vec<_> = row
            .get_values()
            .zip(&type_codes)
            .map(|(value, &code)| (code, crate::types::value_to_binary(value, code)))
            .collect();
        seq_cell = encode_frame(&mut out, seq_cell, &encode_binary_row(&cells));
        Ok(())
    })?;

    encode_frame(
        &mut out,
        seq_cell,
        &EofPacket::new(SERVER_STATUS_AUTOCOMMIT).encode(),
    );
    Ok(out)
}

/// Handles `COM_STMT_RESET`: discard the statement's bound-parameter state.
fn reset_stmt(statements: &mut StatementStore, id: u32, seq: u8) -> Vec<u8> {
    let mut out = Vec::new();
    match statements.get_mut(id) {
        Some(prepared) => {
            prepared.statement.clear_bindings();
            if let Err(e) = prepared.statement.reset() {
                return error_response(seq, &e);
            }
            encode_frame(&mut out, seq, &OkPacket::default().encode());
        }
        None => return unknown_statement_response(seq, id),
    }
    out
}

/// Builds an ERR packet for a command that referenced an unknown statement id.
fn unknown_statement_response(seq: u8, id: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let err = ErrPacket::new(
        ER_ERROR_GENERAL,
        format!("unknown prepared statement id {id}"),
    );
    encode_frame(&mut out, seq, &err.encode());
    out
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
