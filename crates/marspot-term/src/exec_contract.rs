//! What crosses each exec of a marspot image, and nothing else.
//!
//! An exec hands the next image every descriptor without `FD_CLOEXEC`
//! and the whole environment.  Each kind of exec below lists the
//! descriptors and the `MARSPOT_*` variables it carries; before the
//! exec every other descriptor is marked close-on-exec and every other
//! `MARSPOT_*` variable removed, and the image that starts checks what
//! it was handed.  The kind travels in [`KIND_ENV`], which the new image
//! consumes; an image started any other way checks nothing.
//!
//! Variables configured for the whole process tree (the state root, the
//! dev seams, the binary overrides) are in [`AMBIENT`] and cross every
//! exec.  A variable in neither list is removed: one that should cross
//! has to be listed here first.

use std::os::fd::RawFd;

/// Names the kind of exec that started this image.
pub const KIND_ENV: &str = "MARSPOT_EXEC_KIND";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecKind {
    /// L1 replacing itself with a staged update
    ShellUpdate,
    /// L1 replacing itself for a lost Apple event registration
    ShellReplace,
    /// L1 started from the bundle moving into `binaries/current/`
    ShellRedirect,
    /// L1 replacing itself to be listed by LaunchServices
    ShellIdentity,
    /// L1 put away for its sessions, or coming back as the app
    ShellResident,
    /// L1 starting L2
    CoreSpawn,
    /// L2 starting a pane's L3
    SessionSpawn,
    /// L3 replacing itself with a staged update
    SessionUpdate,
}

/// A descriptor that crosses an exec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carried {
    /// at this number
    At(RawFd),
    /// at the number this variable holds, when it is set
    NumberIn(&'static str),
    /// one the exec site hands over by name, at a number it passes
    Handed(&'static str),
}

/// What every image of the shell may hand the next one: the marks of
/// why it replaced itself, the lock and what the window looked like.
const SHELL_IMAGE_VARS: &[&str] = &[
    "MARSPOT_SHELL_SELF_UPDATE",
    "MARSPOT_RESTORE_FRAME",
    "MARSPOT_IMAGE_REPLACED",
    "MARSPOT_AE_RETRIES",
    "MARSPOT_LS_PASS",
    "MARSPOT_SHELL_LOCK_FD",
    "MARSPOT_SHELL_LOOK",
    "MARSPOT_UPDATE_WAITING",
    "MARSPOT_DOCK_LEAVE_AT",
    crate::owner::RESIDENT_ENV,
    crate::owner::HEIR_ENV,
];

const CORE_SPAWN_VARS: &[&str] = &[
    crate::shell_proto::ENV_SURFACE_ID,
    crate::shell_proto::ENV_SURFACE_ID_BACK,
    crate::shell_proto::ENV_SURFACE_WIDTH,
    crate::shell_proto::ENV_SURFACE_HEIGHT,
    crate::shell_proto::ENV_SURFACE_SCALE,
    crate::shell_proto::ENV_CONTROL_FD,
    "MARSPOT_ICONS_FROM_SHELL",
];

const SESSION_SPAWN_VARS: &[&str] = &[
    crate::grid_shm::ENV_SHM_FD,
    "MARSPOT_SESSION_ID",
    "MARSPOT_SHM_NAME",
    "MARSPOT_INITIAL_CWD",
    "MARSPOT_SHADOW_OF",
    "MARSPOT_SHADOW_IDLE_S",
];

/// The session's own variables stay; the shadow ones were read and
/// removed at its start.
const SESSION_UPDATE_VARS: &[&str] = &[
    "MARSPOT_L3_HANDOFF_MANIFEST",
    crate::grid_shm::ENV_SHM_FD,
    "MARSPOT_SESSION_ID",
    "MARSPOT_SHM_NAME",
    "MARSPOT_INITIAL_CWD",
];

/// Set for the whole process tree, by a person, a test or the shell's
/// launch, and read wherever they are needed.
pub const AMBIENT: &[&str] = &[
    "MARSPOT_STATE_DIR", "MARSPOT_LOG_DIR", "MARSPOT_LOG", "MARSPOT_LOG_GC", "MARSPOT_LOG_GC_AGE_D",
    "MARSPOT_LOG_KEEP", "MARSPOT_LOG_MAX_MB", "MARSPOT_LOG_SETTLE_MS", "MARSPOT_NO_REDIRECT", "MARSPOT_REDIRECT",
    "MARSPOT_BUNDLE_DIR", "MARSPOT_SAFE_MODE", "MARSPOT_CORE_BIN", "MARSPOT_SESSION_BIN", "MARSPOT_PEER_BIN",
    "MARSPOT_OLD_SESSION_BIN", "MARSPOT_SESSION_CAP", "MARSPOT_SESSION_ENTRY_CHECK_MS", "MARSPOT_BYTELOG",
    "MARSPOT_SCROLLBACK_HOT_CAP_MB", "MARSPOT_SHELL", "MARSPOT_AMBIGUOUS_WIDE", "MARSPOT_SCROLL_FACTOR",
    "MARSPOT_SCROLL_INVERT", "MARSPOT_PASTEBOARD", "MARSPOT_KEY_SCRIPT", "MARSPOT_LATENCY_TRACE", "MARSPOT_TRACE_ESC",
    "MARSPOT_QOS", "MARSPOT_SUPERVISOR_PROFILE", "MARSPOT_UPDATE_FEED", "MARSPOT_MANUAL_UPDATE_ONLY",
    "MARSPOT_NO_WAKE_PREFETCH", "MARSPOT_PTY_BRIDGE_SPIN", "MARSPOT_PTY_INTEGRATION", "MARSPOT_LINK_SNAP",
    "MARSPOT_GBT_PATH", "MARSPOT_FONT_SNAPSHOT", "MARSPOT_SNAPSHOT_OUT", "MARSPOT_SHOT_COLS", "MARSPOT_SHOT_ROWS",
    "MARSPOT_SHOT_SCALE", "MARSPOT_PROBE", "MARSPOT_PROBE_DIE_AT", "MARSPOT_KEEP_PROBE", "MARSPOT_CATALOGUE_TSV",
    "MARSPOT_CC_IDLE_HIBERNATE_S", "MARSPOT_CC_RESUME_WORD", "MARSPOT_USAGE_PASS_SECS", "MARSPOT_ZOMBIE_HISTORY",
    "MARSPOT_SUBMIT_TOKEN", "MARSPOT_QOS_CHILD", "MARSPOT_FRONT_CHILD", "MARSPOT_PANE_CODE_CHILD",
    "MARSPOT_ZOMBIE_CHILD", "MARSPOT_LOCK_ROLE", "MARSPOT_LOCK_PATH",
];

/// Families of [`AMBIENT`] variables: the dev seams and the test hooks.
pub const AMBIENT_PREFIXES: &[&str] = &["MARSPOT_DEV_", "MARSPOT_TEST_"];

impl ExecKind {
    pub const ALL: [ExecKind; 8] = [
        ExecKind::ShellUpdate,
        ExecKind::ShellReplace,
        ExecKind::ShellRedirect,
        ExecKind::ShellIdentity,
        ExecKind::ShellResident,
        ExecKind::CoreSpawn,
        ExecKind::SessionSpawn,
        ExecKind::SessionUpdate,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ExecKind::ShellUpdate => "shell-update",
            ExecKind::ShellReplace => "shell-replace",
            ExecKind::ShellRedirect => "shell-redirect",
            ExecKind::ShellIdentity => "shell-identity",
            ExecKind::ShellResident => "shell-resident",
            ExecKind::CoreSpawn => "core-spawn",
            ExecKind::SessionSpawn => "session-spawn",
            ExecKind::SessionUpdate => "session-update",
        }
    }

    pub fn parse(name: &str) -> Option<ExecKind> {
        ExecKind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Whether the exec replaces the image of the process that makes
    /// it, rather than starting a child.
    pub fn replaces_itself(self) -> bool {
        !matches!(self, ExecKind::CoreSpawn | ExecKind::SessionSpawn)
    }

    /// The value of [`KIND_ENV`] for this exec, made by this process: the
    /// new image takes it as its own only from its own pid or its parent's,
    /// so a value an older build left in the environment is ignored.
    pub fn mark(self) -> String {
        format!("{}:{}", self.name(), std::process::id())
    }

    pub fn fds(self) -> &'static [Carried] {
        match self {
            ExecKind::ShellUpdate | ExecKind::ShellReplace | ExecKind::ShellRedirect => &[],
            ExecKind::ShellIdentity | ExecKind::ShellResident => &[Carried::NumberIn("MARSPOT_SHELL_LOCK_FD")],
            ExecKind::CoreSpawn => &[Carried::At(crate::shell_proto::DEFAULT_CONTROL_FD)],
            ExecKind::SessionSpawn => &[Carried::At(4)],
            ExecKind::SessionUpdate => &[
                Carried::NumberIn(crate::grid_shm::ENV_SHM_FD),
                Carried::Handed("pty master"),
                Carried::Handed("listener"),
                Carried::Handed("control stream"),
                Carried::Handed("session dir lock"),
            ],
        }
    }

    pub fn vars(self) -> &'static [&'static str] {
        match self {
            ExecKind::ShellUpdate
            | ExecKind::ShellReplace
            | ExecKind::ShellRedirect
            | ExecKind::ShellIdentity
            | ExecKind::ShellResident => SHELL_IMAGE_VARS,
            ExecKind::CoreSpawn => CORE_SPAWN_VARS,
            ExecKind::SessionSpawn => SESSION_SPAWN_VARS,
            ExecKind::SessionUpdate => SESSION_UPDATE_VARS,
        }
    }

    /// Whether variable `name` may cross this exec.
    pub fn carries(self, name: &str) -> bool {
        !name.starts_with("MARSPOT_")
            || name == KIND_ENV
            || self.vars().contains(&name)
            || AMBIENT.contains(&name)
            || AMBIENT_PREFIXES.iter().any(|p| name.starts_with(p))
    }

    /// The descriptors this exec carries, numbers read through `env`;
    /// `handed` are the [`Carried::Handed`] ones, at most one each.
    pub fn carried(self, env: impl Fn(&str) -> Option<String>, handed: &[RawFd]) -> Vec<RawFd> {
        let named = self.fds().iter().filter(|c| matches!(c, Carried::Handed(_))).count();
        assert!(handed.len() <= named, "{} hands over {named} descriptors, not {}", self.name(), handed.len());
        let mut fds: Vec<RawFd> = handed.iter().copied().filter(|&fd| fd >= 0).collect();
        for c in self.fds() {
            match *c {
                Carried::At(fd) => fds.push(fd),
                Carried::NumberIn(var) => fds.extend(env(var).and_then(|v| v.trim().parse::<RawFd>().ok())),
                Carried::Handed(_) => {}
            }
        }
        fds
    }
}

#[path = "exec_contract_sweep.rs"]
mod sweep;
pub use sweep::{open_without_cloexec, prepare_command_exec, prepare_own_exec, sweep_env, sweep_fds};
use sweep::{own_marspot_vars, process_env};

#[path = "exec_contract_arrival.rs"]
mod arrival;
pub use arrival::{Arrival, arrive};

#[cfg(test)]
#[path = "exec_contract_tests.rs"]
mod tests;
