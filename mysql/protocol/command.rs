// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Parsing of client command packets (the command phase).

use crate::constants::CommandByte;
use crate::error::Result;
use crate::packet::{utf8, PacketReader};
use crate::stmt::StmtExecute;

/// A decoded client command. Borrows from the packet payload where possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `COM_QUIT`: the client is closing the connection.
    Quit,
    /// `COM_PING`: a keepalive that expects an OK packet.
    Ping,
    /// `COM_INIT_DB`: switch the default schema.
    InitDb(String),
    /// `COM_QUERY`: execute a text SQL statement.
    Query(String),
    /// `COM_STMT_PREPARE`: prepare a SQL statement for later binary execution.
    StmtPrepare(String),
    /// `COM_STMT_EXECUTE`: execute a prepared statement with bound parameters.
    StmtExecute(StmtExecute),
    /// `COM_STMT_RESET`: discard a prepared statement's bound parameter state.
    StmtReset(u32),
    /// `COM_STMT_CLOSE`: deallocate a prepared statement (no reply expected).
    StmtClose(u32),
    /// A command byte we recognize but do not yet implement.
    Unsupported(CommandByte),
    /// A command byte we do not recognize at all.
    Unknown(u8),
}

impl Command {
    /// Parses a command packet payload (the full payload, command byte first).
    ///
    /// An empty payload is treated as `COM_QUIT`, matching how clients behave
    /// when the connection is torn down.
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let Some((&first, rest)) = payload.split_first() else {
            return Ok(Command::Quit);
        };
        Ok(match CommandByte::from_u8(first) {
            Some(CommandByte::Quit) => Command::Quit,
            Some(CommandByte::Ping) => Command::Ping,
            Some(CommandByte::InitDb) => Command::InitDb(utf8("init_db", rest)?),
            Some(CommandByte::Query) => Command::Query(utf8("query", rest)?),
            Some(CommandByte::StmtPrepare) => Command::StmtPrepare(utf8("stmt_prepare", rest)?),
            Some(CommandByte::StmtExecute) => {
                let mut r = PacketReader::new(rest);
                let statement_id = r.u32()?;
                let flags = r.u8()?;
                let _iteration_count = r.u32()?; // always 1
                Command::StmtExecute(StmtExecute {
                    statement_id,
                    flags,
                    params_tail: r.rest().to_vec(),
                })
            }
            Some(CommandByte::StmtReset) => Command::StmtReset(PacketReader::new(rest).u32()?),
            Some(CommandByte::StmtClose) => Command::StmtClose(PacketReader::new(rest).u32()?),
            Some(other) => Command::Unsupported(other),
            None => Command::Unknown(first),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_query() {
        let mut payload = vec![0x03];
        payload.extend_from_slice(b"SELECT 1");
        assert_eq!(
            Command::decode(&payload).unwrap(),
            Command::Query("SELECT 1".to_string())
        );
    }

    #[test]
    fn empty_payload_is_quit() {
        assert_eq!(Command::decode(&[]).unwrap(), Command::Quit);
    }

    #[test]
    fn ping_and_quit() {
        assert_eq!(Command::decode(&[0x0e]).unwrap(), Command::Ping);
        assert_eq!(Command::decode(&[0x01]).unwrap(), Command::Quit);
    }
}
