//! theodolite entry point: parse the config path, load and validate
//! the config (refusing to start on any violation), then serve MCP
//! over stdio until the transport closes.
//!
//! Usage: `theodolite --config <bridge.toml>`
//! (or set `THEODOLITE_CONFIG`).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use theodolite::config::Config;
use theodolite::session::SessionManager;

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
    let code = theodolite::mcp::serve_stdio(Arc::clone(&manager));
    std::process::exit(code);
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
