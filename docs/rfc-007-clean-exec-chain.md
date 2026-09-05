# RFC-007 — the first-execution tax: what it is, and what does not fix it

Status: **solved** (2026-09-05).  The cause is the RESPONSIBLE process,
and the fix is one line in L1 plus a bundle-id TCC grant.  The rest of
this document is the eliminated search space — kept so nobody re-runs
these experiments.

## The answer

`exec` replaces a process's image **and its responsible identity**.  L1
redirected into `Application Support/binaries/current/marspot-shell` at
startup, so the whole tree's responsible process stopped being
`Marspot.app` and became a bare path.  A bare path cannot hold a
bundle-id TCC grant, so the **Developer Tools** exemption — which is
what lets a terminal run code its user just compiled without a
Gatekeeper scan — no longer matched.  That is also why marspot's four
`DeveloperTool` rows are `client_type=1` (path) while `Terminal` and
`iTerm2` are `client_type=0` (bundle id), and why `tccd` was never even
consulted on this chain.

Same script, same minute, one instrument:

```text
  responsible = Marspot.app          0.00 s   performScan 0
  responsible = current/ bare path   0.30 s   performScan every exec
```

Two conditions, both required:

1. `Marspot.app` holds a bundle-id Developer Tools grant
   (System Settings → Privacy & Security → Developer Tools).
2. L1 does not `exec` out of the bundle — shipped in shell 0.7.125,
   `MARSPOT_REDIRECT=1` restores the old path.

L2 and L3 are unaffected and keep updating live: a forked child
inherits the responsible process, so they cost nothing by living
outside the bundle.  iTerm2 does exactly this with its own
`iTermServer` helper under Application Support.

The price is that a new L1 lands on the next cold launch: a bundle
binary cannot be overwritten while its own process runs (AMFI kills it
when the on-disk CDHash stops matching).  L1 moves rarely.

## The observation

A binary built inside marspot pays a Gatekeeper scan on its **first
execution**: `GK performScan`, a round trip to Apple's notarisation
service (`transaction_duration_ms=182` measured, 3 s timeout with
retries), an XProtect pass, all serialised through one `syspolicyd`.
Idle it costs ~0.3 s; under load ~1.3 s; another project's test tier
reported 30 s, and one harness 268 s — the same event at different
queue depths.  A developer's build can never be notarised, so the trip
is paid and discarded every time.

It reads as "the tests got slower", not as a terminal defect.

## What actually separates fast from slow

Measured with one instrument (`/usr/bin/time -p`, 3–5 trials each,
`syspolicyd` CPU delta and `performScan` counts alongside):

| chain | first exec | performScan | chain xattr |
|---|---|---|---|
| `Terminal.app` (platform binary) | **0.00 s** | **0** | none |
| `iTerm.app` (**notarised**, stapled) | **0.00 s** | **0** | none |
| marspot | 0.30–0.49 s | every time | provenance |
| `launchd` job | 0.39–0.48 s | every time | none |
| `sshd` | 0.30–0.36 s | every time | none |

The two fast rows do not scan **at all**.  Everything else scans every
time, whatever its ancestry.

## Ruled out — each with its own control

| hypothesis | control | verdict |
|---|---|---|
| `com.apple.provenance` on the artefact | unmarked artefact via `launchd`/`sshd` chains | **no** — unmarked and still 0.3 s |
| provenance on the process chain | clean chain vs marspot chain, idle AND under load | **no** — identical both ways |
| `kTCCServiceDeveloperTool` grant | marspot holds four (path-type, csreq verified matching) | **no** — `tccd` is never consulted on this path |
| Hardened Runtime | same app signed with/without `--options runtime` | **no** — 0.46 vs 0.47 s |
| `cs.*` entitlements (jit, disable-library-validation, unsigned-exec-memory, dyld-env) | same app with all four | **no** — 0.28 s, unchanged |
| install location | identical app from `~/.local` and `/Applications` | **no** — identical |
| `posix_spawn` disclaim | `responsibility_spawnattrs_setdisclaim` | **no** — `rc=0`, child still marked |
| stripping the attribute | `xattr -d`, on a copy, under sudo | **not possible** — returns 0, silently does nothing |

## What RFC-007 built, and why it was reverted

It booted L3 as a per-session `launchd` job off an unmarked copy of its
binary, on the theory that provenance drove the scan.  It worked as
designed — the L3 process and everything it wrote were unmarked, end to
end — and **bought nothing**: the clean chain scans exactly as often and
costs exactly as much as the marked one.  The early 60x and 11x figures
that motivated it came from comparing two different measuring methods
(two `python3` processes reading `perf_counter` versus
`subprocess.run`), and from single runs.  With one instrument and
repeated trials the difference disappears.

Reverted rather than kept: an unused `launchd` dependency on the pane
spawn path is failure surface for no gain.  It had already leaked 26
jobs in its first hour (`launchd` keeps exited jobs until booted out),
which is the kind of cost it would keep charging.

## Notarisation was tested, and is not it

The last standing hypothesis was that `syspolicyd` exempts execs under
a notarised app.  A test bundle was signed, submitted, **Accepted**,
and stapled.  Its chain measured 0.28 / 0.29 / 0.29 s with
`performScan` running every time — identical to the unnotarised
build.  Notarisation changes nothing here.

`bin/notarize.sh` is kept anyway: signing and notarising a distributed
app is worth doing on its own merits, and it is now a one-liner.

`examples/exec_tax_probe.rs` measures it inside a real marspot shell.
Read the timing together with `syspolicyd`'s CPU delta or its
`performScan` count; the number alone moves 10x with queue depth and
has already misled this investigation once.
