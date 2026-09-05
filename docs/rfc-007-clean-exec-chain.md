# RFC-007 — a clean exec chain for the user's shell

Status: implementing (2026-09-05)

## The defect

Every process under a user-installed `.app` carries
`com.apple.provenance`, and it is inherited: parent → child, and also
executable-file → process.  A file created anywhere under that tree is
stamped too.  So a binary the user builds inside marspot is stamped,
and its **first execution** takes a full Gatekeeper scan — a network
round trip to Apple's notarisation service (3 s timeout, retried), an
XProtect analysis pass, and a `syspolicyd` that serialises all of it.

Measured on this host, same command, same minute:

```text
  launchd-clean chain   0.131 / 0.025 / 0.036 / 0.158 / 0.060 s   (artefacts unstamped)
  marspot's chain       3.630 / 0.340 / 4.135 s
```

The reported case on another project's test tier was 30.55 s, and one
harness took 268.50 s.  It is the same mechanism throughout; what
varies is how busy `syspolicyd` is when the queue is entered.

This is not marspot's bug — iTerm2 pays it too, and its
`kTCCServiceDeveloperTool` grant does not help (verified: no such
decision appears anywhere in the exec log; that grant permits running
non-conforming code, it does not skip the scan).  `Terminal.app` is
exempt only because it is a `/System` platform binary.

But it is marspot's problem: the user's build-test loop is inside
marspot, and the cost does not look like a terminal defect.  It looks
like "the tests got slower".

## What does not work (all verified, not reasoned)

| approach | result |
|---|---|
| strip the xattr | `xattr -d` returns 0 and silently does nothing; not removable, not even on a copy |
| unstamped binary in an unstamped bundle, launched cleanly | process still stamped — `.app` membership alone is enough |
| `posix_spawn` + `responsibility_spawnattrs_setdisclaim` | `rc=0`, child still stamped; it governs TCC responsibility only |
| a `DeveloperTool` TCC grant (marspot already has four) | never consulted on this path |
| system-side exemption | `SystemPolicyConfiguration/Default.plist` carries a kext allow-list and nothing else |

The one thing that does work is not being a descendant of the app: a
process `launchd` starts, executing a file that is itself unstamped,
is clean — and so is everything it forks and every file they write.

## Design

Only the **shell's chain root** has to be clean.  L3 already owns its
PTY, binds its own UDS, and is reached through the registry (RFC-003),
so *how it starts* is not load-bearing for anything L2 does.  Its PPID
is already 1.

1. **A clean copy of the L3 binary.**  `binaries/current/marspot-session`
   is stamped because it was installed by a stamped process, and the
   stamp cannot be removed.  A short-lived `launchd` job copies it to
   `binaries/clean/marspot-session`; the copy is written by a clean
   process, so it is unstamped.  Refreshed when the source's cdhash
   changes, which makes it self-healing across silent updates — no
   change to `install-local.sh` is required.

2. **L3 is started by `launchd`.**  A per-session job replaces
   `Command::new(&session_bin)`.  Its environment carries what the
   spawn path passes today; the job is booted out when the session
   ends.

3. **L3 takes its shm by name.**  Today it adopts an inherited fd
   (`MARSPOT_SHM_FD`), which a `launchd` job cannot hand it.
   `grid_shm::open_region` already exists; L3 gains a by-name branch
   and keeps the inherited-fd branch first, so the change is inert for
   every caller that still passes an fd (`mcli`, tests, an L2 that has
   not been updated yet).

The process tree keeps its shape — L3 → shell, L3 reparented to 1 —
so pidtree, the session cap, and crash isolation see what they see now.

## Rejected

- **A resident clean spawner that receives the PTY fd over UDS.**  That
  reinstates L4 shelld in all but name, and it changes the tree: the
  shell would be the spawner's child, not L3's, which the pane's
  process monitoring reads.
- **Cleaning at install time only.**  It would work, but it fails
  silently the moment anyone installs by another path, and leaves no
  way to notice.  The cdhash check in (1) is self-healing instead.
