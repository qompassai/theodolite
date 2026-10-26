//! theodolite — a headless, deterministic LSP bridge MCP server.
//!
//! Theodolite answers one question: "what do this workspace's
//! pinned language servers say, headlessly, deterministically, with
//! no editor in the loop?" It owns its language-server children end
//! to end, pins them by absolute path and exact version, shapes
//! every result into a canonical, byte-bounded form, and registers
//! a read-only tool surface in v1 (SPEC: `~/workspace/
//! lsp-mcp-study-20261025/SPEC.md`, IDEA-0092).
//!
//! It is the complement of periscope (IDEA-0013), which brokers
//! through the human's live Neovim; theodolite never touches an
//! editor. No network, no filesystem watching, no shell-out beyond
//! spawning the configured server binaries by absolute path.

#![forbid(unsafe_code)]

pub mod config;
pub mod errors;
pub mod lsp;
pub mod mcp;
pub mod session;
pub mod shaping;
pub mod tools;

/// Toolchain pin, mirroring this repo's `rust-toolchain.toml`;
/// recorded in every session transcript (SPEC §4.2).
pub const TOOLCHAIN_PIN: &str = "nightly-2026-09-25";
