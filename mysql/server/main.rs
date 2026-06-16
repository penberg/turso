// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! A proof-of-concept MySQL-protocol front-end for Turso.
//!
//! The server is the only place an async runtime lives: tokio owns the accept
//! loop and the listening socket. The wire protocol itself comes entirely from
//! the sans-IO [`turso_mysql_protocol`] crate, and query execution goes through
//! the synchronous [`turso_core`] engine.
//!
//! Because `turso_core` is a synchronous, embedded engine, each accepted
//! connection is handed to a dedicated blocking task ([`tokio::task::spawn_blocking`])
//! that drives the per-connection state machine with ordinary blocking socket
//! I/O. Tokio supervises the listener and the pool of connection workers; the
//! database work never blocks an async reactor thread.

mod connection;
mod session;
mod show;
mod types;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{error, info};
use turso_core::{Database, DatabaseOpts, OpenFlags};

/// Command-line options for the server.
#[derive(Parser, Debug)]
#[command(
    name = "turso-mysql-server",
    about = "MySQL-protocol front-end for Turso"
)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:3306")]
    listen: String,

    /// Database file to open, or `:memory:` for an ephemeral database.
    #[arg(long, default_value = ":memory:")]
    database: String,
}

/// Shared server state handed to every connection.
pub struct Server {
    /// The opened Turso database; each connection calls `connect()` on it.
    pub db: Arc<Database>,
    /// Monotonic source of per-connection ids advertised in the handshake.
    next_connection_id: AtomicU32,
}

impl Server {
    fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            next_connection_id: AtomicU32::new(1),
        }
    }

    fn next_connection_id(&self) -> u32 {
        self.next_connection_id.fetch_add(1, Ordering::Relaxed)
    }
}

fn open_database(path: &str) -> anyhow::Result<Arc<Database>> {
    let opts = DatabaseOpts::new().with_attach(true);
    let (_io, db) = Database::open_new(path, None::<&str>, OpenFlags::default(), opts, None)
        .with_context(|| format!("opening database {path}"))?;
    // Materialize the engine's `sqlite_sequence` bookkeeping table — which the
    // engine creates only once some `AUTOINCREMENT` table exists — by creating
    // and immediately dropping a throwaway one. `sqlite_sequence` persists after
    // the drop (empty), so the `information_schema.TABLES` emulation can always
    // reference it for the `AUTO_INCREMENT` column, even in a database that has
    // no auto-increment tables of its own. Best-effort: if it fails, only that
    // column degrades (to an error) for such a database.
    if let Ok(conn) = db.connect() {
        let _ = conn.execute(
            "CREATE TABLE IF NOT EXISTS __mysql_autoincrement_seq_init \
             (id INTEGER PRIMARY KEY AUTOINCREMENT)",
        );
        let _ = conn.execute("DROP TABLE IF EXISTS __mysql_autoincrement_seq_init");
    }
    Ok(db)
}

fn main() -> anyhow::Result<()> {
    // Each connection is handled on a blocking-pool thread that recurses through
    // the parser and the synchronous engine, so a deeply nested expression
    // recurses deeply. The default ~2 MiB thread stack overflows (and aborts the
    // whole process) on only a few dozen levels of nesting, so give the runtime's
    // threads a generous stack — applied to the worker *and* blocking-pool
    // threads — to tolerate the nesting depths a real query may reach.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(64 * 1024 * 1024)
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let db = open_database(&args.database)?;
    let server = Arc::new(Server::new(db));

    let listener = TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("binding to {}", args.listen))?;
    info!(addr = %args.listen, database = %args.database, "turso-mysql-server listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!(error = %e, "accept failed");
                continue;
            }
        };
        let server = server.clone();
        let connection_id = server.next_connection_id();

        // Hand the socket to a dedicated blocking worker. `into_std` gives us a
        // blocking std socket; the protocol state machine then runs with plain
        // synchronous I/O alongside the synchronous database engine.
        let std_stream = match stream.into_std() {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to convert stream");
                continue;
            }
        };
        if let Err(e) = std_stream.set_nonblocking(false) {
            error!(error = %e, "failed to set blocking mode");
            continue;
        }

        info!(%peer, connection_id, "client connected");
        tokio::task::spawn_blocking(move || {
            if let Err(e) = connection::handle(server, std_stream, connection_id) {
                info!(connection_id, error = %e, "connection closed");
            } else {
                info!(connection_id, "connection closed");
            }
        });
    }
}
