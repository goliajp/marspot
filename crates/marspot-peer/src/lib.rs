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
//! - `panes`, `resolve`, `claude_hook`: marspot's pane names, which
//!   pane a recipient string means, and the Claude Code hooks that let
//!   a Claude session address peers by them.
//! - `spool`, `gemini_hook`, `kimi_hook`: the file a Gemini or Kimi
//!   session's mail waits in, and the hooks that hand it over at a
//!   turn boundary.

pub mod claude_hook;
pub mod envelope;
pub mod frame;
pub mod gemini_hook;
pub mod json;
pub mod kimi_hook;
pub mod mcp;
pub mod notice;
pub mod panes;
pub mod peers;
pub mod proc;
pub mod rand;
pub mod registry;
pub mod relay;
pub mod resolve;
pub mod rpc;
mod sha1;
pub mod spool;
pub mod ws;
