// CARVE-OUT: holds the module list, the window and redraw constants, and main(), whose ~390-line launch sequence keeps it over the limit; shrink on every touch
//! marspot-shell — the outer process.
//!
//! Owns the NSWindow + NSApp event loop and a CAMetalLayer that
//! displays one IOSurface.  Spawns `marspot-core` as a child, hands it
//! the IOSurface ID via environment variables, and presents whatever
//! the child writes.
//!
//! Designed to be *boring* — anything substantive (parser, renderer,
//! input handling, layout) lives in the core process, behind a binary
//! we hot-swap during silent updates.  Keeping shell logic minimal
//! and dependencies thin means the shell almost never has to update,
//! which is what makes "no window flicker on upgrade" reachable.

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use marspot::{lx_debug, lx_error, lx_event, lx_info, lx_warn};
use marspot::app::{run_app, EventProxy, MarspotApp, MarspotAppCtx, WindowAttrs};
use marspot::input::{MarspotKeyEvent, Modifiers};
use marspot::shell_proto::{
    encode_file_drop, encode_focus, encode_hello, encode_mouse, encode_ping, encode_preedit, encode_scroll,
    encode_surface_attach, encode_surface_attach_window, encode_window_chrome, struct_to_mods_byte, MsgType,
    DEFAULT_CONTROL_FD, ENV_CONTROL_FD, ENV_SURFACE_HEIGHT, ENV_SURFACE_ID,
    ENV_SURFACE_ID_BACK, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH, PROTO_VERSION,
};

mod agentsvc;
mod apple_events;
mod banner;
mod bounded;
mod core_icons;
mod launch;
mod ls_records;
mod cli_socket;
mod control_reader;
mod dev_window;
mod frame_layer;
mod identity;
mod inherited;
mod instance;
mod observe;
mod orphans;
mod ownership;
mod pane_control;
mod pane_decls;
mod pane_status;
mod pane_tables;
mod plugins;
mod present;
mod present_shaders;
mod pump;
mod receipts;
mod reclaim;
mod resident;
mod self_exec;
#[cfg(feature = "latency-trace")]
mod screen_check;
mod shader_warm;
mod sup_log;
mod supervisor;
mod supervisor_profile;
mod surfaces;
mod theme_follow;
mod windows;
mod core_spawn;
mod core_update;
mod lifecycle;
mod dev_seams;
mod active_msg;
mod pane_sessions;
mod autorun_cli;
mod pane_cli;
mod shell_new;
mod window_frames;
mod app_events;
mod core_conn;
mod key_send;
mod shell_window;
mod pane_upkeep;
mod supervisor_tick;
mod supervisor_deadlines;
mod shell_app;
use banner::BannerKind;
use pane_sessions::{ActivePaneSession, SupervisorOpHost};
use control_reader::{control_reader_loop, ShellInbox};
use launch::*;
use plugins::host::ShellPluginHost;
use plugins::PluginRegistry;
use frame_layer::FrameLayers;
use surfaces::SurfacePair;
use core_conn::CoreConn;
use shell_app::ShellApp;
use shell_window::{resize_work, AttachedGeometry, ResizeWork, ShellWindow};
use supervisor::{BinaryTree, ProbeVerdict, SupervisorState};
use supervisor_deadlines::{
    forgive_stall, pong_overdue, restart_cooldown, CRASH_WINDOW, HELLO_TIMEOUT,
    MAX_CRASHES_IN_WINDOW, PING_INTERVAL, PONG_DEADLINE, SUPERVISOR_STALL_GAP,
};

const DEFAULT_TITLE: &str = "Marspot";
const DEFAULT_W_PT: f64 = 1200.0;
const DEFAULT_H_PT: f64 = 800.0;
/// Size of the window a launch with no saved layout opens — a first
/// run, or the launch after the user closed everything.  Smaller than
/// `DEFAULT_*` on purpose: one pane, so a window sized for a wall of
/// them would be mostly empty.
const FRESH_W_PT: f64 = 800.0;
const FRESH_H_PT: f64 = 600.0;
// How often the supervisor wakes with nothing to do, while there is
// anything to poll for (see `pump` for when there is not).
//
// Everything that has to happen promptly knocks: the core's reader
// thread wakes the loop when a frame or a reply arrives, the command
// socket after queueing a request, the plugin host after queueing an
// op.  What is left on this timer is polling, and the fastest poller
// is the pane sweep at one second (`pane_status::SWEEP_INTERVAL`); the
// plugins want two, the core healthcheck five, autorun thirty.  So the
// timer runs at the rate of its fastest consumer and no faster.
const REDRAW_INTERVAL_MS: u64 = 1_000;

fn main() {
    // the child that checks the registration: one event and out
    if std::env::args().nth(1).as_deref() == Some(apple_events::CHECK_ARG) {
        std::process::exit(apple_events::check_main(std::env::args().nth(2).as_deref()));
    }
    // before this image starts a child; the per-render subcommands below skip it
    if inherited::after_exec() { inherited::adopt() }
    // before anything else: AppKit must not register this image first
    apple_events::settle_after_exec();
    // before anything reads or writes the state directory
    if std::env::args().nth(1).as_deref() == Some(identity::LS_REFRESH_ARG) {
        identity::ls_refresh_main();
    }
    // one-time Caches → Application Support move; before logx and any path
    marspot::paths::migrate_legacy_state_root();
    // The claudecode status-line hook dispatches before everything
    // else — before logx, before the redirect into current/.  Claude
    // runs it on every render of every cc pane, so it stays a
    // read-stdin-write-one-file process: no log stream to open, no
    // supervisor bookkeeping, no window.  It needs the migration
    // above only so it computes the same state root the badge reads.
    match std::env::args().nth(1).as_deref() {
        Some("--cc-statusline") => std::process::exit(plugins::claudecode::statusline_ingest()),
        // a pane's agent reporting a submission, for the same reasons: stdin
        // in, one socket out, no log stream and no window (`receipts`)
        Some("--submit-receipt") => std::process::exit(receipts::report_from_stdin()),
        Some("--codex-locks") => std::process::exit(plugins::peerbus::codex_locks_main()),
        _ => {}
    }
    // `println!` to a closed pipe (`marspot-shell --status | head`) exits by the EPIPE
    // convention instead of panicking: SIG_DFL for SIGPIPE terminates the process.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    // component="shell" in the shared marspot.log; idempotent across self-exec restarts
    marspot::logx::init("shell");
    marspot_term::signals::log_fatal_senders("L1_SIGNAL");
    apple_events::report();
    inherited::report();

    // Rollback subcommands dispatch BEFORE the redirect: when
    // current/ is broken, exec'ing into it would eat the command.
    match std::env::args().nth(1).as_deref() {
        Some("--rollback-shell") => std::process::exit(cmd_rollback("shell")),
        Some("--rollback-core") => std::process::exit(cmd_rollback("core")),
        _ => {}
    }

    // One shell per state dir, settled before the redirect into `binaries/current/`: a
    // launch that loses never execs and never reaches the launch journal the crash-loop
    // checks count from.  Subcommands talk to the running shell and take no lock.
    if !std::env::args().nth(1).is_some_and(|a| a.starts_with('-')) {
        identity::settle();
        instance::hold_or_exit();
    }
    maybe_redirect_to_current_shell();
    if std::env::var_os("MARSPOT_SHELL_SELF_UPDATE").is_some() {
        // Take it out of the environment before anything else is
        // spawned.  Children inherit it, and every child that runs
        // this binary logged the line again: 38 of them in a day
        // against 6 real updates, one a minute, and the log read as if
        // L1 were re-execing itself all day (2026-09-28 — it sent this
        // author chasing a self-update that never happened).
        // SAFETY: single-threaded, before any thread is started.
        unsafe { std::env::remove_var("MARSPOT_SHELL_SELF_UPDATE") };
        sup_log::log("SHELL_SELF_UPDATE", "new shell exec'd from current/");
        // Self-fire SIGUSR1 so the main loop, once it lands on Idle
        // after the post-execv boot dance (active L2 spawned +
        // HELLO_ACK + first SurfaceReady), proactively applies any
        // remaining pending/marspot-core + pending/marspot-session.
        // Without this, the just-finished L1 swap leaves L2/L3
        // pending/ sitting there until install-local's 8 s fallback
        // retrigger or the next user-initiated focus-loss — that's
        // the "L1 flashes, then several seconds before content
        // updates and input wakes up" wait the user sees.
        // (install-local DID stage all three together as one
        // operation; the post-execv shell should treat them as one
        // operation too.)
        SIGUSR1_FLAG.store(true, Ordering::Release);
    }

    // CLI subcommands before AppKit or a window: --status / --trigger talk to a
    // running shell and must not take over the run loop themselves.
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => {
            print_version();
            return;
        }
        Some("--status") => {
            print_status();
            return;
        }
        Some("--trigger") => {
            let code = cmd_trigger();
            std::process::exit(code);
        }
        Some("--panes") => {
            match cli_socket::list_panes() {
                Ok(mut panes) => {
                    // Sorted by address, not by whatever order the
                    // registry happened to be in: a list you read is a
                    // list you scan.
                    panes.sort_by(|a, b| a.2.cmp(&b.2));
                    println!("{:>6}  {:<34}  directory", "id", "address");
                    for (sid, cwd, addr) in panes {
                        println!("{sid:>6}  {addr:<34}  {cwd}");
                    }
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--autorun") => {
            // `--autorun` alone lists; `--autorun <pane> [on|off]`
            // switches or asks about one.
            let target = args.get(2).cloned().unwrap_or_default();
            let on = match args.get(3).map(String::as_str) {
                Some("on") => Some(true),
                Some("off") => Some(false),
                None => None,
                Some(other) => {
                    eprintln!("unknown mode {other:?}; use on or off");
                    std::process::exit(2);
                }
            };
            match cli_socket::autorun(&target, on) {
                Ok(Ok(text)) => println!("{text}"),
                Ok(Err(msg)) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--read") => {
            // `marspot-shell --read <pane> [-n <lines>]` — the pane's
            // screen, plus that many lines of what scrolled off it.
            let target = args.get(2).cloned().unwrap_or_default();
            let extra = args
                .iter()
                .position(|a| a == "-n")
                .and_then(|i| args.get(i + 1))
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0);
            if target.is_empty() {
                eprintln!("usage: marspot-shell --read <pane> [-n <lines>]");
                std::process::exit(2);
            }
            match cli_socket::read_pane(&target, extra) {
                Ok(Ok(text)) => println!("{text}"),
                Ok(Err(msg)) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--send") => {
            // `marspot-shell --send <pane> <text…>` — the rest of argv
            // is the message, joined with spaces, so it can be written
            // without quoting in the common case.
            let target = args.get(2).cloned().unwrap_or_default();
            let text = args[3.min(args.len())..].join(" ");
            if target.is_empty() || text.is_empty() {
                eprintln!("usage: marspot-shell --send <pane> <text…>");
                std::process::exit(2);
            }
            match cli_socket::send_text(&target, &text) {
                Ok((true, msg)) => {
                    println!("{msg}");
                }
                Ok((false, msg)) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("cannot reach the running shell: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--help") | Some("-h") => {
            println!(
"marspot-shell — supervisor for the marspot terminal.\n\
\n\
Usage:\n\
  marspot-shell                Start the supervisor (window + core).\n\
  marspot-shell --version      Print version / git / build info.\n\
  marspot-shell --status       Summarise state from supervisor.log + live PIDs.\n\
  marspot-shell --panes        List panes: session id, name, working directory.\n\
  marspot-shell --autorun [<pane> [on|off]]\n\
                               Keep a rotation going by itself: when the session\n\
                               says it is done, send /clear then 继续 autorun;\n\
                               when it stalls on a server error, nudge it with\n\
                               继续, backing off each time.  No argument lists\n\
                               the panes it is on for.\n\
  marspot-shell --read <pane> [-n <lines>]\n\
                               Print what the pane says: its screen, plus that\n\
                               many lines of what has scrolled off it.\n\
  marspot-shell --send <pane> <text…>\n\
                               Type text into a pane and press Enter.  The pane\n\
                               is named by its working directory's last component\n\
                               (`spg`), its full name (`spg#3381`), its agent and\n\
                               name (`claude:spg`), or a session id.  A name that\n\
                               fits several panes is refused with their names.\n\
  marspot-shell --trigger      Apply a staged pending update on a running shell\n\
                               (sends SIGUSR1 to the supervisor process).\n\
  marspot-shell --rollback-shell   Quarantine current/marspot-shell, restore prev/.\n\
  marspot-shell --rollback-core    Quarantine current/marspot-core, restore prev/.\n\
                               Both run offline — restart Marspot afterwards.\n"
            );
            return;
        }
        // An unrecognised option is a mistake, not a launch.
        //
        // Falling through to the GUI start meant `marspot-shell
        // --typo` opened a window; harmless enough for a typo, and
        // not at all harmless for a Claude Code status-line hook,
        // which runs on every render — pointed at a binary too old to
        // know `--cc-statusline`, that is one window per render.
        // (Measured: an 0.7.82 shell handed the flag came up as a
        // full supervisor — shell.pid, control socket, session dir —
        // and had to be killed.)  A bare `marspot-shell` is still the
        // GUI launch, and positional arguments still fall through.
        Some(a) if a.starts_with('-') => {
            eprintln!("marspot-shell: unknown option `{a}` — see --help");
            std::process::exit(2);
        }
        _ => {}
    }

    if resident::asked() {
        ownership::hand_over_if_outside(true);
        resident::run();
    }
    ownership::hand_over_if_outside(false);
    ownership::announce();
    if resident::update_was_waiting() {
        SIGUSR1_FLAG.store(true, Ordering::Release);
    }
    identity::take_coalition(SIGUSR1_FLAG.load(Ordering::Acquire));

    // The default SIGPIPE above is for the CLI's stdout.  The GUI shell
    // writes to its core's socket, and a core that just died makes the
    // next write a SIGPIPE: with the default action that ended the whole
    // app silently before the supervisor could see the core was gone and
    // start another.  Ignored, the write fails with EPIPE instead.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    // Same reasoning as L2's loop: this thread runs the supervisor\n    // tick and every AppKit callback, and it was measured 100.8 s\n    // late during the 2026-08-11 incident.
    // Measured on an idle bench host with background load as the only
    // variable: at default QoS a frame's p99 went from 293 µs (idle)
    // to 6,082 µs (load 11), while the GPU's own account of the same
    // frame never moved (111 → 115 µs).  Nothing got heavier; this
    // thread simply stopped being scheduled.  Raising it to
    // USER_INTERACTIVE, same machine and load, seconds apart, both
    // orderings: p99 325 µs — the tail is gone.  See `marspot::qos`.
    let qos_ok = marspot::qos::raise_current_thread_to_user_interactive();
    lx_event!(
        "STARTUP",
        "marspot-shell starting",
        qos_user_interactive = qos_ok as u32,
        version_shell = env!("MARSPOT_VERSION_SHELL"),
        version_core = env!("MARSPOT_VERSION_CORE"),
        git = option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        build_ts = option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown"),
        pid = std::process::id()
    );
    sup_log::log(
        "STARTUP",
        &format!(
            "shell={} core={} git={} pid={}",
            env!("MARSPOT_VERSION_SHELL"),
            env!("MARSPOT_VERSION_CORE"),
            option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
            std::process::id()
        ),
    );
    // 2026-07-28 — crash-loop brake.  Runs before anything expensive
    // (updater thread, AppKit, core spawn): if this launch is part of
    // a rapid-restart burst, sleep first so the burst cannot saturate
    // the machine, and past the safe-mode threshold flag the whole
    // boot as spawn-nothing-fresh (`MARSPOT_SAFE_MODE`, read by this
    // process AND inherited by the core it spawns).
    match assess_launch_history(&read_launch_stamps(), now_unix_f64()) {
        LaunchVerdict::Normal => {}
        LaunchVerdict::Backoff(secs) => {
            lx_warn!(
                "shell.crash_loop.backoff",
                "rapid relaunches detected — sleeping before boot",
                sleep_s = secs
            );
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
        LaunchVerdict::SafeMode(secs) => {
            lx_event!(
                "SHELL_SAFE_MODE",
                "crash loop detected — this boot reattaches but spawns nothing fresh",
                sleep_s = secs
            );
            sup_log::log("SHELL_SAFE_MODE", "crash loop — safe-mode boot");
            // SAFETY: startup path in main(), before any thread is
            // spawned; children inherit it.
            unsafe { std::env::set_var("MARSPOT_SAFE_MODE", "1") };
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
    }
    install_sigusr1_handler();
    // Record our pid for this state dir so `--trigger` / dev-push /
    // install-local signal THIS shell, not whichever marspot-shell
    // `ps` lists first.  execv on self-update keeps the same pid, so
    // the file stays valid across a shell swap.
    write_shell_pid();
    // The silent-update poller: downloads new releases into
    // `binaries/pending/` in the background, for as long as this is kept.
    // The product version, not a layer's build number: the updater
    // compares against a release tag.
    let updater = marspot::updater::spawn(marspot::PRODUCT_VERSION.to_string());

    // Frame restore: the predecessor shell (self-update execv) hands
    // its exact window frame over via env so this process reopens in
    // place instead of jumping to the default rect.  Consumed here —
    // removed from our env so spawned children (core) don't carry it.
    let restore_frame = std::env::var("MARSPOT_RESTORE_FRAME").ok().and_then(|s| {
        // SAFETY: startup path in main(), before any thread is spawned.
        unsafe { std::env::remove_var("MARSPOT_RESTORE_FRAME") };
        supervisor::parse_restore_frame(&s)
    });
    // F3+6.1 — fall back to the persisted window-state.bin when the
    // env-var path (used by L1 self-execv to round-trip through itself)
    // didn't carry a frame.  Sane bounds: w/h > 50 pt guards against a
    // corrupt file shrinking the window to a sliver.
    let restore_frame = restore_frame.or_else(|| {
        // The boot window is entry 0 of the list; the rest are applied
        // as their windows are reopened.
        let w = marspot::state::read_windows()?.into_iter().next()?;
        if w.w > 50.0 && w.h > 50.0 { Some((w.x, w.y, w.w, w.h)) } else { None }
    });
    // With nothing to restore this is a first run — or the launch
    // after the user closed the last pane of the last window, which
    // deletes both saved files.  A modest window centred on the main
    // screen is the right thing to hand someone with no layout of
    // their own yet; `run_app` centres it because there is no frame.
    let (boot_w, boot_h) = match restore_frame {
        Some(_) => (DEFAULT_W_PT, DEFAULT_H_PT),
        None => (FRESH_W_PT, FRESH_H_PT),
    };
    let attrs = WindowAttrs {
        title: DEFAULT_TITLE.to_string(),
        width_logical: boot_w,
        height_logical: boot_h,
        frame_pt: restore_frame,
        // Terminal-black from the first paint: a core (L2) swap that
        // briefly uncovers the layer must show steady black, never a
        // lighter chrome flash.
        bg: theme_follow::at_launch(),
    };
    let proxy = EventProxy::new();
    let app = ShellApp::new(proxy.clone());
    // in a pool that ends with the app: what AppKit was handed before
    // its loop ran (the boot window, its view, the layers and surfaces
    // behind them) is otherwise kept for the life of the process, and
    // the process may now outlive the app by days
    objc2::rc::autoreleasepool(|_| run_app(app, proxy, attrs));
    // what only the open app needs: the release poll, the table of the
    // host's processes, the megabyte the quit read their arguments into
    drop(updater);
    observe::put_away();
    marspot::pidtree::release_room();
    if resident::staying() {
        resident::run_in_place();
    }
}
