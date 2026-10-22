//! marspot-peer — messaging between agent sessions over the sockets
//! they already listen on (see the crate manifest for scope).
//!
//! - `frame`, `envelope`, `registry`: the Claude Code cross-session
//!   protocol — inbox socket frames, the message envelope, the session
//!   registry other sessions read to find each other.
//! - `ws`, `rpc`: a client for the Codex app-server control socket.
//! - `peers`, `mcp`, `notice`: the sending side, for the command line
//!   and for Codex as an MCP server.
//! - `relay`: the process that stands for a Codex session on the inbox
//!   sockets.

pub mod envelope;
pub mod frame;
pub mod json;
pub mod mcp;
pub mod notice;
pub mod peers;
pub mod proc;
pub mod rand;
pub mod registry;
pub mod relay;
pub mod rpc;
mod sha1;
pub mod ws;
