//! What a pane session answers the host with.

/// What the host should do after the plugin sees a keystroke.
/// "Forward to PTY" isn't a host concern — a plugin that wants the
/// key to reach the shell can call its own `ShelldClient::send_input_to`
/// from within `on_user_key` and then return `Swallow`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyHandling {
    /// Done — drop any further L2 processing.
    Swallow,
    /// End the session immediately; on_end fires with PluginRequested.
    EndSession,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// Plugin called `host.end()`.
    PluginRequested,
    /// User pressed Esc 3× in 5 s (L2 force-end).
    UserEscape,
    /// The pane's session left the registry.
    PaneClosed,
}
