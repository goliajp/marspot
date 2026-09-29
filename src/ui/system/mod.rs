//! marspot UI `system` — platform-specific chrome.
//!
//! Today there's only `macos` (marspot is macOS-only).  If/when we
//! port, this is where `windows/` / `linux/` would land — each
//! mirroring the same surface (traffic lights / title bar / etc.)
//! with its own native palette.

pub mod macos;
