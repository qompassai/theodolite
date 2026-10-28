//! theodolite entry point: parse the config path, load and validate
//! the config (refusing to start on any violation), then serve MCP
//! over stdio until the transport closes.
//!
//! Usage: `theodolite --config <bridge.toml>`
//! (or set `THEODOLITE_CONFIG`).
//!
//! As of 0.2.0 (WS2 Wave 4 of the MCP standardization program,
//! decision D-113) the MCP framing runs on rmcp, the official Rust
//! MCP SDK, and speaks the modern protocol era only (2026-07-28):
//! sessions open with `server/discover` and every request carries
//! its protocol version in `_meta` — there is no `initialize`
//! handshake. Legacy contact is refused at the stdio front door
//! (see `theodolite::guard`) with the typed `-32022`
//! unsupported-version error. The bridge's sessions, tools, and
//! on-disk behavior are unchanged.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::transport::async_rw::AsyncRwTransport;
use theodolite::config::Config;
use theodolite::guard;
use theodolite::server::TheodoliteServer;
use theodolite::session::SessionManager;
use tokio::sync::Mutex;

/// Duplex buffer between the era guard and the SDK service. Request
/// lines can reach the 2 MiB line bound; the SDK consumes
/// incrementally, so this only needs to cover response bursts.
const DUPLEX_BUFFER_BYTES: usize = 1024 * 1024;

fn main() {
    // Supervisor mode (SPEC section 3.6): the bridge re-executes
    // itself with --supervise to own one session's process tree.
    // This dispatch precedes all config handling; a supervisor
    // never reads bridge.toml.
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.first().map(String::as_str) == Some(theodolite::supervisor::SUPERVISE_FLAG) {
        std::process::exit(theodolite::supervisor::run_supervisor(&raw_args[1..]));
    }
    let config_path = match parse_config_path() {
        Ok(path) => path,
        Err(message) => {
            eprintln!("theodolite: {message}");
            std::process::exit(2);
        }
    };
    let config = match Config::load(&config_path) {
        Ok(config) => Arc::new(config),
        Err(error) => {
            eprintln!("theodolite: {}", error.to_json());
            std::process::exit(2);
        }
    };
    let manager = Arc::new(SessionManager::new(config));
    let code = serve(Arc::clone(&manager));
    // Transport EOF: every session is shut down under SPEC §3.6
    // discipline before returning, as the baseline did.
    manager.shutdown_all();
    std::process::exit(code);
}

/// Build the runtime and serve until stdin EOF: the guard owns
/// stdio, the SDK service sits behind an in-process duplex, and a
/// pump copies its responses back to stdout through the same lock
/// the guard writes with.
fn serve(manager: Arc<SessionManager>) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("theodolite: cannot start runtime: {error}");
            return 1;
        }
    };
    match runtime.block_on(run(manager)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("theodolite: serve loop ended: {error}");
            1
        }
    }
}

async fn run(manager: Arc<SessionManager>) -> std::io::Result<()> {
    let (client_side, server_side) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let (server_read, server_write) = tokio::io::split(server_side);
    let (client_read, client_write) = tokio::io::split(client_side);
    let stdout: guard::SharedStdout = Arc::new(Mutex::new(tokio::io::stdout()));

    let ordering = guard::Ordering::new();
    let handler = TheodoliteServer::new(manager);
    let serve = tokio::spawn(async move {
        let transport = AsyncRwTransport::new_server(server_read, server_write);
        match handler.serve(transport).await {
            Ok(running) => running
                .waiting()
                .await
                .map(|_| ())
                .map_err(|e| std::io::Error::other(e.to_string())),
            // The client closed the connection before selecting a
            // lifecycle (a probe, a spawn check, a client that only
            // ever wanted the refusal): the baseline treated bare
            // EOF as a clean end of session, and so do we.
            Err(rmcp::service::ServerInitializeError::ConnectionClosed(_)) => Ok(()),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    });
    let mut pump = tokio::spawn(guard::pump(
        client_read,
        Arc::clone(&stdout),
        Arc::clone(&ordering),
    ));

    // When stdin reaches EOF the guard shuts the duplex write side
    // down, which ends the SDK service; the pump ends when the
    // service's transport drops. The pump wait is bounded as a
    // backstop: a response still in flight when the service ends
    // gets its chance to drain, but a wedged pump cannot hang exit.
    let guard_result = guard::run(tokio::io::stdin(), client_write, stdout, ordering).await;
    let serve_result = match serve.await {
        Ok(result) => result,
        Err(e) => Err(std::io::Error::other(e.to_string())),
    };
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut pump)
        .await
        .is_err()
    {
        pump.abort();
    }
    guard_result.and(serve_result)
}

fn parse_config_path() -> Result<PathBuf, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--config") => {
            let value = args
                .get(1)
                .ok_or_else(|| "--config needs a path value".to_string())?;
            Ok(PathBuf::from(value))
        }
        Some("--help") | Some("-h") => Err("usage: theodolite --config <bridge.toml>".to_string()),
        Some(other) => Err(format!("unknown argument {other:?}")),
        None => std::env::var("THEODOLITE_CONFIG")
            .map(PathBuf::from)
            .map_err(|_| {
                "no config: pass --config <bridge.toml> or set THEODOLITE_CONFIG".to_string()
            }),
    }
}
