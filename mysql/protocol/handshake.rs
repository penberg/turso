// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! The connection-phase packets: the server's `HandshakeV10` greeting and the
//! client's `HandshakeResponse41` reply.

use crate::constants::{capabilities as cap, collation, DEFAULT_AUTH_PLUGIN};
use crate::error::Result;
use crate::packet::{utf8, PacketReader, PacketWriter};

/// The initial greeting a server sends to a freshly connected client
/// (protocol version 10).
#[derive(Debug, Clone)]
pub struct HandshakeV10 {
    /// Human-readable server version string, e.g. `"8.0.0-turso"`.
    pub server_version: String,
    /// Per-connection thread/connection id.
    pub connection_id: u32,
    /// 20-byte authentication scramble (`auth-plugin-data`).
    pub auth_plugin_data: [u8; 20],
    /// Capability flags the server advertises.
    pub capabilities: u32,
    /// Default collation id for the connection.
    pub charset: u8,
    /// Initial server status flags.
    pub status_flags: u16,
    /// Name of the authentication plugin the client should use.
    pub auth_plugin_name: String,
}

impl HandshakeV10 {
    /// Builds a greeting with sensible defaults for the given identity and
    /// scramble. The caller supplies the random scramble bytes.
    pub fn new(server_version: impl Into<String>, connection_id: u32, scramble: [u8; 20]) -> Self {
        let capabilities = cap::CLIENT_PROTOCOL_41
            | cap::CLIENT_SECURE_CONNECTION
            | cap::CLIENT_PLUGIN_AUTH
            | cap::CLIENT_LONG_PASSWORD
            | cap::CLIENT_TRANSACTIONS
            | cap::CLIENT_LONG_FLAG
            | cap::CLIENT_CONNECT_WITH_DB;
        Self {
            server_version: server_version.into(),
            connection_id,
            auth_plugin_data: scramble,
            capabilities,
            charset: collation::UTF8MB4_GENERAL_CI,
            status_flags: crate::constants::status::SERVER_STATUS_AUTOCOMMIT,
            auth_plugin_name: DEFAULT_AUTH_PLUGIN.to_owned(),
        }
    }

    /// Serializes the greeting into a packet payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = PacketWriter::with_capacity(64);
        w.u8(10); // protocol version
        w.null_terminated(self.server_version.as_bytes());
        w.u32(self.connection_id);
        // auth-plugin-data-part-1: first 8 bytes of the scramble.
        w.bytes(&self.auth_plugin_data[..8]);
        w.u8(0); // filler
                 // Capability flags, lower 16 bits.
        w.u16(self.capabilities as u16);
        w.u8(self.charset);
        w.u16(self.status_flags);
        // Capability flags, upper 16 bits.
        w.u16((self.capabilities >> 16) as u16);
        // Length of the full auth-plugin-data (CLIENT_PLUGIN_AUTH is set): the
        // 20-byte scramble plus its trailing NUL = 21.
        w.u8(21);
        w.fill(10); // reserved
                    // auth-plugin-data-part-2: remaining scramble bytes, NUL-terminated.
        w.bytes(&self.auth_plugin_data[8..]);
        w.u8(0);
        w.null_terminated(self.auth_plugin_name.as_bytes());
        w.into_bytes()
    }
}

/// The client's reply to the greeting (the `HandshakeResponse41` packet).
#[derive(Debug, Clone)]
pub struct HandshakeResponse41 {
    /// Capability flags the client supports (intersected with the server's).
    pub capabilities: u32,
    /// Largest packet the client is willing to send/receive.
    pub max_packet_size: u32,
    /// Client's chosen collation id.
    pub charset: u8,
    /// Login username.
    pub username: String,
    /// Raw authentication response (scrambled password); not verified yet.
    pub auth_response: Vec<u8>,
    /// Initial database, if `CLIENT_CONNECT_WITH_DB` was negotiated.
    pub database: Option<String>,
    /// Auth plugin the client used, if `CLIENT_PLUGIN_AUTH` was negotiated.
    pub auth_plugin: Option<String>,
}

impl HandshakeResponse41 {
    /// Parses the response from a packet payload.
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = PacketReader::new(payload);
        let capabilities = r.u32()?;
        let max_packet_size = r.u32()?;
        let charset = r.u8()?;
        r.skip(23)?; // reserved filler
        let username = utf8("username", r.null_terminated()?)?;

        let auth_response = if capabilities & cap::CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            r.lenenc_bytes()?.to_vec()
        } else if capabilities & cap::CLIENT_SECURE_CONNECTION != 0 {
            let len = r.u8()? as usize;
            r.bytes(len)?.to_vec()
        } else {
            r.null_terminated()?.to_vec()
        };

        let database = if capabilities & cap::CLIENT_CONNECT_WITH_DB != 0 && !r.is_empty() {
            Some(utf8("database", r.null_terminated()?)?)
        } else {
            None
        };

        let auth_plugin = if capabilities & cap::CLIENT_PLUGIN_AUTH != 0 && !r.is_empty() {
            Some(utf8("auth_plugin", r.null_terminated()?)?)
        } else {
            None
        };

        Ok(Self {
            capabilities,
            max_packet_size,
            charset,
            username,
            auth_response,
            database,
            auth_plugin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_encodes_to_expected_shape() {
        let hs = HandshakeV10::new("8.0.0-turso", 42, [1u8; 20]);
        let bytes = hs.encode();
        // protocol version
        assert_eq!(bytes[0], 10);
        // server version is NUL-terminated right after.
        let nul = bytes[1..].iter().position(|&b| b == 0).unwrap() + 1;
        assert_eq!(&bytes[1..nul], b"8.0.0-turso");
    }

    #[test]
    fn response_decodes_username_and_db() {
        // Build a minimal CLIENT_PROTOCOL_41 response by hand.
        let mut w = PacketWriter::new();
        let caps = cap::CLIENT_PROTOCOL_41
            | cap::CLIENT_SECURE_CONNECTION
            | cap::CLIENT_CONNECT_WITH_DB
            | cap::CLIENT_PLUGIN_AUTH;
        w.u32(caps);
        w.u32(0x0100_0000); // max packet
        w.u8(collation::UTF8MB4_GENERAL_CI);
        w.fill(23);
        w.null_terminated(b"root");
        w.u8(0); // empty auth response (secure connection: 1-byte length)
        w.null_terminated(b"mydb");
        w.null_terminated(b"mysql_native_password");
        let payload = w.into_bytes();

        let resp = HandshakeResponse41::decode(&payload).unwrap();
        assert_eq!(resp.username, "root");
        assert_eq!(resp.database.as_deref(), Some("mydb"));
        assert_eq!(resp.auth_plugin.as_deref(), Some("mysql_native_password"));
    }
}
