//! Forge as an MCP server (ARCHITECTURE.md §18).
//!
//! `forge-gui mcp-server` speaks MCP over stdio (JSON-RPC 2.0, one message
//! per line) to whichever agent spawned it and forwards the tools that need
//! live editor state to the GUI through a Unix socket ([`bridge`]). The
//! socket path travels in `FORGE_GUI_SOCKET`, the agent tab that owns the
//! session in `FORGE_AGENT_TAB`, and the session directory in
//! `FORGE_WORKSPACE`; without a socket the server still answers with what
//! the filesystem and `git` can tell.
//!
//! Tool names use `_` instead of the `/` of the architecture document
//! because MCP restricts names to `[A-Za-z0-9_-]`.

pub mod bridge;
pub mod server;

pub use bridge::{BridgeRequest, BridgeResponse};
pub use server::{ServerConfig, serve_stdio};

/// MCP protocol revision this server advertises.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
