# CHANGELOG

Per-layer history.  Until 2026-06-23 this lived inline in
`version-vector.toml` as one ever-growing comment per layer (≈25 KB
single lines, unreadable in `git diff`).  Migrated here verbatim;
the toml now only carries `name = "X.Y.Z"  # short subject` and a
pointer back to this file.

Going forward: bump version in `version-vector.toml`, append one
entry to the relevant layer below.  Keep entries reverse-chronological
(newest on top).  Full code-level detail belongs in the commit message
— this file is a release narrative, not a paste of every diff.

Older entries below the explicit-version block keep their original
F-tag prefix (`F3+12.6`, `F2+2a`, …) as the heading; map an F-tag to
its commit via `git log --grep 'F3+12.6'` etc.

**2026-07-29 — second migration.**  The 2026-06-23 move fixed the
symptom, not the habit: over the following five weeks 110 versions were
again appended inline to `version-vector.toml`, growing those three
lines back to 6.0 / 12.9 / 3.8 KB — most of the way to the ~25 KB that
made the first migration necessary, and unreadable in `git diff` for
exactly the same reason.  Those 110 entries were moved here verbatim
(L1 0.7.1-0.7.27, L2 0.12.1-0.12.61, L3 0.11.5-0.11.28) and the toml
lines are back to `name = "X.Y.Z"  # one-line subject`.  If you find
yourself appending a second `前 X.Y.Z …` clause to a toml line, that is
the regression — the entry belongs in this file.

## L1  marspot-shell

Current: **0.7.139**

### 0.7.139

Both agent plugins declare the pane they are driving, every tick.

`set_pane_agent_tui` — see L2 0.12.186 for what it fixes.  Re-issued
on the heartbeat rather than on change, because a declaration sent
once never reaches a core that was swapped after it.

### 0.7.138

The codex pane stops claiming the wheel, and stops claiming `<u>`.

**The wheel.**  It was routed into codex's transcript key because a
codex pane had no history of its own (see L3 0.11.79).  It has one
now, so the wheel does what it does everywhere else — which is what
claudecode already did, and what this was asked to match.  Ctrl-T
remains codex's own key for anyone who wants its transcript.

**`<u>`.**  Declared per-pane because codex printed its own `<u>`
markup as text.  Measured across 102 MB of real codex traffic since:
`<u>` appears **twice**, `</u>` eight times — not even paired — while
`<h2>` and `<p>` appear 529 times, because the model prints HTML
documents into the pane.  Eating a real document's tags is now 250x
more likely than fixing codex's own.  `render_u_tags` in settings.toml
still turns it on for anyone who wants it everywhere.

### 0.7.137

### 0.7.137

Right-click on a codex badge picks the reasoning effort.

The item RFC-008 listed as not done, deferred on the grounds that
codex's resume semantics were unknown and guessing puts a plugin in a
position to kill a running agent.  Measured instead of guessed:

- `codex resume --last` filters the picker by WORKING DIRECTORY, so
  inside a pane's own cwd the most recent session is that pane's.
- a `-c` value is parsed as TOML and falls back to the raw string, so
  `-c model_reasoning_effort=high` needs no quotes — which matters,
  because the command line rejects quotes as a class rather than
  escaping them.
- `low`/`medium`/`high` come from codex's own serde variant table; the
  only `minimal` in that binary belongs to filesystem paths.

On killing a running agent: claudecode's profile-cycle does exactly
this today, and the click is the authorisation.  What it blocks is a
session held by another process, not an agent that happens to be busy.

The effort goes through `-c`, not an edit to `~/.codex/config.toml`:
the config says what a FRESH codex starts with, and one pane's choice
has no business deciding that for every other.

### 0.7.136

The rollout index is cached by working directory.

0.7.135 looked at the twelve newest session files, and the directory
holds thirty: of three codex panes only one found its own record and
the other two fell back to the global config.  Falling back is safe by
design, but a third of the panes is not "done".

The fix changed the shape as well as the bound: WHICH file belongs to a
cwd barely changes (rescanned every 20 s), while that file's CONTENT
changes constantly (re-read only when its mtime moves).  Steady state
is one `stat` per codex pane.

### 0.7.135

The codex badge reads what THIS pane's session is running.

It read `~/.codex/config.toml`, which says what a fresh codex would
start with — not what the one in this pane is doing.  Two panes on
different efforts both showed the global value.

codex writes a `turn_context` per turn carrying `cwd`, `model` and
`effort` together, so the last one in a session's rollout is the
answer, matched to a pane by the codex process's own working
directory.  Bounded: rollouts reach 63 MB in the field, so only the
last 256 KiB is read, and a record that cannot be found leaves the
badge on the global fallback rather than on a wrong value.  Parsed by
field rather than as JSON, so a format change degrades to "no facts"
instead of to a wrong badge.

Measured on the machine it shipped to: two codex panes that had both
read `medium` came back `high` and `medium`.

### 0.7.134

The markup declaration is re-issued every tick, like a badge.

It was issued once per pane, and the once landed in the gap a session
image swap opens: L1 declared 1.4 s after the L3s were signalled, their
control connections were mid-rebuild, and the frame was dropped.  No
pane ever heard it — the feature shipped and did nothing.

A badge survives that because its plugin re-issues it every tick, so a
dropped frame costs a tick.  This now does the same: one nine-byte
frame every two seconds per codex pane, and an L3 that receives the
same value twice does nothing with the second.

### 0.7.133

The codex plugin declares that its pane's program prints markup it
does not render (`PaneRenderMarkup`, msg 80).

codex does not render HTML, so a model that writes `<u>…</u>` has its
markup arrive on screen as text.  Whose output should be read that way
is not a property of the terminal — it belongs to the pane, and only
the plugin driving a program knows the program does not render its own
HTML.  As a global setting it ate the tags out of the conversation
specifying the feature.

Declared when codex is recognised, withdrawn when it exits, and
replayed on every core handshake for the reason the wheel keys are.

### 0.7.132

Codex's wheel maps to the LINE keys, not the page keys.

`WHEEL_UP`/`WHEEL_DOWN` were `PgUp`/`PgDn`, so one flick of a finger
threw away a whole screen.  The caller already turns a trackpad's
pixels and a mouse's notches into an accelerated line count; handing
that to `CSI A`/`CSI B` is what iTerm2 does, and what "一行行带加速"
means.

Plain `CSI A`, not `SS3 A`: codex never turns on application cursor
keys — `CSI ? 1 h` appears zero times across a full session's byte log.

### 0.7.131

Wheel declarations survive a core swap.

A badge is re-issued by its plugin every tick, so a frame dropped while
no core is attached costs one tick.  A wheel declaration is issued ONCE
per pane — the plugin says how the wheel reaches it and has nothing to
repeat — and the drain dropped it silently when `active` was None.

That is not a rare window.  It is exactly the gap a core swap opens,
and it is where codex landed: L1 re-execed, the plugin declared a
second later, the core was still booting, and the pane had no wheel
keys for the rest of its life.  The wheel then did nothing at all,
which is what was reported after the marker fix was installed.

The shell now keeps the mapping and replays it on every handshake, and
logs both the declaration and the replay.

### 0.7.130

Codex's wheel keys become named constants, and tests pin them.

The marker is asserted to be one the shared predicate can use, and to
match a row captured off a real transcript — the letter-spaced heading
that the declared string failed to match for three rounds.  A blank or
malformed marker would silently turn the wheel back into a blind
toggle, so that case is pinned too.

### 0.7.129

codex declares its `/TRANSCRIPT/` marker.

The rule codex draws across the top while its transcript is open, and
it survives paging — so L2 can read the state instead of remembering
it.  Necessary because `Ctrl+T` toggles: a remembered flag going stale
would close the transcript rather than open it.

### 0.7.128

codex declares how the wheel reaches it.

`Ctrl+T` to open its transcript, then `PageUp`/`PageDown` — measured
against a real pty rather than read off its help.  No key is declared
for leaving: that stays the user's `Esc`.

The declaration is sent once per session and cleared when codex leaves
the pane, so an unchanged two-second scan does not re-push it.

### 0.7.127

codex gets a badge (RFC-008, first slice).

The plugin framework was already built for more than one agent: the
pane→process binding takes a predicate, badges go through
`PluginHost`, and the registry dispatches to whichever plugin
recognises a session.  So this is a new plugin, not a fork of
`claudecode.rs` — 200 lines against that file's 7,900.

It reads `model` and `model_reasoning_effort` from
`~/.codex/config.toml` and publishes `gpt-6-astra·high`, the shape
claudecode's badge already uses, so a window holding both reads as one
system rather than two unrelated tools.  Only top-level keys count:
the file carries `[projects."…"]` tables whose own `model` would
otherwise win by being last, and the badge would follow whichever
project was listed rather than what codex is running.

Recognition walks argv[0] rather than `comm`, for the reason
claudecode does: a released agent renames its process.  The basename
must match `codex` exactly — `codex-code-mode-host` is a helper codex
spawns, and accepting it would bind a pane to the wrong pid.

Sessions come from the registry rather than pane indices, because a
pane index moves when panes are dragged or closed while a session id
is what a badge is addressed to.

Deliberately absent: claudecode's profile-cycle (SIGTERM, await-quiet,
relaunch with `--resume`) leans on claude's session-resume semantics.
codex's equivalent is not established, and guessing would put a plugin
in a position to kill a running agent mid-task.

### 0.7.126

Badge-menu requests are logged on arrival.

An empty menu deliberately gets no reply — L2 opens nothing either
way.  That is still right, but it meant a right-click that reached L1
and found no plugin willing to answer looked exactly like one that
never arrived.  Now the request logs with its item count, and an empty
result logs a warning naming the session.

### 0.7.125

The startup redirect is off: L1 keeps running the bundle binary.

Redirecting cost the whole app its Gatekeeper exemption.  `exec`
replaces the image and with it the process's RESPONSIBLE identity —
it stops being `Marspot.app` and becomes a bare path under Application
Support.  A bare path cannot hold a bundle-id TCC grant, so the
Developer Tools exemption (the one that lets a terminal run code its
user just compiled without a scan) stops matching.  Measured, same
script, same minute:

```text
  responsible = Marspot.app          0.00 s   performScan 0
  responsible = current/ bare path   0.30 s   performScan every time
```

Under load the second row is 1.3 s; a busy test tier reported tens of
seconds.  Every binary built inside marspot paid it once.
`Terminal.app` and `iTerm.app` never did, and this — not provenance,
not notarisation, not Hardened Runtime — is the whole reason.

The cost: a new L1 lands on the next cold launch rather than instantly,
because a bundle binary cannot be overwritten while its own process
runs (AMFI kills the process when the on-disk CDHash stops matching —
2026-06-16, nine panes went blank exactly that way).  L1 moves rarely
(0.7.x against core's 0.12.x) and L2/L3 still update live: a forked
child inherits the responsible process, so they cost nothing by living
outside the bundle.  `MARSPOT_REDIRECT=1` restores the old path.

Requires the bundle-id Developer Tools grant for `Marspot.app`
(System Settings → Privacy & Security → Developer Tools).  Without it
this change is inert, not harmful.

### 0.7.124

The profile cycle now checks whether the session can be resumed
*before* it takes claude down.

The cycle kills first and resumes second, so a `--resume` the CLI was
never going to accept does not merely fail — the pane loses the
conversation outright and the way back is `claude attach <id>` typed
by hand.  The case that does this is a session with a background job
attached: claude answers `Session <id> is running as a background
session` and exits within two seconds of starting.

Seen 2026-09-01 on `uuid=1bcedee1`: `session.bound` at 04:08:06,
`session.unbound` at 04:08:08, then the cycle's `await_quiet` ran out
its 30 s against a shell prompt.

The test mirrors the CLI's own, read out of `claude` 2.1.252: among
the live session records under `<config-dir>/sessions/` (one JSON file
per pid), a holder is one whose `sessionId` matches and whose `kind`
is anything other than `"interactive"`.  Records outlive their
processes, so the pid has to still be alive; the config dir checked is
the profile the *resuming* claude would run under, because that is the
directory it will read.

Anything unreadable answers "no holder" — the gate stops a cycle
already known to be futile, it does not demand proof that one is safe.

A refused click gets a four-second `⚠ held by bg job` badge and
nothing else: no keys taken, no screen held, nothing typed into the
pane.  A click the cycle declines must cost no more than a click that
did nothing.

### 0.7.123

A `terminate` step now also gives up the mouse reporting the program
it killed had switched on.

A signal produces no bytes, so a `SIGTERM`ed TUI never sends the
`CSI ? 1002 l` a clean exit would have sent — and L3's terminal goes
on believing a process that no longer exists wants mouse reports.
What the pane holds by then is usually a shell prompt, and every
scroll after that is encoded as `CSI < 64;x;y M` and typed into it.

Fired when the process is confirmed *gone*, not when the signal is
sent: a program that ignores SIGTERM still owns its modes, and the
escalation to SIGKILL is what settles it.

### 0.7.122

**角标右键弹不出菜单时,至少要说一句话。**

`pane_badge_menu` 有两条出路都是静默的:`last_meta` 里没有这个 session,或者
算出来的行是空的。菜单为空 = 核心什么也不弹,于是「右键没反应」在日志里
**不留任何痕迹** —— 三处静默(命中落空 / 这里 / 核心收到空 items)彼此
无法分辨,这正是它难查的原因。

两条出路现在各记一行 Warn,带上 session、当前 profile、以及扫到的 profile 列表。

值得记下的不对称:角标是这个插件发布、核心一直留着的**看板**,而菜单是每次
右键**现算**的。两者可以不一致 —— 不一致时,屏幕上有一个角标,背后什么都没有。

### 0.7.121

**停着的 pane 拿不到 effort。**

0.7.120 上完之后,只有 4 个 pane 的角标带上了 effort,其余十几个没有。

原因不在 effort 本身,在**推送记录的旧形状**。claude 只在重绘时才重跑 hook,
而没人在里面的 pane 不重绘 —— 于是它一直留着 0.7.116-0.7.119 写的那份**两行**
记录。读的时候两行被当成「claude 说了:没有 effort」,阶梯就此短路,而那个
pane 自己的 transcript 里 `"effort":"high"` 明摆着(实测三个空闲 session,
三个都有)。

两行是**那个版本没有这一行**,也就是**不知道**,不是**没有**。现在分开:

- 三行(第三行可以是空的)= claude 说了,照说的算
- 两行 = 说不出来 → effort 这一半落到 transcript 上补

只补缺的那一半,而且只在两边说的是**同一个 model** 时才补 —— 别的 model 服务
的那一轮的 effort,是关于另一件事的数字。

parked pane 可以停好几天,所以这不是「等一会儿就好了」;红-绿验过。

### 0.7.120

**角标除 model 外还写 effort:`P3@opus-5·high`。**

effort 跟 model 是一起被说出来的 —— 三个信源没有一个只说其中一半:

| 信源 | model | effort |
|---|---|---|
| status line 的 payload | `model.display_name` | `effort.level` |
| transcript 的 assistant 记录 | `"model":"claude-opus-5"` | 同一行的 `"effort":"high"` |
| 启动横幅 | `Fable 5` | `with high effort`(原来被当噪音丢掉)|

所以两者合成一个 `ModelBadge`,沿同一条信源阶梯走,谁也不用把一个问题答两遍。

分隔符用 `·`,是 claude 自己在那行横幅上用的
(`Fable 5 with high effort · Claude Max`)—— 角标和它描述的那块屏幕标点一致。

**没有 effort 时就不写**。有的 model 根本没有这档设置,claude 的 payload 也
只在支持时才带 `effort`;`/model` 的确认串同样不说 —— 那一刻 model 刚换,它
将以什么 effort 跑是下一轮的事。缺就是缺,不猜。

横幅这一路有个小坑:名字在 `(` 处截断(`Opus 5 (1M context)`),而限定词在括号
**之后**,所以两者读的是同一行的不同片段。写的时候注释先预言、测试随即撞上、
再修 —— 括号那条用例现在钉着这件事。

**顺带修一处测试污染**:对账 hook 的那段原本挂在 `scan_once` 里,而 `scan_once`
是测试直接调用的 —— 于是单测会去读真实的 settings.toml、并按它去改真实的
Claude Code 配置。挪到 worker 的 tick 上;测试碰不到它了。

### 0.7.119

**hook 的开关进了设置面板,注册由 L1 自己对账,安装脚本退役。**

0.7.118 把 hook 改成了 opt-in,但 opt-in 的入口是 `bin/install-cc-statusline.sh`
—— 一个**仓库里的**脚本。装了 marspot 的人手上没有它,所以那个 opt-in 对他们
等于不存在。而且它是第二个写 cc settings.json 的人,跟别的写入方会打架。

现在只有一个真相:设置项 `claudecode.statusline_hook`(默认 **off**)。

- 设置面板多一个 "Claude Code" 分组、一个开关,cost 行直说这是别人的文件。
- L1 的 claudecode 插件在自己的 sweep 里**对账**:开关开着而 cc 那边没有 →
  装上;关掉而装着 → 摘掉;装着但 binary 换了地方 → 重新指向。开关没动时
  一分钟才核一次,两秒的扫描上只多一次 map 查找。
- 注册逻辑整套从 shell 脚本搬进 Rust(settings.json 的定位 / 剪切 / 写回、
  软链穿透、`sh -c` 引号、串联的编解码),所以它跟着二进制走,不跟着仓库走。
- `bin/install-cc-statusline.sh` 删除;`bin/install-local.sh` 不再碰 cc 的
  配置 —— 装 marspot 不再有这个副作用。

写回前有一道结构校验(括号 / 引号 / 逗号配平),不通过就**拒绝写**并记一行。
`settings.json` 写坏会把人锁在自己的工具外面,这是不能赌的一处。

三种文件形状(单行 / 缩进 / 末位成员)开→关 round-trip 逐字节还原;别人已有的
status line 连空格带引号原样交回。

### 0.7.118

**pane 自己的横幅排到 `last_model` 前面 —— profile 轮换后的过期 model,不用
任何配置就修好了。**

0.7.116 用 cc 的 status line 拿到了权威来源,但那条通道要往 cc 的
settings.json 里写一行。给别人装的时候它会在好几处直接哑掉:对方已经配了
statusLine(我们原本一律不动)、托管配置里 `disableAllHooks: true`、workspace
trust 没接受、config dir 布局不一样、以及卸载 marspot 之后留下一个指向不存在
二进制的命令。所以它不能当主路径。

而查下来,零配置那条路本来就有一个次序 bug。`model_from_banner` 会把 pane 的
屏幕重放成 grid、从启动横幅里读 model —— 这正是 `--resume` 之后唯一说得出话
的信源。但它挂在调用点的 `.or_else()` 上,而 `model_for` 在围栏后面读不到东西
时**先返回了 `last_model`**(上一个 profile 的 model)。于是新横幅明明就在屏
幕上,永远轮不到它。

`model_for` 现在按信源质量排:

1. claude 自己报的(status line hook)—— 没有围栏、不落后,但默认没装
2. 围栏之后的 transcript —— 说话时权威,只在轮次边界和 `/model` 说话
3. **pane 自己的屏幕** —— resume 之后答案一直在这儿
4. 最后才是上次见过的 model

**hook 同时降级成 opt-in**:

- `bin/install-local.sh` 只跑 `--refresh` —— 已经装了的保持指向对的 binary,
  但绝不会替谁装上。改别人的 settings.json 不该是装 marspot 的副作用。
- **已有 statusLine 的不再被跳过,而是串联**:原命令搬进 `--chain <command>`,
  hook 记完自己的之后用同一份 payload 跑它、原样转发它的输出。cc 只允许一条
  status line,而最想要这个功能的人往往正是已经写了一条的人。
- `--uninstall` 把键删掉,串联过的原命令放回去。三种文件形状(单行 / 缩进 /
  末位成员)round-trip 都逐字节还原。

### 0.7.117

**未知 flag 不再当成一次启动。**

0.7.116 的 status-line hook 把 `marspot-shell --cc-statusline` 写进了 cc 的
settings.json,而 bundle 里那个 binary 可能是旧的(装的时候它正在被运行中的 app
占着,覆盖要等下次冷启动)。实测 0.7.82 拿到这个它不认识的 flag:一路穿过
所有 CLI 分支,**起了一个完整的 supervisor** —— shell.pid、控制 socket、
session 目录都建了,只能 SIGKILL。claude 每次重绘调一次,也就是每次重绘一个窗口。

两头都堵:

- `Some(a) if a.starts_with('-')` 落在 CLI match 的最后:未知选项打一行到
  stderr、exit 2,不再往下走到 GUI 启动。裸 `marspot-shell` 仍是启动,位置
  参数仍然穿过去。
- `bin/install-cc-statusline.sh` 不再假定 bundle 认得这个 flag,而是**核对
  版本**:候选依次是 bundle 和 `binaries/current/`,取第一个 ≥ 0.7.116 的;
  一个都没有就不装 hook(角标回落到 transcript 扫描)。已经装过但指向旧
  binary 的,会被重新指向。

顺带修一个会让 hook **完全不触发**的坑:claude 是**过 shell** 执行
`statusLine.command` 的,而 `binaries/current/` 的路径里带空格
(`Application Support`)。不加引号时 claude 一次都不会调它 —— 两种写法都
实测过。现在路径带引号写入。

### 0.7.116

**角标的 model 一直是错的,而且从来不更新。**

角标写 `P3@opus-5`,cc 的 `/model` 菜单里勾在 `Fable`。不是显示慢了一拍 ——
是这个事实**还不在我们读的那个文件里**。

model 一直是从 session transcript 反推出来的,而 transcript 只在两个时刻
写下 model:一次 assistant 轮次结束时,以及 `/model` 打印确认时。中间的空档
里它什么也不说。于是:

- 在一个停着的 pane 上切了 model → 下一次回答之前 transcript 里没有任何痕迹
- profile 轮换(`claude<N> --resume <uuid>`)→ 新进程启动时写的 `mode` /
  `permission-mode` 记录都不带 model,围栏后面读不到东西,角标退回
  `last_model`,也就是**上一个 profile 的** model,并且一直挂在那儿

把 tail 扫得再勤也治不了 —— 2 秒扫一次和 20 秒扫一次读到的是同一个空档。

改成让 claude 自己说。Claude Code 的 status line 是唯一一条载有「claude 此刻
认为自己是哪个 model」的通道:它把 `model.display_name` 放在 JSON 里交给
配置的命令,并且是**状态变化时触发**,不是定时轮询(实测 2.1.239:空闲 session
约 14 秒一次,启动时立刻一次 —— 后者正好补上 resume 的空档)。

- `marspot-shell --cc-statusline` 读 stdin,把 `<short-model>` 落到
  `<state>/plugins/claudecode/model/<session-uuid>`,rename 落盘,不打印任何
  东西。它在 `main()` 里**排在 logx 和 current/ 转发之前** —— claude 每次
  重绘都会跑它,它必须始终是一个「读 stdin 写一个文件」的进程。
- 角标优先读这份记录,读不到才回落到原来的 transcript 扫描(装 hook 之前
  就已经在跑的 session、以及没有这条通道的旧 claude 走这条路)。
- 记录按 3 天 TTL 由写入侧自己清,不会无界增长。
- `bin/install-cc-statusline.sh` 把 hook 写进 cc 的 settings.json(profile 之间
  是同一个共享文件的软链,realpath 去重后只改一次),**已有 statusLine 的配置
  一律不动**。屏幕上不多任何东西:hook 无输出,而 claude 把空的 status line
  渲染成没有这一行(对 2.1.239 实测过)。

`short_model` 吃 `display_name` 而不是 `id` —— 前者跟 `/model` 菜单里的字
一模一样,后者带 `claude-opus-5[1m]` 这种后缀,会被整条丢掉。

### 0.7.115

**刚进 cc 的 pane 角标还是只有 `P3`,没有 `@model`。**

banner 明明写着 `Fable 5 with high effort`。问题在于取法:原来是把整行交给
`short_model` 洗干净,而那一行同时是装饰、名字和限定词:

```
  ▛▀▜  Claude Code v2.1.232
  ▙▄▟  Fable 5 with high effort · Claude Max
```

两处同时坏,截图里两处都中:

- **ASCII 艺术 logo 跟文字同一行。**`short_model` 见到任何非 ASCII 字符就整条
  拒绝 → 返回空 → **一个字都取不到**(这就是截图的现象)。
- **去掉 logo 也不对。**`Fable 5 with high effort` 会被整段变成名字,再撞 16 字
  上限 → `fable-5-with-hig`。

带括号那种 `Opus 5 (1M context) with high effort` **一直是靠巧合работать的** ——
截断括号顺手把 ` with high effort` 也切了。所以只有新版这种不带括号的 banner 露馅。

改成**取出**名字,而不是在名字周围修剪:跳过装饰 → 取家族词 → 取版本 → 在第一个
既不是家族词也不是版本的词处停下。banner 以后再加什么后缀,都只会让名字提前结束,
不会混进来。

**时机**同步修:`BANNER_RETRY` 原来是固定 30 秒。若第一次尝试正好赶在 claude 画出
banner 之前,角标就要空整整半分钟 —— 而那恰好是用户盯着一个新开 pane 的那半分钟。
改成按 bytelog 大小分档:≤256 KB(刚开的 pane)每 3 秒重试,重放成本本来就被这个
大小兜住;大了才退回 30 秒。这条限流服务的两种情况本来就是相反的。

### 0.7.114

主线程同样升到 `USER_INTERACTIVE`(理由与实测见 L2 0.12.131)。

L1 这条尤其对得上账:08-11 那次事故里,新加的见证器量到 shell 自己的 tick
**迟了 100.8 秒**,而这个线程跑的正是 supervisor tick 和每一个 AppKit 回调。它被
调度走 → 核心的沉默被算到核心头上 → SIGKILL。默认 QoS 就是这条链的起点。

### 0.7.113

0.7.112 装机后,日志里立刻出现一条 `SUPERVISOR_STALL gap_ms=16139` —— 那 16
秒是 shell 从构造到第一次 supervisor pass 的**启动过程**,不是看门狗迟到。
`last_tick_at` 改成 `Option`,首次 tick 不参与判定。

后果不在行为(那一刻要「宽恕」的 deadline 本来就是刚设的),在于**日志**:
一条每次开机都出现的假 stall,会让以后任何读日志的人从错误的地方开始查。测
量装置的失效长得跟数据一模一样,这条也一样 —— 只不过方向反过来。

### 0.7.112

**唤醒设备,窗口冻在一条「Marspot stopped — please restart the app」后面。**

日志里这条横幅只有一个来源:崩溃预算被打爆(5 分钟内 4 次)。而 4 次重启是
同一个形状 —— 2026-08-11T13:21–13:22Z 三次连着,每次都是「spawn → 十几秒后
HELLO_ACK → 几百毫秒内 PONG_TIMEOUT」。

根因在计时的起点。`last_pong_at` 在 spawn 那一刻就开表,注释写的是「freebie
until first ping」,但启动不是白送的:附 surface、reattach 十三个 L3、建
layout,机器一忙就要十几二十秒。等核心终于能应答时,15 秒的 deadline 早过完
了,于是它刚握完手就被判「卡死」——实测握手后 184 毫秒。杀掉重开,新核心撞
同一堵墙,四次之后预算见底。**一个在对方能开口之前就起跑的 deadline,量的是
我们的启动,不是它的死活。**

三处改动:

1. **PONG 计时从 HelloAck 起算。**核心真正能应答的那一刻才开表,整整一个
   `PONG_DEADLINE` 归它。
2. **看门狗先看自己有没有在跑。**新增 `SUPERVISOR_STALL_GAP`(= 一个 ping
   周期,约标称节拍的 20 倍):两次 tick 之间超过这个数,说明 shell 自己被
   挂起了 —— 系统睡眠、编译风暴占满核心、主线程卡在 AppKit。这段时间里的
   「沉默」不是证据,因为根本没人在听;deadline 把这段时间**还回去**,而不
   是记在核心账上。日志出 `SUPERVISOR_STALL`。
3. **预算见底不再是死路。**原来 `auto_restart_disabled` 一旦置上就没有任何
   代码清得掉,横幅挂到用户自己退出 app 为止 —— 所以「唤醒设备总能看到」:
   风暴发生在半夜,横幅一直等到早上。改成冷却重试,首次等一个 `CRASH_WINDOW`
   (5 分钟,此时崩溃记录本来也过期了),每再爆一次翻倍,封顶 8×。重启**速
   率**依然有界(预算的本意),但坏掉的只是一阵负载时,它自己会回来。

横幅同步改名 `UpdateFailed` → `RestartsPaused`,文案改成「Marspot's core
keeps stopping — it will retry on its own」:没有任何更新失败,退出 app 也
从来不是解法。

判定逻辑抽成三个自由函数(`pong_overdue` / `forgive_stall` /
`restart_cooldown`)配单测 —— 原来它埋在 AppKit 结构体里,没法在没有窗口的
情况下验证,这也是这个 bug 活到今天的原因之一。

**未修**:触发第一次重启的是 L2 单次 render 阶段耗时 41.14 秒(核心自己的
`l2.loop.stall` 报的)。那是独立的性能问题,需要单独 decomposition,没有在
这次一并动。

### 0.7.111

**刚开的 pane 角标只有 `P1`,没有 `@model`。**

session 要到**第一次回答**才在 transcript 里写下模型:实测一份活文件的前 10
条记录是 `mode` / `permission-mode` / 若干 attachment,`"model"` 最早出现在
第 11 行。所以从开一个 pane 到它答第一句话之间,transcript 是真的说不出来 ——
而这段时间取决于用户什么时候打字,可以很长。角标空着一半,读起来像
「marspot 没注意到这个 pane」。

但 claude **第一帧就把它印在屏上了**:

    Claude Code v2.1.227
    Opus 5 (1M context) with high effort · Claude Max

pane 自己的屏幕因此是最早的信源,而 `pane_read` 本来就会把 bytelog 重放成
一张网格。**只在 model 读不到时才读屏**,transcript 一旦说得出就接管;解析锚
在版本行上而不是模型名上(模型名每个版本都变,它周围的框架不变),`·` 之后
是套餐名不是模型,切掉。

每个 pane 每 30 秒最多重放一次:不是 claude 的 pane、或者横幅已经滚过去的
pane,不该为此每次扫描买一次 512KB 重放。

### 0.7.110

随 `ui::chrome_scale` —— 显示器缩放成为全局单位,shell 侧只是链接到它。

### 0.7.109

**退出全屏还是重叠约一秒 —— 数据早就到了,慢的是画面。**

日志显示 `windowWillExitFullScreen:` 在转场**开始**时就送出了 69.0(退出结束
是 600ms 之后)。所以位置信息不晚,晚的是那一帧。

原因是我把这个通知接到了 `resized` 上:它会**重建一对 IOSurface**,而此刻窗口
尺寸根本没变。这一趟换面要走 L1→L2→渲染→L1 的整趟握手,于是动画期间呈现的
仍是旧帧 —— 工具栏还压在刚滑回来的按钮上。

新增一条只说这件事的消息 `WindowChrome`(12 字节:window_id + 边界)。
**chrome 变了、像素没变**,就不该动 surface。老 core 读不懂这个类型会跳过,
而跳过的语义正好对:保留它已有的 chrome。

### 0.7.108

**退出全屏时让位太慢。** `windowDidExitFullScreen:` 在动画**结束后**才发,而
红绿灯在动画**过程中**就滑回来了 —— 中间那一整段动画里,工具栏还压在它们上面。

补上 `windowWillExitFullScreen:`,它在转场**开始时**发。问题是那一刻
styleMask 仍然写着全屏,现测只会得到 0。所以窗口现在**记住它上一次不在全屏
时量到的那个边界**,并在「正在退出」这段时间里用它作答 —— **回答窗口要去的
地方,而不是它此刻还在的地方**。转场结束时清掉这个状态,恢复实测。

进入方向不需要对称处理:进全屏时红绿灯是先消失、工具栏后移动,中间是一段
空白;退出时是先出现、后让位,中间是重叠。空白无害,重叠才是毛病。

### 0.7.107

**测量的时机错了,不是判据错了。**

`windowDidResize:` 在全屏**动画进行中**就开始发,而 AppKit 要到动画**结束**
才置上 `FullScreen` 那一位。搭在 resize 上的那次测量描述的是「窗口在路上」
而不是「它落在哪」—— 用户切了两次全屏,日志里两次都是 69.0。

改成监听 `windowDidEnterFullScreen:` / `windowDidExitFullScreen:`,它们在转场
**之后**才发。不新造事件,只是把 `Resized` 再派发一次:resize 那条路本来就会
重新测量并发送,这里要的就是**同一件事在唯一正确的那一刻**做一遍。

也解释了沙箱先前为什么「看起来是对的」:`bin/run.sh` 跑的是**单进程渲染器**,
它在别的事件里还会重建布局,自己把错的那次纠正了 —— 我拿它去验分层那条路,
等于什么都没验。这次是**在沙箱里起真的 `marspot-shell`**(和装机同一条 L1/L2
路)验的:全屏后日志 `right_phys=0.0`,退出后 69.0。

### 0.7.106 — 单进程渲染器也接上测量

`src/main.rs` 那条路上测量值被写死成 `None`,所以沙箱里的工具栏永远停在兜底
的 84pt。修完之后沙箱才第一次能复现全屏。

### 0.7.105

**切 profile 会 resume 到几小时前的 session。** 根因:**argv 说的是进程
「起步于」哪个 session,不是它「现在在」哪个。**

`claude --resume X` 把 X 写进 argv 并留在那里一辈子。而 `/clear` 会在**同一个
进程里**开一个新 session,`/resume` 也会换一个 —— argv 冻在原地。插件把 argv
当作唯一的铁证,于是这个 pane 一直报着一个**没人再写的** transcript:

- 角标的 model 读的是那份死文件
- 回收后 resume 的是那场旧对话
- 切 profile 带回来的是用户几小时前离开的 session ← 用户报的这个

实测:torajs 的 argv 是 `--resume f7a8a54b`,而真正在被追加的是
`e024458b`,新 3 分钟且还在长。

**能证明「哪个 session 是活的」的只有一件事:哪个 transcript 正在被写** ——
claude 每次写完就关文件,没有 fd 可查。所以 argv 之后再问一句:这个项目里
有没有一个**未被别的 pane 认领、且这个 claude 启动之后被写过、且明显比 argv
那份更新**的 session?有就是它。

5 秒的余量防止启动瞬间两份文件同时被碰而来回跳;真的 `/clear` 之后旧
transcript 从此不动,这个余量不花任何代价。

已知边界:**同一个项目开两个 pane** 时这个办法分不出谁是谁 —— 两边看到的
最新文件是同一个。认领集合把它给 `shelld_sid` 小的那个,另一个保留自己的
argv;这和猜测路径一直以来的平手规则相同。

### 0.7.104

**badge 变化时打一行诊断。** 角标由四个各自独立的查找拼出来(profile 标签、
绑定的 uuid、transcript 路径、model),任何一个哑掉都只留下一个**半截角标**,
而半截角标不说明是哪一个哑了 —— 上一条 bug 花了一张截图加一轮手工挖掘才
定位到"扫错了目录"。

只在角标**改变**时打(`sid / badge / was / cfg / uuid`),稳定的屏幕零成本;
下次再出现半截角标,日志里直接能看出是四个里的哪一个。

### 0.7.103

**一个 pane,一次读。** `CLAUDE_CONFIG_DIR` 原来每次扫描要读三遍:profile
标签一遍、transcript 根一遍(上一版新加的)、`BindMeta.config_dir` 一遍。
三次独立的读正是上一条 bug 的形状 —— 标签说 P3、扫描去了默认目录。

现在在每个 pane 的 facts 里读一次,三处共用。顺带省掉每 pane 每 2 秒两次
`KERN_PROCARGS2`(整块 argv+env 的拷贝);16 个 pane 就是 24 次/秒 → 8 次/秒。

### 0.7.102

**非默认 profile 的 pane 找不到自己的 transcript**,所以 badge 只有 `P3`、
没有 `@model`(2026-08-10 报告:torajs)。

profile 标签读的是进程的 `CLAUDE_CONFIG_DIR`,所以它是对的;而 transcript
的扫描**写死在 `~/.claude/projects`**,于是除默认 profile 外一律扫空。同一个
pane 的两个答案必须出自同一处,否则就会像这样一半对一半错 —— 实测:torajs
的 jsonl 在 `~/.claude-profile-3/projects/…/f7a8a54b….jsonl`(3.9 MB,里面
最后 256 KB 有 67 条带 model 的记录),而插件在另一个目录里找。

每个 pane 现在带上自己的 projects 根(`CLAUDE_CONFIG_DIR/projects`,读不到
就退回默认),扫描按 (根, 项目目录) 成对去重 —— 同一个项目开在两个 profile
下是**两个目录**,只按项目名去重会只走先来的那个。

**顺带修了一条会看人下菜碟的测试。** `bin/test.sh` 从不设 `MARSPOT_STATE_DIR`,
所以任何读设置的测试读的都是**开发者自己**的 `settings.toml`;那条断言默认
回收阈值的测试,在作者从面板里把回收关掉那天开始失败,而且只在他的机器上
失败 —— 测试报告的是它所在机器的状态,不是代码的状态。整个测试进程树钉到
沙箱状态目录(和 2026-07-03 `unset MARSPOT_SESSION_ID` 是同一类事故的另一半),
那条测试自己也显式钉住设置。

### 0.7.101

**`settings.toml` 落地** —— 设置面板的地基,先把回收那一组接上。

`~/Library/Caches/marspot/settings.toml`,行式 `key = value`,可手改。三项:

```
reclaim.enabled = true
reclaim.idle_minutes = 30
reclaim.prefetch_on_return = true
```

**改完一秒内生效,什么都不用重启。** 每次 pane sweep(本来就每秒跑)多一次
`stat`;文件动了才重新解析。

**重写会原样保留它读不懂的东西** —— 未知的键、注释、顺序。这是这个会话里
反复学到的那条规则,搬到配置文件上:降一次级、或者用一个还不认识某个键的
构建打开一次面板,都不该把它悄悄删掉。

判据也写进文件头了:**只有「没有普适正确答案」的决定才配住在这里**。回收
的那几条安全规则(不动有焦点的 pane、有活在跑就不动)不是设置,是正确性 ——
把它们放进来只会招人破坏。

环境变量仍然压过文件(`MARSPOT_CC_IDLE_HIBERNATE_S` 等):沙箱脚本和 soak
测试用的是它,而 env 表达的是「这一次运行」,文件表达的是「用户一直想要的」。

### 0.7.100

切 profile 时把**要 resume 的那个 uuid** 记进日志。

报告:torajs 换 profile 好像会丢当前的 ctx。查不了 —— 日志把 pane、两个
profile、五个步骤全记了,唯独没记**它 resume 的是哪一段会话**,而那正是
唯一要紧的事。torajs 的项目目录里躺着 6 段**真**对话(1152 / 632 / 547 …
个 assistant 回合,各几 MB,`/clear` 轮转攒下来的),所以「绑的是哪一段」
不是细节,是全部。

现在两行都带上:`cycle.menu_pick … uuid=…` 和新的
`cycle.resuming pane N → P2 resuming uuid=…`。

### 0.7.99

**回到 marspot 就开始预热,不等你点。**

0.7.98 挡住了「你正坐着的那个」,但报告里的两个 pane(torajs 停放 27 分钟、
marspot 自己停放 56 分钟)是**真的该停放**的 —— 用户离开了将近一小时,
策略做对了。痛的不是判断,是回来时每碰一个都要 3 秒。

而那 3 秒缩不动:几乎全在 claude 自己身上(启动 + 读完整个记录 + 画第一
帧)。所以只能换个时候还这笔账 —— app 重新获得焦点的那一刻就开始唤醒所有
停放的 pane,**每秒一个**,跟你读第一个 pane 的时间重叠掉。

不并发是故意的:十六个 claude 同时启动是一次 CPU 尖峰,而这台机器的卖点
就是没有尖峰。中途你自己点开的 pane 会从队列里跳过 —— 对一个已经醒了的
run 再唤醒一次,等于往当前画面上打字。

`MARSPOT_NO_WAKE_PREFETCH=1` 关掉。

### 0.7.98

**你坐着的那个 pane 永不回收。**

报告:稍微空一会儿回来用 marspot,第一个 pane 要卡几秒才能操作,看起来
根本没到回收的标准。

两件事凑在一起:

- 决定回收的时钟是**会话记录文件的年龄**,而它不知道人在哪。你坐在一个
  pane 里读上一轮的回答,记录已经 30 分钟没动 —— 于是这个 pane 在你眼皮
  底下被停掉,下一次击键要等三秒(唤醒几乎全花在 claude 自己身上:启动 +
  读完整个记录 + 画第一帧)。
- 而回收是**刻意做成看不见的**,所以那三秒没有任何解释,读起来就是
  marspot 卡住了。

现在多一条闸,也是唯一一条关于**人**而不是关于会话的:有焦点的 pane 不
回收。你离开时坐的那个位子一直留着。

顺带把日志的谎修了:`hibernate.start … idle=Ns` 印的一直是 pane 自己的
状态时钟(`held`),那个数在一个闲置了几小时的会话上会显示 `0s` / `1s`,
于是日志看起来像「策略在没有理由的情况下开火」。现在印的是**真正决定的
那个时钟**(会话记录年龄),`held` 留在括号里作参考。

完整的亮度 / 回收规则表在 `docs/pane-attention.md`。

### 0.7.97

冻结一放开就立刻重扫一次,亮度跟内容**同一拍**回来。

0.7.95 让被冻住的 pane 保持亮度,代价是放开之后要等下一次 sweep(最多
一秒)才跟上 —— 方向对了(内容先、亮度后),但看得出来是两件事。

不能直接拿上一份快照的等级发出去:那份快照描述的是**停放中**的 pane
(状态机一直在跑,冻住的只是呈现),照它发就会在程序刚回来的那一刻把
pane 调暗。所以是先把状态机推一步再发 —— 这一步才让等级是真的。

### 0.7.96

**回收不再报 `zZ`,唤醒等画完再放开。**

接着 0.7.95(亮度冻住)把回收剩下的两处可见性也关掉:

- **徽章**。杀掉 claude 会让这个 pane 从扫描的 mapping 里消失,而 mapping
  正是清徽章的那条路 —— 于是停放中的 pane 先把角落清空,再挂上 `zZ`。
  一个「什么都没动」的 pane 连着换两次角落,等于自己宣布正在被处理。
  现在徽章和标题冻在回收前那一个上,由插件按同样的节奏重新断言(所以
  中途重启的核心回来也戴着它)。`zZ` 取消。哪个 pane 停放着,日志和
  `dormant.tsv` 里有。
- **唤醒的收尾**。claude 不是一口气画完一个 resume 的会话:画一段、停下
  来接着读记录、再画、才安定。原先「静 500 ms」就落在这些停顿里,于是
  冻结在半张屏幕上放开,剩下的部分用户是看着它长出来的 —— 实测一次唤醒
  1.9 秒就收工,而那个会话要几秒才安定。静默窗口提到 1.5 秒:多等是不
  花钱的,屏幕上是用户离开时那一帧;`WAKE_WATCHDOG` 兜住极端情况。

### 0.7.95

**画面冻住的 pane,亮度也得冻住。**

回收本该是看不见的:画面停在用户离开时那一帧。但 hold 停的是**格子**,
不是底下的字节流 —— 回收要杀掉 pane 里的程序,杀掉会产出输出,状态机读
到输出就判「忙」,pane 于是**亮到满**,五秒后又**暗两格**到 dormant。
2026-08-03 实测日志:

```
01:43:02  awaiting_user → busy:output   recede 1 → 0   ← 亮起来
01:43:07  busy:output   → dormant       recede 0 → 2   ← 暗两格
```

内容一动没动,pane 自己在那儿闪。围着一帧静止画面画的东西,得跟着一起
静止,否则这个 pane 就在自己宣布正在被处理。

现在:pane 的画面被 PaneSession 冻住期间(`FREEZE_GRID`),它的 recede
等级保持在冻住时的值,释放后随下一次 sweep 跟上。

### 0.7.94

`SurfaceAttachWindow` 多带一个尾部 u32:这个窗口的**槽位**。核心原先靠
到达顺序把窗口配到存档记录上,而那只在「每个存档窗口都正在被重新打开」
时成立 —— 也就是冷启动。核心热替换(静默更新)不重开任何窗口,只是把
已经开着的重新报一遍,于是一个**停放中**的窗口会把自己的记录让给下一个
报到的活窗口,两个窗口互换 pane。旧版本的 shell 不带这个字段,核心退回
到达顺序,跟以前一样。

### 0.7.93

**关窗口是收起,不是扔掉。**

两个窗口,先关哪个决定了你会失去哪个 —— 这不是任何人能记住的规则。原因
有两处,都在这一版改掉了:

- 关掉一个窗口会**退役它的每个会话**;而关掉最后一个窗口(= 退出 app)
  只是 SIGTERM 一遍,回来时 pane 从 state.bin + bytelog 重生 —— 目录还在、
  历史还在,但里面跑着的东西没了。所以「先关谁」直接决定了谁被拆、谁被
  重生,两种都不是「原样回来」。
- 关掉**第一个**窗口还会让整个 app 静止:核心送来的唤醒是发给引导窗口的
  (它是「进程级事件」的约定收件人),引导窗口一关,唤醒全被丢弃 ——
  剩下的窗口停在最后一帧,supervisor 也不再走。

现在:关窗口把它的布局**停放**下来,会话照跑;退出也不动任何已注册的
会话,下次启动直接重新接上同一批 L3、同一批 PTY。唤醒改投给还在的窗口。
真正要拆掉一个窗口的办法是关掉它的 pane。

无法触达的孤儿 L3(注册表里没有的)退出时照扫不误 —— 留着的是我们打算
接回来的那些。

窗口几何按**自己的槽位**写,不按在列表里的位置:关掉一个窗口曾经让它
后面每个窗口下移一格,于是幸存者互相继承了对方的位置。

### 0.7.92

**进了 claudecode,右上角什么都没有** —— 不是慢半拍,是要等到你说第一句话。

badge 一直挂在「认出这个 pane 的会话」上,而会话文件是 claude 答完第一轮
才写的。启动到第一次提问之间往往隔着好几分钟,这段时间里 pane 明明在跑
claude,角落却是空的 —— 看起来像 marspot 没认出来。

会话 uuid 早就不显示在 badge 上了(那 36 个十六进制字符对人没有意义),
badge 现在只有 profile 和 model 两半。profile 从进程的 `CLAUDE_CONFIG_DIR`
一直读得到 —— 所以这段时间里诚实的答案是 `P1`:账号是知道的,model 还
不知道,等会话文件出现的下一个 tick 再补上 `@model`。

代价是这样的 pane 没有 uuid,而回收和 profile 切换都要靠 `--resume <uuid>`
才能把会话原样带回来。两条路径现在都在空 uuid 上直接停手并说明原因 ——
把一个还没落盘的会话拿下来再 resume 一个「空」,是唯一无法挽回的错。

### 0.7.91

**切完 profile 就没有 model 了** —— 而且不是一会儿,是一直没有。

围栏本身是对的:换进程之后,旧进程写的记录不描述新进程,所以读 model 时
不能翻到围栏后面去。问题是**新进程在答完第一轮之前根本不写 model** ——
resume 之后它写的是 `mode` 和 `permission-mode`,两条都不带 model。于是
从切换到会话下一次开口之间,围栏后面**什么都没有**。一个停放着的 pane
就是几个小时的光秃秃 `P4`。

把这个渲染成「没有 model」是说谎:会话有 model,只是我没看见它写。所以
记住最后一次真读到的那个,读不到就继续显示它,读到新的立刻换掉。

代价是「resume 之后 model 真的变了」那一种情况下,会有一段陈旧 —— 长度
是这个会话答一次话的时间。换来的是 badge 不再每次回收/切换就掉一半。

不给全新会话编:从没写过 model 的会话仍然只显示 profile,`settings.json`
里的默认值不作数(现网多数 pane 跑的就不是那个默认值)。

### 0.7.90

**badge 不许空** —— 昨天把 session uuid 从 badge 里拿掉时,顺手让
「profile 和 model 都读不到」落到空字符串。看起来无害,其实拆了一条
不成文的契约:core 拿「这个 pane 有 badge」当「cc 插件认领了这个
pane」,空字符串会把整条记录删掉,于是这个 pane 的链接扫描器悄悄退回
非 cc 模式 —— claudecode 的定宽硬换行不再合并,**跨行的路径就此不可
点**,而屏幕上没有任何迹象说明为什么。

两者都读不到的条件是进程环境里根本没有 `CLAUDE_CONFIG_DIR`(`claudeN`
别名都会设,所以现网这条路今天没被踩到)。落到 `"cc"`:两个字符,把
契约重新立住,顺带告诉读的人这个角落里是个 claude 会话。

### 0.7.89

**恢复时还是黑一下** —— 前两次修的都是「时钟从什么时候开始算」,而真正错的是
**「画完了」的判据本身**。

日志给出确凿时间:

```
01:55:16.792  5/7 resume         ← 命令刚排进队,还没到 PTY
01:55:16.792  6/7 await_process
01:55:16.810  7/7 await_quiet    ← 18 毫秒后就认定「进程在了」
01:55:17.637  Done               ← 再 827 毫秒就解冻
```

`await_process` 做的正是它字面的事:**进程存在**。fork + exec 只要几毫秒,
所以它没说错 —— 但进程存在离画完差着好几秒:claude 要先把整个会话记录读完。
那段停顿里终端只见过一个清屏和几个模式设置,于是「有过输出 + 静止 500ms」
成立,冻结在**屏幕刚被清空**的那一刻撤掉。那就是黑的那一下。

`AwaitQuiet` 加上字节下限:第一帧是几十 KB 的文字和颜色,启动噪音是几百字节,
门槛取 2 KB,两边都留足空间。

三次修同一个症状,教训是同一个:**「它安静了」和「它好了」不是一回事**。
前两次分别把「谁的输出算数」和「用哪个时钟」修对了,但都没问「多少输出才
算一帧」。

### 0.7.88

**badge 不再挂 session uuid** —— 36 个十六进制字符,没人能对它做任何事。

它给**机器**命名会话,而每个需要它的机器(日志、`dormant.tsv`、resume
命令行)本来就有。挂在屏幕角落只是把 pane 自己的标题挤掉,读的人一无所得。

现在:`P3@fable-5`;profile 读不出来但知道在跑什么模型时,只显示模型;两个
都不知道就不显示 —— 空角落至少诚实。停放的 badge 也从 `zZ <uuid>` 变回
`zZ`,那三个字符就是全部要说的话。

顺带修一个搬迁时丢掉的节流:**停放中的 pane 每个 tick 都在重发 badge**。
`on_tick` 跑在重绘泵上,有 pane 在画时是 16 ms 一次,而「等用户回来」这一步
可以挂几小时 —— 一个什么都不做的 pane 每 16 毫秒发一帧 wire,正是
CLAUDE.md §3 说的 background creep。手写版本当年有 8 秒节流,搬成脚本时丢了。
现在:转圈的步骤按 125 ms 走一帧(一秒一圈),停放的步骤 8 秒重申一次(只为
让重启后的 core 拿回 badge)。

写这条节流时第一版**完全没生效**(十秒测试里发了 625 帧):`badge_at` 取自
run 自己的时钟,比较却用了真实时钟。可注入时钟的价值正在于此 —— 测试当场
把它抓出来了。

### 0.7.87

`pty_op` 的每一行日志带上 pane id。

三次唤醒、两次回收,而日志读不出哪次属于哪个 pane —— 两条 run 在两个 pane
上交错,读起来像一条不可能的 run。排查「有没有打错 pane」的时候发现日志
本身分不清 pane,那就得先补这个。

### 0.7.86

**停放期间画面根本没被冻住** —— 自愈逻辑在回收开始后 6 毫秒就把它放了。

日志一行看穿:

```
01:35:27.839  reclaiming pid 51933
01:35:27.845  pane 414 has a live claude again   ← 6 毫秒后释放了冻结
```

扫描每 2 秒一轮,那份 mapping 是**杀 claude 之前**取的。「已经回来了就放掉
冻结」这条自愈规则于是在刚杀完的那一瞬命中,pane 整个停放期显示的是 shell
提示符,而不是用户离开时的那一帧 —— 也就是之前反复修的那个症状,还剩这最后
一条路径没堵。

挂着唤醒的 pane(`armed`)是我们**故意**在冻的,自愈一律不碰它。

### 0.7.85

**唤醒把 `claude --resume …` 打进了一个活着的会话** —— 今天出现了好几次。

现场:pane 一点就能用(说明它根本没被回收,或者早就回来了),但输入框里
躺着一整句 `printf '\033[H\033[2J'; CLAUDE_CONFIG_DIR='…' claude --resume
06a6587d-…`,要用户自己删掉。

日志把两个 bug 都摊开了:

```
00:06:39  reclaiming pid 94718        ← 回收开始
00:06:40  terminate → await_user      ← 0.6s 后 claude 死了,停放
00:06:41  dormant … can be woken      ← 又武装了第二个唤醒
00:08:44  by focus → 4/6 resume       ← 点击,打出 resume
00:08:46  Done + cc.reclaim(id=2) 又起
```

**① 同一个 pane 被武装两次。** 回收路径提交了 run 却没把 pane 标记成
`armed`,于是两秒后的重挂扫描看到一条「没人管」的 dormant 记录,又起一个
run —— 后者顶掉前者,两条路抢着打同一句话。

**② 打字之前不复查。** 停放到用户回来之间隔着任意长的时间,而**用户的那次
点击就在这个间隔里**;这期间会话可能已经由别的途径回来了。新增步骤
`stop_if_process`:要安排的事情如果已经发生,这个 run 就地成功结束,一个字
都不打。

第二条是通用原语,不是补丁:任何「把某个东西弄回来」的脚本都需要在动手前
问一句「它是不是已经回来了」。

### 0.7.84

**autorun 说得出自己为什么不动手,并且能从自己挖的坑里出来。**

用户问「torajs 怎么不继续了」,而日志里**一个字都没有** —— 这条策略只在
动手时记账,不动手时完全沉默。「什么都没发生」读起来和「卡死了」一模一样,
而只有一种是好事。现在每一次不动手都有一个具名理由(pane 忙 / 还在静置 /
有人在打字 / 等上一次动作生效 / 已放弃 / 没什么可做),按理由去重后落一行。

查下去发现的真问题:torajs 的输入框里躺着一句 **`继续 autorun` 没发出去**
—— 粘贴落地了、紧跟的回车没生效。而 0.7.82 加的那道「输入框非空就不动」的
闸,于是把策略**永远挡在了它自己没做完的动作前面**。一道能被自己的输出
锁死的闸不是闸,是陷阱。

现在:框里那句**如果正是我们自己刚打的**,就补一个回车把它送出去(受同样
的超时和退避约束);是**别人**写的半句,照旧一步不动。顺带修了检查顺序 ——
重试分支原来排在这道闸前面,于是重试会绕过它;重试也是粘贴,粘在别人半句
上并不会因为「这是第二次」而变得可以接受。

### 0.7.83

**回收从来没触发过 —— 我们量错了时钟。**

用户问:inputx、devops 这些十几个小时都没动过,为什么没被回收?查下去:

```
23:10:47  awaiting_user held_s=1767 → busy:output
23:40:47  awaiting_user held_s=1768 → busy:output   ← 整整 30 分钟后
```

`1767` 秒 = 距 1800 秒门槛**差 33 秒**,每次都差这么一点。真凶在 bytelog
最后几十个字节里:`Checking for updates` —— **claude 自己每 30 分钟查一次
更新**,在角落里写一行再擦掉。一个 pane 的日志里出现 **23 次**。

这几十个字节让终端「忙」半分钟,把安静时钟清零。而门槛正好也是 30 分钟,
于是这是一场更新检查每次都赢的比赛 —— 调低门槛也没用,任何门槛都会被这个
周期性写入清零。

**改的是量什么**:回收的时钟换成**会话自己的记录文件的年龄**。终端的家具
不会写 transcript。inputx 的记录最后一条是 12:22,而它的终端 23:44 还在
「忙」—— 前者才是「这个会话闲了多久」的答案。

`hibernate.waiting` 现在两个数都报:`idle 40000s of 1800s (terminal quiet 1s)`。

### 0.7.82

**输入框里有没写完的话时,autorun 一律不动。**

粘贴落在光标处,紧跟的回车会把整行提交 —— 也就是把**你没写完的那句话,
后面接上 `/clear`,一起发出去**。这是这条策略唯一一个「人事后没法撤销」的
错误,所以它比其它任何一道闸都硬:提示符 `❯` 后面只要还有非空白的东西,
这一轮什么都不做。

框线一并处理:输入行是画在边框里的(`│ ❯ …  │`),两头的竖线都不算内容。

### 0.7.81

`--read` 不再把宽字符的哨兵原样吐出来。

一个宽字符占两格,第二格存的是 NUL 哨兵 —— 那是**排版事实,不是字符**。
原样收进字符串就落在每个 CJK 词中间(`继\0续`),终端把它吞成一个空格,
所以肉眼看只是「字间距怪怪的」;直到有人拿这段文本去匹配才会咬人 ——
而 autorun 策略读的正是这段文本。

### 0.7.80

**autorun 只对「等着用户的会话」动手,不是对「安静的 pane」。**

原来的闸是 `quiescent`,而**没有 claude 的 pane 也是安静的** —— 那种情况下
`/clear` 会被 shell 当成一条不存在的命令执行,接着 `继续 autorun` 再来一条。
本来是去帮忙的,结果在人家 pane 里留下两个报错。

闸收紧成 `PaneStatus::AwaitingUser` 这一个状态:绑着程序、且它刚打完一轮。
`Empty`(没有程序)、`Dormant`(已停放)、`Contradiction`、`Unknown` 一律不
动 —— 这条策略打的字只有正在运行的会话看得懂。

### 0.7.79

**autorun 的判据按真实日志改了两处** —— 装机后第一眼就发现原版会误伤。

**① 程序自己的 `/clear` 不是指令。** torajs 的历史里 `/clear` 出现 39 次,
只有 6 次是真信号(`守恒精确)—— 可以 /clear 了`),其余全是 claude 自己的
东西:

| 行 | 是什么 |
|---|---|
| `⎿  Tip: Use /clear to start fresh when …` | 它自己印的提示(**按窗口宽度换行,同一条出现了四种长度**) |
| `❯ /clear` | 命令被敲进去时的回显 —— 包括我们敲的 |
| `/clear (reset)  Start a new session …` | 输入 `/` 时弹出的命令面板 |

所以先扔掉程序的家具(`⎿ ❯ ✻ ⏵ │ ─ ╭ ╰` 开头、含 `Tip:`)、再扔掉「本身
就是那条命令」的行,剩下的才拿去匹配。提示条按 `Tip:` 标记认,不按文本认
—— 它的文本会被换行切碎。

**② claude 自己会重试十次,那期间不该插嘴。** 全机器 46 条 API Error 行里
唯一一条真报错是 `✻ API error · Retrying in 0s · attempt 1/10` —— 它自己在
处理。我们要接手的是**那十次用完之后**。另外 45 条是**会话在讨论错误**
(一段解释错误分类器的散文),按词命中会让任何谈论自己工具链的会话被打字。

### 0.7.78

**rotation 自动跑:`marspot-shell --autorun <pane> on`。**

一个长期跑的会话是按 rotation 工作的:干完一段,说一句「可以 /clear 了」,
然后停在那儿等人。没人就永远等下去。这条策略替人打那两行 —— 另外,当它是
被服务端错误卡住而不是被自己卡住时,单独给一句「继续」。

判据(`plugins/autorun.rs`,纯函数 + 11 条规格):

- **绝不往正在干活的 pane 里打字。** 每个动作都要求 pane 安静 **且** 底下
  没有进程在跑。打进忙碌会话的一行,轻则被忽略,重则被当成话回答。
- **一个安静期只动一次。** 动完之后要等 pane 重新忙起来才认下一次 —— 屏幕
  上那句话还在,不能连着触发两遍。
- **动作可能被吞掉,所以会重试,有退避,有上限。** 6 次(30s / 60s / 2m /
  5m / 10m / 15m)之后停手并明说,不会热循环。
- **pane 一动,一切归零。** 它动了,说明我们等的事已经发生。
- 「rotation 结束」认的是 `/clear` 这个命令本身(措辞每轮都不一样),但必须
  出现在**最后十几行**里 —— 五十行前说过的是滚动历史,不是指令。
- 服务端错误**优先于** rotation 标记:错误是后发生的那件事,而且这时 /clear
  会把重试需要的上下文丢掉。

开关按**工作目录**存(`autorun.tsv`):名字会因为出现同名 pane 而变、id 会
因为会话重建而变,而「这个项目的那个 pane」不会。每 30 秒看一眼;所有触发
条件本来就要求 pane 安静得更久,看得更勤只会让它更早地对没把握的证据动手。

两行是**一个 operation** 发的,不是两个:队列串行化的是整个 operation,拆开
会让别的东西挤进 `/clear` 和后一句之间;两行之间等画面静止 700ms —— `/clear`
会重启会话的 UI,打进那个空档的一行会掉。

### 0.7.77

**会听了:`marspot-shell --read <pane> [-n <lines>]`。**

不需要订阅、不需要缓冲、不需要长连接 —— L3 早就把每个 pane 的 PTY 输出
全量追加在它自己的 bytelog 里,记录一直在磁盘上。所以「听」是**要看的时候
去看一眼**,不是架一根管子一直接着。

回来的是**人看到的那一屏**,不是原始字节流。原始输出是转义序列、光标移动和
重绘 —— 一个 claude pane 每秒把同几行重画很多遍,「最后 4KB 输出」没有意义,
而「屏幕上现在写着什么」正是要问的。把前者变成后者的东西叫终端模拟器,
而 marspot 就是一个:把尾巴喂进一个无头 `Terminal`(尺寸取该 pane 的),
读它的网格。

- 只读尾部 512 KB,所以开机第一天和第三十天读一个 pane 一样快。
- `-n N` additionally 给出已经滚出屏幕的 N 行(从旧到新排在屏幕之前)。
- 重放只在一个方向上是近似:窗口之前设的状态(某个颜色、某个模式)会缺,
  但绝不会凭空多出来。全屏程序重绘的频率远高于这个窗口的长度。

### 0.7.76

**编号跟屏幕走,不跟时钟走。** `#1` 是靠上靠左的那个 —— 窗口 → 行 → 列,
也就是人读它们的顺序。

上一版按 session id(创建顺序)排,在列表里看不出问题,在屏幕上是错的:
`doracawl#2` 坐在 `doracawl#1` 左边 —— 老的那个被拖到了右边,而没人按
「谁先开」去数 pane。

连带的语义:**拖动 pane 会换号**,因为号就是位置。这跟 `w(n,x,y)` 是一套
说法 —— 两者都指「那个格子里的那个」。挤不进网格、落到侧栏的 pane 排在
所有上屏 pane 之后(按 id),仍然有稳定的名字。

规格补了 4 条(截图那个原例、读序、无格子的 pane、拖动换号),先写测试。

### 0.7.75

**pane 改名功能删掉了,名字改成纯粹的显示。**

以前一个 pane 可以被右键 Rename、双击标题条改名,名字存在 `custom_title`
里、随会话跨重启带回来。删掉的理由不是它没用,是它**让「哪个 pane 是哪个」
有了第二个真相**:`--send spg` 认的是目录派生出来的名字,而标题条上可能写着
别的。名字要么是派生的,要么是权威的,不能一半一半。

现在:名字 = 工作目录最后一段,重名时全部带号,一律现算 —— 标题条、
`--panes`、`--send` 看到的是同一份。规则进了 `marspot::pane_name`(库层),
因为 L1 要拿它解析地址、L2 要拿它画标题,两份实现就是两套规则。

`#k` 的完整规格(12 条测试先立的):加一个同名的,**所有**同名 pane 一起
带号(不是「先来的保留裸名」);再加一个只是接在后面,前面的号不变;关掉
中间一个,后面的补位;**回到只剩一个,号自动去掉,但它仍然认自己那个号**
—— 存了 `spg#1` 的调用方不会因为对方关了一个就失效。

磁盘格式没动:`SavedPane.custom_title` 字段留着但没人写,老版本仍能读新
文件,反之亦然。

### 0.7.74

**三种寻址:name / id / w(n,x,y)**,名字全自动、不许改也不许重。

- **名字**取自工作目录最后一段。**重名时所有同名的都带号** —— `spg#1`
  `spg#2`,不是「第一个叫 spg、第二个叫 spg#2」;一个名字绝不能悄悄地
  指「先来的那个」。号按 session id(创建顺序)排,关掉一个,剩下的下次
  列名单时自动补位;名字不落盘、每次现算,所以不会过期。
- **id** 是背后那个真地址:永不复用、永不移动,同目录的两个 pane 也只能靠
  它区分。
- **w(n,x,y)** 是「第 n 个窗口的第 x 列第 y 行那一格」,全部从 1 数起。它
  指的是**那个格子里的 pane**,谁在里面就是谁 —— 布局一动它就指向别人,
  这正是它的用途。格子信息来自 L2 每次开关 / 换焦点 / 改布局时落盘的布局。

`--panes` 现在把三种写法都列出来,照抄即可:

```
    id  address                             directory
   390  spg  w(2,1,1)                       /Users/x/workspace/goliajp/spg
   394  doracawl#1  w(1,3,4)                /Users/x
   386  doracawl#2  w(1,4,4)                /Users/x
```

### 0.7.73

**同名 pane 怎么办** —— 三种说法,外加一份名单。

同名不是意外,是常态:同一个项目在两棵树里各开一个,`spg` 就有两个。原来
只会回一句「匹配到 2 个,说具体点」—— 等于把问题丢回给调用方。现在:

- **会话 id**(`390` / `#390`)—— 永远唯一。
- **路径尾**(`goliajp/spg`)—— 精确到段,不会顺带匹配上 `goliajp/spg-old`。
- **裸名字**(`spg`)—— 先精确匹配最后一段,再退到唯一子串。

歧义仍然拒绝、绝不猜,但错误信息现在**把候选连 id 一起列出来**,下一次尝试
是复制粘贴而不是调查:

```
"spg" matches 2 panes — say which:
   390  /Users/x/workspace/goliajp/spg
   412  /Users/x/workspace/stables/spg
```

外加 `marspot-shell --panes` 列全部 pane(id / 名字 / 目录 / 标题)。名单由
**L1 自己那份视图**回答,不是 CLI 各查各的 —— 名字的含义只能有一个来源。

### 0.7.72

**队列起会话不该走插件权限闸** —— 第一次真实投递就被自己人拦下了。

`PtyOps::pump` 通过 `PluginHost::begin_pane_session` 起会话,而那个方法检查
**当前插件**的权限。CLI 发起时根本没有插件身份,于是:

```
cli.send on pane 390: missing permission: PermissionSet(16)
```

—— 主循环向自己申请一个它没有身份去持有的权限,消息没进 pane。

队列改成依赖一个只有两个方法的 `OpHost`(起会话 + 记日志)。插件宿主自动
满足它(插件那侧照旧走权限闸),主循环用自己的实现 —— 它本来就是授权方。

### 0.7.71

**第一条 session 间通信:`marspot-shell --send <pane> <text…>`。**

把文字送进某个 pane 并回车。听起来是最小的功能,但它要的东西正好是以后
每个协议都要的:

- **给没有名字的 pane 起名**。pane 只有工作目录,人嘴里的名字是它的最后
  一段(`spg`)。先精确匹配那一段,再退到唯一子串;**歧义直接拒绝**,不猜 ——
  猜错的代价是把话打进别人的会话。而且目录得**实时从 shell 进程读**:
  注册表里那份是启动时的,cd 过的 pane 全写着 `/Users/doracawl`。
- **不跟那个 pane 正在做的事撞车**。请求走 L1,进同一个队列。pane 上有别的
  操作在跑就**拒绝**而不是排队 —— 发的人想知道消息「现在」进去了,回收结束
  之后才落地的消息,落到的已经不是他以为的地方。
- **多行文本不能被一行行执行**。走 paste 通道(L3 知道对端开没开 bracketed
  paste),回车**单独一步**发 —— 混在文本里它就成了消息的一部分。

为什么不直连 L3:新客户端会顶掉 L2 正在用的 control writer,CLI 一断开那个
pane 就不再刷新。命令 socket 在 L1(`l1-cmd.sock`,状态目录里,同用户可达,
不是带认证的通道)。

投递本身**不冻结画面也不锁键盘** —— 这是递东西,不是接管;那个 pane 的人
可以继续打字。

### 0.7.70

`signal` + `await_gone` 合成一个 `terminate` 步骤。

原来的写法是两步:升级策略配在 signal 上,却由后面那个 await_gone 执行 ——
runner 得往回看一步才能找到它,读的人得知道这两步是一对。它们本来就是
一件事(「让这个进程消失」),而且两个调用方都是连着用的。

少一个概念,少一处耦合,两条脚本各少一步。

### 0.7.69

**把 PTY 操作这块封装成能给「session 间通信」当地基的样子。**

补的都是「从『一个 pane 自己的操作』跨到『A 给 B 递东西』时会立刻塌」的
地方:

- **一个 pane 同时只跑一个脚本**(`PtyOps`)。两段脚本往同一个 PTY 打字会
  互相穿插,到达的既不是这条命令也不是那条。以前靠「只有两个调用方、而且
  都是用户手动触发」侥幸成立;session 间投递意味着操作**指向别人的 pane**、
  **在发送方想发的时候到达**、而那个 pane 可能正在被回收。队列按 pane 排,
  每 pane 上限 8 条(超了告诉调用方,不是无限堆积),启动顺序按 sid 排定 ——
  HashMap 的顺序不是顺序,日志会变成噪音。
- **迟到的完成报告不会释放别人的 pane**:报告要认 id,不认 pane。
- **`Step::paste` 走 paste 通道**,不是裸写字节。只有 L3 知道 pane 里那个
  程序开没开 bracketed paste,而多行文本不带它就是一行行当命令执行 ——
  递「一条消息」给另一个会话必须走这条。新 wire `PaneInjectPaste`(L1 →
  L2 → L3)。
- **单次投递上限 64 KB**。不是性能限制,是炸裂半径:从程序的角度看,这条
  路送进去的一切都是「用户敲的」;在还没有调用方的时候把「有人往别人的
  session 里灌几 MB」变成不可能,比事后补便宜。
- 每次投递落一行日志(目标 pane + 字节数)。

claudecode 的两条脚本(回收、profile 切换)现在都从这个服务走,没有绕过
「一次一个」这条规则的路径。

### 0.7.68

**「往 PTY 里注入操作」变成一个基础能力**(`plugins/pty_op.rs`),两个业务
都搬上去了。

在此之前这套东西手写了两遍 —— profile 切换和闲置回收 —— 各自展开同一副
骨架:冻结、发信号、3 秒升级到 SIGKILL、轮询进程表、打一行命令、静置、
看门狗、spinner、收尾。两者真正不同的只有两处:**打什么命令**、**等什么
等多久**。写第二遍的东西就该抽出来。

调用方现在声明一段脚本,执行由这个模块负责,连同那些第二遍最容易写漏的
部分:

- **清理恰好一次**,所有出口都走到 —— 正常结束、超时、用户按 Esc、pane
  被关掉。「pane 冻着但没人来解冻」是这块代码最坏的故障,不该由每个调用方
  各自记得。
- **每个等待都有期限**,只有「等用户回来」是故意无限的;期限按步计,一步
  慢不会吃掉下一步的预算。
- **冻结用 L3 的**,不是 core 的 —— 静默更新会重启 core。
- 效果全走 `OpEnv`,所以整条脚本能在一个测试自己控制的时钟里跑完,不用
  真的睡过超时。

`PtyCommand` 把命令拼装也收了进来:值不安全就**拒绝生成**(返回 `None`),
不再是「拼好了再想起来检查一下」。回收因此多了一条硬保证 —— profile 读不
出来就根本建不出脚本,绝不会退回到默认账号。

顺带把 profile 切换的收尾也修了:它以前固定静置 600ms 就解冻,大会话
(实测 317 MB 记录要 10 秒以上才画完)会闪出一屏 shell;现在跟回收走同一
条「等它画完」的路。

`claudecode.rs` 净减 727 行 / 新模块 944 行(含文档和 5 条单测)。

### 0.7.67

**回收的冻结改由 L3 持有** —— 装机会重启 core,冻结在 core 里就活不过装机。

用户报告:恢复 sentori 那个 pane 时,它已经是 zsh 界面了。查下来跟唤醒无关
—— `PANE_SESSION_CAP_FREEZE_GRID` 只让 L2 不去 pump 自己那份网格,而**每次
静默更新都会重启 core**(今天六次)。新 core 的 `pane_sessions` 是空的,于是
立刻 pump:claude 退出后排在队列里的字节 —— 提示符 —— 一次性画出来。L1 两秒
后重新挂上唤醒会话,又把冻结按在 zsh 画面上。

冻结挪到 L3:新 wire `PaneHoldGrid`(L1 → L2 → L3),L3 收到后照常读 PTY
(子进程绝不能被堵),但把字节**存着不喂给解析器**,释放时一次性喂完。
L3 活得比 core 长,所以这个冻结用户不会看到它破掉;释放是一步到位的,
中间态一帧都不画。

L1 这边:SIGTERM **之前**就 hold,唤醒画完才 release;重新挂载时幂等地
再 hold 一次(L3 已经在 hold 就是空操作);另外每轮扫描做一次自愈 ——
凡是「我们 hold 过、但 claude 已经回来」的 pane 一律释放,防止 release 那
一帧正好撞上 core 重启被丢掉。

### 0.7.66

**「静止 3 个 tick」实际是 48 毫秒** —— tick 不是 250 ms,是 16 ms。

`on_tick` 跑在重绘泵上:窗口闲着时 ~250 ms,而**有 pane 在出帧时 16 ms**
—— 唤醒正处在后一种情况。所以上一版写的「连续 3 个 tick 静止 ≈ 750 ms」
真实只有 48 ms,刚好落在 claude 打完 banner、还没画出第一帧的那个停顿里,
冻结就在那儿撤了。真机日志:resume 发出后 **357 ms** 会话就结束了。

判据换成挂钟时间:claude 出现后,输出静止满 **500 ms** 才算这一帧画完。
时长不受 cadence 影响,tick 快慢都一样。

同一个毛病顺手修掉:dormant pane 重申 badge 用的是 `spin_phase % 64`,
按 16 ms 算是每秒一帧 wire —— 一个什么都不做的 pane 每秒发一帧,正是它
自己注释里说要避免的 background creep。改成每 8 秒一次的挂钟判断。

`hibernate.ended` 现在带上 `reason=` / `claude_seen=` / `still_ms=`:
「claude 画完了」和「宿主把会话拆了」以前在日志里长得一模一样。

### 0.7.65

**恢复时黑屏一下** —— 上一版的屏幕擦除被当成了「claude 画完了」。

解冻条件是「有过输出 + 静下来」。而 resume 那行自带的擦屏本身就是输出,
擦完到 claude 真正画出第一帧之间的那段空档正好是「静下来」—— 于是冻结在
claude 动笔前一拍就撤了,露出刚被擦干净的空屏,也就是那一下黑。

改成**从 claude 出现那一刻开始量**:进程表里看到 claude 之前的字节一概不算
(回显不算,擦屏也不算),看到之后才记基准、才开始数静止的 tick。这样
「有过输出」指的一定是 claude 自己画的东西。

30 秒看门狗单独判,不再混在这个条件里。

### 0.7.64

**resume 那一行连痕迹都不留。**

冻结画面挡住了「看着它发生」,但解冻之后那行还在:shell 提示符加一行
`CLAUDE_CONFIG_DIR='…' claude --resume …`,楔在恢复出来的会话上面。

resume 命令前面挂一个屏幕擦除(`printf '\033[H\033[2J'; …`)。回显发生在
冻结之下没人看得见,命令一执行先把这一屏抹掉,claude 再在干净的屏上画 ——
解冻时 pane 上只有 claude 自己画的东西。

只用 `\033[2J`,不带 `\033[3J`:后者会把 scrollback 一起清掉,而 scrollback
是用户的东西。测试里专门断言这一点。

### 0.7.63

**唤醒等的是「画完」,不是「进程起来了」。**

冻结画面本来就该盖住整个恢复过程,但 `Waking` 的结束条件是
`claude_is_back()` —— 进程表里出现 claude 就解冻。而 shell fork 出 claude
只要几百毫秒,claude 画完自己的界面要一秒多:中间那一秒多冻结已经撤了,
用户看到的正是回显的 `claude --resume …` 那行和启动输出往上滚,也就是冻结
本来要挡的东西。

改成等「画完」:记下发 resume 时 bytelog 的长度,之后每 tick 比一次 ——
**有过输出、并且连续 3 个 tick(约 750 ms)没再增长**,才算这一帧画完,
这时才解冻并交还键盘。自校准:恢复得快就解冻得快,慢就多等一会儿,不需要
猜 claude 首帧要写多少字节。

30 秒看门狗保留:resume 根本画不出来(claude 不在 PATH、profile 目录没了)
时仍然把 pane 还回去,而不是把键盘永远锁住。

### 0.7.62

**回收之后 `dormant.tsv` 又空了** —— 这次是杀进程期间的扫描判的死。

0.7.4x 修过一次:触发回收的那次扫描是在 claude 还活着的时候取的,拿它去
判刚写下的记录,读起来就是「claude 回来了」,当场删掉。加了
`scanned_at <= created_at` 的守卫。

漏掉的是**紧随其后的那几次**。SIGTERM 不是瞬时的,claude 要落盘、要退出,
花的时间超过一个 2 秒 tick;这期间每次扫描都还能在进程表里看到它、还带着
它的 binding。守卫只挡「更老的扫描」,挡不住「刚好在杀的过程中取的扫描」。

真机证据:21:15:47 pane 384 被回收(30 分 22 秒闲置,门槛到点),
`dormant.tsv` 同一分钟内被重写成空,pane 随后落到 `empty` 而不是
`dormant` —— 唤醒路径没了,那个 session 只能手敲 resume 找回来。

守卫改成 `created_at + KILL_GRACE`(15 秒)。多留几秒的代价是零:唤醒前
本来就要问内核这个 pane 是不是真没有 claude,残留的记录不会误触发。

### 0.7.61

**闲置时钟真正跨重启活下来了** —— 上一版把它自己弄丢了。

`pane-state-clock.tsv` 存的是「这个 pane 从什么时候起是这个状态」,让分钟
级的门槛能熬过一次静默更新。0.7.60 加的 PTY 观测把它废掉了:新进程不知道
每个 pane 上次说话是什么时候,于是所有 pane 的安静计时从零开始 —— 头 30 秒
一律判忙。忙跟时钟里写的状态不是同一个,于是刚恢复的时钟当场被丢掉。

实测证据:这次装机日志里 `clocks_restored panes=18` 紧接着 18 条
`to=busy:output held_s=0`。18 个时钟恢复了,18 个立刻作废。后果不只是暗一下 ——
我每 20 分钟装一次机,30 分钟的回收门槛就永远到不了。

时钟文件加第四列存 bytelog 大小。启动时大小没变 = 这个 pane 一个字都没说过,
安静计时直接按已经满了算。旧的三列文件照常读(第四列缺就当没有,那些 pane
的计时从现在起算)。

### 0.7.60

**pane 自己在往终端上写字,就是在忙** —— 记录和进程表都看不见这件事。

屏幕上的证据:一个正在思考的 claude,上一轮已经结束(所以记录读起来是
「在等用户」),底下只挂着常驻工具(所以进程表是空的),CPU 增量在容差内
—— 三项检查一致地判它在歇着,于是它被调暗了,而它的转圈动画每秒往终端写
好几次。

补上第三个观测:pane 自己的 bytelog 大小。L3 把 PTY 吐出的每一个字节都追加
在那里,大小就是「这个 pane 说了多少话」的累计值,每轮扫描一次 `stat`。
本机实测十二秒:思考中的 claude 写了 695 字节(它的转圈),两个在干活的写了
约 10 KB,一个真在等用户的写了 **0**。

安静门槛 30 秒 —— 比它要看的东西宽裕得多(转圈一秒好几次),但目的是熬过
「两帧之间停一下」的程序,不是抢反应速度;下游真正会动作的门槛都是分钟级。

已停放的 pane 是唯一例外:画面冻着,唯一可能写字的是我们自己敲进去的那行
resume,所以它保持 Dormant。

### 0.7.59

闲置回收门槛 30 分钟,**外加两道否决:自动运行的和自己定了闹钟的不回收**。

门槛从 20 分钟改成 30(用户拍板)。但门槛本身管不住真正危险的那一类 ——
一个 claude 可能整晚在 `/loop` 里跑,每轮之间安静十几分钟;它的记录看起来
跟一个等用户的 claude 没有区别,而回收它等于掐掉一次正在进行的自动运行。

所以判定里加了两道否决,任一命中就永不回收:

- `work_in_flight` —— claude 底下还挂着真在干活的进程。工具进程(`*-mcp`、
  `rust-analyzer`、`caffeinate`、没有子进程的 shell)不算,它们本来就一直
  在;剩下的任何后代都算。
- `own_timer` —— 记录尾部出现 `ScheduleWakeup` 调用或自动运行的哨兵字符串,
  说明这个会话自己安排了下一次醒来,安静只是两次之间的间隙。

两道否决都会写进 `blocking_reason`,所以日志里能直接看到某个 pane 为什么
没被回收,不用回头猜。

### 0.7.58

后退等级跟着会话生命周期走,不是掐表:提示符上的 shell 不算闲。

### 0.7.57

L1 只报「这个 pane 闲不闲」,**不透明度阶梯归渲染层**。

上一版 L1 直接送 0.18 的 alpha,等于让 L1 决定看起来多暗。但「多暗」取决于
**这是不是用户正在用的那个 pane** —— 那是渲染层才知道的事(L1 没有焦点
信息,也不该为了这个去要)。

现在 L1 送 `IDLE_MARK`(1.0),含义是「状态机认为它在歇着」;深浅由
`render_metal::attention_scrim` 决定(见 L2 0.12.76)。判据仍在 L1(歇满
5 分钟、或已停放),表现仍在 L2,各自只做自己知道的事。

### 0.7.56

只在**内核确认** pane 里真的没有 claude 时,才给它挂唤醒会话。

重新挂载唤醒(重启后)原来只看扫描的绑定表:「没有绑定」就当「没有
claude」。但绑定会比重启慢一两拍 —— 在那个窗口里给一个**已经有活 claude**
的 pane 挂上唤醒会话,下一次聚焦就会把 `claude --resume …` 敲进那个正在
跑的 claude 的输入框里,停在那儿等用户按回车。

这正是用户报的「必须要回车才能完成 resume 的输入」的形状。这次真机上没
撞到(19:07 那次唤醒查下来是正常的:焦点触发 → 注入带 profile 的 resume →
2 秒后 `session.bound P1@opus`),但触发条件随时都在。

改成问内核:重新挂载前走一次 proc 表,pane 底下真没有 claude 才挂;
`shell_pid_for` 查不到那个 session(0)也拒绝 —— 查不到的 pane 不等于空的
pane。一次 proc 表遍历换掉一整类「把命令敲进别人输入框」的 bug。

### 0.7.55

resume 把 profile 带回来;读不到 profile 就不回收。

用户报的:「之前是 Px 就要用 claudex 来 resume」。之前的实现是拿
`profile_num` 拼 `claude<N>`,而 `profile_num` 读不出来时(255)会**悄悄退回
plain `claude`** —— 那不是降级,那是把会话恢复到**另一个账号**下面。

两处改:

1. **不再拼别名,直接用观测到的 `CLAUDE_CONFIG_DIR`**。`claude1/2/3` 是
   用户 rc 里的交互别名
   (`alias claude1='CLAUDE_CONFIG_DIR=~/.claude-profile-1 claude'`),
   复现别名要赌那个文件此刻仍然定义着它;而我们在杀进程之前本来就从活
   进程上读到了那个变量。现在 resume 行是
   `CLAUDE_CONFIG_DIR='<原样的目录>' claude --resume <uuid>` —— 别名自己
   的展开,写明白了。目录随 dormant 记录落盘(第 5 列),重启后唤醒也对。
2. **读不到 profile 就拒绝回收**(`hibernate.unknown_profile`,变化才落)。
   不回收的代价是内存;回收错的代价是用户的会话出现在别的账号下。

目录会原样进一条 shell 命令行,所以先过 `shell_safe`:带引号、反引号、
`$`、换行、超长的一律拒绝 —— 拒绝而不是转义,这种路径不值得猜。

顺带记一条实测:`proc_env_value` 对**加固签名的系统二进制**读不到环境
(argv 能读、env 被屏蔽),而真 claude 是普通用户态二进制所以读得到 ——
这就是为什么生产里 badge 一直能显示 P1/P2/P3,而全链路测试里用
`/bin/sleep` 当替身时读到的是 None。测试因此显式提供 profile,并断言
唤醒时把它原样带了回去。

### 0.7.54

休眠时画面冻在原样;闲置的 pane 变暗。

用户看到的问题:回收时 claude 退出 → 露出 zsh 提示符 → 唤醒时又当着面把
`claude --resume …` 敲进去。那些都是机械过程,不是他留在屏幕上的东西。

两层分别给两件事:

- **cc 层:冻住画面**。`HibernatePaneSession` 现在带
  `PANE_SESSION_CAP_FREEZE_GRID`(这个能力早就有,profile cycle 一直在
  用,我第一版判断错了没要)。从发出 SIGTERM 那一刻起 L2 保持最后一帧,
  杀进程和 resume 都发生在画面背后,直到 claude 重新画出来才恢复实况。
- **zsh 层:只是透明度**。新增 `PaneIdle` 帧(L1 → L2,67):安静满 5 分钟
  的 pane 拿 0.18 的 scrim,忙的和状态不明的一律不暗(把在干活的 pane 画暗
  是对 pane 撒谎)。dormant 的 pane 立刻暗 —— 它不是在休息,是被停放了,而
  画面又冻着,那层暗是屏幕上唯一说明这件事的东西。

这就是两层叠起来的样子:**cc 冻画面 + 静默进出,generic 加一层暗**。
渲染侧三种「后退」(拖拽源 / 空座位 / 闲置)共用一个 scrim 原语,**最深者
胜**而不是叠加 —— 两层 0.22 叠出来是 0.39,那是另一个颜色。

判据在 L1(`idle_dim_for`),L2 只负责画;帧只在值变化时发,一个歇了一小时
的 pane 花一帧,不是一秒一帧。

### 0.7.53

聚焦即开始恢复;claude 一回来就交还键盘。

用户实测反馈两条:「必须回车才能完成 resume 的输入」「恢复时间非常长」。
查下来是同一个根:唤醒只由**按键**触发,而且唤醒后我们还会**继续吞键最多
20 秒**——用户按下的回车被吃掉,claude 起来了键盘还不还,两件事叠起来就是
「又要回车、又很久」。

- **焦点触发**:新增 `PaneFocused` 帧(L2 → L1,66)。L2 在主循环里比较每个
  窗口的焦点 sid,变化才发一帧(焦点按人的速度动)。L1 把它派发给 pane 自己
  的 PaneSession(`on_focus`)和所有插件(`Plugin::on_pane_focused`)。
  休眠的 pane 一被看,就开始 resume —— claude 的冷启动跟用户读屏并行,
  而不是等他们先试着用一下。
- **一回来就放手**:`Waking` 阶段原来死等 20s 才结束,现在每拍检查
  pane 里是否又有 claude(只在唤醒在飞时才走一次 proc 表),有就立刻
  `end()`,把键盘还回去;30s 只作为兜底。

按键路径保留(pane 已经是焦点时不会再有焦点变化事件),两条路共用同一个
`begin_wake`。

PTY 全链路测试跟着改:唤醒改成走 `on_focus`,并且断言 shell **真的执行了**
那一行 —— 原来只断言「回显里出现了 --resume」,而回显跟「停在提示符上等
回车」长得一模一样,正好漏掉用户报的这个 bug。顺带发现测试里 PATH 没设,
唤醒实际启动的是**真 claude**(输出里能看到它的信任提示),现在把替身放进
pane 的 PATH。

### 0.7.52

dormant 记录不许被「创建它的那次扫描」判死。

第一次真回收发生了(10:14:11,三个 session,idle 5408s,cpu_delta 1-2ms
over 30s,一秒内全部进 dormant,claude 从 4.64GB 降到 3.92GB)。但
`dormant.tsv` **是空的**,而且那三个 pane 的合成状态走成了
`awaiting_user → contradiction:idle/awaiting_user → empty`,不是
`dormant`。

根因:`rearm_dormant` 用 `result.new_mapping` 判断「claude 是不是回来
了」,而触发这次回收的那份扫描是**杀之前**采的 —— 那时绑定当然还在。于是
刚建好的记录当场被判成「回来了」删掉:内存里没了、盘上写成空、
`activity_for_unbound` 于是报 `Absent` 而不是 `Dormant`。

后果不是理论上的:唤醒会话还挂在内存里(所以按键仍能唤醒),但**下一次
自更新之后就没了** —— 那三个 session 只能靠手敲 resume 命令找回。

改:`DormantRecord` 记 `created_at`,`ScanResult` 记 `scanned_at`,
一条记录只能被**比它新**的扫描判决。落盘加第 4 列,老文件缺列时读作
epoch(即「足够老,正常判决」)。

现场那三条记录我按日志里的 uuid 和 profile 手工写回了 dormant.tsv,
新版本会在下一轮扫描里重新挂上唤醒会话。

### 0.7.51

CPU 基线跨扫描保留 —— 否则回收**从来不可能触发**。

这个 bug 是上一条那个刷屏日志喊出来的:日志里出现
`shelld_session=414 — cpu sample spans only 2s`,而 414 已经过了一小时
阈值。也就是说它早该被回收,卡住它的是我自己那条采样窗规则。

根因:扫描每 ~2s 一轮,而基线在**每一轮**都被替换,于是
`since_sample` 永远 ≈ 2s,而 `should_hibernate` 要求 ≥30s —— 这个门
任何 pane 都过不去。四条判据里有一条恒假,整个回收等于没上线。

改:基线只在超过 `CPU_BASELINE_WINDOW`(60s)之后才换,于是 delta 的含义
变成「最近一分钟烧了多少 CPU」,正是要问的那个问题;门槛用窗口的一半
(30s),这样 pane 在一个窗口走到一半时就能符合条件,不必等一个完整的新
窗口。

单测钉住:2s 之后的扫描不许移动基线,超过窗口才移动。

教训跟这一整轮同一条 —— **把「为什么还没发生」写进日志,是唯一能发现
「它永远不会发生」的办法**。四条判据全绿的单测覆盖不到「窗口本身被
调用节奏压成了 2 秒」这种事。

### 0.7.50

`hibernate.waiting` 的去重键改成**不含数值**的类别。

0.7.48 号称「原因变化才落一行」,实测 25 分钟落了 **3030 行** —— 因为
原因串里带着当前闲置秒数(`idle 532s of 3600s`),每次扫描都在变,于是
「变化才落」= 每次都落,约 2 行/秒、1MB/小时。8MB 的日志一天不到就会把
真正的历史轮转出去。自己写的判据被自己的格式串废掉。

现在 `blocking_reason` 返回 `(类别, 行)`:类别是
`below_threshold` / `short_cpu_sample` / `cpu_busy`,**不含任何数值**,
去重按类别;行里照样带数字,只是只在类别变化时打一次。

单测钉住这条:`idle_for(1800)` 和 `idle_for(1801)` 必须给出同一个类别。

### 0.7.48

候选 pane 会说自己在等什么。

`hibernate.waiting`,只在**原因变化**时落一行:「idle 1800s of 3600s」/
「cpu sample spans only 5s」/「subtree burned 900ms of cpu in 60s」。
只对**已经安静且在等用户**的 pane 说话 —— 比这更忙的不是候选,没有什么
要解释;否则窗口里每个忙碌 pane 都会一秒一句。

理由很实际:在第一次真回收发生之前,日志里什么都没有,而「什么都没发生」
读起来跟「策略坏了」一模一样。现在这两件事分得开。

### 0.7.47

idle 时钟跨 L1 自更新存活。

装上 T1 之后翻日志发现的:今天有三个 pane 在 `awaiting_user` 上分别坐了
**4.6 小时**、2.0 小时、1.9 小时,而 `hibernate.*` 一次都没触发。时间点
对得上 —— 它们都结束在 hibernate 上线之前,所以那次没触发是对的。但顺
着看下去暴露了真问题:**状态机在内存里,每次 L1 self-execv 都把每个 pane
的时钟清零**。我今天装了 6 次,等于把所有 pane 的闲置计时按了 6 次复位;
一个「一小时」的阈值在这种节奏下永远够不着。

闲置是 pane 的属性,不是「盯着它的那个进程」的属性。

- 每次有状态提交时把 `(sid, 状态标签, 起始时刻)` 写进
  `pane-state-clock.tsv`(原子 rename;稳态下的 sweep 一个字节都不写);
- 启动时读回来,某个 pane **重新确认**出同一个状态时,把它的起始时刻
  倒回去 —— 注意仍然要走满 `CONFIRM_TICKS` 的确认,只是不假装它刚醒;
- 三条不信的规矩:标签对不上就丢(不留给后面的状态误用)、超过 7 天不信
  (那更像是谁出错留下的文件)、时刻在未来不信。

存的是**标签**而不是枚举:这个文件会活过写它的那个版本,标签对不上时
自然失配,正好是想要的结果。

### 0.7.45

休眠变成一等状态 —— `Dormant` 进状态机,决策不再读插件私有集合。

0.7.43 的实现有个分层缺口:claude 被收掉之后插件报 `Absent`,合成出来是
`Empty` —— 从状态机看,这个 pane 跟一个**干净的空 pane 长得一模一样**。
不重复回收它靠的是插件私有的 `dormant` 集合,一条绕过状态机的旁路。

后果很具体:等 zsh 层(冷存 shell)建起来,它看到 `Empty` 就会去拆这个
pane,而且**不知道**这里躺着一个可以 resume 的 claude 会话 —— 那条 resume
会静默丢掉。

现在:

- `Activity::Dormant` 由插件上报(`activity_for_unbound`:同样是「没有
  绑定」,parked 的报 Dormant,真空的报 Absent);
- 合成出 `PaneStatus::Dormant` —— **安静,但带着一笔待恢复的债**;
- `owes_restore()` 是给下一层看的判据:安静态有三个,只有这个欠着东西;
- 回收判据不再有「这个我是不是已经收过了」的私有检查 —— `Dormant` 不是
  `AwaitingUser`,它在跟所有人一样的证据上被同一个 gate 排除。

顺带钉住一条时序:插件报 `Dormant` 而内核说有作业占着 tty 时(claude 被
唤醒、或用户自己起了东西,插件还没扫到),**内核赢** —— pane 读作
`Busy(fg_job)`,插件下一轮自己纠正。

全叉乘测试从「有且只有两对是安静的」改成三对,并断言只有 `Dormant`
那对 `owes_restore()`。

### 0.7.44

idle 闭环在**自己的 PTY** 上跑通了 —— 回收 → 休眠 → 按键 → resume,
一个测试走完。

0.7.43 收尾时我把「必须在 marspot 的窗口里验」当成了前提,于是绕去合成
键盘事件、绕去往 L3 的 UDS 口注入(那条路是死的:`uds_server.rs:10`
写着 accept 后只记日志就丢)。前提本身是错的:测试自己 `forkpty` 出来的
PTY,master fd 就在手里,`write()` 就是键盘。

`the_whole_idle_loop_runs_on_a_pty_this_test_owns` 用的全是真机件:

- 真 zsh 跑在测试自己的 PTY 上,真 registry entry(entry.toml)让扫描
  像发现真 pane 一样发现它;
- 真扫描完成绑定(替身 claude + 真实形状的 transcript);
- 真状态机吃 `pidtree::observe_pane` 的真观测,连喂 3 拍确认到
  `quiescent`;
- 真策略发真 SIGTERM —— 断言那个进程**真的没了**,而且 shell 真的把 tty
  收了回去;
- 真唤醒:一个按键走 `HibernatePaneSession::on_user_key`,断言写进 PTY
  的字节正好是 `claude --resume <uuid>\r`,并且 shell 把它回显了出来。

三个坑都是实测出来的,顺手记着:

1. 替身 claude 不能用**拷贝**的系统二进制 —— 拷完 exec 会被签名校验杀掉
   (`zsh: killed`),要用符号链接跑原始已签名的那个;
2. 判据看的是 **argv[0]**,所以 `#!` 包装脚本会得到 `/bin/sh` 并被正确
   拒绝,夹具得是真二进制;
3. `proc_cwd` 报的是解析过的路径(`/private/var/...`),而 `temp_dir()`
   给的是符号链接那个(`/var/...`)—— 不 canonicalize,fixture 的
   transcript 就放在扫描永远不看的目录里。

唯一没被这个测试覆盖的是 claude 自己对 `--resume` 的反应,那属于 claude;
字节送达的那条路是 profile cycle 每天在生产里走的同一条。

### 0.7.43

idle 的 claude 自动回收,按键唤醒同一个 session。

状态机把「这个 pane 到底在不在干活」答清楚之后,idle 这件事闭环:静止
超过阈值就 SIGTERM 收掉 claude(PTY 和 scrollback 留着,那只值 3.5MB),
badge 变 `zZ <uuid>`;用户在那个 pane 里按任意键,自动
`claude[N] --resume <uuid>` 把同一个 session 拉回来。真机现值:12 个
claude 占 4.1GB。

**阈值 1 小时,按代价选的**:prompt cache TTL 就是一小时,过了这个点
下一次请求本来就要重付全量 input token —— 回收的边际成本只剩冷启动。
低于 TTL 去收是花钱换内存。`MARSPOT_CC_IDLE_HIBERNATE_S=0` 关掉。

**四条判据全是否决项**:状态机 `quiescent` 且状态是 `AwaitingUser`
(`Empty` 也安静但那是「没有 claude 可收」);该状态已持续 ≥ 阈值;
claude **子树 CPU** 自上次采样几乎没动(容差 50ms);采样窗口 ≥ 30s。
第三条是实测逼出来的 —— 每个活着的 claude 都常驻子进程(smix-mcp /
rust-analyzer / caffeinate / 长命 zsh),「没有子进程」当判据永远不成立。

**发信号前复核 pid**:重读 cmdline 确认还是 claude。pid 会被内核回收,
拿旧 pid 发 SIGTERM 的下场是打到无关进程。顺带量清楚一件事:活 claude
的 `ps comm`(argv[0])是 `claude`,而 `pbi_comm`(可执行文件名)是版本
号 `2.1.220` —— 守卫读 argv 才对。

**唤醒路径跨 L1 自更新不丢**:execv 换掉所有 PaneSession,而 pane 还在、
claude 已收 —— 不补的话下一次按键当 shell 命令跑掉。dormant 集合落盘,
下一轮扫描重新挂上;解码时校验 uuid 字面(它要进 shell 命令行),坏行
丢掉不修。

策略读**本轮**扫描的 binding 而不是上一轮的 `last_meta` —— pid 马上要被
发信号,该用手上最新的那个。

验证:906 tests green。策略层单测从否决面写满(低于阈值 / 机器不确定 /
没有 claude / 子树在烧 CPU / 采样跨度为零 / 无 profile 时 resume 命令
必须是 `claude` 不是 `claude255` / 坏 dormant 行不许进 shell 命令),
外加三个「真资源」测试:真起一个 argv[0]=claude 的进程走完整回收路径并
**确认它真的被信号杀掉**、pid 不再是 claude 时拒绝发信号、claude 回来
后 dormant 记录必须出列。

**未验证**:整条真链路(真 claude 被回收 → 按键唤醒)还没在装机上跑过。
沙箱注入这条路走不通 —— L3 的 UDS listener 目前「accept 后只记日志就
丢」(`uds_server.rs:10`),外部注不进去。

### 0.7.42

`child_pids` 的返回值是**条目数**不是字节数 —— 0.7.41 那个挂起作业检测
一直是死的。

真 PTY 上跑一遍就露了:起 `zsh -f`、`sleep 30`、`^Z`。表遍历看得见
`comm=sleep status=4`(SSTOP),而 `observe_pane` 报 `Idle` —— 刚修的那个
误放行一点没修上。

根因是 `proc_listchildpids` 的两条约定跟隔壁 `proc_listpids` 不一样,
我照后者写了:

1. 返回值是写入的**条目数**,不是字节数。一个子进程返回 1;除以
   `size_of::<pid_t>()` 得 0 —— 于是「少于 4 个子进程的进程」一律报告
   没有子进程。
2. NULL 探大小那次调用不给这份结果的大小:对一个只有一个子进程的进程
   它回答 971。所以没有可探的大小,直接给缓冲区、满了翻倍。

两条都是实测出来的。新增两个「合成数据永远抓不到」的测试:
`child_pids_finds_a_real_child_process`(真起 `/bin/sleep` 钉约定)和
`observe_pane_sees_a_suspended_job_on_a_real_pty`(真 PTY + 真 zsh,写
0x1a 让行规程翻成 SIGTSTP,走用户那条路)。实测链路:Idle →
Foreground{sleep} → ^Z → PromptWithJobs{stopped:1} → Busy(StoppedJobs)
→ fg → Foreground。

记一笔:测试第一版会挂死在 `Pty::drop`(pane 里留着 stopped 作业时),
13 分钟后被 nextest SIGKILL。测试现在自己收尾(SIGCONT + SIGKILL 到作业
组)。`Pty::drop` 遇到 stopped 作业该不该自己扛得住是 L3 teardown 的
语义问题,单独记着,没混进这个改动。

教训与 0.7.37-0.7.39 那三轮同一条,换了个面:**外部接口的行为要拿真
东西验**。那三轮是 jsonl 的字段顺序,这轮是 libproc 的返回值语义;两次
都是单测全绿而真机全错。

### 0.7.41

pane status 从「采样分类器 + 逐拍 diff」改成**真正的状态机**。

旧的每轮从头算一个标签、跟上轮比。够用来看,不够用来动:没有合法转移
的概念,两层矛盾无法表达,宣布一个 pane 安静之前没有确认,而且「这里
没有 claude」是用「map 里没这一项」表达的。

`marspot::pane_state`:

- **两个输入字母表,一个合成态**。`Generic`(内核视角)× `Activity`
  (插件视角)经 `compose` 一张表折成 `PaneStatus`。表写在文档注释里,
  单测对**全叉乘**断言「有且只有两对是安静的」。
- **矛盾是一个状态**。内核说这个 pane 一个进程都没有、插件却说 claude
  正在半轮里 —— 有一边过期了(通常是绑定活过了进程)。`Contradiction`
  显式留痕,任何策略都不许动它。
- **缺席是一个状态**。`Activity::Absent`(这个 pane 没有 claude)跟
  `Unknown`(还没人看过)分开;插件对每个存活 session 都报,包括没绑定
  的那些。
- **非对称滞回**。进安静态要 3 拍连续一致(`CONFIRM_TICKS`),出安静态
  一拍即出。在只闪了一下的 pane 上动手是昂贵的错误。
- **`quiescent()` 是唯一决策面**,消费者不许 match 裸变体。

观测层同时补上唯一会误放行的盲区:读 `pbi_status` + 数 shell 手里的
作业,`^Z` 挂起和 `cmd &` 后台作业不再和空 pane 同形。

分层也摆正:插件只 `report_pane_activity` 报自己那层,合成、矛盾判定、
滞回、决策全在 shell —— 插件不再自己拼 `fg,working` 字符串,也不再决定
什么算可动。`PaneForeground` / `pane_foreground*` 整套删掉。

### 0.7.40

status 两层都带上「这个状态持续了多久」。

`AtPrompt` 本身不可行动,`AtPrompt 已经两小时`才是能建策略的事实。两层
都存一个起始时间戳:

- 通用层:`PaneStatusTracker` 存 `(状态, since)`;`PluginHost::pane_status`
  返回 `(状态, 已持续多久)`,age 在调用时算,不是存下来的。
- cc 层:`cc_status.changed` 的日志行尾多一个 `(held Ns)`。

关键语义:**时间戳量的是状态,不是扫描**。状态没变就保留原来的戳 ——
每轮扫描都刷新的话,「闲了两小时」这件事永远观测不到。单测钉了这条
(连扫三轮不变,戳必须不动;真变了才重置并报出旧状态持续了多久)。

`shell.pane_status.changed` 同样多一个 `held_s=` 字段。这两个数就是
以后定阈值的原始数据 —— 「这台机器上的 pane 实际闲多久」不该靠估。

### 0.7.39

记录的 type 必须按**顶层字段**读 —— 0.7.38 装上去又被真机推翻一次。

0.7.38 之后 9 个 pane 全是 `fg,working`,`awaiting_user` /
`tool_pending` 一次都没出现过。对着真文件跑等价逻辑才看清字段顺序:

```
user:      {"parentUuid":…,"promptId":…,"type":"user","message":{…}}
assistant: {"parentUuid":…,"message":{…,"type":"message",…},…,"type":"assistant",…}
```

assistant 记录把 `message` 放在**自己的 `type` 前面**,而 `message` 里
有 `"type":"message"` —— 于是「行内第一个 type」把每条 assistant 记录都
读成 `message`,被当记账记录跳过,退回到它前面那条 user 记录 →
`working`。user 记录恰好 type 在前,所以它们是对的,错误看上去就成了
「全是 working」。

改成 `top_level_str`:带深度和转义的扫描,只认深度 1 的键。顺带把
「文本里引用了 `"type":"tool_use"` 字面量」这条也钉进测试 —— JSON 里
那种引用是转义过的,不转义的模式匹配不到(marspot 自己的开发 session
里就有这种文本)。

装之前先拿真文件对账过:10 个 session 里 tool_pending / working /
awaiting_user 三种都出现,不再是一边倒。这一步是前两轮都跳过的。

三轮教训同一条:外部格式的判据,**单测喂原样行、上线前对真数据跑一遍**。
自己编的形状(0.7.37)、只补了尾巴没补字段顺序(0.7.38)都不够。

### 0.7.38

cc status 跳过记账记录 —— 0.7.37 装上去当场被真机推翻。

装完 25 秒后看日志:9 个 pane 里 **7 个是 `fg,unknown`**。根因是
claudecode 在 assistant 收尾消息**之后**还会写一条
`{"type":"system","subtype":"turn_duration",…}`,而 0.7.37 只看字面上的
最后一行 —— 于是「一轮结束、等用户」这个最常见的静止态,恰恰是唯一读不
出来的那个。

改成从尾往回找第一条 `assistant` / `user` 记录(最多回溯 64 行),中间的
system / meta 记录跳过。同时把「记录自身的 type = 行内第一个 `"type"`」
这条抽成 `record_type`,因为真实记录前面还有 `parentUuid` /
`isSidechain` / `promptId`,而内容块的 type(`tool_use` / `tool_result`)
嵌在后面的 `message` 里。

4 个新单测用的是**真实 transcript 的原样行**(含前缀字段和
turn_duration 尾巴)。0.7.37 的测试是自己按 `"type"` 开头编的形状 —— 形状
对了、字段顺序和尾巴都不对,所以测试全绿而真机全错。这条教训值一句:
jsonl 这种外部格式的判据,单测必须拿原样行喂。

### 0.7.37

pane status 的 cc 层:claude 在那个 pane 里到底在干嘛。

通用层(0.7.34/0.7.35)只知道「有作业占着这个 tty」。cc 层从 session
jsonl 的**最后一条记录**细化:

| 末条记录 | 结论 |
|---|---|
| assistant + `text`/`thinking` | `awaiting_user` —— 轮次结束,球在用户脚下 |
| assistant + `tool_use`,后面没东西 | `tool_executing` / `tool_awaiting_approval` |
| user(真提示词 **或** tool_result) | `working` —— assistant 欠下一条记录 |
| 其它(system / 读不到 / 单条超 32KB) | `unknown` |

**只有 `awaiting_user` 表示「没有东西在飞」**,其余全部(含 `unknown`)
都必须按「别碰这个 pane」对待 —— 这条写在类型的文档里,因为第一个消费者
就是要拿它决定能不能杀 claude。

`tool_use` 那条的二分靠进程:工具真在跑时,claude 底下会有一个**比那条
记录年轻**的子进程;停在批准提示上则一个都没有。长命子进程(MCP server,
随 session 一起起来的)比记录老,不会误判。用的是扫描器本来就走过的那张
proc 表,没有新增开销。

记录形状是从真实 transcript 里抄的,不是猜的 —— 分类完全押在 `"type"`
出现的位置上,形状猜错测了等于没测。6 个新单测。

日志同通用层:只在变化时落 `plugin.claudecode.cc_status.changed`,写成
`<fg|bg>,<activity>` 两层一起报 —— 通用层答「claude 是不是占着键盘」
(被 Ctrl-Z 挂起 / 退到后台的 claude 读作 `bg`,哪怕 jsonl 还停在半轮),
cc 层答「它在里面干嘛」,互相推不出来。

### 0.7.36

静默更新不再把第二个窗口的位置尺寸抹掉。

现场:每次 `install-local` 之后,主窗口(一堆 session)好好的,另一个
窗口回到默认的 1200×800 并且换了位置。日志里每次更新都有一行
`shell.window.restore_no_frame frame_index=1` —— 从 07-28 起就在,一次
没落空。

`window-state.bin` 是按索引配对的列表(entry i ↔ 窗口 i)。L1 self-execv
把所有 NSWindow 都拆了,继任进程先开窗口 0(用 `MARSPOT_RESTORE_FRAME`
里交接过来的 frame),`window_opened` 顺手存一次几何 —— 那一刻
`self.windows.len() == 1`,而旧的 `save_window_frames` 写的是「当前活着
的窗口」整表,于是文件被一条 entry 覆盖,**窗口 1 的几何在 core 来要它
之前就被删了**。几十毫秒后 core 请求 frame_index=1,读到 None,窗口就
开在默认矩形上。

改成按索引合并、只覆盖不缩短(`merge_window_frames`,5 个单测):
- 活窗口占自己的槽位;
- 活窗口还没报 frame 的槽位保留磁盘上的值 —— 旧代码那个 `filter_map`
  会把它整个跳过,于是后面每个窗口的 entry 都往前挪一格;
- 磁盘上多出来的尾部保留 —— cold launch(含 execv 后那次 boot)时,那
  正是 core 马上要问的几何;
- 都没有的槽位写一个零尺寸洞,而不是跳过 —— restore 侧本来就有
  `w > 50 && h > 50` 的判据,读到洞就等于「这个窗口开默认矩形」,但索引
  不会错位。

窗口关掉后它的几何留在文件里不再清理,这是有意的:core 是按它自己的
per-window 布局记录来驱动 restore 的,没有窗口的索引不会有人来问。

同一条路上第二个错配也修了:交接给继任进程的 frame 原来取
`ctx.window_frame_pt()`,而这条路跑在 redraw pump 上,ctx 是「本轮恰好
驱动 tick 的那个窗口」—— 两个窗口开着时,boot 窗口可能被告知开在第二个
窗口的位置上。现在明确取 `windows[0].last_saved_window`。

### 0.7.35

pane status 通用层接上 tick:每秒一扫,变化才落日志,插件能读。

- `pane_status::PaneStatusTracker` 挂在 `poll_supervisor`(~250ms)上,
  自己按 1s 闸门,来源是 `session_registry::list_session_entries()`
  (通用来源,不经任何插件),每 pane 1-2 个 syscall —— 用的是新加的
  `pidtree::pane_foreground_probe`,不是 `list_all_procs`(那个每个
  host 上的 pid 一次 `proc_pidinfo`,~600 次/秒,不是给每秒扫的形状)。
- 只在**变化时**落 `shell.pane_status.changed`(INFO,因为运行时默认
  级别就是 Info,DEBUG 在真机上不存在)。一个 pane 在提示符上坐一小时
  只打一行。
- `PluginHost::pane_status(sid)` 给插件读快照(READ_PANE_INFO 权限),
  在插件 tick **之前**扫,所以同一轮里插件看到的是本轮的值。
- 上界:每轮用 registry 当前的 sid 集合**替换**整张表,pane 关掉下一轮
  就忘掉,不是只插不删(CLAUDE.md §3)。
- `sweep()` 返回 `Option<Vec<Transition>>` 而不是 `Vec`:空 vec 不等于
  「没事发生」—— pane **关闭**会丢 key 却不产生 transition,把空 vec 当
  no-op 会让已关 pane 的最后状态永远留在插件读到的那张表里。None =
  被闸门跳过,Some(空) = 扫过但没变,仍要重新 publish。

`Job` 的形状同时改成 `{ pgid, leader: Option<JobLeader> }`:一出现第二个
生产者(单 pid 探针)就发现旧形状撒谎 —— 探针认不出 leader 时,pane
仍然**在跑东西**,不能退成 `Unknown`。「组在前台但认不出成员」是真状态,
现在是一等公民。

### 0.7.34

pane status 的第一层:pidtree 现在认得「这个 pane 前台是谁」。

`proc_bsdinfo` 里本来就有 `pbi_pgid`(进程自己的 group)、`e_tdev`
(控制终端)、`e_tpgid`(该终端的**前台** group,等于对着那个 tty 调
`tcgetpgrp()`)。`list_all_procs` 每个 pid 都已经在取这个 struct,只是
从来没读这三个字段 —— 所以 `pane_foreground(shell_pid, &procs)` 是
**零新增 syscall**,一次 proc 表扫描喂所有 pane。

判据:tty 前台 group == shell 自己的 group ⟺ zsh 停在提示符;否则是
作业在前台,取 group leader(pid == pgid;leader 已退出但 group 还在时
退到该 group 里最老的成员)。同 pgid 号在别的 tty 上也会出现,所以匹配
必须带 `tty_dev` —— 不带就会把隔壁 pane 的作业算成自己的。

这一层**不需要 shell 配合**:zsh 什么都不用装,已经能分辨「在提示符 /
在跑 claude / 在跑 cargo」。它也刻意不认识任何具体程序 —— 插件层
(claudecode 读 jsonl)在 `Job` 之上细化,这里不长那种知识。

实测 18 个 pane:10 个 `Job`、8 个 `AtPrompt`,leader pid 与 `ps` 里的
claude pid 一一对上。顺带量到一件事:claude 的 `pbi_comm` 是
**`2.1.220`**(它 exec 的是版本号命名的文件),不是 `claude` —— 想知道
「这是哪个程序」必须读 `proc_cmdline`,`comm` 只能当粗标签。这条写进
了 `leader_comm` 的文档,免得下一个消费者踩。

### 0.7.33

只重编:window header 合并成一行(见 L2 0.12.68),`HEADER_PT` 在共享 lib
里从 62 变成 32。shell 侧行为不变 —— 它不读这个常量,只是把 NSWindow 交
给 core。版本号照样 bump:共享 lib 变了而某层没 bump,`install-local.sh`
会判该层 unchanged 跳过 staging,新二进制静默留在磁盘上。

### 0.7.32

claudecode 的 jsonl 扫描只走「有 pane 的项目」,每个项目留最新 4 个
session,并且会**收缩**。

三件事一起改,因为它们是同一个循环的三个面:

- **范围**:原来每 tick `read_dir` 整个 `~/.claude/projects`(这台机器
  ~50 个目录),其中只有 ~10 个有 pane 会被问到。现在按 pane 的 cwd 直接
  `join(encoded)`,不扫无关目录。为此把 pane 那一趟挪到 jsonl 那一趟
  **前面** —— 先知道要问哪些项目,再去读。
- **深度**:每个项目从「只留 mtime 最新的一个」改成留最新 4 个。一个候选
  的时候,同项目的两个 pane 里必然有一个拿不到 badge,哪怕它自己的
  session 是活的 —— 0.7.31 的互斥约束需要有备选才能发挥。
- **上界**:`seen` 原来只插不删,一个跑几周的 shell 会攒下它见过的每一个
  session 文件。现在每轮按存活集合 `retain`,上界 = pane 数 × 4;
  `model_cutoff` 同表清理。

范围收窄抵掉了深度加倍:10 个目录 × 4 个文件,比原来 50 个目录 × 1 个
文件读得还少。

### 0.7.31

claudecode badge 不再把同一个 session 发给两个 pane。

2026-07-30 现场:session 383 和 394 都 cwd 在 `qualcomm/insight`,badge
都是 `9e304c9a`。而 383 的 claude argv 明写 `--resume 9e304c9a`,394 的
claude 是 07:12 起的裸 `claude`,那个项目里根本没有 07:12 之后新建的
session 文件 —— 394 戴的是别人的号。根因:反查只按「项目目录 → mtime
最新的 session」,没有任何互斥,而 scanner 每个项目只留最新一个 session,
所以同项目的 N 个 pane 必然拿到同一个 uuid。

三条约束叠上去:

1. **argv 是唯一权威**。`argv_session_uuid` 在 claude 自身 + 它的子树里
   找 `--session-id`(fork-resume 时它才是真正在写的那个)和
   `--resume <uuid|path>`。daemon 形态(`claude daemon run` → pty host →
   version 二进制)把 flag 埋在好几层下面,所以要走子树 —— 但只走 claude
   的子树,不是整个 pane 树:后者要为 rust-analyzer / node / 每个 build
   job 都付一次 `KERN_PROCARGS2`。
2. **一个 uuid 只发一次**。先给能拿出 argv 证明的 pane,再让剩下的 pane
   在**未被占用**的候选里挑。没得挑就**不发 badge** —— 空着比戴错号诚实。
3. **活性闸**:候选 session 的 mtime 必须 ≥ 这个 claude 进程的启动时间。
   自启动以来没被写过的文件,不可能是它正在写的 session。少了这条,一个
   在「最新 session 是两天前」的项目里新起的 pane 会戴上那个死 session。

顺带 `pidtree::ProcRow` 带上 `start_unix`(`pbi_start_tvsec`)—— 这张表
本来就每个 pid 拉一次 `proc_bsdinfo`,启动时间白送,第 3 条要用。

分配顺序按 shelld session id 固定:同项目的歧义每 tick 解成同一个答案,
badge 在 pane 间来回跳比「这是个猜测」更糟。

### 0.7.30

IME 候选框跟着自己的窗口走。caret 走的是 core 的控制 socket,shell 在
`user_event` 里排空它 —— 那是关于**进程**的事件,永远派发给第一个窗口。
于是 `ShellInbox::CaretRect` 拿 `ctx` 当目标(单窗口时两者恰好重合),
每个窗口的 caret 都落到了窗口 1 的 view 上:窗口 2 对
`firstRectForCharacterRange:` 只能答零矩形,macOS 就把候选框停在屏幕角
落,不在光标底下;窗口 1 的锚点则被窗口 2 的坐标覆盖。

帧里本来就带着 window id,现在按它路由 —— 新增
`app::set_caret_rect_phys_for(window_id, rect)`,跟 `open_window` /
`close_window` 同样入队,在这次派发的尾部落到目标窗口的 view 上(处理
器整个跑在 `APP_STATE` 的可变借用里,不能就地再借)。旧 core 不带 id
时 `trailing_window_id` 仍退回 `FIRST_WINDOW_ID`。

`bin/test-multi-window.sh` 加一段验收:`MARSPOT_DEV_CARET_PROBE=1` 让
shell 拿 AppKit 问 view 的同一个问题去问它,打印锚点;两个窗口的锚点
必须各自落在自己的 frame 里。种子布局的两个 frame 不重叠,所以走错
view 的 caret 一定越界。

### 0.7.29

Probe 改成一轮覆盖两层。`install-local.sh` 总是把 shell 和 core 一起
staged,所以把它们放进同一个探测轮次:两层都通过时,**在 execv 之前**
先把 core promote 进 `current/`,后继 shell 起来就直接 spawn 新 core。

在此之前每次装机会起两次 core:execv 后的新 shell 从 `current/` 起了
*旧* core(core 那时还没 promote),几秒后 SIGUSR1 触发探测 + 换核再把
它退役。两次 core 启动、两次 L3 控制流重连,只为一次 install。

`SupervisorState::Probing` 不再带层参数,结果由 `ProbeVerdict {shell,
core}` 承载(`None` = 这一轮没探)。探测失败的那一层单独进隔离区,不
影响另一层。

### 0.7.27

共享 crate(terminal BCE)重链接.

### 0.7.26

RFC-006 —— mouseDragged 每 tick 解析悬停窗口并转其局部物理坐标(MouseDrag 尾巴),mouseUp 带落点坐标(窗内=局部/窗外=屏幕点),WindowOpenRequest 尾带开窗位置提示.

### 0.7.25

共享 lib(state v3)重链接.

### 0.7.24

RFC-005 步骤 5(拖拽半)—— MouseUp 尾追落点窗口 id:AppKit 把整场拖拽都发给按下的窗口,只有 L1 能在松开瞬间用 windowNumberAtPoint 说出指针真正落在哪扇 marspot 窗口(顶层判定,别家应用压着时=0).

### 0.7.23

RFC-005 步骤 5(菜单半)—— 处理 WindowCloseRequest(空窗自关,走红按钮同一条路,最后一窗不关)+ WindowOpenRequest 的 WINDOW_OPEN_USER 哨兵(用户动作绕过复原闸:safe mode 拦的是自动增殖,显式移 pane 不 spawn).

### 0.7.22

黑屏第二根因:present 循环挂在回调的 ctx 窗口上,而所有 wake 都按设计路由到 boot 窗口 —— 新窗口画了/装了 pair/有 presenter,但 present() 从未被驱动.改成 redraw 遍历所有欠帧窗口各自 present,每窗口首帧记 WINDOW_FIRST_PRESENT(E2E 断言升级到 present 侧,负向验过:锁回单窗口立刻 FAIL).

### 0.7.21

修 Cmd-N 新窗口黑屏 —— window_opened 只建了 surface 对没建 presenter(把 IOSurface 贴上 NSView 的那层只有 boot 窗口在 resumed() 里有),core 画得再对新窗口也是永远的黑矩形;promote 分支 presenter 缺失从静默跳过改成 ERROR,E2E 补 present 侧断言(WINDOW_PRESENTER_READY 必现 + no_presenter 必零,负向验过).

### 0.7.20

共享 crate(terminal CSI E/F)重链接.

### 0.7.19

2026-07-28 事故防线:崩溃循环刹车 —— 启动先读 launch journal,60 秒内 ≥3 次启动指数退避(2/4/8…封顶 30s),300 秒内 ≥5 次进 safe mode(MARSPOT_SAFE_MODE 传给 core,只 reattach 不新 spawn、拒绝复原额外窗口)+ CrashLoop 常驻 banner 明告用户.外部拉起器没有退避,刹车就装在 shell 自己身上.

### 0.7.18

修关第二个窗口崩溃 —— 程序创建的 NSWindow 默认 releasedWhenClosed=YES,close() 释放一次、我们的 Retained 再释放一次,窗口在关闭动画还持有它时就被释放,SIGSEGV 死在 -[_NSWindowTransformAnimation dealloc];关键窗口被关、焦点交接动画在跑时必现.加 setReleasedWhenClosed(false) + MARSPOT_DEV_CLOSE_EXTRA 测试缝(performClose: 走真按钮路径,先激活+置 key —— 后台关非 key 窗口不触发).

### 0.7.17

**重新启用 Cmd-N**(前置的 4d/6/4e 全部就位:开窗不再能收窄持久化)+ 加 MARSPOT_DEV_EXTRA_WINDOWS 开发缝(脚本没法给沙箱应用发按键,新窗口那条路否则无法自动化).

### 0.7.16

RFC-005 平权审计:core 热替换/崩溃重启后按窗口重放 SurfaceAttachWindow(原来只有启动窗口的 pair 进得去新 core,其余窗口从此没人画、pane 被当孤儿收进第一个窗口)、复原请求只认本次启动的第一个 core(否则替换 core 会把已经开着的窗口再开一遍)、banner 推给所有窗口的 presenter(原来只有第一个响应回调的窗口显示/清除)、stale-present 兜底改成每窗口(原来全局,一个 60Hz 刷的窗口让别的窗口永远不显得 stale).

### 0.7.15

RFC-005 步骤 6b — 收 WindowOpenRequest 复原窗口:按 window-state.bin 的第 N 条几何开窗(索引越界不算错,退回默认矩形);开窗路径与 Cmd-N 合流成 open_window_with_frame;新窗口一诞生就把自己的 frame 播种进几何缓存,列表里不留洞.

### 0.7.14

RFC-005 步骤 6a — window-state.bin v2:按创建序存所有窗口的 frame(原来是单帧,谁最后动谁赢,其它窗口的几何直接丢);v1 文件读成一元列表.

### 0.7.13

Cmd-N 崩溃修复 — 开/关窗口改走延迟队列(事件处理器都跑在 app-state 借用里,直接动它必 panic);surface 改在窗口真正存在后按它自己的度量创建.

### 0.7.12

RFC-005 步骤 4c — Cmd-N 开新窗口(L1 拥有窗口,自己拦截不绕 core)+ 关窗只关那一个窗口、最后一窗才退出;每窗口自带 surface pair.

### 0.7.11

自更新先验证后继者能起来再 exec —— 起不来就隔离,不再拿整个应用去赌(7/26 事故:adhoc 签名的后继者被 AMFI 杀,GUI 连同 16 个会话一起消失,日志全空).

### 0.7.10

RFC-005 步骤 4b — surface 对/presenter/首帧闸门收进 ShellWindow(每窗口一份);SurfaceReady 按 surface id 反查窗口,这正是它不需要在 wire 上带 window_id 的原因.

### 0.7.9

RFC-005 步骤 4a — 事件按窗口路由:view/delegate 带 window_id,ctx 即窗口身份,输入帧的 window_id 从 ctx 取(不再是常量).

### 0.7.8

RFC-005 步骤 2 — 输入帧带 window_id,attach 走新旧双发.

### 0.7.7

cc @model 切 profile 后不再显示旧模型 — 按 claude pid 变更设读取围栏.

### 0.7.6

共享 crate(layout 第 5 钮)重链接.

### 0.7.5

DevPanel 冷启动永远隐藏 — 不再恢复持久化的 visible(一次调试开过就每次自启的烦恼);位置尺寸照旧恢复,toolbar 第 4 icon 召唤.

### 0.7.1

snapshot v5 重链接.

### 0.6.88

笔记见 git blame.

### 0.7.28

更新链装 pre-swap probe。换核 / 自更新前先在**后台线程**跑一次候选
的 `--version`,一举两得:证明这个 image 能起来,并且把 macOS 的
Gatekeeper 评估在"现役进程还在服务"的时候付掉。`promote_pending`
用 `rename` 保 inode,所以预热的裁决正是真启动会命中的那个。

2026-07-29 事故:另一个项目的 cargo build 洗版 `syspolicyd`,新
`marspot-core` 在内核 exec 路径里卡了 204 秒 —— 而单核换核早已先把
老 core 杀了,窗口就冻在死 core 的最后一帧三分半钟,强关重开只是把更
多 core 排到同一个队列后面。老代码把这段间隙当成"~200-500ms",那是
个没有上界的假设。

`SupervisorState` 加 `Probing(ProbeLayer)` 态(127f3c9 砍 dual-core
时特意留下的枚举骨架)。探测期间老 core 照常画、照常收输入;探测失败
= 候选进隔离区、现役一秒没停。**刻意不设超时**:超时只能在"放弃更
新"和"照杀不误"之间二选一,后者正是冻屏那条路,而慢裁决的代价不过是
更新晚点生效。

### 0.7.0

RFC-004 D.1 — 状态根迁出 Caches。L1 main 最早入口(logx 前)跑
`migrate_legacy_state_root()`:`~/Library/Caches/marspot` →
`~/Library/Application Support/marspot`,rename 原子 + 旧位置留
symlink,活进程 fd 不断、未升级 binary 经 symlink 照常。macOS 把
Caches 当可清区,用户终端 history 不能住那里。

### 0.6.29

User 反馈 "现在 sfpro 这样太丑了" — 撤 B0.7 chrome SF Pro,回到 Monaco 统一字体.

0.6.28 修了拉伸变形,但代价是 SF Pro 走 mono-aligned advance(每字符占 cell_w 等宽,字距不自然),视觉比 Monaco 还丑.

修法:`dev_window` + render_metal 4 个 chrome 渲染点(dev_panel × 2 + context_menu × 2)`encode_canvas_into` 的 `ui_font` 参数全部传 `false`.framework 端 `FontKind::Ui` 路径 + ui_font_metrics + resolve_char_ui 保留(隐性,不再走),将来 atlas refactor 完成后单独 commit 重新接通.

shell 0.6.28 → 0.6.29;core 0.10.78 → 0.10.79.

### 0.6.28

**修 0.6.27 chrome 字"全变形"** — atlas slot 跟 GlyphInstance size 不一致导致字符被横向拉伸.

User 截图显示 SF Pro 渲染后 'm' / 'o' / 'a' 等被拉成奇怪的扁平字符,字符之间有 5pt+ 空白(看起来像 "L a y o u t").

根因:
- `atlas.get_or_rasterize` 用 `SlotMetrics { cell_w: ui_cell_w }` rasterize 所有 glyph,所有字符都画进 ui_cell_w 宽的 slot
- `GlyphInstance.size = [advance_px, ...]` — 每个 glyph 显示用真实 advance
- 宽字符 'm' (advance ~14pt) 被画在 8pt 宽的 atlas slot,然后 stretch 到 14pt 显示 → 横向拉伸 1.75x → 变形

修法 v1:回到 **uniform cell_w 推进**(mono-like).Both kind 共用 `cell_w × n_cells` 作为 advance + slot 宽 — 字符不变形,字距统一.SF Pro 看起来像 SF Pro mono.

真 proportional 需要重写 atlas:per-glyph slot 大小,GlyphInstance size 匹配真 bounding box.留 v2+ atlas refactor.

shell 0.6.27 → 0.6.28;core 0.10.77 → 0.10.78.

### 0.6.27

**B0.7 真 PTY / UI font 分离 — chrome 走系统 UI font(SF Pro on macOS).**

User 报"现在除了 pty 内,其他的都不设置字体,系统默认".

`font_cache.rs`:
- 新 const `UI_FONT_NAMES` cascade:`.AppleSystemUIFont`(动态 SF Pro)→ `SFPro-Regular` → `SF Pro Text` → `HelveticaNeue` → `Helvetica`
- 新 const `UI_FONT_POINT = 13.0`(macOS 标准 UI size)
- `FontCache` 加字段:`ui_font_idx / ui_cell_w / ui_cell_h / ui_ascent`
- `build()` 加载 UI font 并 intern 到 registry,用 `0` 字符的 CT advance 算 `ui_cell_w`(proportional 字体的近似)
- 新 `resolve_char_ui(ch)` — UI font 优先,glyph=0 时降级到 `resolve_char` 老 cascade(CJK / emoji fallback)

`render_metal.rs`:
- 新 enum `FontKind { Terminal, Ui }`
- `push_text_run_kind(...)` 替代 push_text_run 共享代码 — 按 kind 走不同分支:
  * Terminal:`resolve_char` + 均匀 `cell_w × n_cells` 推进(mono 格子)
  * Ui:`resolve_char_ui` + per-glyph **真实 advance**(`CTFontGetAdvancesForGlyphs`),proportional 排版
- `push_text_run()` 老 API 保留为 Terminal kind wrapper
- `encode_canvas_into()` 加 `ui_font: bool` 参数 → 传给 `build_canvas_runs` → `push_text_run_kind`
- `render_canvas_into_layer()` 加 `ui_font: bool`
- `ui_font_metrics()` 现在返回真实 UI font 的 `(ui_cell_w, ui_cell_h, ui_ascent) × MARSPOT_UI_FONT_SCALE`

`dev_window.rs`:render path 传 `ui_font: true` — dev panel chrome 整套 SF Pro 渲染.

5 个 `encode_canvas_into` 调用站点:
- `render_canvas_into_layer`(dev_window)→ `true`
- 2× dev_panel build path(主窗内,旧路径)→ `true`
- 2× context_menu path → `true`
- 1× test helper → `false`(测试 mono)

预期视觉效果:
- Dev panel / tab strip / sidebar / context menu / dev panel — SF Pro 渲染(细更现代,proportional spacing)
- PTY/terminal grid — Monaco 12pt mono 不变
- CJK 等 UI font 不覆盖的字符 — fallback 到 mono cascade(CJK font),保证显示

留 caveat:
- v3 framework 的 `Text` layout 用 `cell_w × char_count` 算宽,UI font 是 proportional → layout 估算 vs 真实渲染宽度有偏差(单字符 ±20% 范围),text rect 可能略宽于实际渲染.
- 实际 paint 用 per-glyph advance 是对的,所以视觉位置准确,只是 layout 计算的容器宽度可能略宽 — 单行内 OK,wrap 计算可能略偏.

shell 0.6.26 → 0.6.27;core 0.10.76 → 0.10.77.

### 0.6.26

**PERF 修 — dev panel 每帧 re-layout 致 idle 12%+ CPU.**

User 报"用起来这么卡".诊断:
- shell PID 58907 跑了 16h,占了 39:30 CPU time,12.3% 现 CPU snapshot
- root cause:`ShellApp::redraw` 每次都无条件 `dev_window.render(&state)` if visible
- 主窗 redraw 由 PTY traffic 驱动可达 ~60fps
- dev panel View 树有 ~580 节点(L1 token 扩 + L2-L6 各 30-50 项)
- 每帧 build + layout + paint 整树 → ~10ms × 60fps = 600ms CPU/s ≈ 60% 单核

修法 — `dev_panel_dirty: bool` 脏标志:
- init `dev_panel_dirty: true`(首帧 render 一次)
- 仅当 state 变化时 set true:
  - visibility flip(toolbar icon click)
  - DevPanelClick(active_tab / active_section 切换)
  - DevPanelScroll
  - DevWindowChanged(window resize / move)
  - theme::version() 变(theme swap)
- `redraw()` 中:`if dp_visible && self.dev_panel_dirty { render + clear flag }`
- 主窗依然每帧 redraw,但 dev_window 跳过(预期 < 1% CPU when dev panel 静止)

注:dev_window 的 NSWindow 本身仍存在,只是不重 paint.PTY traffic 不再波及 dev panel layout.

shell 0.6.25 → 0.6.26;core unchanged.

### 0.6.25

**Component Library v4 — P0 第一步:Token v4 全 land.**

按 `docs/ui-component-library.md` §3 起手,token.rs 从 233 行扩到 ~580 行.

`color::*` 扩(每条都有 Dark + light::* + hc::* 三套):
- `FG_INVERSE / FG_LINK`(已有 FG/FG_MUTED/FG_DISABLED)
- `SURFACE_0..4` + `OVERLAY` (5 个 surface 层级 + modal scrim)
- `INFO / CRITICAL`(配齐 4 级 severity)
- `FOCUS_RING / DISABLED_BG / DISABLED_FG`
- `TAB_{ACTIVE,INACTIVE}_{BG,FG} + TAB_HOVER_BG`
- `SIDEBAR_{BG,FG,ACTIVE_BG,ACTIVE_FG}`
- `STATUS_BAR_{BG,FG}`
- `DIFF_{ADD,REMOVE,CHANGE}_BG + DIFF_{ADD,REMOVE}_FG`

`terminal::*` — 全新 namespace(社区 theme 对齐):
- 核心:BG / FG / CURSOR_BG/FG / SELECTION_BG/FG / LINK / BOLD_FG
- `terminal::ansi::{BLACK..WHITE}`(8 normal)
- `terminal::ansi::bright::{BLACK..WHITE}`(8 bright)
- `terminal::search::{MATCH, MATCH_CURRENT}`
- 三套 palette:Dark / light::* / hc::*

`motion::*`(全新):INSTANT / FAST(120ms)/ NORMAL(200)/ SLOW(350)/ VERY_SLOW(700)+ `motion::curve::{STANDARD, DECEL, ACCEL, LINEAR, SPRING}` re-export AnimCurve.

`border::*`(全新):NONE / HAIRLINE(0.5pt)/ THIN(1)/ MEDIUM(2)/ THICK(3).

`layer::*`(全新):CONTENT(0)/ STATUS(10)/ STICKY(100)/ TOOLBAR(200)/ POPOVER(1000)/ MODAL(2000)/ TOAST(3000)/ TOOLTIP(4000)/ SYSTEM(9999).

`text::*` 补:LINK / ERROR(已有 CAPTION/BODY/HEADER/LARGE_HEADER/HINT/CODE).

`themed::*` 全闭包覆盖:
- `themed::color::*` — 49 fn(各 UI token Dark/Light/HC dispatch)
- `themed::terminal::*` — 8 fn(核心)+ `themed::terminal::ansi::{r,g,b,...}` 8 + `themed::terminal::ansi::bright::*` 8 + `themed::terminal::search::{match, match_current}` 2

实现方式:每个 `themed::xxx::yyy()` 内联 `match current() { Light => ..., HC => ..., _ => Dark }`.helper `fn pick(dark, light, hc) -> Color`.

DevPanel L1 Foundation section "Tokens" 段刷新,10+ done_row 反映全部 token 类目.

dev_panel hit_test 测试 `hit_test_lands_on_menu_row` 修:0.6.21 menu 重构后 idx 2 = SECTION_L2(不再是 SECTION_UNITS).

下一步 P0:
- ThemeFile TOML schema + load_from_file(`toml` + `serde` 加 dep)
- iTerm2 / Base16 / Alacritty / Kitty / Warp / WT import adapter
- 11 built-in themes 数据文件

shell 0.6.24 → 0.6.25;core 0.10.75 → 0.10.76.

### 0.6.24

Phase B0 — primitive 完善 + font 分离基础(回应 user "grid list 建立在 model 基础 + PTY/UI font 区分").

**[B0.1] Grid col_gap / row_gap 分开**:
- `View::Grid { ..., col_gap, row_gap }` 替代单一 gap
- `grid(items, cols, cell_w, cell_h, gap)` 保持 backward compat(col_gap=row_gap)
- 新 `grid_with_gaps(items, cols, cell_w, cell_h, col_gap, row_gap)` 显式分开
- VariableGrid 早已支持 `(col_gap, row_gap)` tuple

**[B0.2] lazy_vstack_padded**:
- `lazy_vstack_padded(id, items, item_h, gap, leading_margin, trailing_margin)`
- LazyVStack 包一层 Padding(top: leading, bottom: trailing)
- 用于 sticky-header / 滚动 boundary 留呼吸感

**[B0.4-B0.6] Font 分离基础**:
- `MetalRenderer::ui_font_metrics()` — UI chrome 用,scaled by `MARSPOT_UI_FONT_SCALE` env(0.0..4.0)
- `MetalRenderer::terminal_font_metrics()` — terminal grid 用
- `MetalRenderer::chrome_font_metrics()` 保留作 backward compat
- v1 三者 = same(共用 FontCache)
- `dev_window` 切换:`renderer_font_metrics() / chrome_cell_dims_phys()` 改用 `ui_font_metrics`
- env 试:`MARSPOT_UI_FONT_SCALE=0.88` → dev panel 文本 layout 变小;glyph 仍 terminal size,出现 visual mismatch — 由 B0.7 真 multi-font FontCache 解决

**剩下 B0.7** (v2+):FontCache 扩展支持多 font kind,真不同字体 rasterize.MARSPOT_UI_FONT_NAME env load 独立 CTFont.

DevPanel Model 更新:
- L1 新加 "Font 分离(PTY vs UI)" 块 — ui_font_metrics/terminal/chrome [✓];真不同 font [v2+]
- L4 加 lazy_vstack_padded + grid_with_gaps;清掉 Grid 重复行

shell 0.6.23 → 0.6.24;core 0.10.74 → 0.10.75.

### 0.6.23

P3 Phase A 整发清(线性 checklist 一个个推):

**[A1] HighContrast theme swap hook** — `theme::version()` AtomicU64 counter,`set_current()` 自动 `fetch_add`;ShellApp::redraw 比对 `last_theme_version`,变了就 `request_redraw()`.

**[A2] `.transition()` modifier** — declarative enter/exit anim spec:
- `Transition { kind, duration_ms, curve }` data type
- `TransitionKind::{Opacity, Scale, Slide(SlideDirection), Combined}`
- `SlideDirection::{FromTop, FromBottom, FromLeading, FromTrailing}`
- const constructors `Transition::fade / scale / slide`
- bake 进 `Decoration.transition`
- Driver(`LifecycleEvent` → 启 Anim<T>)留 P3i 真组件时绑

**[A3] AnimRegistry frame schedule** — 真 anim 调度:
- `AnimRegistry { inner: HashMap<u64, AnimSlot>, last_tick, next_key }`
- `anim_start(dur_ms) -> u64`,`anim_tick(Instant::now())`,`anim_progress(key)`,`anim_any_active()`,`anim_gc()`
- ShellApp::redraw 每帧 tick + gc + active 时 request_redraw
- idle CPU=0 保留(无 active anim 时 0 redraw)

**[A4] `.transform / .mask / .blend_mode` modifier types**:
- `Transform { translate_x/y, scale_x/y, rotate_deg }` + `identity/translate/scale/rotate` const ctors
- `BlendMode` 12 modes(Normal/Multiply/Screen/Overlay/...)
- `Mask(Box<View>)`
- bake 进 `Decoration.transform / blend_mode`;Translate 已 paint(走 offset 累加器),Scale/Rotate/Mask/BlendMode 数据 land,真 paint 留 Metal pipeline v2+

**[A5] LazyHStack/Grid variable tracks** — `VariableGrid`:
- `View::VariableGrid { items, tracks_w, tracks_h, gap }`
- `GridTrack::{Fixed(L), Flex(N), Auto}` — Fixed claim 尺寸,Flex 按 weight 分 leftover,Auto v1 fallback 32pt
- `variable_grid(items, tracks_w, tracks_h, col_gap, row_gap)` builder
- DevPanel L4 加 demo:Fixed(20) Flex(1) Fixed(40) × 2 rows

DevPanel Model 更新:
- L2: `.transform / .mask / .blend_mode` → [✓](API);真 mask stencil + scale/rotate Metal vertex transform = v2+
- L4: 加 VariableGrid demo
- L6: theme swap hook → [✓];AnimRegistry frame schedule → [✓];.transition() → [✓]

shell 0.6.22 → 0.6.23;core 0.10.73 → 0.10.74.

### 0.6.22

继续推 v3 doc 还能 land 的待补项.

framework 新增:
- `Key` enum (Char / Enter / Esc / Tab / Backspace / Arrows / F1-F12 / ...)
- `KeyEquivalent { key, mods }` + `KeyEquivalent::{cmd, cmd_shift, ctrl, plain}` const constructors
- `FocusId(u32)` newtype
- `Modifier::Shortcut(KeyEquivalent, ActionId)` + `.shortcut()` fluent
- `Modifier::Focusable(FocusId)` + `.focusable()` fluent
- `Modifier::AutoFocus` + `.auto_focus()` fluent
- `Modifier::OnAppear(ActionId) / OnDisappear(ActionId)` + 同名 fluent
- `Decoration` 加 `shortcut / focus_id / auto_focus / on_appear / on_disappear` 字段
- `AnimCurve::Spring { bounce }` — damped-cosine 关闭式 (真 ODE 弹簧仍 v2+)
- `Modifiers` 加 Hash derive(用于 KeyEquivalent 表)

Lifecycle reconcile 升级:
- 旧:简单 retain HOST_STATE 中 live ids
- 新:diff `PREV_LIVE_IDS` thread_local vs 当前 frame → 生成
  `Vec<LifecycleEvent>` = `Appear { id, action }` / `Disappear { id, action }`
- host 拿 events 调 `OnAppear` / `OnDisappear` 绑定的 ActionId reducer
- 收集 ScrollView 自带 id 跟 Modifier::Id

新 L5 composable preset:
- `context_menu(items, divider_after_idx, ActionId)` — Card 容器 + 行 + 可选 divider
- `breadcrumb(segments)` — Home › Section › 最右 active
- `list_row(label, trailing, selected, ActionId)` — sidebar/table 通用行

DevPanel Model 反映:
- L4 加 "Keyboard / Focus / Lifecycle modifiers" 块 — .shortcut/.focusable/.auto_focus/.on_appear/.on_disappear 全 [✓]
- L5 加 ContextMenu / Breadcrumb / ListRow live demo (各自 visible)
- L6 Lifecycle 详化:reconcile 返 Vec<LifecycleEvent>;.on_appear/.on_disappear [✓]
- L6 Animation:Spring curve 加入 easing 阵列(5 curves 视觉对比);v2+ 只剩 "真 frame schedule"
- L5 真组件迁移 ContextMenu / Sidebar / Table → 标注 "preset 已落,真组件替换 marspot 现存 仍 v1 待补"

shell 0.6.21 → 0.6.22;core 0.10.72 → 0.10.73.

### 0.6.21

回应 user "每一个 Layer 作为左边 menu 的 submenu / 每个 Layer 做细致 / items 垂直间距稍微加大 / L5 L6 还有没做完的做完".

Menu 重构:
- MENU_W_PT 140 → 170 容纳 "L# Foundation" 等更长 label
- SECTION_LABELS 加 6 个 sub-item("Model" 之下加 "  L1 Foundation", "  L2 Box Model", ... "  L6 Cross-cutting" 前缀 2 空格 indent)
- 老 Colors/Units/Rects/Lines/Text 仍在底下保留(legacy canvas builder)
- 新 SECTION_L1..SECTION_L6 常量(10..15 magic numbers,不跟老 1..5 冲突)

每 layer 独立 scrollable page:
- 新 build_l1_view ... build_l6_view 6 个 builder fn
- 每个走自己 scroll_view + ViewId(0xDE7_0010 / 0020 / 0030 / 0040 / 0050 / 0060)
- 新 `scroll_id_for_section(active_section)` 公开 fn,wheel handler 按当前 active section 路由 scroll delta
- ShellApp::dev_panel_scroll 改用 `scroll_id_for_section(self.dev_panel.active_section)` 而不是固定 MODEL id

每 layer 详细化:
- L1 Foundation:Length 3 bar / Color rgba / tokens (10 swatch + 6 ruler + 5 radius chip) / Identity/HostState/Lifecycle
- L2 Box Model:padding/border/radius/shadow demo box / Opacity 5 阶梯 / Clip status / AspectRatio 2 方向 demo / LinearGradient 2 / Material 3 / Elevation E0..E3 ladder / Hidden vs Collapsed
- L3 Primitives:Canvas atoms / Shape 3 / Image type / v2+ Path SDF
- L4 Layout:View atoms / Containers VStack/HStack/ZStack 3 demo / Constraints / AlignCross / Distribute 5 mode / Anchor / ScrollView/LazyVStack/LazyHStack / Grid 4×8 demo / Gesture / Stateful (Toggle + Picker live demo)
- L5 Components:**新加** card/panel/badge/tooltip/tab_strip preset(modifier chain 组合)+ 真组件 migration roadmap [v1 待补]
- L6 Cross-cutting:Lifecycle / Accessibility / Theme(Dark+Light+HighContrast 视觉对照,**新加** HighContrast palette)/ Animation Anim<T> + 4 easing curve 视觉 demo / i18n

framework 新增 composable presets:
- `card(child)` - bg_raised + border + radius + shadow E1
- `panel(child)` - bg_panel + padding MD + radius MD
- `badge(label, c)` - pill-shape Text + bg=color + radius PILL
- `tooltip(label)` - small popup + border + shadow E2
- `tab_strip(labels, selected, ActionId)` - horizontal tabs + click 派发

theme::color::hc 新调色板(HighContrast):
- 纯黑/纯白 BG/FG + 鲜黄 accent + 强对比 border = AX 友好

间距加大:
- scrollable_page vstack_gap 8pt → 12pt
- 底部 padding 16 → 20pt

shell 0.6.20 → 0.6.21;core 0.10.71 → 0.10.72.45 view 域 tests 维持 PASS.

### 0.6.20

把 v3 doc 剩下能纯 Rust land 的都落地 + DevPanel Model section 大幅扩 visual sample(用户报 "sample 内容很少").

framework 新增:
- `View::LazyHStack { items, gap, item_width, id }` + `lazy_hstack()` builder
- `View::Grid { items, cols, gap, cell_w, cell_h }` + `grid()` builder — uniform-cell grid 简版,variable tracks 留 v2+
- `View::Toggle { id }` + `toggle(id)` builder + `ToggleState { on: bool }`
- `View::Picker { id, options }` + `picker(id, options)` builder + `PickerState { selected }`
- 视觉:Toggle = capsule + circle knob(state-driven 位置),Picker = 横向 segment + 选中高亮 + label
- `InputEvent` enum:Click/DoubleClick/RightClick/DragBegin/Move/End/Hover/Scroll
- `DragInProgress { drag_id, started_at, current, modifiers }` + `delta()`
- `Anim<T> { from, to, elapsed, duration, curve }` + `Lerp` trait(`Color` / `f64` 已 impl)+ `AnimCurve::{Linear, EaseIn, EaseOut, EaseInOut}` + `.ease(t)`
- `Point` / `Modifiers` 类型
- Light theme token data — `color::light::*` 调色板 + `themed::color::*` 闭包查 active 主题

DevPanel Model section 视觉扩张:
- L1: Length 三 bar 渲染对比 + space 标尺(XS/SM/MD/LG/XL/XXL 实际宽度)+ radius chip(5 档圆角实例)
- L2: opacity chip ladder(1.0/0.75/0.5/0.25/0.1)+ LinearGradient 两种方向 demo + Material 3 styles + elev::E0..E3 shadow ladder + aspect_ratio 2:1 demo
- L3: Shape 三个 demo(Circle / Capsule / RoundedRect)
- L4: Distribute 5 mode 横向并排迷你示意 + Grid 4×2 颜色方块 demo + Toggle on/off 两个 + Picker 实例(预设选中 "Light")
- L4 Gesture 行 → [✓] 不再 v1 待补
- L6 Animation → [✓](数据类型 + Lerp + AnimCurve)真 frame 调度仍 v2+
- L6 Theme → 加 Light 数据已落

shell 0.6.19 → 0.6.20;core 0.10.70 → 0.10.71.45 view tests PASS.

### 0.6.19

P3 roadmap pure-Rust 部分推完:LazyVStack / Image / LinearGradient / Shape / Material API / ThemeId / Accessibility stub.AppKit-heavy 部分(真 NSVisualEffectView vibrancy / TextField IME / Metal scissor clip / Path SDF)留 v2+.

**P3r LazyVStack**:
- `View::LazyVStack { items, gap, item_height, id }` + `lazy_vstack(id, items, item_h, gap)` builder
- 自带 scroll 容器(ScrollState 跟 ScrollView 共用),无需 wrap in ScrollView
- 只 layout visible window 内 items(`visible_range(items_len, item_h, gap, offset, viewport)`)
- 假 uniform-height 假设(v1);variable-height 走 HostState item-h cache,留 v2+
- 6 个 lazy unit tests

**P3q Image / Gradient / Shape / Material**:
- `View::Image(Image { source: ImageSource, mode: ContentMode, tint })` + 4 个 source(Glyph/Raw/IOSurface/Named)
- `View::Shape(ShapeSpec)` — Circle / Capsule / RoundedRect(Path 留 v2+)
- `Modifier::BackgroundGradient(LinearGradient)` — paint 走 16-band 近似(真 Gradient primitive 留 v2+ Metal)
- `Modifier::BackgroundMaterial(MaterialStyle)` — v1 = 半透明 BG_PANEL fallback(真 NSVisualEffectView 留 v2+)
- `Color` 插值 helper `sample_gradient` + 16-band painter

**P3v ThemeId**:
- `ThemeId::{Dark, Light, HighContrast}` enum
- `theme::current()` / `theme::set_current(id)` AtomicU8 全局
- v1 只 Dark token 数据;Light/HC token 数据留 v2+

**P3u Accessibility stub**:
- `.accessibility_label(s)` / `.accessibility_role(AxRole)` modifier
- `AxRole::{Button, Heading, ListItem, TextField, Image, StaticText, Group, Link, Checkbox, Toggle}`
- bake 进 `Decoration.ax_label / ax_role`;真接 NSAccessibility 留 v2+

DevPanel Model section 大更新:LazyVStack/Image/Gradient/Material/Theme/AX/Lifecycle/HostState 全切 [✓].留 [v2+] 的:Material 真 vibrancy / mask/transform/blend / TextField / Animation / Light theme token data / i18n / Path SDF / LazyHStack+Grid.

shell 0.6.18 → 0.6.19;core 0.10.69 → 0.10.70.45 view 域 tests PASS.

### 0.6.18

P3o HostState 通用 + P3t lifecycle reconcile 骨架.

新 `src/ui/view/state.rs`:
- `HostState` — `HashMap<(ViewId, TypeId), Box<dyn Any>>` 按 (id, T) 双键存,同一 ViewId 可挂多种 stateful 类型(eg 一个 view 同时有 ScrollState 和 TextFieldState 不冲突)
- `get<T> / get_mut<T> / insert<T> / entry_or_default<T> / remove<T> / retain_ids` 6 个公开 API
- thread_local `HOST_STATE` + `with_host_state` / `with_host_state_mut` 闭包入口
- `reconcile(&laid)` — 遍历 LaidOut 树收集所有 ViewId (Modifier::Id + ScrollView 自带 id),retain HOST_STATE 中存在的 keys,其他丢掉(on_disappear)

老 `scroll::SCROLL_STATES` thread_local 暂保留(P3o 第二步迁过去再删,API 兼容).

未来 stateful view(TextField / Toggle / Picker / LazyVStack offset)按统一 pattern 走 HOST_STATE.

4 个 state unit tests PASS.无 lib 回归.

### 0.6.17

P3 roadmap 推三件 — P3l TextStyle + P3m Opacity/Clip/AspectRatio/Collapsed + P3n Gesture modifier.

**P3l TextStyle 形式化**:
- 新 `TextStyle { size, weight, color }` struct + `Text::style(s)` builder
- 新 `theme::text::{CAPTION, BODY, HEADER, LARGE_HEADER, HINT, CODE}` 6 个 token
- 新 `theme::elev::{E0, E1, E2, E3}` 4 档 Material-style elevation shadow token
- 退掉 `TextWeight::Dim = alpha × 0.6` hack:Dim variant 删,Bold 留(无 op,文档化为 v2+ 真 weight 落)
- DevPanel `hint/body/h1/mono` 全切到 token::text::* — 之前散的 `Text::new().color(...).size(...).weight(...)` 都退场

**P3m Opacity / Clip / AspectRatio / Collapsed 真实施**:
- `Modifier::Opacity(f64)` — paint 累乘下,所有 fill/border/text/Filled/Hairline alpha 都乘
- `Modifier::Clip(ClipShape::Rect | RoundedRect(L))` — descendants 走 culling clip;真 pixel-clip 留 P3 follow-up(Metal scissor)
- `Modifier::AspectRatio(ratio, Fit | Fill)` + `apply_aspect()` 在 layout pass 中改 inner_c;`FrameSpec.aspect` 也接通
- `Modifier::Collapsed(bool)` — `display: none` 语义,layout 直接 zero-size return,不走 child layout 也不 paint
- `Decoration` 加 `opacity / clip` 字段,custom Default(opacity=1.0)

**P3n Gesture modifier 完整化**:
- `Modifier::OnDoubleClick / OnRightClick / OnScroll / OnDragBegin`
- 新类型 `ScrollWheelId(u32) / DragId(u32)` 跟 `ActionId / HoverId / ViewId` 同 newtype 模型
- 新 `hit_test_double_click / hit_test_right_click / hit_test_scroll / hit_test_drag_begin`
- 共享 `hit_test_field` helper(泛型 `field: impl Fn(&Decoration) -> Option<T>`),click/hover/double/right/scroll/drag 都走同一路径
- 共享 `establishes_clip()` 把 ScrollView + Clip modifier 的视口语义合并

DevPanel Model section 状态标更新:
- L2 opacity/clip/aspect_ratio: `[v1 待补]` → `[✓]`
- L4 Gesture (hit-test 层): `[v1 待补]` → `[✓]`
- L4 完整 InputEvent + DragInProgress 仍 `[v1 待补]`(下一发)

shell 0.6.16 → 0.6.17.31 view tests + 9 dev_panel tests + 4 scroll tests + 7 layout tests 全 PASS.无 lib 回归.

### 0.6.16

修中文乱码 + 实施 ScrollView,响应用户 "panel 要可以滚动" + "中文问题要解决".

CJK / 宽字符宽度:
- v3 doc 列了 `Length::Ch`,但 Text layout / paint / truncate 都按
  `chars().count() × cell_w` 算,CJK 字宽 1 cell 算成 1 而非 2,
  导致 `[v1 待补]` 渲染成 `[v1 待` (补] 越界 hstack 后被吃掉),
  整片中文挤碎 ("乱麻").
- 新 helper `view::layout::text_width_cells(s)` 走 `marspot_term::
  grid::char_width(c)` —— 跟终端 grid 用同一张 East Asian Wide 表
- Text layout / paint / truncate / hstack 宽度量度全切到 cells,
  CJK 现在按 2 cells 算,跟实际渲染对齐.

ScrollView(v1 vertical-only):
- 新 `src/ui/view/scroll.rs`:`ScrollState { offset_y, content_h,
  viewport_h }` + thread_local `SCROLL_STATES: HashMap<ViewId, _>`
  + 公开 API:`scroll_state` / `set_scroll_state` / `apply_scroll_delta`
  / `forget` / `with_scroll_state`
- 新 `View::ScrollView { child, id }` variant + `scroll_view(id, view)`
  builder
- Layout:child 按 unbounded height 排布,然后整 subtree shift -offset_y
- Paint:viewport culling — rect 完全在 clip 外 skip,边缘允许 overflow
  (Canvas 没 clip primitive,真 scissor 走 v3 follow-up)
- Hit-test:ScrollView 给 descendants 加 clip,被 culled 的视图收不到点击

DevPanel 接入:
- `build_model_view()` 把 vstack 内容用 `scroll_view(DEV_PANEL_MODEL_
  SCROLL_ID, content)` 包起来
- DevPanelView 加 `-scrollWheel:` selector,dispatch EventKind::
  DevPanelScroll { delta_y_pt }
- ShellApp 加 `dev_panel_scroll` impl:`apply_scroll_delta(MODEL_ID,
  delta_y_pt × scale × 3.0)` (3x = 滚动手感乘子) + request_redraw
- 现在 dev panel 可以滚到底看 L6 / footer

`build_dev_panel_canvas` 签名跟 callers (`dev_window.rs` + 2 处
`render_metal.rs`) 已有 chrome_ascent.shell 0.6.15 → 0.6.16.
39 view tests PASS,无 lib 回归.

### 0.6.15

UI 模型 v3 + DevPanel Model section 补完整.

v2 doc 自查发现 SOTA framework 的常用 primitive 缺了不少:
- ScrollView / LazyVStack / Grid 等容器
- Image / Gradient / Shape / Material backdrop
- 真实 Gesture model(Drag / DoubleClick / RightClick / Hover state / Scroll)
- Stateful views(TextField / Toggle / Picker)
- Identity-state map(`Modifier::Id(ViewId)` 之前没 host 接)
- Lifecycle(on_appear / on_disappear)
- Accessibility 接口
- TextStyle 形式化(代替散的 size+weight+color)
- `TextWeight::Dim = alpha × 0.6` / `ZIndex` modifier 不读 等 v2 hack 该明确

`docs/ui-system-model.md` 重写为 v3(18 章 + 完整 implementation roadmap P3a-v + 11 个 component 迁移 + SOTA self-assessment).写明哪些 ✓ / v1 待补 / v2+,以及为什么.

DevPanel.Model section 同步:每条 model 加状态标 `[✓]`(绿)/ `[v1 待补]`(黄)/ `[v2+]`(灰).新增 L6 "Cross-cutting" 列 Lifecycle / Accessibility / Animation / Theme / i18n.footer 指向 doc.

shell 0.6.14 → 0.6.15.9/9 dev_panel tests PASS,无回归.

### 0.6.14

修 0.6.13 dev panel L1/L4 文字撞下面 swatches/squares 的真根因.

`canvas::TextPrim::y` 注释说"Top-left in physical pixels",render_metal 处理 Text primitive 时**自己** `baseline_y = t.y + ascent`(line 5059).但 v2 paint.rs `paint_atom` 自己**又**加了 ascent:

```rust
let baseline_pt = phys_to_pt(rect.y + ctx.ascent_phys);  // ← 错,自己加了
canvas.text(.. baseline_pt, ..)                          // renderer 再加 → text 实际 y = rect.y + 2 × ascent
```

结果每个 text 都比 layout 算的位置低一个 ascent(~12 pt),撞下一行.L1 swatches 跟 Color: 文本撞,L4 mini squares 跟 VStack/HStack/ZStack 标签撞.

修:paint_atom 改用 `top_pt = phys_to_pt(rect.y)`,把 top 传给 canvas.text,renderer 自己加 ascent.5 个 paint/hit_test 测试仍 PASS.

shell 0.6.13 → 0.6.14.

### 0.6.13

DevPanel Model section 完整呈现 v2 model + **用 v2 framework 自身渲染** —— 自反 demo:看到的这个面板,就是用面板描述的体系建出来的.

落地内容:
- 新 `build_model_view() -> View` 返一棵完整 View 树
- L1-L5 五层各一段:
  - L1 Foundation:Length(Pt/Pct/Ch)/ Color / Tokens 含 5-swatch palette demo
  - L2 Box Model:一个 padding + border + radius + shadow 全有的 box demo + 3 条 hint
  - L3 Primitives(Canvas):Canvas + submission order 说明
  - L4 View Tree + Modifiers(新):原子/容器/modifiers 列表 + **VStack/HStack/ZStack 三个 mini demo 并排**(各 3 色块)+ Constraints/Align/Distribute/Anchor
  - L5 Components:未来要迁的组件清单
- 渲染路径走 `layout_view + paint_into` 在 dev panel canvas 里 layout + paint — Model section 完全走新 framework;其余 sections (Colors/Units/Rects/Lines/Text) 仍走老 canvas builder
- View 加 fluent helpers:`.vstack_gap(L)` / `.hstack_gap(L)` / `.align_cross_*` / `.distribute(D)` — 在 stack view 上配 gap/align/distribute 不用 enum 解构

`build_dev_panel_canvas` 加 `chrome_ascent: f32` 参数(layout 把 text 基线放 `rect.y + ascent`).callers(`dev_window.rs` + `render_metal.rs` 两处)同步.

shell 0.6.12 → 0.6.13.9/9 dev_panel tests PASS,zero 编译回归.

### 0.6.12

UI 系统 v2 framework 落定(`docs/ui-system-model.md` 的 P3a → P3h + P3k).新增 ~1500 LOC,无组件迁移(留 P3i),零回归,213/213 lib tests PASS.

落地的(实际可用):
- **P3a** `src/ui/theme/token.rs`:18 色 token + 6 space 档 + 4 radius 档 + PILL
- **P3b** `Length::Ch(f64)` 单位 + `resolve_for_axis_with_cell` 三参数 resolver(老 2 参数 resolver Ch → 0 兜底)
- **P3c** `src/ui/view/{types,view}.rs`:`View` enum(Text/Spacer/Filled/Hairline/VStack/HStack/ZStack/Modified)+ `Modifier` 链(Padding/Background/Border/CornerRadius/Shadow/Frame/Offset/ZIndex/Hidden/OnHover/OnClick/Id)+ `Edges`/`Anchor`(9 anchor SwiftUI shape)/`FrameSpec`/`Shadow`/`ActionId`/`HoverId`/`ViewId`。fluent chain API(`Text::new("h").padding(MD).background(...).border(...).frame(...)`)。
- **P3d/e/f** `src/ui/view/layout.rs`:Constraints two-pass 算法(完整 Flutter 同形),VStack/HStack/ZStack + Spacer flex,Modified 走 Padding/Frame/Background/Border/Shadow/Offset/Id 烤进 `Decoration`。
- **P3g** `src/ui/view/paint.rs`:LaidOut → Canvas,submission order = z order,Text truncate End/Middle/None + TextWeight::Dim(alpha × 0.6)+ TextAlign Leading/Center/Trailing。
- **P3h** `src/ui/view/hit_test.rs`:tree-walk post-order + reverse children = deepest+topmost wins,跟 paint 的 z 顺序一致。
- **P3k** `src/ui/theme/mod.rs` re-export,ThemeId 全局 slot 留好(v1 只有 Dark)。

留 P3i 的(下次):各组件(ContextMenu / LayoutModal / DevPanel / Table / Sidebar)从手算 canvas builder 迁到 View 树。P3j(ViewPainter 退役)= P3i 完成的副产品。

参考 `docs/ui-system-model.md` 全 doc 看完整设计。

shell 0.6.11 → 0.6.12.27 个新 view-tree tests + 全部 213 lib tests PASS.

### 0.6.11

UI tab 左 menu 新增第一项 **Model** — 讲清 marspot UI 系统的 CSS 类心智模型,新人 / future-self 打开 dev panel 第一眼看到的就是体系本身,不是 swatches.

四段:
- **Length = Pt | Pct**:`Pt(N) ≈ CSS Npx`(逻辑 pt,scale-independent);`Pct(F) = F × parent`
- **Rect = box-model**:`.at(x,y)` / `.size(w,h)` / `.fill(color)` / `.border(width, color)`(inside-stroke,box-sizing: border-box)/ `.radius(r)` / `.shadow(blur, off, c)` —— 跟 CSS 一一对应
- **Color = CSS rgba**:`Color::rgba(r, g, b, a)`,alpha 0..1 跟 CSS rgba() 一致
- **Z order = submission order**:later `.draw()` paints on top,无 z-index 概念;配 3 个重叠 rect 视觉 demo(红→绿→蓝 后画的在上)

默认 `active_section = SECTION_MODEL`(不再 SECTION_COLORS),fresh open 直接看 model 而不是色块.菜单顺序:Model / Colors / Units / Rects / Lines / Text.

shell 0.6.10 → 0.6.11.9/9 dev_panel tests PASS.

### 0.6.10

dev panel mouse routing 接通,tab strip + 左 menu 终于真能点.0.6.9 把 sample 都画上但所有 click 都打不动 —— 因为 dev window 只有原生 NSView,AppKit 把内容区 click 都吃了.

实现:
- `dev_window.rs` 新 `DevPanelView`:NSView 子类,acceptsFirstMouse / acceptsFirstResponder / isFlipped 都开,`mouseDown:` 转 logical-pt(view-local,y=0 顶)走 `dispatch_event_pub(EventKind::DevPanelClick { x_pt, y_pt })`.NSWindow contentView 换成这个子类.
- `app.rs`:新 `EventKind::DevPanelClick { x_pt, y_pt }` + `MarspotApp::dev_panel_click(_ctx, _x_pt, _y_pt)` 默认 no-op + dispatcher hook.
- `ui/components/dev_panel.rs`:layout 常量提到 `pub const TAB_BAR_H_PT / MENU_W_PT / TAB_PAD_X_PT / ...`,渲染跟 hit_test 共用同一组数,click 落到的矩形跟画的矩形按 pixel 对齐.新 `hit_test(state, chrome_cell_w_pt, x_pt, y_pt) -> Option<DevPanelHit>`,返 `Tab(usize)` 或 `Section(usize)` 或 None.4 个新 hit_test 测试覆盖 tab / menu / 内容空区 / 非 UI tab 不映射 menu 四种 case.
- `ShellApp::dev_panel_click`:查 dev window 的 chrome cell metrics(新 `DevWindow::chrome_cell_dims_phys` API)算 logical-pt cell width,call hit_test,改 `active_tab` / `active_section`,redraw 同步进窗.

渲染层同步改:UI tab 不再 stack 全部 5 段 sample,改成 `match state.active_section` 只画当前选中那一段.menu 一点就切.

shell 0.6.9 → 0.6.10.9/9 dev_panel tests + 189/189 lib tests PASS.

### 0.6.9

dev panel 加 tab strip + UI tab(左 menu + 右 sample 区).tab strip 三个 tab(UI / Tokens / Components,后两个是 placeholder).UI tab 内左侧 140pt menu 列五项(Colors / Units / Rects / Lines / Text),右侧 content 区把五段 sample 全部 stack 显示:

- **Colors** — 7 个色块演示 `Color::rgba` 7 种命名色 + alpha 0.5 半透明
- **Units** — 5 条横向 bar 对比 `Pt(40/80/160)` 跟 `Pct(0.25/0.50)`,带标签
- **Rects** — fill / border(透明 fill + 1pt 描边)/ radius(10) / shadow 四种 builder 用法
- **Lines** — 1/2/3pt 线宽 + alpha 0.30 + 一条 diagonal 演示 line 不锁轴
- **Text** — 5 种 token 色 + 一种显式 Color::rgba 红色

DevPanelState 加 `active_section: usize`(默认 `SECTION_COLORS`),菜单当前 row highlight 走它.mouse routing into dev window 还没接,所以菜单点不动 —— 当前 right area 永远 stack 全部 sample,user 直接看就行(等 mouse 接通后改成 active_section 切单一 section view).

shell 0.6.8 → 0.6.9.

### 0.6.8

修 0.6.7 自己引入的启动 panic.0.6.7 把 `apply_saved_frame` 放进 `ShellApp::resumed`,但 `setFrame_display(r, true)` 同步 fire `windowDidMove:` 进 delegate → `dispatch_event_pub` → `APP_STATE.borrow_mut()` —— `resumed` 这时还借着 APP_STATE.borrow_mut,re-entrant borrow panic.objc2 declare_class 方法是 `nounwind` → 进程 abort,LaunchAgent rate-limit 不拉,marspot 完全打不开.

修法跟 `set_visible_deferred` 同套路:新加 `PENDING_FRAME: RefCell<Option<(x,y,w,h)>>` thread_local;`apply_saved_frame` 改成只写 PENDING_FRAME(不调 AppKit);`drain_pending_actions` 排两件事 —— 先 apply frame(避免窗口先在默认位置闪一下再跳到 saved),再 apply visibility.同时 `run_app` 在 `state.app.resumed()` 之后多加一次 `drain_pending_actions`,确保 boot 路径的 deferred 在 borrow 释放后真的 land.

### 0.6.7

修 0.6.6 引入的 dev panel 持久化漏空 —— `app.rs::run_app` 里 `dev_window::ensure_built` 调用在 `app.resumed()` **之后**.L1 的 `resumed` 走的是"read `dev-window-state.bin` → `with_dev_window` → `apply_saved_frame`",但那一刻 dev window 还没建,`with_dev_window` 静默返 `None`,saved frame 丢.

现象:每次重启 / 重 install 后 dev panel 都在默认位置 / 默认尺寸打开,完全不读 disk 里持久化的 frame.

修:`ensure_built` 挪到 `resumed` 调用之前,确保 L1 `apply_saved_frame` 能找到真实的 NSWindow.顺序敏感的初始化错位,纯调用顺序问题,代码体小但行为完全反转.`dev_panel.visible = saved.visible` 那条本来就 work(不依赖 dev_window 存在),所以 visible 在 0.6.6 里 OK 只是 frame 漏.

### 0.6.6

UI-system dev panel 接入 installed 环境(从 sandbox 搬过来).在 toolbar 第 4 个 chrome icon 点一下,独立的 dev panel NSWindow 显示/隐藏;位置、尺寸、可见性都持久化到 `~/Library/Caches/marspot/dev-window-state.bin`(MAGIC 0xA5505012),L1 self-execv / 重启都保留.

实现拆三块:
- 新 `MsgType::DevPanelToggle = 55` wire frame(L2 → L1,空 payload).
- L2 (`marspot-core`):删掉 L2 那份死代码的 `dev_panel: DevPanelState` 字段;icon click 改成 push `DevPanelToggle` 帧.L2 不再保留可见性,L1 是 single source of truth.
- L1 (`marspot-shell`):接管 `dev_panel: DevPanelState`,接收 `DevPanelToggle` 帧 → 翻 visible + 保存状态.`MarspotApp` trait 加 `dev_window_changed` 默认 no-op hook,`dev_window` 自己的 `NSWindowDelegate` 触发的 `DevWindowChanged` event 现在跑过 app 回调,L1 借此 dedup 写盘(round 到 pt + cache last_saved_dev_window).redraw 路径每帧调 `dev_window::with_dev_window` 同步 AppKit 可见性 + render.

新加 `state::SavedDevWindow { display_id, x, y, w, h, visible }` + `read_dev_window` / `write_dev_window`(跟 main 的 `SavedWindow` 同布局多一个 visible bit).`dev_window::apply_saved_frame` 用来 boot 时 restore;`display_id` 跟 main 同 NSScreenNumber 取法.

工作流变更:`bin/run.sh` sandbox 不再做 UI 迭代,直接 `bin/install-local.sh` 在 installed 环境跑.

### 0.6.5

claudecode session.bound 改 main-side transition-only.0.6.4 worker scan_once 每次都 push 12 行 session.bound 进 log_lines,12 panes × 0.5Hz = 6 行/s 永久背景噪声.改:worker 不再生成 session.bound;main 拿 ScanResult 跟 self.last_mapping diff,真变了才 log + 对消失的 binding 补 session.unbound.跟 0.6.3 之前 transition-only 语义对齐.steady-state 0 行.

### 0.6.4

claudecode plugin tick 异步化.原 tick 同步走 ~/.claude/projects/*.jsonl read_dir + metadata + tail_last_message_type(32KB read) + pidtree::list_all_procs + per-session BFS + proc_cwd + proc_env_value(CLAUDE_CONFIG_DIR),disk 抖一下就 700ms-2s,3 次连续超 HOOK_BUDGET(100ms)被 host 永久 disable → user 反馈 "P<n> badge 点了没反应"(plugin 死,on_pane_badge_click 不 dispatch).改:新增 claudecode-scan worker thread(`WorkerCtx` 持 projects_root + ShelldClient + seen HashMap),tick 仅 try_recv ScanResult + host.set_pane_badge,稳态 < 1ms.WorkerCtx::scan_once 算 new_mapping/new_meta + 攒 log_lines,worker_main 循环 recv→scan_once→send.scan_inflight 防 worker 慢时 tick 累积请求.scan_res_rx 包 std::sync::Mutex 满足 `Plugin: Sync`(同 MonitorState.rx 套路).stop() drop tx → worker recv Err 退出 → join.不动 plugin host 的 HOOK_BUDGET / BUDGET_OVERSHOOT_LIMIT,plugin 自己守纪律.11/11 plugin tests + 188/188 lib tests PASS

### F3+6.1.1 windowDidMove / windowDidResize 写 window-state

F3+6.1.1 windowDidMove / windowDidResize 写 window-state.bin 加 dedup.原版 live drag 时 resized fire 60+/s × atomic .tmp+rename = 写 storm + ramp shell CPU.改 ShellApp.save_window_state_if_changed:cache last_saved_window: Option<(x,y,w,h,display_id)>,round 到 1pt 比较,真变了才写.live drag 现在 1 个 final save 而不是 60+

### F3+6.1 NSWindow 位置/尺寸/所在屏幕持久化

F3+6.1 NSWindow 位置/尺寸/所在屏幕持久化.独立 window-state.bin (MAGIC 0xA5505011, VERSION 1) 让 L1 自管,避开和 L2 shell-state.bin 写 race.boot:env MARSPOT_RESTORE_FRAME (L1 self-execv 路径) → state::read_window() (cold boot) → WindowAttrs.frame_pt;w/h ≤ 50pt 丢弃防 corrupt 把窗口缩成一条缝.MarspotApp trait 新加 fn moved(_ctx),NSWindowDelegate windowDidMove: dispatch → EventKind::Moved → app.moved(ctx).ShellApp::resized + ShellApp::moved 都 save_window_state(ctx):ctx.window_frame_pt() + ctx.window_display_id() (新加 [NSWindow screen].deviceDescription[NSScreenNumber] → u32 CGDirectDisplayID).每次写 ~50us atomic .tmp+rename,live drag 60+ 写/s 仍 OK

### F2+2a claudecode 插件 `attach_raw_only` 永久 Unsupported 之后插 `monitor_unsupported…

F2+2a claudecode 插件 `attach_raw_only` 永久 Unsupported 之后插 `monitor_unsupported` latch,一次 fail 就停止重试.之前 9 pane × 0.5Hz tick = 4.5/s `monitor.attach_failed` Warn 占 94% log 总量;现在静音.同步 F2+2b 把 install-local.sh 里 RFC-003 已退役 L4 shelld 的 LaunchAgent bootstrap 块改成 retirement 注释(`install-shelld.sh` 本就不存在,verdict_sum=0 分支早就坏)

## L2  marspot-core

Current: **0.12.186**

### 0.12.186

"An agent TUI paints this pane" is declared, not inferred.

Reported as: three relative paths in one block became links and the
fourth did not.  The fourth was the only one broken across two rows.

Replayed from the pane's own bytelog, the answer is exact: with the
pane's TUI flag on, the scanner merges the hard wrap and finds all
five spans; with it off, it finds three and the wrapped one is missing.
So the path logic was right and the flag was wrong.

L2 inferred that flag from two proxies — a plugin's wheel-key
declaration, or a non-empty badge.  Both are sent once, or only when
they change.  A core swap starts the new L2 with empty maps, and after
the 03:29 swap neither ever arrived again: codex's wheel keys were
declared once at 02:01, and its badge (`gpt-6-astra medium`) had not
changed since.  The pane silently lost its TUI-shaped link scanning.

This is the same defect `<u>` had, and the same fix it got:
`MsgType::PaneAgentTui`, re-issued every tick, so a swap costs one
tick instead of lasting until something happens to change.  Naming it
also removes the inference, whose own comment already admitted a badge
"can momentarily read empty".

The old inference stays as the fallback for a pane no declaration has
arrived for, so an L2 running ahead of its L1 still behaves — the wire
rule this repo learned the hard way.

### 0.12.185

A relative path is a link, resolved against the pane's own directory.

`src/main.rs`, `./notes.md`, `Cargo.toml` — written the way people
actually write them, and until now inert, because the scanner only
recognised what it could resolve on its own: absolute (`/…`) and
home-relative (`~/…`).

The filesystem stays the arbiter, exactly as it is for an absolute
path: a candidate becomes a link only when `<cwd>/<candidate>` exists.
But it must not be ASKED about every word on screen, and a word that
happens to name something in `~` (`Music`, `Public`) must not light up
in prose.  So a candidate needs evidence of being a path before it is
worth resolving — an explicit `./` or `../`, a separator, or a file
extension of 1..8 alphanumerics starting with a LETTER.  That last rule
is what keeps `1.5`, `v1.2.3` and `2026.09` out while letting `a.c` in.

What is drawn and what is meant are separate: the underline covers the
eleven characters of `src/main.rs`, and Open and Copy act on
`/w/proj/src/main.rs`.  `LinkRange` carries that as `target`, the same
split the `file://` handling already used for its scheme.

Two things this change deliberately does NOT do.  It issues no
filesystem calls of its own — resolution goes through the same
non-blocking oracle as everything else, where a single `lstat` under a
network mount has been measured at six seconds on the render thread.
And the relative branch runs LAST, after URL, email, IP and UUID, so
nothing another kind already claimed can be re-read as a filename.

The hit-test resolves against the same cwd the render pass did.  A
different one would underline a span and then click nothing.

### 0.12.184

Focus events reach the program, and opening a history view no longer
travels in it.

**Focus.**  codex turns on DEC 1004 and leaves it on — 12 sets, 16
resets, ON at the end of a real 22 MB session — and marspot accepted
the mode and then never sent a single event.  The terminal now tracks
it, reports `CSI I` / `CSI O` on change, and carries the mode across
the L3 execv handoff (modes bit 9) so an image swap does not stop
answering a question the program is still asking.  DEC 2031
(colour-scheme change) stays accepted and unreported on purpose:
marspot has one palette that never changes, so there is no event, and
the real session never queried the scheme either.

**Opening.**  The tick that opened a plugin's history view also sent
that tick's whole scroll distance in the same buffer.  A flick is one
wheel event carrying tens of lines, so reaching for history opened it
and immediately threw the user into the middle of it — landing inside
whatever the program had printed there rather than at the edge they
reached for.  Opening is one gesture; moving inside is the next one.

### 0.12.183

Scrolling into codex's transcript stopped toggling it shut again.

Reported as: reaching history with the wheel is very choppy, and iTerm2
and Terminal are not.

The wheel sends the plugin's `enter` key (codex: ctrl-T) whenever the
program's on-screen marker says the view is closed.  That marker cannot
appear until codex has repainted and the frame has been published and
read — **59 ms apart in the best case in the user's own log**.  A
trackpad delivers ticks at 60–120 Hz, so four to seven more of them
arrived inside that window, each seeing a view that still read closed,
and each sending the toggle again.  The transcript opened and shut
several times inside one flick.

The state log could not see it, because it records the OBSERVED open
flag and a flap that resolves before the next publish never changes it
— the same silence the comment beside it already warns about.  There is
now a sampled line for the suppressed case, so the next report of this
shape has evidence instead of a guess.

The fix is not a remembered "it is open" bool: that was tried, and
because the program leaves the view on its own too, a stale flag made
the next tick close what the user was reading.  It asks a narrower
question — has enough time passed since we last asked for the answer to
be visible (`wheel_marker::should_send_enter`, 400 ms) — and the
scroll keys keep flowing throughout either way.

### 0.12.182

Fix: the prediction sweep 0.12.181 added crashed the app at startup.

`expire_stale_predictions` walked every pane and asked each one for its
in-process `Terminal`.  An L3 pane has none — its terminal lives in the
session process, and `PaneBackend::terminal()` says so by panicking.
L3 is the default, so the first pane hit it: `internal error: entered
unreachable code: L3/Vacant pane has no in-process Terminal`, before
logx is up, which is why the logs were empty and nothing was left
behind but a missing window.

`terminal_opt` / `terminal_mut_opt` return `None` for the backends that
have no terminal here, and both sweeps skip those panes.  The panes L2
owns directly are the only ones that ever needed the sweep; L3 runs its
own, which is where the live path was tested and why this got through.

### 0.12.181

The same prediction deadline in the two panes L2 owns directly.

L3 owns the pane the user is normally typing in, but core keeps its
own sessions on the non-L3 path and mcli is its own binary — both call
`predict_byte`, so both had the hole L3 0.11.74 describes: a program
that echoes nothing leaves the guesses painted.

Neither loop could simply poll for it.  Core folds the deadline into
the `recv_timeout` it already computes, so it wakes on a 10 ms tick
only while a guess is outstanding and keeps its one-second idle
timeout otherwise.  mcli has no loop of its own — it is driven by
AppKit — so a predicted keystroke arms exactly one wake-up through the
event proxy.  CPU at rest, a hard project constraint, is unchanged in
both: with nothing predicted there is nothing to wake for.

### 0.12.180

`PaneRenderMarkup` is forwarded to the pane's own L3.

L2 does not act on it: the terminal that would draw the markup lives in
L3, and L2's grid is a mirror of what L3 publishes.  A declaration that
names a session with no pane here is logged rather than dropped — it is
re-sent every tick, so a steady stream of those would mean the sid the
plugin uses and the one the pane answers to have drifted apart.

### 0.12.179

The wheel believes the program over the screen (DEC 1007).

Entering codex's transcript reads `?1049h · ?1007h · CSI J · ?2026h`.
`?1007` is alternate scroll mode — "on this screen the wheel is the
arrow keys" — which is xterm's name for exactly the question three
rounds of scanning its heading for `/TRANSCRIPT/` were trying to
answer.  Inference lost three times, most recently to letter-spacing.

`view_is_open` now takes the program's own statement first and falls
back to the marker for programs that never learned to say it.

Not a new capability: `less` and `man` already got the wheel through
the alt-screen route (0.12.169).  What is new is that the answer is
stated rather than guessed.

### 0.12.178

"Does an agent TUI paint this pane" stops being read off the badge.

A path printed by codex came out underlined only as far as
`…/lab36-continus/`, losing the `.tmp/…` tail and everything after the
line break (2026-09-06 field report).  Bisected against the real row —
73 columns, codex's `›` glyph, codex's own break with a two-space
hanging indent:

    one line, any mode        → whole path
    codex's own wrap, tui on  → whole path
    codex's own wrap, tui off → truncated at `…/lab36-continus/`

So the shape was handled; the pane was in the wrong mode.  The mode
came from "the plugin badge is non-empty", and codex's badge is built
from a model and an effort read off disk — a read that comes back
empty leaves the badge empty, and with it the pane silently stops
merging wrapped links.  Coupling link scanning to whether a plugin
managed to render a caption is the actual defect.

The declaration of wheel keys is the durable fact instead: a plugin
only makes it about a program it is driving, and it survives a core
swap.  `SessionView::agent_tui` carries it, and the hit-test reads the
same answer as the render pass — the two disagreeing would underline
one span and open another.

(A DECAWM wrap whose continuation begins with a space is still not
merged, and should not be: there the space is the next character, so
the path really does end.)

### 0.12.177

A frame no longer blocks the main loop.

Every frame ended in a synchronous `waitUntilCompleted()` on the loop
that also handles input, forwards keys to L3, and paints every window.
So however long the GPU queue took, the terminal was deaf for exactly
that long.  Measured across 166 stalls on a working machine:

    mean wait 374.5ms    mean GPU execution 3.3ms    worst 3644.8ms

We waited 113× longer than the GPU worked, once for three and a half
seconds.  The GPU was not busy — we were queued behind a loaded
machine's other work and chose to stand there ("在我们这开 codex，输入
有时候都会卡，在 iTerm2 很流畅").

The frame is now committed and left running; its command buffer hangs
off `WindowRender` and each pass polls `status()`.  Only `Completed`
flips the surface and sends `SurfaceReady`, so the contract that the
shell only ever samples a finished surface is untouched — it just no
longer costs the loop the GPU's queueing time.

One frame in flight, never two, which keeps every existing invariant:
the instance pool is still refilled in place (nothing reads it once the
frame completes) and the two surfaces still alternate a full frame
apart.  A window with a frame in flight starts no new one and keeps
`needs_render`, so it paints the moment the GPU frees up.

Attach keeps the blocking render: it acks `SurfaceReady` in the same
breath, and announcing an unfinished surface there is a black window.
It happens once per attach with nobody typing.

Idle CPU is a hard constraint and holds: the 1 ms poll exists only
while a frame is in flight, and at rest there is none — the idle
timeout is still a second.  Sandbox measured shell 0.1% / core 0.0%.

Gate on mini: 11/11 pass.

### 0.12.176

Only an upward tick opens a plugin's scroll view.

Reaching for history is an upward gesture.  A downward tick with the
view closed means "I am at the newest, show me what is below" — and
answering that by opening a history view is a surprise (asked for
2026-09-06).  Such a tick is now not ours at all: it falls through to
the pane's own routing untouched, rather than being swallowed.

An OPEN view still takes both directions, or there would be no way to
page back down to where the user came from.

Checked against a live codex, all three cases:

    closed + down  → nothing sent, view stays closed
    closed + up    → entered once, stayed open, content moving
    open   + down  → paged back, toggle never pressed

### 0.12.175

The wheel path gets a log.

It had none — not the declaration arriving, not the open/closed
reading, not a key going out.  Three fixes in a row could be inspected
only by asking the user to scroll, which is why each one shipped
looking right.  Now a declaration logs on receipt, and the open/closed
reading logs once per TRANSITION (a momentum scroll is many events; the
thing worth seeing is whether the view opened and then stayed open).

### 0.12.174

The wheel-marker rule moves into `marspot::wheel_marker`.

It lived in this binary, so nothing outside could call it — and a probe
that wants to check it against a live session had to carry its own
copy.  A copy is always right about itself.  That is how three fixes in
a row looked correct: not one of them ever fed a real screen to the
real predicate.

The module now owns the whole rule, including "a plugin that declared
no marker is treated as open", so that an unobservable view never gets
a blind toggle press.  `examples/wheel_replay` replays L2's per-event
decision against a running session and calls the same function:

    real marker      → entered 1x, open across 8 ticks, content moving
    marker that never matches → toggle re-sent every tick, ends closed

The second line is the reported symptom, reproduced on demand.

### 0.12.173

The wheel marker is matched with whitespace squeezed out of both sides.

0.12.172 moved the scroll view's state from a remembered flag to a
marker read off the screen — the right shape, still not working.  The
third report of the same symptom: entering codex's transcript with the
wheel, then moving it again, flickers and drops straight back out.

`examples/dump_row.rs` against a live transcript gives the reason in
one line.  Codex draws its heading letter-spaced —
`/ T R A N S C R I P T / / / ...` — while the plugin declares the
compact `/TRANSCRIPT/`.  A literal match is never true, so L2 read the
view as closed on every tick and re-sent `Ctrl+T`; being a toggle, that
closed the view the previous tick had opened.

A program draws headings for humans, not for matchers.  Both sides now
drop whitespace (and wide chars' trailing `\0`) before comparing.  This
cannot invent a match: a marker is a distinctive run, and squeezing only
demands its non-blank characters appear in order.

The regression test pins the row captured off the real transcript, and
asserts the literal match on it is false — if that assertion ever fails,
the screen changed and the test is testing nothing.

### 0.12.172

The scroll view's state is read off the screen, not remembered.

0.12.171 kept a flag: set when `enter` was sent, cleared on the user's
`Esc`.  Reported the same day — after leaving codex's transcript,
scrolling could not get back in.

Two things were wrong with remembering it.  The program leaves that
view on its own as well as by the user's key, so the flag went stale
with nothing to clear it.  And `enter` is typically a **toggle**:
measured, a second `Ctrl+T` closes codex's transcript (33 of 33 rows
revert, `PageUp` stops working).  So a stale flag does not merely fail
to open the view — it shuts it, which is exactly what was seen.

`PaneWheelKeys` now carries a `marker`: text the program shows while
its view is open.  L2 scans the visible grid once per wheel event and
sends `enter` only when the marker is absent.  There is no flag left
to go stale.

### 0.12.171

A plugin can own its pane's wheel (RFC-008, `PaneWheelKeys`).

codex could not be scrolled at all: it repaints in place, so nothing
reaches scrollback, and it asks for no mouse reporting, so the wheel
was not forwarded either.  Reaching it means pressing its own keys,
and only its plugin knows them — verified by injection, `PageUp` alone
does nothing until `Ctrl+T` opens its transcript view.

So the plugin declares (`enter`, `up`, `down` as bytes) and L2 runs
it.  Asking L1 per tick would put a round trip inside a momentum
scroll; L2 holds the declaration and the `entered` flag instead, and
sends `enter` once when scrolling starts from the program's normal
view.

Nothing here leaves that view.  Per the user's ruling — the wheel may
take you in, never throw you out — one stray tick at the bottom would
otherwise close what was being read.  A real `Esc` keypress clears
`entered`, tracked at the key-forward site rather than inferred.

Panes with no declaration are untouched: claudecode still gets mouse
events, and everything else still scrolls its own scrollback.

### 0.12.170

`file://` URLs are one link, scheme included.

claudecode writes `file:///Users/…/paper1-discovers.html` when it
points at a file it just produced.  The scheme was never recognised —
only `http://` and `https://` were — so the path after it matched on
its own: the click worked, but the underline started four characters
late and left a `file:` sitting outside the link.

It is emitted as `File`, not `Url`, for two reasons.  It names
something on disk, so it should face the same `stat` a bare path does;
a `Url` has no arbiter and would underline a file that is not there.
And the click path prefixes anything not starting `http(s)://` with
`http://`, which would have opened `http://file:///…`.

The span covers what is drawn, so selecting the link gets the whole
thing; the click strips the scheme and undoes percent escapes first,
since `open(1)` wants a path and `file://…/a%20b` names `a b`.  An
invalid escape is left as written — a filename may contain a bare `%`,
and mangling it would turn a working link into a missing file.

### 0.12.169

The wheel reaches a full-screen TUI that does not ask for a mouse.

codex could not be scrolled at all.  It redraws in place, so no line
is ever pushed into scrollback and there is no history to move; and
unlike claudecode it never enables mouse reporting, so the wheel was
not forwarded either.  It landed in an empty ring and did nothing.

Measured, same pane geometry:

```text
  claudecode  flags=0x1d  MOUSE_TRACKING on   scrollback_len 0
  codex       flags=0x05  MOUSE_TRACKING off  scrollback_len 0
```

Both have no scrollback — that part is correct and not the bug.  The
difference is the one bit: claudecode gets the wheel as SGR mouse
events and does its own history.

So the wheel now takes a third route when a session is in the
alternate screen and has NOT asked for mouse reporting: arrow keys,
which is what iTerm2 and Kitty do for this case and what `less`,
`man` and a mouse-less `vim` already understand.  DECCKM decides the
encoding — an app-cursor-keys program wants `ESC O A`, and `ESC [ A`
would leave a stray `[` in its input.

The choice is a named function (`wheel_route`) rather than nested
conditions, because the case that would hurt is invisible otherwise:
a plain shell at an empty prompt has exactly the same scrollback depth
as a redraw-in-place TUI, and arrows there walk shell history and
rewrite what the user has typed.  Alt-screen is what separates them,
and a test pins it.

### 0.12.168

A path wrapped short of the pane edge joins up again.

Reported: in a 73-column pane, a path claudecode had wrapped
underlined only as far as `…/lab36-continus/` — the `.tmp/…` tail was
dropped, and the file existed all the way down.

claudecode does not wrap at the pane edge.  It wraps at its own
content width, an indent inside it, and that width differs per block
(`⏺`, `⎿ `, plain prose).  Here the first row stopped at column 65 of
73 — eight cells short — and the flush test allowed exactly eight.
Off by one.  The constant had been measured off a `⎿ ` block in July
and was never going to hold for the next block type.

So the geometry stops deciding.  A trailing token carrying a `/` is
signal enough on its own: it widens the allowance and, for zero-indent
continuations, replaces the old "previous row must be COMPLETELY full"
demand outright — that demand described a mid-word wrap at the pane
edge, which is not what claudecode produces.  Both were already backed
by arbitration that geometry is not: a File match must survive `stat`,
`cc_zero_indent` keeps URL and Email matches from crossing the join at
all, and 0.12.163's seam candidate offers the break point as a path
end when the join was wrong.  Prose keeps the tight two-cell bound.

### 0.12.167

The badge-menu path stops failing silently.

Reported twice: after many hours up, right-clicking a claudecode
pane's badge does nothing, and quitting marspot fixes it.  Not
reproducible on demand.

The path had no way to say where it broke.  `core.badge_hit_miss` is
supposed to be exactly that witness, and it had fired **zero times in
13,289 log lines** across the failing session — because
`badge_miss_report` read `p.shelld_session_id()?` inside its loop.  A
single pane with no session id yet (one still starting, one whose L3
just died) returned from the whole function, so any pane behind it
could never be reported.  `hit_test_pane_badge_prefix` right next to
it does the same lookup with `continue`; this one was the odd man out.

Also instrumented, because each of these was indistinguishable from
the others at the outside — "clicked, nothing happened":

- `core.badge_menu.requested` — L2 did ask (debug)
- `core.badge_menu.no_session` — badge hit, pane has no session, the
  click falls through to the generic menu (was silent)
- `shell.badge_menu.request` — L1 received it, with the item count
- `shell.badge_menu.no_items` — no plugin offered anything, so
  nothing will open (was silent by design)

The plugin already logged its own two exits (`badge_menu.no_bind`,
`badge_menu.empty`).  With the witness repaired the next occurrence
says which link broke instead of leaving 13k lines of nothing.

### 0.12.166

RFC-007 reverted: L3 goes back to a direct fork.

The clean-exec chain worked exactly as designed — the L3 process and
every file it wrote were unmarked, verified end to end — and bought
nothing.  Measured with ONE instrument and repeated trials, a chain
with no `com.apple.provenance` anywhere pays the same first-execution
scan as marspot's own, idle (0.39–0.48 s vs 0.30–0.49 s) and under
load (1.28–1.33 s vs 1.16–1.35 s).  `sshd` is in the same band.

The 60x and 11x figures that motivated it were artefacts: the two
sides had been timed by different methods (two `python3` processes
reading `perf_counter` versus `subprocess.run`), and the 11x was a
single run of each.  With one instrument the difference is gone.

What separates the fast terminals is not the chain at all.
`Terminal.app` (an Apple platform binary) and `iTerm.app` (notarised,
stapled ticket) log **zero** `performScan` and cost 0.00 s; everything
else scans every time.  Provenance, Hardened Runtime, `cs.*`
entitlements, install location, `DeveloperTool` TCC grants and
`posix_spawn` disclaim were each ruled out with their own control —
see `docs/rfc-007-clean-exec-chain.md`, kept as the record so the
search space is not re-explored.

Keeping an unused `launchd` dependency on the pane spawn path is
failure surface for no gain; it had already leaked 26 jobs in its
first hour.  `examples/exec_tax_probe.rs` survives as the measuring
tool.

### 0.12.165

Exited session jobs are reaped.

`launchd` keeps a job in the domain after its process exits until
someone removes it, so RFC-007's per-session jobs accumulated: one per
pane ever opened, plus one per test that spawned a real L3.  Twenty-six
of them were sitting in `launchctl list` within an hour of shipping it
— exactly the unbounded growth this project refuses everywhere else,
and invisible unless that list is read.

A job with no pid has already exited and holds nothing worth keeping,
so the spawn path reaps those first.  Live jobs are never touched, and
a dev sandbox's jobs (different label hash) are collected by the same
pass once their processes are gone.

### 0.12.164

L3 is booted through `launchd`, off an unmarked copy of its binary.

RFC-007.  Every process under a user-installed `.app` carries
`com.apple.provenance`; it is inherited by children and by whoever
execs a marked file, and every file such a process writes is marked in
turn.  So a binary built inside marspot is marked, and its **first
execution** pays a full Gatekeeper scan — a notarisation round trip
(3 s timeout, retried) plus an XProtect pass, all serialised through
one `syspolicyd`.  Same command, same minute, measured here:

```text
  clean chain     0.131 / 0.025 / 0.036 / 0.158 / 0.060 s
  marspot's       3.630 / 0.340 / 4.135 s
```

Reports from a heavier test tier reached 30.55 s, and one harness
268.50 s.  It reads as "the tests got slower", not as a terminal
defect, which is why it went unattributed for so long.

Not marspot's bug — iTerm2 pays it too, and its `DeveloperTool` TCC
grant does not help (verified: no such decision appears anywhere in
the exec log).  `Terminal.app` is exempt only for being a `/System`
platform binary.  But it is marspot's problem, because the user's
build-test loop lives here.

Two conditions, both necessary: the process must not descend from the
app, and the file it execs must itself be unmarked.  So `binaries/clean/`
holds a copy written BY a clean process (`cat` + rename under a
`launchd` job — `cp` copies extended attributes, which reproduced the
very mark the copy exists to shed), refreshed when the source's
(len, mtime) moves, and L3 boots from it as a per-session `launchd`
job.  `MARSPOT_CLEAN_EXEC=0` returns to the direct fork; a failure on
either half falls back to it with a warning rather than costing a pane.

The process tree keeps its shape (L3 → shell, L3 at PPID 1), so
pidtree, the session cap and crash isolation see what they saw.

### 0.12.163

The seam claudecode broke a line at is offered as a path end.

This is the defect the 2026-09-04 report was actually about; 0.12.161
and 0.12.162 fixed two real neighbours of it and left it standing.
Replaying session 385's bytelog is what separated them — the rows in
question carry `wrapped=false`, so none of the DECAWM-based
reproductions had been touching this path at all.

claudecode hard-wraps with a hanging indent, and the merge that undoes
it pops the previous row's trailing blanks and skips this row's
indent.  When the break landed *inside a word* that is exactly right:
`…provenance-exec-tax` + `-2026-09-04.md` has to join seamlessly.  When
it landed *on a space* the space is now gone from both sides, so
`…provenance-probe.sh -n 5` merged to `…probe.sh-n 5`, which is not a
file — and the walk back through punctuation settled on the directory,
`…/spg/`.

The two shapes are indistinguishable on the grid: both first rows fill
the width, both continuations open with `-`.  So the seam is not
decided either way — it becomes a candidate end, tried after the full
span (which keeps a genuinely word-broken name matching whole) and
before punctuation (which is a guess, where this is structure).  The
code had promised this for a while: `LineSegment`'s comment already
said File matches were arbitrated by a "segment-boundary retry" that
was never actually written.

### 0.12.162

Links near the bottom of a claudecode pane survive the caret.

0.12.161 stopped the chromeless-composer branch from claiming a
soft-wrap continuation, which fixed the truncation it was reported
for.  It left the larger half of the same defect standing: a path that
does NOT wrap, on the last line of output, lost its link **entirely**.

The branch exists because claudecode v2.1.212 draws a composer with no
box and no rules — just a prompt line — so there is no chrome to find
and the caret's own row has to be the exemption.  But the caret comes
to rest at the end of the last line of output and stays there for most
of a pane's life.  "The caret is on this row" identifies the composer
about as well as it identifies any other row.

The exemption is now gated on the row looking like a prompt the app
drew (`❯ > › ⟩ » $ %` as its first non-blank glyph).  The asymmetry is
deliberate: a wrong guess here shows one spurious link inside a
composer, while a wrong guess the other way silently drops every link
near the bottom of the pane — which is where the output being read
actually is.

### 0.12.161

A path that wraps onto the last row keeps its tail.

The composer exemption — the rows a claudecode pane draws for its
input area, which must not be scanned for links — has a branch for the
chromeless composer v2.1.212 ships: no box, no rules, just a prompt
line, so the caret's own row is the exemption.  When the upward walk
found no rule above the caret, that branch claimed the region anyway,
from the caret down.

The caret spends most of a pane's life resting at the end of the last
line of output.  When that line is a path long enough to soft-wrap,
its continuation row is exactly where the caret sits — and the phantom
exemption swallowed it.  The scan then saw only the first row,
`…/notes/provenance-probe`, which is not a file, and backed off
through the candidate ends to the longest one that is: `…/spg/`, four
segments short of what the link pointed at.

A composer's first row is drawn by the app; it is never the tail of a
line the terminal wrapped.  That distinction now gates the branch, so
a caret resting on a continuation leaves the exemption alone.

### 0.12.160

A path with a Chinese gloss glued to its end keeps its extension.

`——` attaches an explanation to the thing just named, with no space
in between, so the greedy path scan swallowed the gloss whole:
`…/paper1-discovers.html——页首是三条路与定论`.  Arbitration then walks
back through the candidate ends, longest first, asking the filesystem
about each — but it only cuts at marks it recognises, and the dash
family was not among them.  The walk went straight past the extension
to `…/paper1-discovers`, missed, and settled two levels up on the
directory that happened to exist.  The link stopped at
`…/lab36-continus/`.

The dash family (– — ― − －) joins the cut points, along with · • ． ～.
ASCII `-` stays out — filenames are full of it.  Adding a cut point
can only ever add a candidate, and candidates are offered to the
filesystem longest-first, so a name that really does carry a dash is
still matched whole before any shorter cut is tried.

### 0.12.159

The IME candidate window follows the caret into panes whose app has
hidden the cursor.

The anchor marspot hands AppKit was gated on DECTCEM: a pane whose
program had asked for an invisible cursor published no caret rect at
all, and macOS fell back to parking the candidate window wherever it
liked — typically far from the pane being typed into.  claudecode
keeps the cursor hidden for the entire time a task runs, which is
exactly when the next message gets typed, so the same pane anchored
correctly when idle and wrongly when busy.

Visibility was never the right question.  The pre-edit overlay is
painted at the grid cursor no matter what DECTCEM says, and the grid
cursor is where the composed text lands either way.  The anchor now
comes from `PaneBackend::ime_caret_cell`, which answers with the grid
cursor for any pane that owns a PTY and `None` only for a vacant slot,
which has nothing to insert into.  mcli's single-pane path lost the
same gate.

### 0.12.158

Routes the new `PaneResetMouseReporting` frame to the pane's L3.

Forwarded rather than acted on: the terminal whose modes these are
lives in L3, and L2's `mouse_tracking_active` is a mirror of what L3
publishes next.  Same shape as `PaneHoldGrid`.

### 0.12.157

The wheel now goes where the keyboard goes: a pane a plugin is
holding no longer receives scrolls.

Keys have routed to L1 since RFC-003 whenever the held session asks
for `LOCK_KEYS`; the wheel never did, and fell straight through to
the PTY.  On a pane whose program had mouse tracking on, that means
`apply_scroll_lines` encoded each notch as `CSI < 64;x;y M` and typed
it in — so scrolling during a profile cycle wrote mouse reports into
the shell prompt the cycle had just uncovered, and the echo of them
kept the PTY busy enough that the cycle's `await_quiet` could only
ever run out its 30 s (seen 2026-09-01: a screenful of
`^[[<64;37;32M` at a `git:(develop)` prompt).

Dropped rather than routed up to the plugin: a held pane's picture is
frozen, so there is nothing for a scroll to move.

### 0.12.156

A right-click on a pane badge that does nothing now says why.

Reported as "the badge no longer opens its menu; quitting and
reopening Marspot fixes it".  Three hypotheses were measured and all
three were wrong: the `·` in `opus-5·high` is one cell wide, not two,
so the badge's box did not drift; reclamation is off on this machine
and `dormant.tsv` is empty, so no pane was wearing a frozen badge with
its binding gone; and `new_mapping` / `new_meta` are filled on the
same line, so a pane cannot be badged without an identity.

What the hunt did establish is that the chain has **three silent
ways to do nothing** — the hit-test missing, the plugin returning no
rows, and this side receiving none — and not one of them left a
trace.  That is the defect worth fixing before guessing a fourth
time.

- A right-click landing in a title strip whose pane carries a badge,
  but missing it, logs the click, the box the hit-test computed, the
  badge text, and the terms that go into the box (`cell_w`,
  `reserved`, `focused_idx`, `update_pending`).  The box is computed
  twice from the same inputs — here and in the renderer — and nothing
  was checking that the two agree.
- An empty `PaneBadgeMenu` reply logs the session it was for.

Behaviour is otherwise unchanged.  The next occurrence names itself.

### 0.12.155

A verdict the probe already holds is never withdrawn.

Reported as "an unfocused pane has no links, focusing it brings them
back — and sometimes it has them anyway".  The refresh was not the
waste it looked like; the answer was evaporating, and then getting
stuck that way:

1. A path verdict expires on a 5 s TTL.  `lookup_or_queue` had a held
   answer and did not use it: it queued a re-probe and returned
   `Unknown`.  The scanner reads `Unknown` as "not a link yet", so the
   next rebuild of that pane drew those rows as plain text.
2. The worker re-probed and got *the same* answer, so `changed` was
   false and the generation did not move.  That part is right, and is
   what 0.12.153 deliberately fixed: the counter invalidates **every**
   pane's instance cache, and rebuilding all 14 to re-confirm an
   answer already on screen is worse than the flicker.
3. So nothing rebuilt that pane again.  It sat there without links
   until something unrelated disturbed it — focusing it, for one,
   since `window_focused` is in the pane fingerprint.

"Sometimes it has them anyway" is which side of a lapse window the
rebuild happened to land on.

`Unknown` is now only for a path nobody has ever answered.  A lapsed
TTL means "worth asking again", not "no longer true", so the held
verdict is served while the refresh runs — in both cache generations,
with a cold hit still promoted.

The property that buys, worth stating plainly: **what is on screen
changes only when the filesystem's answer changes.**  No clock takes a
link away.

One thing the report guessed at that is already true: the scan does
not run per frame.  Each pane's instances are cached by fingerprint
and the scan happens only on a real rebuild — which now means only
when the content changes or a verdict actually flips.

### 0.12.154

The click hit-test uses the async oracle too.

0.12.152 moved the *render* pass off the filesystem and the render
stalls stopped dead — but `l2.loop.stall` immediately started
reporting the `events` phase instead, at 3.44 s / 5.47 s / 6.39 s.
Sampling the core the moment it logged `l2.loop.stalling` caught it:
`mouse_down → hit_test_link_at_xy → FsOracle::probe → lstat`, 948 of
1556 samples.  Every click in a pane was doing a full-screen blocking
stat sweep.  (The call site was missed on the first pass because the
grep that was supposed to find every caller was truncated by a
`head -20` — grid_links.rs's own tests filled the window.)

Beyond not blocking, this is the correct oracle for the hit-test on
its own merits: a link is clickable because it is *painted*, and it is
painted because the render pass got `Exists` from this same cache.
Asking a different oracle lets the two disagree in both directions —
an underline that does nothing, or a click firing on a row that shows
no link.

### 0.12.153

`link_probe` bumps its generation only when a verdict is new or
flipped, not on every landed probe.

The counter invalidates *every* pane's instance cache, and a verdict
expires on a 5 s TTL, so re-confirming an answer the renderer had
already drawn would rebuild all 14 panes on that timer — the instance
cache defeated by a clock rather than by change.  Caught reading the
first post-fix stall line, which showed `13/14rebuilt`.

### 0.12.152

Link scanning no longer stats the filesystem from the render thread.

`build_instances` scans every rebuilt pane for clickable spans on every
frame, and deciding whether a token is a *file* link meant calling
`lstat` right there.  On a machine with network mounts that is not a
syscall, it is a network round trip.  Measured on the dev box:
`lstat` on a missing path under an SMB-over-Tailscale mount has a
median of 1.3 us and a **maximum of 6.13 s**; `/Volumes/home` peaked at
5.98 s; a `/home/...` path — any line mentioning a Linux path — goes
through the `auto_home` autofs map and costs 17.8–22.7 ms *every*
scan even when the network is healthy.

That is what the live 14-pane window was dying of on 2026-08-22: the
core logged 110 build-bound `l2.loop.stall` frames in two hours,
64.3 s of build time against 0.5 s of GPU, single frames of 2.1 s to
13.6 s, with `l2.loop.stalling — every pane is frozen right now`
throughout.  A `sample` of the core put 675 of 849 render samples
(79.5 %) inside `lstat` beneath the link scan.  The shell's watchdog
then read the stalls as a hung core and killed it four times in
90 seconds, ending in a five-minute gap with no core at all.

Volume was never the problem — replaying each pane's real bytelog
(`examples/link_scan_probe`) puts the whole window at 0–37 probes per
scan.  Tail latency was.  So no cache fixes this; only not waiting
does:

- `marspot-linkify` performs no I/O at all any more.  Existence is
  decided by an injected `PathOracle` returning `Exists` / `Missing` /
  `Unknown`.  The scanner treats `Unknown` exactly like `Missing`, so
  "not resolved yet" needs no notion of pending anywhere downstream.
- `marspot::link_probe` is the render path's oracle: an all-memory
  lookup that answers immediately, queues misses to one worker
  thread, and never holds a lock across a syscall.  Bounded by
  construction — two 4096-entry cache generations that rotate and
  promote instead of the old wholesale `clear()` at 256, a 512-entry
  queue that drops on overflow, and a 5-minute TTL for any probe that
  took over 20 ms so a stalled mount is paid for once, not on a timer.
- Its generation counter is folded into the per-pane instance-cache
  fingerprint, so a pane whose grid did not change still rebuilds once
  when link answers land and the underline actually appears.

Steady-state scan of a grid holding `/home/kevybench/boxpre.log`, 30
scans, median of 4 runs: **19.3 ms → 0.028 ms** (~690x), and now
constant across local / autofs / SMB / NFS paths instead of tracking
whatever the mount is doing.

### 0.12.151

**`~/…` 的文件链接,点「Open file」永远没反应。**

```
$ /usr/bin/open '~/Downloads/x.pdf'
The file /Users/doracawl/workspace/goliajp/marspot/~/Downloads/x.pdf does not exist.
```

`~` 是 shell 的语法,shell 之下没人展开它 —— `open(1)` 把它当成 cwd 底下一个**名叫
`~` 的目录**。而 linkify 判断路径存不存在时**是**展开的(`path_exists` 里拼 HOME),
于是链接照常带下划线、点下去什么也不发生。**每一个 `~/` 开头的文件链接都是这样**,
不是某个文件的问题。

两处一起修:

1. **展开移进 `marspot-linkify::expand_user_path`,由 `grid_links` re-export。** 检测
   和动作必须对「`~/…` 是什么」有同一个答案;两份实现迟早漂移,这次就是漂了。
2. **失败不再无声。** `spawn` 只报告「进程起不来」;`open(1)` 自己失败(通常是路径没
   了)只是退出码非零 + 一行没人读的 stderr。现在动作前若路径不存在,记一条
   `core.link_open_missing` —— 链接是 stat 过才画的,所以这种情况意味着**文件在那一帧
   之后被移走了,而那一行没有重绘过**(按需重绘是 idle CPU ≈ 0 的前提,不是 bug)。

顺带把 open 参数的计算从上下文菜单的事件路径里提成自由函数 `open_arg_for`,否则唯一
真正出错的那条分支从外面根本测不到。红-绿验过:把展开去掉,测试立刻 FAIL。

**本次报告的那个文件另有原因**:`~/Downloads/Maintained-but-Not-Internalised_ICLR2027
-submission-draft_2026-08-21.pdf` 磁盘上确实没有 —— 现存的是
`Maintained-but-Not-Internalised_ICLR2027.pdf`(少了 `-submission-draft_2026-08-21`)。
即便修好展开,那条链接也打不开,因为目标不存在;新加的日志正是为了让这种情况说得出话。

### 0.12.150

**焦点框的右边和下边不是高亮白 —— 被邻居的遮罩压暗了。**

用户的描述几乎就是答案:「似乎被右、下、右下三个 pane 的半透明 border 盖住了一半」。
不是 border(未聚焦的 pane 不画边框,`GridItem::paint` 在 `!focused` 时直接 return),
是 **scrim** —— RFC-006 那层「未聚焦即退后」的半透明黑,盖住整块 pane 矩形。

为什么只有右和下:焦点环画在**接缝**上,而接缝的几何是不对称的。接缝厚度是
`gutter × 4`,间隙只有 `gutter`:

    左边线覆盖 [x-g, x+3g]   → 3g 落在自己身上(focused,scrim = 0)
    右边线覆盖 [x+w, x+w+4g] → 整条落在右邻居的矩形里

而 scrim 走 **overlay pass**,焦点环走 **BG pass** —— overlay 在后。于是最该亮的两条
边,恰好是被邻居调暗的两条。左和上没事,纯粹因为它们画在自己身上。

修法不是把环挪进 overlay 了事:那条管线是 SDF 的,`smoothstep(-aa, aa, d)` 会给硬边
镶一圈 AA,而这个环的注释里写着它要 pixel-perfect 地**替换**灰接缝的颜色。所以**画
两遍**:BG 那遍照旧(像素精确),scrim 之后在 overlay 再画一遍同位置同色的。第二遍的
AA 边缘混合的是它自己下面那条一模一样的矩形,所以看不出来。

回归测试搭一个 2×2、三个邻居都带 scrim 的布局,断言 overlay 里有那 8 个矩形**且排在
所有 scrim 之后**(顺序才是关键,不然等于没画)。红-绿验过:把第二遍去掉,测试立刻失败。

### 0.12.149

**连着三条路径,中间两条没有下划线。**

```
  /Users/…/spg-repro/替换验收标准.md          ← 有
  /Users/…/spg-repro/SENTORI_2026-08-        ← 有(跨行)
18_REPORT_5.md
  /Users/…/spg-repro/drop-column-che         ← 有(跨行)
ck.sql
  /Users/…/spg-repro/join-bugs.sql           ← 没有
  /Users/…/spg-repro/describe/               ← 没有
  /Users/…/spg-repro/bind/                   ← 有
```

六个路径**都存在**,所以不是 stat 的事。

**合并本身没做错。** 这是 cc 的 TUI 输出(bytelog 里是 `\x1b[H  /Users/…`:绝对定位 +
两格缩进),`join-bugs.sql` 那行距右边缘 2 列、`describe/` 那行距 6 列,都落在路径的
8 列宽容内;而下一行有 2 格缩进 —— **这正是「一个路径被切断后带悬挂缩进续行」的形状**。
三行于是合并成一条逻辑行:`…/join-bugs.sql/Users/…/describe//Users/…/bind/`。

拆开它是「接缝重试」的活:逻辑行在**行接缝**处切一刀,看前缀是不是真路径。它算得
完全正确 —— 实测每一步都返回了正确的切点(71、136)。**但它从来没被调用**:

```rust
if looks_like_path(&chars[i..end]) {          // ← 检查的是整条合并串
    if let Some(b) = retry_file_at_segment_boundaries(…)
```

而整条合并串是几个路径首尾相接,**当然不像路径**。这道守卫恰好挡掉了这个函数唯一
存在的理由。它内部本来就对每个候选**前缀**做了 `looks_like_path` —— 那才是该问的
问题。去掉外层守卫。

回归测试自建三个临时文件/目录,按同样的几何(每行距右边缘 2 列、下一行 2 格缩进)排
成三行,断言三条各自成链。

### 0.12.148

**同 L3 0.11.54:孤立可打印字符直接派发,不过状态机。**

### 0.12.147

**同 L3 0.11.53:RAM ring 写入不再补齐整行。**

### 0.12.146

**gate 现在拿产品路径跟竞品比,而且要求赢。**

`vs_best_other` 问的是「marspot 比最快的另一个终端如何」,而这个问题诚实的主语是
**用户的 pane 在做什么**(core → L3、文件 scrollback),不是 mcli。此前只能拿 mcli 比
——L3 探针是坏的,而且两个数当时也不是同一个 metric。两件都修好了(同一套认证协议:
32 MiB + DSR 往返),所以 `load_live` 改为优先取 L3。

空闲 mini 实测,产品路径 vs ghostty:

| 语料 | 起点 | 现在 | | ghostty | |
|---|---:|---:|---|---:|---|
| ascii | 40.9 | **194.5** | 4.8× | 101.7 | **+91%** |
| mixed | 35.0 | **167.8** | 4.8× | 101.7 | **+65%** |
| cjk | 39.8 | **165.7** | 4.2× | 145.9 | **+13.6%** |
| emoji | 39.7 | **123.1** | 3.1× | 115.7 | **+6.4%** |

`mars_vs_best_other_min` 因此锁在 1.81 / 1.57 / 1.08 / 1.01 —— **四条都在 1.0 以上**,
gate 从此在「产品不再赢过实测最快的竞品」时变红。这是要求,不是愿望。

二进制上限同批重锁:mcli 1392000 → 1452592、marspot 1567136 → 1626608。两者在这段
提交里各长了约 43 KB,**超过注释里说的「20 KB 以下不必看」的门槛,所以看了**:增长
来自 `async_writer` 模块与重写过的 scrollback / bytelog 路径,换来的是产品路径
2.9-4.8×。

### 0.12.145

**同 L3 0.11.52:scrollback 推行路径的零分配与扁平编码。**

### 0.12.144

**同 L3 0.11.51:磁盘写移出解析线程(`async_writer`)。** 收益整个落在 L3(只有真实
pane 用文件 scrollback 与 bytelog),实现与契约变化记在那条。

### 0.12.143

**产品路径 2.1-2.8× —— 索引文件每行一次 8 字节的 write。**

| 语料 | 之前 | 现在 | | vs ghostty |
|---|---:|---:|---|---:|
| ascii | 40.9 | **111.0** | 2.7× | **+9%** |
| mixed | 35.0 | 97.5 | 2.8× | −4% |
| cjk | 39.8 | 98.5 | 2.5× | −32% |
| emoji | 39.7 | 83.9 | 2.1× | −27% |

`FileScrollback` 有两个文件:`bin`(行记录)与 `idx`(每行 8 字节的偏移)。`bin` 一直是
`BufWriter`,而 `idx` 是**裸 `File`** —— 注释写着理由:「每次 8 B 一个 syscall
(~5 µs),well within budget」。

估算没错,**预算量错了工况**:交互输出每秒推几行,`cat` 一个大文件每秒推 20 万行。
33.5 MB 的 cjk 语料 ≈ 279k 行 = **279k 次 8 字节 write syscall**,profile 里是解析
线程 63% 的样本。

改成缓冲,并把两个缓冲当**一对**来定容量 —— 关键不是各自多大,而是**谁的未落盘窗口
更大**:

    bin  64 KiB / 每记录约 100 B ≈ 640 行
    idx   4 KiB / 每记录 8 B     = 512 行

idx 的窗口严格小于 bin,恢复扫描赖以工作的不变式(索引绝不声称数据文件拿不出的行)
就还在。Rust 的 `BufWriter` 默认 8 KiB = 1024 行,**比 bin 还大**,会把不变式反过来 ——
`file_torn_write_no_past_eof_orphans` 当场抓住了这一点(它就是为守这条而写的)。

原设计的不对称才是真正奇怪的地方:`bin` 缓冲、`idx` 不缓冲,意味着非正常退出时两者丢
的是**不同的尾巴**,靠 `open()` 扫描去和解。现在两者丢同一段。

### 0.12.142

**探针可信了,于是看见了真数字:用户那条路,四条语料全输 ghostty 60-73%。**

| 语料 | L3(产品) | mcli(此前 bench 报的) | ghostty | 产品 vs ghostty |
|---|---:|---:|---:|---:|
| ascii | 40.9 | 104.9 | 101.7 | **−59.8%** |
| mixed | 35.0 | 101.7 | 101.7 | **−65.6%** |
| cjk | 39.8 | 101.7 | 145.9 | **−72.7%** |
| emoji | 39.7 | 93.2 | 115.7 | **−65.7%** |

此前所有竞品对照用的都是 `mcli` —— 一个进程、一个线程、**内存 scrollback**。产品是
L3:每个 pane 一个进程,**文件 scrollback**。两者不是同一条路,而我们拿前者跟竞品比了
很久。

### 怎么让探针可信的

不是给它加功能,是让它的数字**能被解释**,并且拿与计时无关的证据佐证:

1. **profile**:主线程 63% 的样本在 `Grid::scroll_up → FileScrollback::push_line →
   File::write_all → write`。
2. **旁证**(探针现在每次都打印):一趟 33.5 MB 语料,session 写出
   `scrollback.bin` 26.8 MB + `bytelog` 34.1 MB = **61 MB**,写放大 1.8×。1.22 s 写
   61 MB ≈ 50 MB/s,正是磁盘该有的样子。
3. **指纹**:L3 在四条语料上都是 35-41 MB/s,**几乎与内容无关**。被 parse 限制的系统
   不会这样(ascii 的 parse 比 emoji 快 3 倍);被磁盘带宽限制的系统正是这样。
4. **稳定性**:mini 空闲时三次 trial 的离散度 ±0.1-0.6%。本机早先那 18/26/29 的抖动
   是本机磁盘与负载,不是探针。

### 同时更正两条我自己发布过的错误结论

- **「磁盘 scrollback 只值 ±4%」——作废。** 那是用 `MARSPOT_DISK_SCROLLBACK=0` 量的,
  而这个开关**早就被删了**(`terminal.rs` 的注释白纸黑字:"both were no-ops");
  File scrollback 由 `MARSPOT_SESSION_ID` 是否存在决定。开关无效却看起来生效 ——
  又一次「装置的失效长得跟数据一样」,而且我踩了两遍。CLAUDE.md 里同一句过期描述一并
  更正。
- **「L3 慢 3.5 倍」的第一版证据**——那次探针不读 UDS,L3 卡在往没人收的 socket 写帧。
  已在 0.12.141 记过,这里补一句:修好之后结论方向没变,但根因完全不同(不是 IPC,是
  磁盘)。

### 下一轮

瓶颈是**每字节写 1.8 倍到磁盘**,而且写在解析线程上。scrollback 与 bytelog 都是产品
特性(无限历史、execv 重放),不是能删的东西 —— 要动的是**它们什么时候写、在哪个线程
写**。`MARSPOT_BYTELOG=0` 已可给 bytelog 那半定价(+5%);scrollback 那半没有开关,要
定价得先造一个。

### 0.12.141

**重建 `l3_throughput` 探针 —— 产品路径的 gate 空转了两个月。**

`bin/measure-l3.sh` 测的是「用户真正体验的那条路」(shell → core → L3),
`bin/bench.sh --full` 读它的结果。RFC-003 Phase 6g(`888bf1b`,2026-06-17)删掉 L4
shelld 时,连带删掉了 `crates/marspot-session/examples/l3_throughput.rs` ——
**但没删调用它的脚本**。于是脚本一直去跑 `target/release/examples/l3_throughput`
这个 shelld 时代的残留二进制,每个 trial 都失败,而
`bench/results/l3-throughput.json` 停在最后一次成功(2026-06-14),看起来跟新鲜数据
一模一样。又一次「测量装置的失效长得跟数据一样」。

按三层架构重写:spawn 一个 L3、UDS 握手、GridResize(**不发它的话 session 永远不开
PTY**)、通过 `$SHELL` 交给它一个 trial 脚本(L3 走 `local_session` 直接 spawn
`$SHELL`,**不认 `MARSPOT_SHELL`** —— 那是 in-process 那条路的覆盖),脚本以 DSR 往返
收尾。

**探针尚未可信,数字先不入账。** 一个真正的 core 还会读 shm、回 SurfaceReady;第一版
探针连 UDS 都不读,于是 L3 往一个没人收的 socket 写帧、被 backpressure 卡在 `write`
上 —— profile 显示它 75% 的样本在 `write`,读起来像是「L3 慢 3.5 倍」的铁证,实际是
harness 的 bug。补上排空线程后同一配置三次给出 26 / 29 / 18 MB/s。在探针能完整扮演
L2 之前,产品路径的吞吐**记作未测**,而不是记一个难看的数。

顺带加了 `MARSPOT_BYTELOG=0`(对称于 `MARSPOT_DISK_SCROLLBACK=0`),用来给 L3 那两份
磁盘写定价。

### 0.12.140

**同 L3 0.11.48:pty reader 的空探测桥接。** 引擎在 L2/L3 共用,改动与实测记在
L3 0.11.48。

### 0.12.139

**把「live 管道」拆成能分别测量的四段。**

竞品对照说 marspot 在 cjk 输 43%、emoji 输 24%,而 headless parse 是 224–236 MB/s、
live 只交付 101.7 —— 中间那 2.3× 里同时坐着 pty、reader 线程、parser、事件循环和
GPU,一个比值说不出是谁。

三个只在 `bench-pty` feature 下编译的模式,各切一刀:

- `pty-raw` —— `cat` 跑在真 pty 下,用最朴素的阻塞 `read(2)` 循环排空,没有线程、
  没有 channel、没有每批一个 Vec。剩下的全是子进程的 write、内核 tty 层(含把
  `\n` 变 `\r\n` 的 ONLCR 膨胀)和 read 系统调用本身。
- `pty-drain` —— 换成真实的 reader 线程 + channel,但不解析。
- `pty-feed` —— 再加上 `Terminal::feed`。差 live 一个窗口。

`--bench parse` 另加 `:<chunk>`:live 从不把整个语料交给 parser,reader 每次送
READ_BUF(64KB),`pump` 每块调一次 `feed`。批量车道是对连续切片的扫描,跨块的 run
会被切断 —— 一次性喂完再叫它「parser 的速度」是虚报。(实测:虚报得不多,64KB 分块
与一次性喂差 <1%,四条语料都是。这条**推翻了**「分块是 live 损失的原因」这个假设。)

第一批读数(cjk):

| 段 | ms/MB |
|---|---:|
| pty-raw | 5.7 |
| pty-drain | 5.7 |
| + parse | 8.1 |
| live | 9.8 |

**pty-raw ≈ pty-drain** —— 我们的 gather 架构(线程/channel/Vec/探测 poll)几乎不
花钱,那 5.7 基本是内核与 `cat` 的共通成本,ghostty 同样要付。而 ghostty 的**全程**
是 6.85 ms/MB,低于我们光 gather 的开销 —— 它只可能是把 parse 藏进了 gather 的
影子里,而我们实测是两者相加。这是下一轮的靶,写在 docs/perf.md。

顺带一个关于计量的教训:这些计数器第一版直接编在 release 里,每次 read 三个原子加。
8.4MB 语料 = 8360 次 read,gate 上 cjk/emoji 各掉 0.5% —— **贴着地板 FAIL**。移到
feature 后面立刻回来(cjk 224.4 / emoji 147.4,还是在 load 8.8 的机器上)。二进制也
从 1571168 降回 1551424 字节。**开发工具不该进 shipped binary**,这条 `snapshot`
feature 的注释早就写过了。

### 0.12.138

**`CSI 6 n` 一直没实现 —— 问光标在哪的程序,等到的是沉默。**

DSR(Device Status Report)不是冷门序列:readline 重画一个折行的提示符时就靠
`CSI 6 n` 问列号,得不到回答的程序会一直等。marspot 此前把它丢进
`term.csi.unsupported` 的采样日志里,再没有下文。

补上两条:

- `CSI 5 n` → `CSI 0 n`(活着)
- `CSI 6 n` → `CSI <行> ; <列> R`,1 基。没有 DECOM 可言,绝对位置就是唯一读数,
  也正是 DECOM 关闭(到处都是默认)时程序期待的那个。

参数不认识就沉默,不回一个畸形的报告。

发现它的过程值得记一句:竞品对照的数字对不上 —— 同一个终端同一天两次跑,CJK
从 0.27s 变成 0.08s。根因是 `time cat` 的原理只能证明**字节被写进了 PTY**,
证明不了终端处理完了;一个终端靠丢弃或合并就能让 `cat` 提前返回,从而「赢」。
让终端自己确认的标准办法就是 cat 完发一个 DSR、等它回答 —— 而我们自己答不了。
所以这条既是兼容性缺口,也是把 live 测量做可信的前置(见 L2 0.12.136 那条起头的
测量装置工作)。

### 0.12.137

**emoji 语料的每个字形都在走最慢的那条路。parse 112 ms → 52 ms(2.17×)。**

起点是一次对照:live 管道今天在 mini 上跑 `cat` emoji 需要 175 ms,而 6-08 的
二进制在**同一台机、同一天、同一份语料**上只要 110 ms。先把装置本身钉死 —— GUI
会话(`launchctl asuser`)与 ssh 会话逐样本一致,排除环境;交错 A/B 五轮零重叠,
排除噪声(mini 上另一个项目在编译时,同一 commit 能测出 130 和 290 两个值,靠交
错 + 取最小值才拿到可信数)。**1.64× 是真回归。**

分类实验一刀切开(headless parse,同机同天):

| 语料 | 6-08 | 今天 |
|---|---|---|
| ascii | 183 MB/s | 429 MB/s |
| mixed | 182 | 257 |
| cjk | 222 | 232 |
| **emoji** | **208 (40.3 ms)** | **75 (111.2 ms)** |

parse 自己多花 71 ms,live 差值 70 ms —— **退化 100% 在 parser 的 emoji 路径**,
管道其余部分没有份。

`sample` 定类别:Unicode 表查找占 38%。而语料的真相是 **零 ZWJ、零 VS16,每个
emoji 都是单码点**,`🚀 ✨ 🎉` 之间是单个空格 —— ASCII 车道要 ≥2 字符、四字节车道
要 ≥8 字节,**两条批量车道全部够不着**,于是每个字形都付完整的 cluster 状态机。
非 ASCII 每字符 84 ns,而走批量车道的 CJK 只要 12.9 ns。

五项改动,各自单独定价(本机交错 A/B,min-of-5,emoji 一趟的毫秒数):

| # | 改动 | ms | 增益 |
|---|---|---|---|
| — | 基线 | 113 | — |
| 1 | `char_width` 不再无条件算 `is_ambiguous_width`(只有 `All` 模式读它) | 105 | +7% |
| 2 | 单码点 emoji 收进 fast class,不进 cluster 状态机 | 81.6 | +22% |
| 3 | 四字节车道门槛 8 → 4(一个序列就值得直接解码) | 70.5 | +13% |
| 4 | fast class 的 `gbp()` 查表换成两段范围排除 | 54.5 | +23% |
| 5 | 三条车道按首字节分派,不再各问一遍 | 52.5 | +4% |

**累计 2.17×**,并且没有一条语料变慢:ascii +1.2% / mixed +6.5% / cjk +3.7%
(共享热路径的改动必须全语料矩阵重测 —— 只测新能力那条语料看不见 icache 与布局税)。

安全性不是靠"emoji 一般不会那样"论证的。fast class 的既有不变式是**批量提交总把
最后一个字符留在缓冲里**,所以随后到来的 VS16 / ZWJ / 肤色修饰符仍然遇到一个未关
闭的 cluster,走慢路径。需要排除的只有边界依赖邻居的那些:区域指示符(🇯🇵 是
GB12/13 配对成的**一个** cluster)与肤色修饰符(GBP=Extend)。#4 把这个判断从表查
询换成范围,代价是范围可能过期 —— 于是加了一条**遍历全码点空间**的测试,拿两张表
自己重新推导这个类:Unicode 更新哪天引入第三个例外,构建就红,而不是悄悄把一个
cluster 劈成两半。UAX #29 官方一致性测试与 1091 条测试全绿。

### 0.12.136

**`--bench parse` 加重复次数:100 ms 的窗口,采样器看不清里面有什么。**

一趟 8 MB 语料 ~100 ms。要问"这段时间到底花在哪一类工作上",采样器需要的是秒级
窗口,而不是一百毫秒。此前每次想 profile 都得手工套循环,而每轮一个新进程,采样
落在进程启动上的比落在 parser 上的还多。

`parse:<path>:<repeat>` —— 同一份字节喂 N 遍,每遍一个全新 terminal,计时只圈
`feed`,构造成本不进数字。语法向后兼容:不带 `:N` 就是原来的一遍。

这条本身不改任何运行时行为,是下一条(emoji parse 攻坚)的前提 —— 没有它,
`sample` 拿到的全是噪声。

### 0.12.135

**上一条列表项的 URL,把下一条的 `-` 吃进去了。**

```
- 村子 → http://192.168.50.20:6031/index.html?page=pages/village/index
  - 阿云的屋 → http://192.168.50.20:6031/index.html?page=pages/room/index&room=ayun
```

第一条取出来的是 `…/village/index-` —— 尾巴上那个 `-` 是**第二行的行首**。

跨行合并的判据全是几何:上一行顶到右边缘、下一行缩进 0–4 格。这两条这里**都成立**
—— 第一条正好占满整行,第二条是嵌套项,缩进 2 格,跟悬挂缩进长得一模一样。几何分
不出"被切断的 token"和"下一个列表项"。

零缩进那一路本来就有兜底(`cc_zero_indent`:最弱的那种合并,URL 不许跨过去),所以
这个 bug 只在**有缩进**时显形 —— 第一版不带缩进的复现跑出来是绿的,换成现场那两行
才红。

分得出来的不是几何,是**标记**:换行是把一个 token 劈成两半,下一行开头必然是这个
token 的后半段;一个孤零零的 `-` 后面跟着空格,不是谁的后半段,是渲染器摆在那儿的
列表符号。而且就算合并了也只能给上一行的尾巴粘一个标点,永远粘不来路径内容 —— 没有
任何上行空间,只有下行风险。

于是在合并判据的最后加一道:下一行首个 token 若是**单个非字母数字字符后接空白**
(`-` / `*` / `+` / `>` / …),或 `1.` / `2)` 这样的有序标记,就不算续行。真正在
token 中间断开的续行(哪怕它以 `-` 开头,比如 `-more/parts`)照旧合并,单测钉住了
这一面。

### 0.12.134

**一条挺常规的路径,只有前半段是链接。**

```
● Write(~/workspace/labs/lab36-continus/.claude/reports/2026-08-16-e18-ve
  rdict.md)
        ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~ 只有这一截有下划线
```

路径跨了两行。合并两行的判据要求上一行**顶到最右列**(2 列宽容),行尾是 `/` 时
才放宽到 8 列。这里断在 `…e18-ve`,行尾是 `e`,只能拿 2 列 —— 实测真实缺口是
**2 列**,而 2 列的 slack 只容忍 1 列,**差一列**。

合并失败之后:候选串成了 `…-e18-ve`,不存在;于是回退到最长存在前缀,正好落在
`~/workspace/labs/lab36-continus/` —— 就是截图里下划线停住的地方。

注释里 2026-07-18 那次已经发现"cc 的 `⎿` 缩进块在离右边缘几格处就换行",但只给
`/` 结尾放宽了。**真正的信号不是"这行满了",是"一个路径 token 被切断了"** —— 而
换行落在哪个字符上,路径自己说了不算。

改成:回溯行尾那段 url/path 字符,若其中含 `/`,就按被切断的路径处理,拿同样的 8
列宽容(容忍 7 列缺口)。`/` 结尾那条保持原样,现在只是它的特例。

回归测试**扫过 0–7 列的每一种缺口**,不是钉一个宽度 —— 只钉一个宽度的话,只能证明
那一个宽度。

### 0.12.133

**设成 7 列,每次换核心都跳回 6。**

选择器的上限早就从 6 提到了 9,但恢复布局的两条路径里各留了一份写死的
`clamp(1, 6)`:

```
src/bin/marspot-core.rs:5198   restore_saved_window
src/bin/marspot-core.rs:8806   boot
```

**保存那一侧一直是对的** —— 盘上 `shell-state.bin` 解出来就是 `0700 0200`
(7×2)。所以文件说 7、屏幕说 6,除了列数变少之外没有任何异常迹象,只有用户看得
见。这不是"漏改了一处",是**共享上限被抄成了副本**:一份改了,两份没改,而副本
不会报错。

两处都改成 `clamp(GRID_MIN, GRID_MAX)`,跟选择器用同一个常量。

回归测试对 `GRID_MIN..=GRID_MAX` **整段**参数化 —— 只测 7 的话,下次把上限提到
12 又会留下同样的副本。红-绿验过:把任一处改回字面量 6,测试立刻失败。

### 0.12.132

**你在 cc 输入框里打路径,打到一半它就自己划了下划线。**

设计上输入框**整块不该被扫描**(边打边闪下划线、右键弹链接菜单,都不对)。判据是
`find_input_box_rows`:找最底部的圆角框 `╭…╮` / `╰…╯`。而 claudecode 现在的输入框
长这样(从活着的 pane 字节流里抓的,2026-08-14):

```
─────────────────────────   ← 灰色横线
❯ <光标>
─────────────────────────   ← 灰色横线
⏵⏵ bypass permissions on
```

**一个圆角字符都没有。** 找不到框就返回 `None`,豁免整个失效,输入框于是当正文扫。
函数注释里已经记着上一次:v2.1.212 把框去掉,导致最底部的框变成欢迎横幅。**同一处
栽两次,原因一样:判据挂在 cc 的装饰上,而 cc 会重新装修。**

改成挂在**光标**上 —— 光标是协议不是样式,再怎么重画也去不掉,而 cc 的光标就在输入
框里。规则:

- 光标要靠近底部(距最后一行 ≤ `MAX_COMPOSER_ROWS`,且不超过视图一半)。更靠上说明
  它在输出里,那时候往下豁免会吞掉真文本 —— **那比现在这个 bug 更糟**;
- 豁免从光标行到最后一行(输入框下面只有 cc 自己的页脚);
- 向上**有界地**扩到最近的分隔线(`─` 横线或老的 `╭`),让多行输入整块被覆盖。找不到
  就只豁免光标那一行 —— 少豁免可以补,吞掉输出不可以。

圆角检测保留作后备,只服务光标不在视野里(往回翻)的情况。

三条回归测试钉住:今天这套 chrome、v2.1.212 那套无 chrome、以及光标在输出里时**不
许**吞掉下方的行。另有一条把"表格行不是分隔线"钉死。

写测试时被自己坑了一次:第一版 fixture 用了 `/tmp/report.txt` 这种编造路径,全被存在
性校验滤掉,测试于是"通过"得毫无意义 —— **反过来也说明,你截图里那条路径之所以被识别,
正是因为它真的存在**。fixture 换成 `/etc/hosts` `/usr/bin`。

### 0.12.131

**GPU 的活一点没变重,是我们没被调度。**

`gpu_exec`(命令缓冲自报的执行时间)加进来之后,一次受控实验就把问题定死了。同一
台空闲 bench 机,唯一变量是后台 CPU 负载,每档 2000 帧:

| 负载 | 帧 p99 | **GPU exec p99** | **GPU wait p99** |
|---|---:|---:|---:|
| 空闲 | 293 µs | 111 µs | 279 µs |
| 14 hogs(load 6.2)| 7,453 µs | **117 µs** | **7,425 µs** |
| 42 hogs(load 22.9)| 1,720 µs | **115 µs** | 1,546 µs |

**GPU 自己报的执行时间纹丝不动(111 → 117 µs),同一个等待的墙钟涨了 27 倍。**
活没变重,是等它的那个线程不再被运行。而它不被运行的那段时间里,**每个 pane 都是
冻的,看门狗的 PONG 倒计时在跑** —— 这就是「Marspot stopped」横幅的最后一环。

macOS 按 QoS 调度,而从 shell 起来的进程拿的是未指定/默认档 —— 跟它抢 CPU 的批处理
同一档。终端的渲染循环按定义就是 `USER_INTERACTIVE`(「有个人在等这一帧」)。

同机同负载、相隔几秒、两个顺序各跑一遍:

| | 帧 p99 | GPU wait p99 | 最差一帧 |
|---|---:|---:|---:|
| 默认 QoS | 4,953–6,082 µs | 4,912–6,042 | 9.28 ms |
| USER_INTERACTIVE | **323–325 µs** | 308–310 | 0.34 ms |

**p99 快 15–18.7 倍,最差一帧快 21 倍**,负载 11–15 下的表现跟空闲基线(293 µs)
基本一致。顺序反过来跑结论不变(ON 323 → OFF 4,953 → ON 324),排除热身。

只升**画帧的那个线程**,不升整个进程:读 transcript、扫状态的工作线程保持默认 ——
全都升等于都没升。L3 也**故意没升**:一个窗口可以有 81 个 L3,把它们全塞进最高档
是另一种风险,而且没有任何测量说它需要。

### 0.12.130

**L2 真正画的那条路径,此前没有任何像素断言。**

`render_layout_to_texture` 是 L2 每帧走的路。suite 里唯一提到它的地方是一句注释:
真回读"需要一张 `StorageModeShared` 的目标纹理,渲染器现在没暴露",然后就搁在
那儿了。在每个 pass 都新建缓冲的年代这还能忍;**0.12.128 改成就地重填之后不能忍
了 —— 那种改动的失败方式是无声的、在像素上的、发生在"上一帧更大"的那一帧。**

补的测试把契约写成断言:**用重填缓冲画出来的一帧,必须和用新缓冲画出来的逐字节
相同**,包括中间夹了一帧更大的、在缓冲里留下尾巴之后。四帧:小(冷池)→ 小
(热池,必须与前者相同)→ 大(必须与前者不同,否则这测试什么也没证明)→ 小
(必须回到第一帧,否则就是继承了大帧的残留)。

写这个测试时先被自己坑了一次,记在代码注释里:第一版四帧全都 `seq: 0`,渲染结果
完全一样 —— 因为 per-pane 实例缓存 fingerprint 的是 `view.seq`,grid 内容根本不
参与哈希(会话内容一变就会 bump seq)。**缓存是对的,是测试在骗它。**

顺带清掉 0.12.129 引入的两个多余 `unsafe`。

### 0.12.129

分配那条关掉之后,4.5 小时 22 条卡顿里 `instbuf` **全是 0.0ms/0.0MB**,剩下两个
模式各占一半:

- **GPU 等待**:81 / 100 / 206 / 317 / 597 / 748 / **831** ms
- **build**(`glyphs` 全 0):42 / 77 / 84 / 120 / 268 / 387 / **539** ms

两个都是"名字"不是"成本",继续拆:

- **`gpu_exec_us`** —— 命令缓冲自报的 `GPUEndTime - GPUStartTime`,跟墙钟等待并
  排。**等得久但执行很短 = 我们在排队或被调度走了,不是活重** —— 这两件事的解法
  完全不同,而只看墙钟分不出来。
- **`build_panes_us`** —— build 里 13 个 pane 的循环 vs 其余(chrome / 侧栏 /
  面板 / 弹层)。
- **`panes_rebuilt` / `panes_total`** —— 每帧有几个 pane 没命中实例缓存。**每帧
  全部重建 = 缓存没在工作**,这件事任何计时都看不出来,只有计数看得出来。

仍然没有实施优化 —— 上一轮的教训就是别照着自己的猜测开工。

### 0.12.128

**找到了:每帧新建 MTLBuffer,在负载中的机器上要 83–289 毫秒。**

0.12.127 装机后收到的 12 条真实卡顿,9 条是同一个形状:

```
290ms  encode_289.2ms (instbuf_289.0ms / 1.0MB)  gpu_1.1ms
216ms  encode_215.0ms (instbuf_214.9ms / 1.0MB)  gpu_1.1ms
175ms  encode_173.3ms (instbuf_173.2ms / 1.0MB)  gpu_1.1ms
...
```

`instbuf` 占 `encode` 的 **99.9%**,`encode` 占整帧的 **99%**。同样是分配 1 MB,
空闲 mini 上 0.09 ms,负载中的开发机上 83–289 ms —— **单个调用被拉长一千到三千
倍**。`newBufferWithBytes` 要向内核申请并锁定内存、再向 GPU 驱动注册;它一阻塞,
**每个 pane 都冻住,而看门狗的 PONG 倒计时正在跑**。这就是那条"please restart
the app"横幅的上游。

修法是 `CLAUDE.md` 本来就写着的那条:**每帧热路径零分配**。新增
`InstanceBufferPool` —— 每个 pass 一个常驻缓冲,容量向上取到 2 的幂只增不减,
`StorageModeShared` 直接 memcpy 进去。稳态一次分配都没有。

**安全边界**(写进类型文档):就地重填只在 GPU 已经用完上一帧时成立。IOSurface
路径每帧以 `waitUntilCompleted` 收尾,成立;活的 `CAMetalLayer` 路径只等到
*scheduled*,不成立 —— 那条路径传 `None`,继续每帧分配。

合成验证:热帧 encode 0.21 ms → **0.05 ms**;冷帧一次性分配 2.12 MB 用 0.03 ms。

`encode_canvas_into`(dev panel / 右键菜单)仍走每帧分配 —— 实测 `canvas` 恒为
0.0 ms,不在这次的攻击面里,而且它的缓冲是按切片循环出来的,槽位不固定。

完整拆解 + 三次被实测否掉的假设(GPU 管线 / 冷字形图集 / `commandBuffer()` 阻塞)
见 `docs/PERF-2026-08-12-first-frame-decomposition.md`。

### 0.12.127

0.12.126 的细分把范围收到了 `encode_passes` 本体:真实卡顿里 `cmdbuf` 和
`canvas` **恒为 0.0 ms**,"`queue.commandBuffer()` 阻塞"和"面板画布"两个假设一起
出局,96–220 ms 全在四个 pass 的编码里。

读代码看到一个显眼的嫌疑:`make_instance_buffer` 每帧、每个 pass 都
`newBufferWithBytes` 新建一个 MTLBuffer —— 13 个 pane 一帧好几 MB 的新分配,正是
`CLAUDE.md` 里"每帧热路径零分配"禁止的形状。但**看代码得出的嫌疑不算数**,给它
单独计时:`instbuf_us` / `instbuf_bytes`。

空闲 mini 上分配 1.59 MB 只要 0.09 ms。所以真实机器上要么这个数会炸,要么成本
在编码器创建/draw call 那边 —— 下一批日志直接判。

仍然**没有实施任何优化**。

### 0.12.126

**仪器装上一小时,真实数据就把合成结论推翻了。**

0.12.125 的合成 bench 指向 `build`(冷字形图集)。装机后收到的头 11 条真实卡顿
说的是另一回事:

| 总计 | build | encode | gpu |
|---:|---:|---:|---:|
| 98–399 ms(8 条)| 0.2–0.5 | **96.5–396.5** | 1.0–2.5 |
| 106–115 ms(3 条)| 0.3–0.7 | 0.7–8.7 | **106.5–114.6** |

**`build` 从来不是成本**(恒 0.2–0.7 ms),而且 `glyphs` 全是 0 —— 稳态卡顿跟
冷图集毫无关系。要是照着合成结论去攻冷图集,就是在一个与真实成本无关的攻击面
上 polish(perf-attack §7 luna 那条教训的现场复现)。

但 `encode` 只是在 CPU 上攒一个 command buffer,不该要几百毫秒 —— 这个数**本身
就说明它不是一段**。于是切成三段:`cmdbuf`(`queue.commandBuffer()`,可能阻塞)
/ `encode`(四个 pass)/ `canvas`(dev panel 与右键菜单,只有开着才非零)。合成
路径上三者是 0.09 / 0.69 / 0.00 ms,所以真实机器上那几百毫秒必然落在其中一段,
下一批日志会直接点名。

仍然**没有实施任何优化**。

### 0.12.125

**「render 花了 41 秒」不是一个能攻的成本。**

`l2.loop.stall` 报到 `slowest=render` 线索就断了。这版在
`render_layout_to_texture` 内部夹三段计时(边界都是编译器不能重排的外部调
用),外加两个旁证,每条卡顿报告都带着它们走:`build_us`(走 grid、推实例,
**以及图集没见过的每一个字形**)/ `encode_us` / `gpu_wait_us`,加
`glyphs_rasterised` 与 `evictions`/`rebuilds`。

新增 `--bench first-frame:<panes>[:ascii|mixed|cjk]` —— 按事故的几何
(3840×2130 / 13 pane / 86×63)**不热身**渲染一帧。仓库里所有 render bench
之前都先跑 5 帧热身,专门把图集填满,所以真正贵的那一帧从来没被测过。

空闲 mini 实测(冷帧 → 热帧一律 ~2 ms):

| 内容 | 冷帧 | build | encode | **GPU 等待** | 字形数 |
|---|---:|---:|---:|---:|---:|
| ascii | 13.0 ms | 5.3 | 0.7 | **6.7** | 85 |
| mixed | 122.2 ms | 115.1 | 0.7 | **6.4** | 8,661 |
| cjk | 536.5 ms | 529.3 | 0.7 | **6.4** | 17,642 |

**GPU 等待在 40 倍的帧成本变化下恒定 6.4–6.7 ms** —— 开工前"两个进程都卡在
显示管线上"的主假设被否掉。冷帧 96–99% 是首见字形的光栅化。驱逐/重建全 0,
"图集撑爆所以 churn"的推断也被否掉。顺带量到 emoji 单字 633 µs = CJK 的
**60 倍**,而跑 claude 的 pane 全是 emoji。

**没有实施任何优化。** 按 perf-attack 的 Pre-Phase-B 闸门,攻击目标要在真实
负载上验到双位数 pp;合成最坏情况不算数。仪器已装,下一次真实卡顿会自己报数。
完整拆解见 `docs/PERF-2026-08-12-first-frame-decomposition.md`,含一次测量装置
自身失效的记录(码点走出 CJK 区跑进 emoji,把单字成本报高 5 倍,看起来完全像
个结论)。

同 commit 把 `bench/baseline.json` 的 marspot 体积地板 1533000 → 1549776:同机
A/B(git stash,几分钟内对跑)显示 `__TEXT` **一字节没动**、`__text` +3548 B
全被页内余量吸收,文件那 +464 B 在 linkedit + 签名里 —— 正是该注释自己说的
"页下噪声"。旧地板只比当时的构建高 72 字节(零余量),所以下一个普通 commit
必然踩线;mcli 那条本来就留着整整一页,现在两条同规矩。

### 0.12.119 – 0.12.124(补记)

这六个版本当时只写进了 `version-vector.toml` 的一行 subject,没有在这里留条
目 —— 跟 2026-07-29 那次迁移要治的是同一个坏习惯,补记如下,细节看 commit:

- **0.12.124 / 0.12.123**(`7116505`)布局网格上限 6 → 9,会话上限同步到 9×9=81
- **0.12.122**(`69d47a0`)面板几何按两个密度各测一遍 —— 逮到 Layout 卡片在小窗口 retina 下溢出
- **0.12.121**(`eaefd5b`)换屏时重建字体并重排网格,终端不再等下次启动
- **0.12.120**(`21459f8`)retina —— 终端与面板一起随显示器缩放,scale=1 逐字节不变
- **0.12.119**(`3ceebed`)chrome 不再乘显示器 scale,盒子和它的文字回到同一个单位

### 0.12.118

**全屏工具栏没生效,而且我之前的沙箱实验证明不了任何事。**

`bin/run.sh` 跑的是 `src/main.rs` 那个**单进程渲染器**(进程名就是 `marspot`,
不是 L1/L2 那一对),而我在它的 `with_chrome` 上把测量值**写死成了 `None`**
—— 于是沙箱里的工具栏永远停在兜底的 84pt,不管全不全屏。**我拿一个被我自己
钉死的路径去验另一条路径。**

单进程这条现在直接问窗口(它手里就有,不用过 wire)。修完在沙箱里真点了
一次全屏按钮:全屏后工具栏移到左边缘 9pt、退出后回到 84pt 且不压红绿灯,
两个方向都截图确认。

顺带确认:`styleMask` 里的 `FullScreen` 位**是对的判据** —— 实测全屏时那三个
按钮既不是 `isHidden`、也没被换到别的窗口(它们在悬停才滑出的标题栏附件里),
只有窗口自己的 styleMask 说得准。

### 0.12.117

**搜索结果的片段被硬切在边框上。** `ListView` 用 `chars().take(n)` 截断 ——
没有任何标记,于是「放不下的路径」读起来就是「到此为止的路径」。

截断规则抽成一处共用的 `ui::core::fit_mono`(Layout 卡片本来有一份):
每侧留出**和定宽时同一个** padding,放不下就以 `…` 收尾,连一个字符都放不下
时给一个 `…` 而不是空行 —— 结果列表里的空行读起来是「没有结果」。按**字符**
切不按字节(CJK 片段切在码点中间就不是文字了)。

顺带:`Aa` 开关和 `×` 挤在一起了。按钮标签改走共用角色之后是**比例字**,
`Aa` 比原先预留的两格宽,于是长进了旁边的 `×` 里。改成按自己的标签实测定宽
—— **一个按自己内容定宽的控件不会撞到邻居**。

`--snapshot` 添 `--panel search`(它画在 pane 里而不是模态,所以挂在 view 上)。
六个面板至此全部可离屏截图。

### 0.12.116

**「还没测到」不等于「测到是 0」。**

上一版把红绿灯右边界存成 `f64`,窗口新建时默认 `0.0` —— 而 `0.0` 的含义是
「没有红绿灯」。于是每个刚建好的窗口在第一趟 attach 送来真值之前,都把工具栏
贴到左边**画在了 OS 的按钮上**(用户截图:三个图标压着红黄绿)。

改成 `Option<f64>`:`None` = 没人量过,`Some(0.0)` = 量过且不在。未测量时退回
历史常量 84pt —— 对绝大多数情况(普通窗口)是对的,只在第一帧到达前保守。

这是本项目 methodology 里那条老账的又一次:**测量装置的「没有读数」和被测量
的「读数为零」长得一模一样**,把它们塞进同一个类型,就一定会有人(这次是我)
把前者当成后者。

顺带:全屏的判据改用 `NSWindowStyleMask::FullScreen`。实测全屏时那三个按钮
**既不是 `isHidden`、也没有被换到别的窗口** —— 它们待在一条鼠标悬停才滑出来
的标题栏附件里,所以只问按钮永远得不到「它们不在」。窗口自己的 styleMask 不
依赖 AppKit 这一版用哪种方式藏它们。

### 0.12.115

**全屏后工具栏挪进红绿灯空出来的位置。**

工具栏原来从固定的 84pt 开始 —— 那个数是**截屏量出来的**(注释里写着:抓了
一帧,量到三个圆点占 x 9..69,留 15pt 间隙)。全屏时 AppKit 把这一簇拿走,
84pt 就成了一个洞,工具栏还老实地坐在洞的右边。

改成**问 AppKit 要真话**:L1 读三个 `standardWindowButton` 的实际位置,取
最右边界(隐藏的、或被挪进全屏悬浮条的不算),换算成物理像素随
`SurfaceAttachWindow` 发给 L2 —— 全屏时它自然是 0。布局于是变成「在这一簇
之后开始;没有这一簇就从窗口自己的边距开始」。84 这个常量只剩下**给老 shell
兜底**的用途。

进出全屏一定伴随窗口尺寸变化,所以这条测量搭的正是已有的那趟 attach。
wire 上是**追加**不是插入:36 / 40 / 48 三种长度都能解,缺的字段读回来是
`None` 而不是从缓冲区尾巴外面捞到的垃圾 —— 三种长度各有断言。

### 0.12.114b — 体积:`--snapshot` 移出发布二进制 + 地板重锁

这一版把两个二进制都顶过了体积地板。查下来**不是膨胀,是一个页**:段对照
显示 `__text` 只涨 2,036 字节、其余段基本不动,而文件涨了 16,512 =
**16384(arm64 一个页)+ 128(签名块)**。arm64 macOS 的 `__TEXT` 按 16 KiB
对齐,所以这两个数**没有低于 16 KiB 的分辨率** —— 20 KiB 以下的跳动说明不了
任何事,该先看 `size -m` 的段表再谈膨胀。这句话写进了 baseline 的注释里。

同时做了一件本来就该做的事:**`--snapshot` 和它的 PNG 编码器移到 `snapshot`
cargo feature 后面,默认不编**。开发工具没有理由待在用户装的二进制里 ——
marspot 因此**降了 34 KB**(1550320 → 1515840)。要用就
`cargo build --features snapshot --bin marspot`。

`bin/test.sh` 改成 `--all-features`:工具可以不进发布二进制,但它的测试必须
照跑 —— 一个校验和没人验的写文件工具,写出来的文件没有看图器打得开。

地板重锁在实测 × 1.06(**一个页的余量,不是一个百分比**):
mcli 1392000 / marspot 1533000。

### 0.12.114

**Layout 面板三处结构问题,一并修掉。**

**一 · 卡片是从面板里溢出去的**,不只是文字。面板宽度是常数 440,卡片再按
「剩余宽度均分、但不低于 80」算 —— 六列时块宽 520 塞进 408 的内容区,直接
画到框外面去了。**一个容器不肯为之增长的最小值,不是最小值,是溢出。**
现在面板为它的卡片块加宽(上限窗口的 92%)。

**二 · 卡片按内容定宽。** 卡片是一枚写着项目名的按钮,那么项目名就该决定
它想要多宽。`lab38-golialab` 在窗口放得下时完整显示;真放不下才截断,而且
带省略号 —— 无声截断读起来像**另一个项目名**,在一个用来选 pane 的面板上
比明显被缩短更糟。截断预算和定宽用**同一个内边距常量**(先前一个留 10px、
一个留 14.4px,于是量好能装下的名字仍被砍掉两个字)。

**三 · 标题栏高度随标题字号走。** 28pt 是标题还用等宽字时定的;换成
`PanelText::Title` 后光标题的 cap 就有 19px,于是标题顶满整条栏、读起来像
横幅。**盛放文字的 chrome,尺寸出自它所盛放的文字。**

**顺带把标题这一档整体下调。** 19px cap 对 11px 的行标签是 1.7 倍,而用户
给的参照(macOS 系统设置)大约 1.3 倍。`Title` 降到 `UiSize::Heading`,
`Section` 与 `Label` 同字号、靠字重/颜色/位置区分 —— 参照里也是这么做的。
梯子的测试从「严格递减」改成「每一档都要能和上一档区分开」(更小,或同字号
更重),因为共用字号是真实的排版选择,而不是缺陷。

命中测试用的是**上一帧发布时算出的**标签宽度,和画出来的那一帧同一个数 ——
重算一遍就是对用户看得见的几何给出第二种意见。

### 0.12.113

**名字里带空格的路径也认了** —— `rm -rf ~/Library/Safari/"Favicon Cache"`
以前只链到 `~/Library/Safari/`(那确实是个真目录,但不是这行指的东西)。

三种写法都收:`"…"`、`'…'`、`\ ` 转义。做法仍然是那条已经定下的规矩 ——
**先放宽扫描,再让磁盘裁决**:扫描把成对引号里的内容整段吃进来(含空格),
验证前用 `unquote_path` 把引号/转义脱掉再 `stat`。

两半契约分开:**下划线覆盖屏幕上画出来的那一串**(连引号一起,那才是用户
指着的东西),而**打开的是脱掉引号后的真路径**。复制同理 —— 复制一个带引号
的字符串,粘到别处还得再脱一次。

落单的引号是散文不是名字:配对搜索有长度上限(96 字符),配不上就当普通
边界断开;而且引号本身也成了**候选切点**,所以万一配错了,磁盘裁决能退回去。
两条都有测试(一条建真的带空格目录跑三种写法,一条确认落单引号不吞整句)。

### 0.12.112

**菜单项降一档,坐回终端自己的密度。**

尺子一直就在旁边:菜单的快捷键提示是等宽字、从没被改过,**9 px 的墨**。
标签在变成比例字之前用的就是同一种等宽字 —— 所以 9 px 正是用户原本拥有、
并且要求拿回来的那个尺寸。用 `Label` 的字号画出来是 **11–12 px**(从截图上
量的),大了约 25%。

`PanelText::Item` 因此改到 `UiSize::Small`(6.6pt → cap 9 px)。实测新的
截图:标签 cap 10 px、旁边等宽 9 px —— 齐平。

测试把这把尺子钉住了:`Item` 的 cap 必须落在 8–10 物理像素之间,并附上
为什么是这个区间(它旁边就站着一列 9 px 的等宽字)。

### 0.12.111

**撤回 0.12.109 / 0.12.110。方向搞反了。**

上一版我发现 chrome 的盒子乘窗口 `backingScaleFactor`、而文字不乘,就把
**盒子**改成跟文字同一个单位 —— 结果整套 chrome 在非 HiDPI 屏上都变大了一倍。
用户的判断是对的:**原来的盒子尺寸才是对的**。终端 cell 也是 1 物理像素/点,
chrome 跟着 `scale` 走本来就与它一致;不一致的是**文字**。

真正的毛病要小得多:**菜单宽度的估算用错了单位**。它按「每字符 × scale」算,
而它估的那段文字是按 `pt × 2` 画的 —— 在 `scale == 1` 的屏上少估了一半,于是
菜单按一半的宽度建出来,标签冲出去。估算改成物理像素(`Item.pt() × 0.52 ×
PX_PER_PT`),与被估的对象同单位。

顺带:菜单项拿到了自己的角色 `PanelText::Item` —— 和 `Label` 同字号但
**字重 400**。标签要压住它下面的说明,所以是 600;菜单项没有要压的东西,
一列半粗的菜单项读起来像一列标题。系统菜单也都是常规字重。

`--snapshot` 保留 `MARSPOT_SHOT_SCALE` —— 从 retina 开发机上复现非 HiDPI 屏,
这是唯一的手段,这一轮的两次判断都靠它。

### 0.12.108

**右键菜单**的标签也走 `PanelText::Label` 了。快捷键提示(`⌘⇧W`)**留在
等宽字** —— 那是一串符号不是散文,等宽让它们右对齐得干净。

菜单宽度原来按每字符 7.0 逻辑单位估(为等宽 chrome 字号写的),标签换字后
每个菜单都比它的文字宽出约 75%。现在这个估值**由角色自己导出**
(`Label.pt() * 0.52`),字号一改宽度跟着走,不会留下一个「对旧字体正确」的
数字。它仍然是估值:菜单宽度夹在 min/max 之间,而行的命中判定只看行高、
从不看文字宽度 —— 差几个百分点只是差几个百分点的留白。

`--snapshot` 再添 `--panel menu`。至此五个面板(settings / cc / process /
layout / menu)都能离屏截图,都在同一把尺上。

### 0.12.107

把上一版建好的角色表铺完剩下的面板:

- **进程面板的标题**原来是等宽字按字符数居中,现在走 `PanelText::Title`
  并**实测宽度**居中 —— 字符数 × 等宽格宽对比例字来说是错的,居中会偏。
  它的正文**不动**:进程树是表格,等宽让列自然对齐。
- **布局面板**:标题 → Title,`Columns` / `Rows` → Label,
  `Total: 3×3 = 9 panes` → Secondary(它是散文,不是数字列)。
- **所有按钮的标签**(`Button` 组件本身)→ Label。改组件而不是改调用点:
  按钮说的是给人读的词,在哪个面板都该是同一号字。
- **布局面板的卡片颜色**是六个手调 RGB 字面量,已换成 token
  (卡片 `SURFACE_3`、拖起来的那个坑 `BG`、落点 `BG_SELECTED`)——
  同一种东西在两个面板必须长得一样,六个字面量就是六次走样的机会。

`dev panel` 不用改:它直接用 `UiSize::Heading` / `UiSize::Body`,而这两个
正是 `PanelText::Section` / `Label` 的来源 —— 已经在同一把尺上,只是叫法
不同。

`--snapshot` 再添 `--panel layout`。

### 0.12.106

**字太大的根因不是某个数字大,是两个面板用了两把尺。**

`Cc` 面板和进程面板的正文一直是**终端等宽字**,只有面板标题走 UI 字
(13pt/600)。设置面板是后来写的,17 / 13 / 12 / 11 / 10.5 全是手挑的 ——
于是它的**行标签**恰好是别的面板**标题**的大小。

新增 `ui::theme::PanelText`:面板文字的五个**角色**(Title / Section /
Label / Secondary / Caption),尺寸由 `UiSize` 的 cap-height 梯子导出
(dev panel 早就用它)。面板要的是**角色**,永远不是数字。

    Title      13.93pt / 700     面板自己的名字
    Section    10.26pt / 600     一组
    Label       8.06pt / 600     一行说的那件事
    Secondary   6.60pt / 400     标签下面那句(代价、含义)
    Caption     6.60pt / 400     脚注,靠颜色区分

等宽字**没有**被取代:进程树、百分比那种表格数据仍然用终端字 —— 列自然
对齐,换成比例字只会抖。这把尺是给散文用的。

设置面板整体按新字号收小(面板 620→440pt,行高 56→41pt),`Cc` 的两个标题
改走同一个角色。**测试也跟着改了写法**:原来断言「行高 ≥ 44pt」,字号一降
就假报警 —— 现在断言的是**比例**(一行里文字占不到 62%),比例才是它真正想
守的东西。

**顺带把两个调色板合成一个。** `cc_palette` 是 `Cc` 面板的私有颜色,设置
面板后来又长了一套几乎一样的 `settings_skin`,两者已经漂了:一个把卡片画
在面板**上面**一级、另一个画在**下面**一级,于是同一种东西在这个面板是浮起
的、在那个面板是内嵌的。现在是一个 `panel_palette`,卡片一律 `SURFACE_3`。

### 0.12.105

**面板加两项**,都是「有真代价、没有正确答案」那一类,也都实时生效:

- **Dim the panes you are not in**(Off / Light / Normal / Deep)——
  注意力阶梯本身(未聚焦 → 静置 → 已回收)**不是设置**:它的顺序说的是
  marspot 对每个 pane 的判断,不归用户改。可调的是它**说话的音量** ——
  一屏九宫格要小声,两个 pane 要大声,两个答案都不错。`Off` 是一个取值,
  不是第二个开关(和 `Never` 同一个道理)。
- **Wheel speed**(Slow / Normal / Fast / Faster)—— 方向**没有**放进来:
  方向有正确答案,就是用户在 macOS 里已经选过的那个(`MARSPOT_SCROLL_INVERT`
  留给那台不对劲的机器)。速度没有正确答案,它取决于鼠标。

`scroll_config()` 原来整个用 `OnceLock` 缓存,于是改了要重启 —— 违反面板
第二条规矩。现在方向仍读一次(机器不会中途换鼠标),**倍率每次手势读一次**,
所以滚轮还在手指底下时改设置就生效了。滚轮事件不是热路径。

设置文件多了两个浮点键(`appearance.dim_scale` / `input.scroll_factor`)。
越界当**手误**忽略而不是夹到边界:`scroll_factor = 100` 更可能是敲错,悄悄
按 8 给他反而什么都教不会。`1.0` 写成 `1`,不给手编的文件添噪音。

### 0.12.104

**先把「我看不见」这件事解决掉。** 面板好不好看没有测试能替,而这一整天每
看一眼都得麻烦你截图、或者把窗口提到你正在做的事情前面。`--snapshot` 这个
开关其实早就在,只是被停用着,注释写的正是缺的三样:Managed 纹理、getBytes
回读、BGRA→RGBA。三样都补上了:

    marspot --snapshot out.png --panel settings

离屏渲染一帧写成 PNG。PNG 编码器自己写(`src/png.rs`,~100 行 std)——
DEFLATE 有 stored block,zlib 收,于是「压缩」就是照抄字节再写对长度;
省下的是一整棵压缩依赖树。CRC32 / Adler-32 对公开向量,块结构按解码器的
走法走一遍,都有测试。

**然后修了两个只有看到才知道的毛病:**

1. **卡内的分隔线根本没画出来。** `fill_rect` 走 cells 管线、`fill_rounded_rect`
   走 ui_rects,而**整趟 cells 在整趟 ui_rects 之前**画 —— 线虽然是在卡片
   之后提交的,却被卡片盖住了。改到同一趟就出来了。
2. **颜色全部换成主题 token**,不再手调 RGB:卡片 `SURFACE_3`(面板是
   `SURFACE_2`,正好高一级)、分段控件 `SURFACE_4`、分隔线 `HAIRLINE`
   (第一版用了更淡的 `DIVIDER`,1 物理像素配 0.08 等于没有)、说明行
   `FG_MUTED`(原来只比标签暗一点,两行在打架),分组标题提到 `FG`。

`marspot` 二进制因此涨了 17.5 KB(1514624 → 1532160),距离地板还剩 8.8 KB。

### 0.12.103

**一个格子画成什么样,只由它自己决定。** 上一版给「本来就该两格宽、却只分到
一格」的字形加了条规则:右边那格是空的就照原尺寸溢出去画(WezTerm 的
`WhenFollowedBySpace`)。结果是 `① ` 满尺寸、`①消` 只有 61% —— **同一个字符,
大小取决于旁边后来写了什么**,于是同一屏里先小后大。不管你更喜欢哪个尺寸,
看着它变来变去都像渲染坏了。

规则整个删掉,连 `GlyphKey::FLAG_OVERFLOW` 一起。一格就是缩到一格,恒定。

圈圈家族确实塞不进一格 —— PingFang 画 `①` 是 11.71px,而格子只有 7.20px。
要满尺寸就得给第二格,而那会移动换行点(试过,当天就退了)。所以它是设置面板
里的开关 `appearance_circled_wide`,默认关;开了就是**处处**两格、处处一样大,
不是按邻居猜。

### 0.12.102

**分组标题改成句首大写**(`IDLE RECLAMATION` → `Idle reclamation`)。全大写是
分组标题挤在行里、只能靠字重区分时的写法;现在它有自己的字号、坐在卡片
上方,全大写只剩噪音。

**沙箱多一个 dev seam:`MARSPOT_DEV_OPEN_SETTINGS=1`** —— 启动两秒后自动
打开设置面板。面板「好不好看」没有测试能替,唯一的办法是看;而在沙箱里
用脚本点工具栏需要辅助功能权限。跟 `MARSPOT_DEV_CLOSE_PANES` 同族,装出去
的 app 里不设。

### 0.12.101

**设置面板重做成分组卡片。** 上一版调的是数字,但毛病不在数字 —— 面板压根
没有字号层级:标签和解释它的那句话是同一个字号的同一种等宽字,只靠颜色分,
那不是层级,是两条互相打架的线。而且所有东西平铺在一片背景上,行与行之间
没有任何分界物,所以怎么调间距都还是「挤在一起」。

**一 · 用上了本来就有的字号系统。** `ViewPainter` 一直只有一个字号(启动时
的 chrome pt)一个字重(600),凡是建在它上面的界面都只有一种字。Canvas 那条
路从 Phase 10c 起就能按任意 pt + 字重排版,dev panel 就是这么排的。这一版把
同样的能力给了直接绘制路径(`push_text_run_ui_sized` / `ui_text_at` /
`ui_text_width_at` / `ui_baseline_centred`),面板于是有了真的三级:标题 17pt/700、
分组标题 12pt/600、行标签 13pt/500、代价行 11pt/400、脚注 10.5pt。

**二 · 分组变成卡片,卡内加细分隔线。** 分组标题移到卡片**上方**(系统设置
就是这么做的),卡片自带一层更亮的底色和一圈几乎看不见的边;卡内相邻两行之间
一条 1px 细线,左边缩进到行文字的起点、右边齐卡片边 —— 列表的通用写法。
单行的卡片没有线,三行的卡片两条线,测试钉住。

**三 · 尺寸全部改用点(pt),面板放大到 620pt 宽。** 原来整面板是终端 cell 的
倍数,想跟着字号走 —— 而这个耦合正是「只有一种字号」的来源。chrome 就是
chrome,按点排,`ViewPainter::PX_PER_PT` 负责落到像素。行高 56pt(两行文字
+ 上下各 10pt),控件竖直居中在标签那条**带**上而不是吊在基线上,代价行因此
永远在控件下方通过 —— 这条也是断言。

分段按钮的宽度、面板自身的高度、点击热区,全部出自**同一次 walk**、同一个
测量函数(字体真实测量,不是字符数估算)。测点仍然是「控件必须恰好在它被画
出来的地方可点」。

### 0.12.100

**设置面板重排。** 三个毛病,三个都是结构性的:

**一 · 节奏。** 一行是「标签 + 它的代价」两条线,它们属于同一个单位;而行与
行之间必须读得出分隔。原来是 0.95 : 2.2 —— 比值 2.3,于是代价那行离**下一行
的标签**几乎和离自己的标签一样近,眼睛分组分错,三行读成六行挤在一起。现在
内 0.9、外 1.7(总间距近 3 倍),分组不再有歧义。测试直接钉这个比值。

**二 · 分段按钮按内容定宽。** 原来五等分,于是 `Never` 撑出按钮、`1h` 在里面
游泳。现在每段宽度 = 自己标签 + padding,整组右对齐到跟开关同一条边。测试
逐个检查:每段必须比自己的字宽,最后一段不许越出控件。

**三 · 底部的空带。** 高度原来是 `CHROME_LINES` 常量猜的,猜多了两行。现在
**高度和布局出自同一次 walk** —— `layout_lines` 走一遍,`panel_rect` 拿它的
总高,画的时候拿它的每个基线。加一行就正好长一行,不会再有空带,也不会挤。

顺带:控件改成**压在标签那条线上居中**,而不是吊在基线下面(它有两个字高,
吊着会压到下面的代价行);面板宽度改由**最长的那条代价文案**决定 —— 标签都
短,代价才是句子,是它在定这个面板的比例。

### 0.12.99

**工具栏第 6 个图标之前根本没画。**

矩形铺好了、点击判定通了、图标组件也写了 —— 唯独漏了那句绘制。于是有一个
**隐形按钮**:点那个空位真的会开面板,但看不见它。上一条 CHANGELOG 说"工具栏
第 6 个按钮",那句话是假的。

改法不是补一句 paint,是让这种漏法**编译不过**:`Layout::toolbar_buttons()`
返回全部六个矩形,画的时候跟一个**等长的图标数组** zip —— 加了矩形不加图标,
长度对不上,编译器当场拦下。测试再钉一层:六个按钮都要有面积、不越窗、不
互相重叠,而且各自的 hit-test 只认自己那一个。

**面板本身重做了外观:**

- **全英文**,跟 app 里其它面板一致(`IDLE RECLAMATION` / `TEXT` /
  `Reclaim idle claude panes` / `15m 30m 1h 2h Never`)。中文标签坐在等宽
  格子上,字距肉眼可见地散
- **分段控件改用共享的 `Button`**,不再是手糊的圆角矩形 —— 圆角、描边、
  选中态都跟工具栏和布局面板同一套
- **分节标题回到终端字体**。`Cc` 面板只有两个顶级标题用 UI 字体,正文一律
  走格子;我每三行插一个 UI 字体标题,读起来像三个面板摞在一起
- 行距、控件宽、面板高重新配过 —— 底部原来空着一大块

### 0.12.98

面板加「文字」组,第一条就是今天那个:**圈圈数字占 2 格**,**默认关**,
旁边写着它的代价 ——

> `①②③` 跟汉字一样大,但会移动换行点 —— 滚过它的段落可能掉字

一个上午发出去又收回来的默认值,变成一个带着标价的开关。这正是这个面板
存在的形状:**没有普适正确答案的决定,连同它的代价一起交出去**。

### 0.12.97

**设置面板** —— 工具栏第 6 个按钮(两条带滑块的横轨,不是齿轮:邻居全是
直线线稿,齿轮的放射轮廓在 16px 上读起来是另一家人)。

写入 `settings.toml`,L1 在下一次 sweep 就按新值判 —— **点一下到生效不到
一秒,什么都不重启**。

三条规矩落进了代码,不是落在文档里:

1. **每一行都写出它的代价**,而且有测试盯着:没有 cost 文案的行编译测试就
   过不去。这个面板里每一项都是「marspot 决定不替你决定」的地方,那就意味着
   每一项都有一个值得说出口的坏处;只列好处的设置面板是在让人闭着眼睛选。
2. **没有「需要重启」。** 做不到不可见的项,这一版不放。
3. **底部显示文件路径。** 面板是编辑它的一种方式,不是唯一的。

行的几何和点击判定**共用同一个 walker**。另一种写法 —— 画一遍、点击处再写
一套坐标 —— 正是「控件在它上面一行才有反应」的来源,而截图看不出是哪一边
错了。测试瞄准每个控件的正中心点下去,必须命中同一行。

两处小心思:关掉总开关后,底下两行**变灰但不消失**(面板不该在光标底下改变
高度);手改成 `= 45` 这种面板没有按钮的值时,**一个按钮都不亮**,也不会
被四舍五入到最近的那档 —— 那等于用户一打开面板就被悄悄改了文件。

### 0.12.96

溢出集合补上圈圈家族的尾巴。歧义表在 U+24E9 截断(那是 UAX #11 的划法),
可 `⓪` 和到 U+24FF 的其余部分、`❶..➓` 是同一个设计、同一个 em 方框、同一
种被压 —— 它们现在也能借右边的空白格。

### 0.12.95

**被压小的字形可以借用右边的空白格。**

上一版的结论「要跟 CJK 同体量就必须占 2 格,没有第二个杠杆」是**错的**,
收回。别的终端根本不走加宽这条路 —— 它们**让字形溢出去**。WezTerm 把这件
事做成一个选项 `allow_square_glyphs_to_overflow_width`,默认
`WhenFollowedBySpace`,适用于「长宽比 > 0.9 的任何字形」。marspot 之前只有
`Never` 那一种行为(`rasterise_glyph` 的 `oversized → scale-to-fit`),所以
它是这里面唯一一个把 `①` 缩到 61% 的。

现在的规则,三个条件缺一不可:

- 格子只给了它 **1 格**(2 格的字形本来就有它被设计的空间)
- 它属于**东亚歧义宽度** —— 正是「字体按 2 格 em 画、我们按 1 格渲染」
  的那个集合,也就是会被压的那批
- **右边那格是空白** —— 溢出落在空处。盖住邻居比画小更糟;落在空格上不
  花钱。最后一列没有邻居可借,不溢出。

宽度一格没动,所以跟 claude 的 `string-width` 不会有任何分歧,不会累积
CUP 偏移。

实现上:同一个字形在一屏里可能两种光栅都要(旁边有东西时用压缩版,旁边
空着时用自然版),所以它们必须是**两个缓存键**。而 `GlyphKey` 的 64 位是
塞满的 —— font_id 占了 32 位去装一个只有几十个字体的注册表。裁到 24 位,
`flags` 从 2 位扩到 4 位,还剩 6 位空着。

### 0.12.94

两件事。

**一、Cc 时间轴的两端对齐到本地整日。**

上一版把日期线放对了位置,但最右边那根线之后还有半天没有线:Claude 1 的
7d 条停在 8/14 12:00,越过了 `8/14` 那根线,而 `8/15` 从来没被画出来 ——
条子的末端没有任何可对照的日期。

范围现在向外吸附到本地午夜,**两端都是**。一次给出两样东西:留白(条子不
再贴着边缘起止),以及每个条子末端**两侧都有一根带日期的线**可读。原来那个
百分比 padding 一并去掉 —— 吸附本身就是留白,两套机制叠着只会把吸附多推
出一整天、白白压缩画面。网格和日期标签的循环改成含右端点,因为范围现在正
好停在一根线上,而那根线正是最后一天里的条子要对照的。

**二、`http://localhost:6014/…` 现在是链接。**

host 过滤器要求至少一个点,于是终端里满地都是的开发服务器地址全被毙掉。
规则**写的**是「没有点、没有数字就拒」,**实现的**只有「没有点就拒」——
数字那一半从来没写进去。

URL 没有文件系统那样的裁判,所以只能让结构说话:**显式的数字端口**。那正
是真地址和 `https://x` 这类占位符的分界 —— 占位符从不带端口,而大家真会
打出来的 `http://host:port` 过不了数字那一关。裸的 `https://localhost`
维持拒绝(那是另一条单独定过的:不带端口时它就是聊天示例的形状)。

### 0.12.93

**Cc 时间轴:日期线一直差着一个时区。**

报告:Claude 4 的 7d 条写着 `00:00`,显然该是 8/8 的 00:00,可条子的末端
落在 8/8 那根线的**左边**。

不是 bar 画错了 —— bar 一直是对的。算术:

| | unix | 本地 |
|---|---|---|
| 那根 bar 的末端(7d reset) | 1786114800 | **8/8 00:00** ✓ |
| 标着「8/8」的那根网格线 | 1786147200 | **8/8 09:00** ✗ |

差 32400 秒 = 9 小时 = JST 偏移。日期线是按 `ceil(t0/86400)*86400` 起步、
每次 `+86400` 走的 —— 那是**UTC 午夜**;而线下面的日期是 `local_mdhm`
写的,**本地日期**。于是每一根线都离它自己的标签差一个时区偏移,只有在
UTC+0 才碰巧是对的。

改成按本地日走:`local_day_start` / `next_local_day_start`(`localtime_r`
定位、`mktime` 回算)。加一天用的是**日历日**不是 86400 —— 跨 DST 的那天
是 23 或 25 小时,而 `mktime` 顺手把月末年末的进位也归一化了。轴下面的
日期标签走同一个循环,一并修好。

### 0.12.92

**标点不再终结文件名 —— 宽着扫,让磁盘裁决。**

0.12.91 修的是一个 case(全角括号),这一条修的是它背后的原则。

路径链接**本来就只在文件真实存在时才发出**。既然末端有这么一个裁判,
前端的扫描就没有理由害怕认错 —— 猜窄了只会**丢真的**,不会防住假的。而
那正是历次现场报告的形状:先是 `(` 粘在路径后面,于是把 `(` 设成硬终止;
然后全角括号在日文文件名里是房规,于是漏掉整条链接。每加一个终止符,就多
一类识别不到的真文件。

所以扫描改成贪婪:只在路径**真的不可能继续**的地方停(空白、控制字符、
`< > " ' \` |` 这些 shell 本来就要转义的),标点一个都不停。然后按候选
**从长到短**逐个问文件系统,第一个存在的胜出。长的优先 —— 两个都存在时,
这行指的是那个文件,不是恰好作它前缀的目录。

一次收敛掉三样东西:窄扫的终止符表、括号特例、标点重试。新识别到的形状
(旧设计结构上做不到的):`plan(v2).md`、`a,b.txt`、`sec；1.md`、
`note【草稿】.md` —— 名字里含标点,后面还紧跟着用同一个标点写的行文。

`:` 是唯一留在切点之外的:`path:120:5` 已由行号剥离处理,`path:note` /
`host:path` 那一族由 2026-07-13 的报告定为宁可漏。

代价量过:同一屏(60 行 × 200 列,一半路径是假的)热态 87 → 95 µs,
冷态 163 → 99 µs;render p99 预算 1301 µs。上限 24 个候选,防止一行纯标点
变成 stat 风暴。

### 0.12.91

**文件名里的全角括号不再切断链接。**

报告:`~/Downloads/株主総会議事録（役員報酬改定・20260805）.pdf` 没被识别。

路径扫描在 `(` 和全角 CJK 括号那一家上硬停 —— 因为中文/日文行文习惯把
它们直接粘在路径后面(`…visibility.md(Ask 12…`),而「文件名里真的含
括号」比「行文紧贴括号」罕见得多,所以当初选了宁可漏不可错。

但全角括号正是日文公文命名的房规,这一条漏得不对。span 被切成
`~/Downloads/株主総会議事録`,不是真路径,于是整行一个链接都没有。

而路径链接**本来就要求文件真的存在**才成立 —— 那是比标点启发式强得多的
裁判。所以现在:撞上括号就试着走完这一对、继续扫下去(`.pdf` 就是这么
捡回来的),让文件系统裁决。**先试长的**:两个都 stat 得过时,这行指的那个
文件应该赢过恰好是它前缀的目录。

凭空造不出链接 —— 行文里的括号短语不会命名一个存在的文件。

### 0.12.90

按 L1 报来的**槽位**认领存档窗口,不按到达顺序(见 L1 0.7.94)。没带槽位
的老 shell 仍走到达顺序。

顺带补一个更早就在的洞:还排在恢复队列里、L1 还没打开的窗口记录,现在
也会被写进 `shell-state.bin`。启动后的第一次存档跑在任何窗口恢复之前,
原先那一下会把文件改写成只剩引导窗口 —— 在这个空当里崩溃,其余窗口的
布局就没了。

### 0.12.89

**关不掉一个窗口的最后一个 pane** —— 菜单项是灰的,[×] 不响应。

现在关得掉,而且一路关到底:窗口的最后一个 pane 关掉,窗口跟着关,下次
不再回来;最后一个窗口的最后一个 pane 关掉,marspot 整个退出,并把存下
来的布局和几何一起删掉 —— 下次打开是一个干净的窗口,1×1、单 pane、主屏
居中、800×600。

这一版把「关」分成了两件事:

- **关窗口** = 收起。它的布局被停放,会话照跑,下次启动原样接回来。
- **关 pane** = 拆掉。一个 pane 都不剩的窗口没有什么值得存的,不停放、
  不恢复。

没有存档的启动因此也从 3×3 九个 shell 改成了 1×1 一个 —— 九个 shell 是
用户自己摆出来的布局,不是见面礼。

两个存档文件都是按位置配对的(`shell-state.bin` 的窗口表 ↔
`window-state.bin` 的几何表),所以停放的窗口保留自己的槽位,拆掉的窗口
把上面的槽位**收拢**一格,L1 同步做同样的事。

### 0.12.88

**句子紧接着路径往下说,链接就没了。**

现场:

```
  - 回执:/Users/doracawl/workspace/goliajp/sentori/tmp/sentori-feedback-
  reply-b-section-2.md;memory 已记(含 deploy workflow 自动打 tag 的坑)
```

整行一个链接都没有。路径是真的,换行合并也是对的 —— 错在扫描器把
`…-2.md;memory` 当成一个 token 去 stat,当然 stat 不到。

`;` 本来就在「结尾要削掉」的名单里,但那只管路径**结束在句尾**的情况;
这里句子没结束,后面紧跟着就是下一句。中文用 ASCII 标点写就是这样:标点
自己承担停顿,后面不留空格。所以这不是边角情况,是**大多数行**。

修法跟已有的两次重试同构:整段先 stat(文件名里真带 `;` 的照样赢),失败
之后把每个标点当成一个候选终点,从右往左试。`:` 特意不在名单里 ——
`path:note` 那一族 2026-07-13 已经定过 **宁可漏**,不猜。

顺序上放在「接缝重试」之前:标点切法保留整个 token 只丢掉散文,接缝切法
是把续行整个扔掉 —— 后者切出来的前缀万一恰好是个**真目录**,就会顶掉这行
真正指向的那个文件。

### 0.12.87

**变暗改成缓动,变亮仍然是瞬间的。**

一个 pane 的注意力等级是分档的(聚焦 / 其他 / 歇着 / 已停放),但**画面
不该是**:两种灰之间硬跳读起来像故障,同样的变化在五分之一秒里缓过去,
读起来才是「这个 pane 退到后面去了」。

变亮不缓动:你刚点进去的那个 pane,必须在你点下的**那一刻**就是你的 ——
在这里加过渡,读起来是点击有延迟,不是动画。

**动画必须会结束**,这是硬约束(CLAUDE.md §2:空闲 CPU ~0%,不许有动画
定时器)。所以它不是定时器:值是时钟的纯函数,只在本来就要出帧的时候采样;
到位之后 `is_moving` 返回 false,窗口回到「只在有变化时才画」。这一条单独
立了一条规格 —— 一个永远说「还在动」的缓动会让渲染循环终身不睡。

状态放在 pane 上而不是渲染器里:渲染器每帧无状态,而 pane 的下标会变 ——
被拖到别的格子的 pane 会继承邻居的动画。

### 0.12.86

**帧率节流里的时钟竞态把整个 app 带走了。**

```rust
Some(t) if t.elapsed() < frame_min_interval => {
    frame_min_interval - t.elapsed()      // ← 第二次读时钟
}
```

两次 `elapsed()` 之间时间在走:守卫里差一点没到,body 里就过了 ——
`Duration - Duration` 下溢直接 panic。窗口窄到十二小时的日志里只中一次,
中的那次是 `overflow when subtracting durations` 打穿 `main`,窗口全没。

改成读一次时钟 + `saturating_sub`:elapsed 已经超过间隔就是 0(立刻画),
这本来就是这段代码想表达的意思。

会话没受影响 —— L3 自持,core 死了它们照常活着,重开后原样接回来。

### 0.12.85

标题条的 `#k` 按屏幕位置算(窗口 → 行 → 列),不再按 session id;计算时要
看**所有窗口的所有 pane**,因为号是位置,单个窗口算不出来。

### 0.12.84

标题条画的是**派生出来的 pane 名**(目录最后一段,重名带 `#k`),不再画
用户自己设的标题 —— 改名这个功能整个删掉了(右键 Rename、双击改名、编辑
态键盘拦截、`custom_title` 的存取与跨重启恢复)。

理由见 L1 0.7.75:名字要么派生要么权威,不能一半一半,否则 `--send spg`
认的和标题条写的可能是两个东西。

### 0.12.83

转发 `PaneInjectPaste` 到 pane 自己的 L3。

跟 `InjectInput` 分开是因为它们不是一回事:后者按字节原样写,而只有 L3
知道 pane 里的程序开没开 bracketed paste。多行文本不经过这个判断,就会被
一行行当命令执行。

### 0.12.82

两个小标题下面各留一行的间隔。

`CLAUDE ACCOUNTS` 离第一张卡片只有 0.3 行,`RESOURCE AVAILABILITY` 离第一
条时间线只有 0.15 行 —— 那个距离读起来像是贴在卡片上的标签,不像盖在一组
东西上的标题。两处都改用同一个 `HEADING_GAP`(1.2 行),不会各走各的;面板高度跟着加回
这两段。给得宽是因为标题用的是更大的 UI 字体,按终端行数量出来的间隔在它
底下看着比数字更紧;而面板本来就离自己的边框还差一大截,不缺这点空间。

### 0.12.81

**每个账号卡片多一行 per-model 上限(当前就是 Fable)。**

feed 里一直有 `model_limits`,是解析器把整个数组丢掉了。这条信息账号级的
两根 bar 说不出来:2026-08-01 的实测数据里,Claude 3 的 7d 是 64%,而 Fable
已经 **85%** —— 开工前最该知道的一件事,面板上却看不见。

卡片的 bar 行现在是 `5H / 7D / <模型名>`,形状完全一样(不该为了半张卡片
再学一种读法);bar 的起点跟着最宽的标签走,不再是写死的第 3 列。卡片和
面板的高度按「多几行」推导,painter 和 `panel_rect` 用同一个式子。

`"reset": null` 解析成 `None` 而不是 epoch 0 —— 没人碰过的模型没有窗口可
重置。

### 0.12.80

转发 `PaneHoldGrid` 到 pane 自己的 L3。

core 不对它做任何事 —— 这正是重点:冻结属于会话,不属于「此刻正在画它的
那个 core」。core 每次静默更新都会换一个,而 pane 停在哪一帧不该跟着换。

### 0.12.79

**bar 的结尾不再被文字挤短** —— 时间轴按数据定尺,标签严格贴在 bar 右边。

轴是写死的 ±6 天,但 7 天窗口的重置可以落在 7 天之后 —— 那种 bar 跑出右
端被夹在轴边,于是它停的位置跟它自己标签上写的时间对不上。实测当时的 feed:
四条 7d bar 里有两条的终点落在 span 的 1.014 和 1.056,都被切了。

轴改成按要画的东西定尺(`timeline_range` 取所有窗口的起止再加 2% 余量),
夹取从此不可能发生。标签的位置回到 `bar 终点 + 0.7 字宽`,不再有「贴不下
就往回挪」的兜底 —— 右侧留白本来就按最宽的标签算过,轴又正好在数据处结束,
最右那条 bar 的标签有它的位置。

标题里的 `(±6D)` 一并去掉:跨度不再固定,写死的数字就是错的。

### 0.12.78

**账号面板只剩 1 个账号,feed 里有 4 个。**

`claude-usage.json` 的每个账号现在带 `model_limits` 数组和 `credits` 对象。
解析器是手写扫描器,当初按「账号对象是平的,下一个 `}` 就是结尾」写的 ——
那个 `}` 现在是内层对象的结尾:账号 1 从一段被截断的切片里解析(它的标量
字段刚好排在嵌套之前,所以看起来正常),接着扫描器把 `model_limits` 的 `]`
当成整个数组结束,后面三个账号直接消失。没有任何报错。

扫描改成认嵌套:`objects_in_array` 按花括号 / 方括号深度切出数组的直接
元素,字段查找 `top_level_value` 只认本层的 key(且要求后面跟冒号,否则
`"name": "status"` 这种值会冒充 key)。

回归测试用真 feed 逐字剪的两个账号,外加一条对本机实际文件的断言:
写进去几个账号就要解析出几个。

### 0.12.77

四档不透明度:**聚焦 100 / 其他 75 / 歇着 50 / 已停放 25**。

上一版只有三档,而且「歇着」的判据是「安静满 5 分钟」—— 真机上那等于
**18 个 pane 里标了 15 个**(8 个停在提示符的 shell + 6 个 claude 在等
用户)。标了几乎所有等于什么都没标,看到的就是「没 idle 的也很暗」。

现在等级跟着**会话自己的生命周期**走:

| 等级 | 含义 | 不透明度 |
|---|---|---|
| — | 你正在用的那个 pane | 100% |
| 0 | 活着(含停在提示符的 shell —— 终端在等你,不叫闲) | 75% |
| 1 | 歇着:绑定的程序结束了一轮又放了一会儿(正往被回收的方向漂) | 50% |
| 2 | 已停放:程序被回收,画面冻着,一聚焦就回来 | 25% |

帧号 67 从 `PaneIdle` 改名 `PaneRecede`,载荷从 f32 alpha 变 u32 等级 ——
L1 说到第几档,L2 决定每档多暗(它还要叠「是不是聚焦的 pane」,那是 L1
不知道也不该知道的)。所有「后退」共用一个 scrim 原语,最深者胜不叠加。

### 0.12.76

不透明度阶梯第一版:聚焦 100% / 其他 75% / 闲置 25%(判据见 0.12.77,
一天内就被真机推翻了)。`attention_scrim` 落在渲染层,聚焦的 pane 永远
不暗;拖拽源(0.38)仍最深,空座位(0.22)不变。

### 0.12.75

新增 `PaneIdle` 帧 + 每 pane 闲置变暗。渲染侧把三种「后退」(拖拽源 /
空座位 / 闲置)收敛成一个 scrim 原语,最深者胜而不是叠加 —— 两层 0.22
叠出来是 0.39,那是另一个颜色。

### 0.12.74

新增 `PaneFocused` 帧(66):L2 在主循环比较每个窗口的焦点 sid,变化才
发一帧。休眠的 pane 因此能「被看一眼就开始恢复」,而不是等用户先按键
(见 L1 0.7.53)。

### 0.12.73

只重编:logx 目录自愈(见 L3 0.11.33)。

### 0.12.72

只重编:`Pty::drop` 的顺序改了(见 L3 0.11.32)。

### 0.12.71

只重编:`pidtree` 换成 `pane_state` 那套(见 L1 0.7.41),core 侧行为不变
—— process panel 用的是 comm 和树形结构,不读那个枚举。

### 0.12.70

只重编:`pidtree::PaneForeground::Job` 换成
`{ pgid, leader: Option<JobLeader> }`(见 L1 0.7.35)。core 侧行为不变 ——
process panel 不读这个枚举。bump 理由同 0.12.67。

### 0.12.69

只重编:`pidtree::ProcRow` 加了 `pgid` / `tty_dev` / `tty_fg_pgid`
三个字段(见 L1 0.7.34)。core 侧行为不变 —— process panel 用 `comm`
和树形结构,不读这三个。bump 的理由同 0.12.67。

### 0.12.68

window header 从「标题条 + 工具栏」两条带子合成一行。

工具栏图标原来在第二条带子里(标题条 32pt + 工具栏 30pt = 62pt),
右上角写着 `Marspot v0.12.67 (7735d33c)`。现在五个图标跟 macOS 的
traffic lights 同排垂直居中,右上角只留 `v0.12.67`,整个 chrome 从
62pt 降到 32pt —— 每个窗口都多出 30pt 的正文高度。

对齐的锚点是 traffic lights 那一行,不是 header 自己的中线:窗口按钮是
AppKit 画的,位置相对窗口顶边固定,不跟着我们的 chrome 高度走。所以
`TRAFFIC_LIGHT_CENTER_Y_LOGICAL = 16` 是**量出来的**(截屏窗口 frame 原点
起算:14pt 圆点占 y 9..23),`HEADER_PT = 32` 是从它推的(2 × 16),按钮
和版本号都按这一行居中。同一张截图里按钮落在常量要求的那一行上,所以
截图坐标系和布局坐标系对得上 —— 这一步是量的自校验,不是眼估。

顺带修掉一处不一致:`LayoutModal` 的 hit-test 传的 top obstruction 是
`TITLE_STRIP_PT`,而渲染传的是 `layout.top_inset`(= `HEADER_PT`),两边
差 30pt。现在只剩一个常量,不一致没地方存在了。

### 0.12.67

只重编:`pidtree::ProcRow` 加了 `start_unix` 字段(见 L1 0.7.31)。core
侧行为不变 —— process panel 和 cwd 扫描都不读这个字段。版本号照样 bump:
共享 lib 变了而某层没 bump,`install-local.sh` 会判该层 unchanged 跳过
staging,新二进制静默留在磁盘上。

### 0.12.66

`core.pane_cwd.changed` 从 DEBUG 提到 INFO。

logx 的运行时默认级别是 Info,所以 0.12.64 加的那行 DEBUG 在真机上
等于不存在 —— 而它正是「pane 标题不对」时第一个该看的日志。频率由
结构封顶:每 pane 每轮扫描最多一行(`resolve_pane_cwd` 只在值真的
变了才记)。

### 0.12.65

cwd 扫描的存档限流改成延迟而非丢弃。

0.12.64 装机后立刻坐实一个洞:第二个 window 的两个 pane 在
`shell-state.bin` 里 `last_cwd` 被写成空串,而且之后再也不会纠正。
时序 —— boot 首轮扫描填满 window 0 并 save(把 5 s 限流窗口吃掉);
window 1 的 restore 发生在这之后,它两个 pane 的 cwd 在下一轮扫描才
填上,那一轮 `changed = true` 但 gap 未到,于是**这次变化被忘掉**;
再往后每轮都「无变化」,空串就留在文件里。live 标题是对的(内存里
已填),但 `last_cwd` 是 L3 死掉后 respawn 的落脚目录,写空等于把那个
pane 送回 `$HOME`。

`cwd_save_pending` 粘滞标志:限流可以推迟一次 save,不能丢。

### 0.12.64

pane title 的 cwd 占位符改由一个 1 秒扫描驱动,不再挂在键盘事件上。

原来的触发集是「焦点 pane 按下 Enter / 切焦点 / 打开 layout modal」。
按 Enter 那一刻 shell 还没执行这一行 —— `cd` 生效前就把 cwd 读了,所以
标题永远落后一条命令;而一个不再被敲键的 pane(`cd x && claude` 一行写完、
脚本里 cd、非 key window 那几个)则根本不会再更新。实测 2026-07-30 现场
16+2 个 pane 里有两个:cache 记 `labs/vectx` 实际在 `qualcomm/insight`,
cache 记 `$HOME` 实际在 `labs/vectx`。目录换了这件事,键盘无从知道。

现在 `sweep_pane_cwds` 在主循环既有的周期块里跑,每 `CWD_SWEEP_INTERVAL`
= 1 s 重读一次全部 pane 的 shell cwd。成本:每 pane 一次
`proc_pidinfo(PROC_PIDVNODEPATHINFO)`,实测 M 系 0.58 µs → 18 个 pane
约 11 µs/s;pid 由 `shell_child_pids` 缓存,entry.toml 不再每轮重读。
搭主循环原有的 1 s idle wake,没有新 timer;一轮扫下来没有变化就不置
`needs_render`,idle 依旧零帧。

顺带把两处旧机制删掉:
- `lazy_fill_missing_cwds` 从 `build_views` 移除 —— 渲染路径现在是纯
  读者,一次 syscall 都不发。
- `cwd_unresolvable` 放弃计数器整套删除。它存在的理由是那个 per-frame
  lazy fill 会 60 fps 重试;而它自己会把一个 pane 的 cwd **永久冻结**
  在旧值上(只有 force 触发能解封)。改成固定间隔后重试率由结构保证,
  这个补丁不再需要,连同它能造成的冻结一起消失。

`last_cwd` 的持久化同样由扫描驱动,但 `CWD_SAVE_MIN_GAP` = 5 s 限流 ——
一个在循环里 cd 的脚本不该变成每秒一次 fsync。

### 0.12.63

清空一个 session 槽之前先试 RFC-004 A.3 的目录锁。

boot 装配走到"这个 sid 要 fresh spawn"时会 `remove_file(entry.toml)`
+ `cleanup_stale_socket()` —— 而这两个文件正是一个 session 之所以可被
找到的全部。在一个还活着的 L3 底下做这件事,会把它永久孤立:listener
fd 仍绑在一个已经不存在的路径上,谁都拨不通,而它自己的哨兵(当时只
盯 entry.toml)什么都看不出来。2026-07-29 发现七个 session 正处于这个
状态 —— 一个正在 execv 中途的 L3 还没来得及写回 entry.toml,这次 boot
扫 registry 时就没把它算进 `alive_ids`。

目录锁是这里的正确权威,恰恰因为它不依赖 registry 是否最新:它由属主
持有整个生命周期,跨 execv 存活,只在进程死亡时释放。`alive_ids` 回答
"扫描那一刻有没有有效 entry",目录锁回答"此刻有没有人住着"。锁被占
→ 槽留 vacant + WARN,绝不动它的文件。

### 0.12.61

共享 crate(terminal BCE 只继承背景色)重链接.

### 0.12.60

RFC-006 渲染 polish —— 拖拽中源 pane 加深色蒙层("你在移这个")、dormant 占位常驻浅蒙层(凹陷感,提示文字透出)、Append 降级幽灵改整窗描边(无假半格);SessionView 增 dormant 位.

### 0.12.59

RFC-006 预览诚实化 —— 幽灵与松手结局共用同一个 resolve_drop_outcome(单一事实源):拖到自己原位(本格任意带 / 邻格近边插回原位)= Nothing,不亮幽灵不动作;6×6 满格边带 = Append,不再画做不到的半格 split;悬停 dormant 占位任意带 = Fill(装进这个空位,占位消费、源槽休眠、零变形);Esc 取消拖拽.

### 0.12.58

RFC-006 步骤 2-5 —— 分区 drop:悬停 pane 分五区(边带 25%,≥48px ≤⅓),边带=split(网格自动变形:1×1 右带→1×2、下带→2×1,满格扩行/列,6 封顶降级 append)、中心=swap(跨窗对调一手完成)、同窗边带=重排不留占位;拖拽全程实时幽灵预览(半透明 accent 矩形,亮的就是松手所得);拖到窗外=新窗 1×1 生在松手点(窗随 pane 走).

### 0.12.57

RFC-006 步骤 1 —— dormant 占位:pane 被移走(菜单/拖拽/移去新窗)源槽留"未激活占位"而非塌缩,布局纹丝不动;占位不可聚焦不可拖不可按键复活,单击才 spawn(绝不自动);窗口存活=非占位 pane 数,清零自关(1×1 拖走=窗随 pane 走);shell-state v3(SavedPane.flags bit0),v2/v1 读为 0,boot 直接物化占位不碰会话机器.

### 0.12.56

RFC-005 步骤 5(拖拽半)—— 按住 pane 标题拖过 10px 进入拖拽(标题显 ⇢),松开落在另一窗=移动、其它任何地方=取消、原地松开=原来的改名点击(改名从 down 延迟到 up,肉感无差);拖拽中关 pane/关窗自动取消.

### 0.12.55

RFC-005 步骤 5(菜单半)—— pane 跨窗口移动:右键菜单 Move to Window N / Move to New Window;整个 Pane 在 WindowState 间 Vec 移动(L3 无感知),源窗索引态修复、目标窗 append+聚焦+成 key;移去新窗 = 按 sid 泊车 + 请求 L1 开窗,attach 到来时用被移的 pane 建窗不新 spawn,优先于复原队列;空窗自请求关闭(WindowCloseRequest=65).

### 0.12.54

共享 crate(terminal CSI E/F)重链接.

### 0.12.53

2026-07-28 WindowServer 事故防线:session 进程总数硬上限 64(spawn 前查 registry 活口,超限拒绝并报错 —— 当晚 176 个把整机压死)+ safe-mode 启动只 reattach 不新 spawn、空槽留 vacant、不复原额外窗口.

### 0.12.52

共享 crate 重链接(app.rs perform_close 测试缝).

### 0.12.51

多行链接的下划线改成按"这一行真正盖住的格子"算 —— 原来除最后一行外一律画到窗格右边缘(软换行成立,别的都不成立):表格单元格里就穿过边框画到了表格外面.顺带 locate() 与 col_skip 变成死代码,删掉.

### 0.12.50

表格里被拆行的链接现在能整条识别 —— 行级合并问的是"上一行是不是顶到了窗格右边缘",而表格单元格是顶到自己的边框(还差好几列,边框字符正好占在启发式要找"最后一个字符"的位置),于是表格中的 URL 只有第一行那截被链接.新增按单元格列带纵向合并的扫描(共享 ≥2 条竖线的连续行 = 表格块),横向分隔行阻断合并,续行以 scheme 开头则拒绝合并(避免把同一列相邻两行的两条 URL 粘成一条).

### 0.12.49

WINDOW_FIRST_FRAME 挪进 render() —— 原来只记主渲染循环那条路,而 attach 分支也渲染(core 热替换后启动窗口的首帧正是走那里),于是把一个健康窗口报成冻住的.

### 0.12.48

每个窗口首帧记一条 WINDOW_FIRST_FRAME(逐帧那条是 1/8 采样的,安静窗口可能一条都不留 —— "这个窗口到底画没画"是黑屏第一个要问的问题).

### 0.12.47

RFC-005 平权审计:hover 改在 render 里按窗口发布(原来在命中测试时推给共享 renderer,A 的悬停画到 B 的工具栏)、cc 用量面板收进 WindowState(原来一开开在所有窗口)、Esc×3 逃生口按 session 计数(原来全局,一个 pane 的按键能替另一个凑数)、启动装配加 reserved_sids(core 热替换时启动窗口会把还没被宣告的其它窗口的活会话当孤儿收养,紧接着那个窗口自己又要 reattach 同一批).

### 0.12.46

RFC-005 步骤 4e — 输入路径平权:每个 CoreEvent 带 L1 打上的 window_id,按事件种类各自决定归属(按下/右键/拖放=落点窗口并取焦点;滚轮/移动=光标所在窗口不改焦点;拖拽/松开=按下时那个窗口;按键/预编辑=键入的那个窗口;应用级 focus=所有窗口;badge/title/PaneSession/注入输入=持有该 sid 的窗口;进程面板每窗口各自 tick).认不出的窗口一律丢弃,不回落 key window.

### 0.12.45

RFC-005 步骤 6b — 复原上次的 N 个窗口:启动时按保存记录请求 L1 开窗(新帧 WindowOpenRequest=64,老 reader 静默跳过,不 bump PROTO),窗口先带自己的网格与"starting…"占位出现,装配走 worker 线程(reattach 会阻塞在 UDS 握手上,开窗不能冻住已有窗口);复原装配不扫 registry(否则会把别的窗口的会话再收养一遍、把不认识的目录退休);off-loop 结果(spawn 完成/控制流重连/搜索结果)改成按 sid 找遍所有窗口 —— 原来只找 key window,焦点一换就静默丢弃.

### 0.12.44

RFC-005 步骤 6a — shell-state.bin v2:窗口列表 + key_window,save 写全部窗口(原来只写 key window 的 panes —— 开第二个窗口的瞬间它就成了 key,16 个 pane 的记录被 1 个覆盖);v1 文件读成单窗口.

### 0.12.43

RFC-005 步骤 4d — 窗口平权(泵与画):每个窗口自持 IOSurface 对/纹理/写入半区/帧间隔闸,attach 按 window_id 路由,渲染遍历所有脏窗口;pump 与搜索去抖走全部窗口(非 key 窗口的 pane 原本一个字节都收不到),退出条件改成"所有窗口的所有 pane 都退了".

### 0.12.42

RFC-005 步骤 4c — 认领新窗口:带着没见过的 window_id 的 attach 帧就是那个窗口的出生证,建 1×1 网格 + 一个 off-loop 新 pane.

### 0.12.41

修 --log-event 从未实现 — install-local 的 sup_log 每次调用都 panic(每次安装 3 个死进程,零日志);那是 2026-06-15 丢 9 个 session 后加的可见性机制,一直是坏的.

### 0.12.40

RFC-005 步骤 3 — renderer 拆分:pane 实例缓存与"欠清屏"标志归 WindowRender(每窗口一份),device/pipeline/字体/两个 atlas/所有 scratch 全共享;实测 atlas 不需要按 scale 分组.

### 0.12.39

RFC-005 步骤 2 — wire 带 window_id + 窗口生命周期帧(不 bump PROTO_VERSION:版本不等会被 L1 杀 core).

### 0.12.38

RFC-005 步骤 1 — 窗口态从 CoreApp 收编进 WindowState(windows: Vec + key_window),访问走 win! 宏保持字段路径以免借用冲突;单窗口行为零变化.

### 0.12.37

custom_title 收进 Pane — 标题成为 pane 的字段随 pane 走,消灭平行数组手工同步;boot 回绑并入 assemble_panes_at_boot(可测).

### 0.12.36

cc 账户按名字里的序号排(1→4),不再跟 collector 的完成顺序;卡片与时间轴同源.

### 0.12.35

交通灯绘制收成一条路径(组件与面板不再各画一份),hover 标记随之对所有自绘标题栏生效.

### 0.12.34

hover 图标掩码直接从系统按钮的截图提取(不再手抄形状)+ 宽高分离居中 + 整像素对齐.

### 0.12.33

hover 图标改按像素掩码画(字形尺寸/基线都对不准:实测我们的 ✕ 是 6x11,原生 6x6).

### 0.12.32

交通灯不再按字号缩放(scale_hint 是 cell_h/20,15 被缩成 12);改为绘制 core 算好的矩形,热区与圆点从此同源.

### 0.12.31

交通灯 hover 显 ×/−/+ 图标,跟系统一致(悬停标题栏三个都显,不是单个).

### 0.12.30

Process Monitor 的交通灯原来是第三份手写副本(自带常量+自带描边),前两轮改的组件它压根不调用;现在共用唯一出处.

### 0.12.29

交通灯去掉内侧描边 —— 它是 alpha 修复后才显形的,从内侧啃掉每边 1px;实测原生 14px 我们只剩 9px.

### 0.12.28

Process Monitor 四边补内边距(内容不再贴边/溢出)+ 绘制与热区共用同一套几何常量.

### 0.12.27

主循环看门狗 — 冻结进行时就报告,不必等迭代结束.

### 0.12.26

低优先级收尾 — L2 停顿阈值改具名常量、attach 分支 phase 改名避免同迭代重名、spawn 死参数删除、失败路也走 adopt_backend、交通灯测试从常量推导.

### 0.12.25

cc modal 几何独立成 ui/components/cc_usage_modal(具名 metric + 卡高推导 + 面板 rect),painter 只管画 —— 与 layout_modal 的分层惯例对齐.

### 0.12.24

ShapeCache 文档归位;共享 crate(握手超时受 deadline 约束、bytelog 恢复工具跨轮转段)重链接.

### 0.12.23

四个 writer 收敛到一个 BoundedWriter<T> + SnapshotWriter 独立成模块.

### 0.12.22

队列容量四处收成一处(frame_writer::cap)+ 快照不再为打日志白拷 100-300KB.

### 0.12.21

pending 槽加超时逃生口(worker 丢了不再锁死复活路径)+ 补 pane/LRU 回归测试(旧的分辨不出 LRU 与 FIFO)+ font_cache 文档与字段名对齐实现.

### 0.12.20

自审收口 — 修重连后 Cmd-C 静默失效(swap_control 漏换 selection_rx)+ 常量文档串位 + 删测试专用包装 + L3 停顿探测器拆细 phase + L2 写失败样板 6→1.

### 0.12.19

交通灯 14pt 追平原生 + mouse-tracking TUI 滚动清选区(claudecode 里滚一下选区不再罩错内容).

### 0.12.18

cc modal 两个 section header 改用系统 UI 字体(SF Pro 系统字号),与原生 header 同尺寸,不再是终端等宽小一号;ViewPainter 补 ui_text 比例字体原语.

### 0.12.17

cc 卡片四边同一 padding + 卡高由内容推导 + chip 文字按 ink 垂直居中.

### 0.12.16

cc modal 版式放宽(卡片内边距 / 两段间距 / OK 与百分比右对齐)+ 建 pane 搬出主循环 + wait_and_connect 调用方预算显式化.

### 0.12.15

阻塞面收口 — L2→L3 每条控制流走 FrameWriter(一个卡住的 L3 不再锁死整个 L2)+ 阴影层预乘修正 + wait_and_connect 单一 deadline(原来是 2×).

### 0.12.14

cc 时间轴右侧留标签槽 — 窗口条的 tag 不再压在 modal 边框上;轴按最宽 tag 收窄,面板同步加宽补回分辨率.

### 0.12.13

ui_rect 混合因子修正(着色器输出预乘,源因子却是 SourceAlpha,alpha 被乘两遍 —— 半透明填充一直不可见)+ 主循环阶段名细分.

### 0.12.12

cc 时间轴日期改虚线贯穿图表 — 日期不再只在轴下当标签,每天一条淡虚线上到图里,条的端点可以就地对着日期读;NOW 保持实线做对比.

### 0.12.11

L2 主循环根治阻塞 — 重连搬线程 / 写 L1 走 FrameWriter / ShapeCache 计数戳 LRU / cwd 负缓存 / .next_id 非阻塞 flock / font_cache 逐条驱逐 + l2.loop.stall 停顿探测.

### 0.12.10

Cmd+Shift+C 开关 Cc usage modal(排在 Cmd-C 复制之前,否则被当成复制吞掉;也排在 LOCK_KEYS 之前,claudecode 占键盘时照样能开).

### 0.12.9

Cc modal 视觉重排(对比度分层 + 全宽条 + 状态 chip)+ 滚轮单事件 tick 上限.

### 0.12.8

Cc modal 状态语义化 + 卡片保留槽布局 — allowed_warning 等原始 header 值不再直出;身份行按预留槽排(状态右对齐→名字→email 按余量截断),任何长度都不重叠.

### 0.12.7

Cc 钮换矢量 icon(UsageBarsIcon,基线+三段升柱)— 文字标签在四个描边 icon 中是异类.

### 0.12.6

cc usage modal 视觉修正 — 条高 0.55ch + 调色接 ui token + 时间轴按用量填充(轨道=窗口区间,彩条=已用部分).

### 0.12.5

cc usage modal — 工具栏 Cc 钮开 Claude 账户用量面板:n 账户卡片(5H/7D 利用率条 + reset/updated)+ ±6 天资源可用时间轴(5h/7d 窗口条 + NOW 线 + 日刻度);数据源 ~/.local/state/devops/claude-usage.json,自写零依赖解析,开着才读(5s 刷新),关闭零 IO.

### 0.12.4

marspot-linkify 石头孵化 — 特征值识别(URL/路径/email/IP/UUID)提取为零依赖独立 crate,CellSource trait 为唯一输入面;grid_links 变薄 adapter,调用面不变.

### 0.12.1

snapshot v5 重链接.

### 0.11.62

笔记见 git blame.

### 0.12.62

加 `--version` 早退,排在任何 env 读取 / state 迁移 / 日志初始化之
前。更新链的 probe 靠它:此前 `marspot-core --version` 会一路走到
`env_required` 然后 panic 退 101,给 core 装 probe 会把所有 core 更
新拒光(probe 判的是 exit 0)。probe 必须无副作用,所以这个分支不迁
状态、不开日志、不读 env。

顺带把 `updater::STAGED_BINARIES` 里的 `marspot-shelld` 去掉 ——
RFC-003 已退役 L4,每次轮询都白刷一条 "tarball had no
marspot-shelld"。

### 0.12.0

RFC-004 装配身份制(2026-07-17 宕机事故的架构级修复)。boot 装配
重写为按格 sid 处理:活(身份验证)→ reattach;reattach 失败 →
SIGKILL 已验证进程 + 原地降级复活(不再 delete_session 毁史);死
且目录在 → 同 id 复活;spawn 失败 → Vacant 占位 pane(保 sid,按
键重试)——槽位永不压缩、永不漂移。判活 = kill(0) + proc_pidpath
身份验证,pid 复用不再误杀无关进程。不在 saved 里的活 session 收养
为额外 pane;未引用的死目录进 retired/ 回收站(14 天 TTL)。标题按
sid 绑定不按下标。[+] 按钮 cap 与 ui::36 统一(私有 9 分叉)。
wait_and_connect 重试面扩大(NotFound/EOF/InvalidData/Reset)。
boot_assembly_tests ×5 集成测试(真 L3 + 沙箱)钉不变式。

### 0.10.79

4 chrome 调用点(dev_panel × 2 + context_menu × 2)`encode_canvas_into(..., ui_font: false)` 改回 — chrome 走 Monaco.framework 路径 land 不变.

### 0.10.78

push_text_run_kind 字符 advance / GlyphInstance slot 回到统一 cell_w(不拉伸).proportional per-glyph 留 v2+ atlas 重写.

### 0.10.77

renderer 完整 font 分离:`FontKind { Terminal, Ui }` + `push_text_run_kind()` 共享代码;`encode_canvas_into` 全签名加 `ui_font: bool` 参数路由到 UI font 渲染.5 调用点同步.

### 0.10.76

Token v4 全 land(token.rs 233→580 行)— 6 个新 const 命名空间 + 3 套 palette + themed dispatch 闭包覆盖.详 shell 0.6.25 entry.

### 0.10.75

Phase B0:Grid `(col_gap, row_gap)` 分开,`grid_with_gaps()` 显式 builder.lazy_vstack_padded(leading/trailing margin).MetalRenderer 加 `ui_font_metrics` / `terminal_font_metrics` 分离,`MARSPOT_UI_FONT_SCALE` env 控制 UI 尺寸.

### 0.10.74

framework Phase A 5 items 全 land:
- AnimRegistry tick/gc/any_active 真 frame schedule
- Transform / Mask / BlendMode modifier + bake
- GridTrack 三模式 + VariableGrid layout
- Transition modifier + bake
- theme version counter

### 0.10.73

framework 加 on_appear lifecycle / Spring anim curve / .shortcut / .focusable / .auto_focus / .on_appear / .on_disappear modifier + 3 个新 preset(context_menu / breadcrumb / list_row).reconcile() 升级返 Vec<LifecycleEvent>.

### 0.10.72

framework 加 composable presets card/panel/badge/tooltip/tab_strip + theme::color::hc(HighContrast)调色板.每个 preset 都是 modifier chain 组合,L5 component migration 用作 building block.

### 0.10.71

framework 加 LazyHStack / Grid / Toggle / Picker layout + paint(纯渲染层 — host state 从 HostState 读).Toggle 渲染按 ToggleState.on 切 capsule 颜色 + circle knob 左右位置;Picker 渲染高亮选中 segment.

### 0.10.70

P3 framework 加 LazyVStack/Image/Shape/Gradient/Material 渲染路径(View enum 6 个新 variants;paint pass 处理).Renderer 端无新 primitive — gradient 用 16 个 solid bands 近似,Image/Material 用 tinted rect 占位.

### 0.10.69

修 `push_text_run` x 推进 — CJK 乱码真根因.0.6.16 用户报"中文还是乱麻",但我以为只是 layout 端 chars().count() 算窄了.其实 renderer 端 `push_text_run` 在 `render_metal.rs:3732` `x += cell_w` 无条件前进 1 cell,但 glyph 按 `n_cells = char_width(ch)` 画到 2 cell 槽里.结果 CJK 字按 2 cell 宽渲染,但下一个字 x 只往前 1 cell → 下一字跟前一字的右半边撞.

修:`x += cell_w * n_cells as f32`.跟 `entry.n_cells` 用同一张 char_width 表的结果保持一致.

shell 0.6.17 → 0.6.18 (HostState 通用).无 lib 回归.

### 0.10.68

toolbar 的 dev panel icon click 改 fire 新 wire(`MsgType::DevPanelToggle = 55`),路由到 L1.L2 删掉自己那份 `dev_panel: DevPanelState`(本来就是 no-op 死代码 —— `dev_window::with_dev_window` 在 L2 进程里永远是 None),也删掉 redraw 路径里那段 dead `dev_window::with_dev_window(|w| w.render(...))` 调用.L2 在 dev panel 这件事上只负责"hit-test 那个 icon 矩形 + 发 wire 帧",其余全在 L1.

详细见 L1 shell 0.6.6 同期条目.

### 0.10.67

link 菜单 Copy 排到 Open 上面.终端场景主要用法是抓 URL/路径(粘到聊天/笔记),不是开浏览器,把高频 action 放第一项.

### 0.10.66

URL/filepath 点击改弹右键菜单(Copy + Open),不再左键直接 spawn_open.两个原因:① 误点 URL 静默触发 `/usr/bin/open` 是体验地雷(意图选中文本结果浏览器/Finder 弹出);② 用户在终端想做的"link 上动作"主要是复制 URL 给别人看 / 别处粘贴,而不是开浏览器.加 ContextMenuAction::OpenLink / CopyLink + LinkContext { text, kind } + ContextMenuState.link.新 helper hit_test_link_at_xy(x,y) 把 cell_pos_hit + hit_test_pane_link + Email 过滤合一,Url / File 返 LinkContext.build_link_menu_items 按 kind 出"Open URL / Copy URL"或"Open file / Copy path".mouse_down 原 spawn_open(&link.text) 整段换成弹同一菜单;mouse_right_down 在 region resolve 前先 hit_test_link_at_xy,有 link 就走 link menu.dispatch_context_action 在 clear 前 snapshot link_text,OpenLink → spawn_open / CopyLink → write_clipboard_text.148/148 lib tests PASS.

### 0.10.65

F3+12.6 右键 ContextMenu 分隔线根因+修.encode_passes 把 overlay_cells 先于 overlay_ui_rects encode → cells pipeline 的红条被菜单 SDF frame 完全覆盖,所以前几次 user 看不到 hairline 也看不到 DEBUG 红条.改走 ui_rects pipeline + 线高 3px(防 SDF AA 把 1-2px 全抹掉)+ 半透明白 [1.0, 1.0, 1.0, 0.22],blend 后 ≈ (82, 84, 89) over (33, 36, 43),清晰可见

### F3+12.5 DEBUG

F3+12.5 DEBUG:分隔线行整块画鲜红 [1,0,0,1].user 试三次都看不到,要先确认 divider 行有没有进 paint loop

### F3+12.4 右键 ContextMenu 分隔线最终修

F3+12.4 右键 ContextMenu 分隔线最终修.0.10.62 走 SDF ui_rects 也看不到,因为 SDF anti-alias 对 1px 高的矩形把整条线判成 anti-alias 边缘带 → 全亮度归零,跟没画一样.改回 cells pipeline `fill_rect`(无 AA)+ 不透明预混合色 [0.42, 0.44, 0.50, 1.0](视觉等价 18% 白 over 暗 bg)+ 线宽 = scale(retina 2px 物理),保证至少 1 物理像素亮起.user 第三次验证再不见就要从 GPU 抓帧了

### F3+12.3 右键 ContextMenu 分隔线再修

F3+12.3 右键 ContextMenu 分隔线再修.0.10.61 改色后 user 仍看不到 → `fill_rect` 走 cells pipeline 不做 alpha blending,[1,1,1,0.18] 不上屏.改用 `fill_rounded_rect(radius=0)` 走 ui_rects SDF pipeline,alpha 真 blend

### F3+12.2 右键 ContextMenu 分隔线现身

F3+12.2 右键 ContextMenu 分隔线现身.原 divider 色 [0.30, 0.34, 0.40, 1.0] 跟面板 BG [0.13, 0.14, 0.17] 太接近,user 看不到.改 macOS 暗色菜单标准的半透明白发丝线 [1.0, 1.0, 1.0, 0.18],blend 后 ≈ (73, 75, 81),跟 (33, 36, 43) BG 清晰对比.同步 y 坐标 round() 到像素栅格防 1px 线被 anti-alias 成两条半亮

### F3+12.1 撤 F3+12 blank-skip

F3+12.1 撤 F3+12 blank-skip.user 装上后立刻看到 bullet 之间 / paragraph 之间 / Enter on empty prompt 产生的真 blank 行进入 history 后消失,红框圈给我看了 —— 用户明示"额外删除了本来应该有的空行".storage 没法在 row content 层面区分 spinner-default-fill vs user-intended blank,两者都是 Cell::default() 全等行.iTerm2/Alacritty 不过滤是对的:dumb-store 保留所有,spinner 污染是已知 trade-off,user 烦了自己清.scrollback_display scenario3 改回 verbatim 断言,clear_scrollback_zeroes_len_in_o1 / scroll_up_more_than_height 改回原 (不依赖 blank-skip).555/555 lib + 6/6 harness PASS

### F3+12 storage policy

F3+12 storage policy:Grid::scroll_up 跳过 `Cell::default()` 全等行(ch=' ' AND attrs 全默认).不是防御性代码,是 marspot 的 use case 决定的 storage 语义:claudecode spinner 高频 emit `cursor-up + erase-line + redraw + \n`,dumb-store 一小时下来积累 5000+ 行连续 default-fill blank,scrollback 不可用(inspect 工具实测 269/280/286/287 session 都是 line[N-5000]="").iTerm2/Alacritty 不过滤是因为它们的 workload 不是 100+ claudecode pane,marspot 的场景下 dumb-store 不是"忠实",是"噪声".判断条件最严:只抓 strict `*c == Cell::default()`,不抓 SGR-tinted blank,不误伤 CJK wrap-pad NUL.skip 时整条 push 跳过(push_line 不调用,total_lines 不增,scroll_push_count 不增),无双轨 chaos.snapshot v3 dedup 不受影响.同步 wipe 12 sessions 的 48 个 scrollback 文件清除 F3+11 era 积累的脏数据.555/555 lib + 6/6 harness(scenario3 重写为 default-blank-drop 反向断言)+ mini 11/11 PASS

### F3+11.2 修 F3+11 RAM ring load 撤太狠

F3+11.2 修 F3+11 RAM ring load 撤太狠.user 装 0.10.57 后回报"history 完全没有,只有新积累几行,上面全黑",12 panes 都是这个状态.根因:F3+11 把 RAM ring load 改成严格(任何 read 失败 propagate),tail-truncate scan 只检最后一条 idx;如果 RAM 窗口(最后 256 条)里中间某条 idx 指向 BufWriter unclean kill 留下的截断 record,read_record_at 返回 Err → 整个 open() 失败 → Terminal::new fallback 到 empty Disk → 用户看到 zero history.恢复 tolerant load(单 record 失败用 blank row 替代继续);这条是**有原则的容错**,不是 paranoid:已知 BufWriter 工作机制下 unclean kill 必然有 0~N 个 tail 半成品,单 record 失败不该让 ALL history 失效.明确写注释区分原则容错 vs 防御性兜底.555/555 PASS

### F3+11.1 修 F3+11 引入的两条回归 + L3 scrolled-view cursor

F3+11.1 修 F3+11 引入的两条回归 + L3 scrolled-view cursor.① user 报 resize / pane 渲染极慢:根因 push_line 撤掉 trim trailing default cells 后每条记录全宽,resize 走 scrollback reflow 时 read 量 5-10×,大文件 session reflow 多秒卡死.恢复 trim(纯磁盘大小优化,语义不变:trimmed-tail 列 read 返回 None,viewport 渲染层 map 成 Cell::default).② idx 改 raw File 直写(不再 BufWriter,8B 一个 syscall 等价 cap=0,撤 push-time 显式 flush hack).③ L3 scrolled-view(view_offset>0)时 grid_shm publish 把 FLAG_CURSOR_VISIBLE bit 抹掉:用户报"光标不应该随滚动改变位置",根因是 L3 pane 经 L3 已经把 view_offset 应用到 cells,但 cursor_col/cursor_row 仍是 LIVE 位置,L2 渲染时把 cursor block 画在 scrollback 内容上.iTerm2 / Alacritty 也是滚动期间隐藏光标的标准做法.④ scrollback_display scenario4 重写为验证 reopen graceful (idx tail-truncate 修复 + 续写正常),不再要求 BufWriter 未 flush 数据被神奇恢复(那是 BufWriter 工作机制不是 bug).⑤ push_line_trims_default_tail 单元测试恢复.7/7 harness + 555/555 lib + mini 11/11 PASS

### F3+11 scrollback 整套撤防御重做

F3+11 scrollback 整套撤防御重做.user 拍板"我们不允许防御性编程不允许做兜底 hack",storage 回到 iTerm2 / Alacritty 标准做法 = dumb append-only log:① Grid::scroll_up 撤掉 blank-skip 全部判断,每一行 scroll-off 都 push,scroll_push_count 回到 unconditional ++ ② FileScrollback::push_line 撤 trim trailing default cells + 撤 effective_len 追踪,每个 cell 原样写,total_lines 就是真长度 ③ FileScrollback::open 撤 rename_corrupt + rebuild_idx_walking_bin + initial_effective 倒扫 + tolerant ram-load,只保留单一 idx-tail-truncate 一致性修复(SIGKILL 后 BufWriter sync 偏移恢复),其他全部错误 → Err 报给上层不偷偷修 ④ scrollback_effective_len API 整套删,grid_shm publish 真长度 ⑤ rename_corrupt + rebuild_idx_walking_bin + rebuild_idx_from_bin 三个 helper 全删 ⑥ idx 改 per-push flush(8 字节 syscall,bin 仍 64KB BufWriter)防 BufWriter 双轨 size 不对称导致 SIGKILL 丢全 idx ⑦ scrollback_display harness 6 scenario 全部按 dumb-store 语义重写(blanks_are_preserved_verbatim 显式验 "blank 也存") ⑧ 老 v1/v2 file 全删 .corrupt 备份 12 个清空.555/555 lib + 6/6 harness + mini 11/11 PASS

### F3+10i-3 撤掉 blank-check 的 `\0` 分支

F3+10i-3 撤掉 blank-check 的 `\0` 分支:user 装完 F3+10i-2 后回报 claudecode pane "宽席问题再次回归".根因:`\0` 是宽字符 trail-cell sentinel(terminal.rs::print 在 col+1 写 `\0` 标 width-2 glyph 的占用位)+ 宽字符 wrap-pad(terminal.rs:1268 在 cols-1 写 `\0` 防 reflow 误判用户敲的空格).i-2 把 `c.ch == ' ' || c.ch == '\0'` 都当 blank 会让"行尾有 wrap-pad + 行首几格空格"的真实 CJK 数据被误判为 blank,push 被 skip → 下一行 wrap continuation 失锚 → 视觉错位.改回只看 `c.ch == ' '`.scenario7 SGR-tinted blank 仍通过(SGR 41 + 20 spaces + SGR 0 喷帧那条仍只全是空格).7/7 scrollback_display + 559/559 全套 PASS

### F3+10i-2 Grid

F3+10i-2 Grid::scroll_up blank-skip 改 ch-only.user 装完 F3+10i 后回报"自己这个 pane 还有问题":claudecode spinner 用 SGR(bg=Indexed/dim/bold)+ 写空格 + reset 模式喷帧,空格 cell 带 attrs ≠ Cell::default(),原 `*c == Cell::default()` 误判成"有内容"继续 push.改成 `c.ch == ' ' || c.ch == '\0'` 只看字符,attrs 不算.加 scenario7_sgr_blank_doesnt_pollute(SGR 41 红 bg + 20 spaces + SGR 0 reset + \r\n × 100 次,验证 scrollback effective_len 增长 ≤ ROWS 即只算 live-area drain).7/7 scrollback_display + 559/559 全套 PASS

### F3+10i scrollback FILE_VERSION bump 1→2 + FILE_MIN_COMPAT 1→2

F3+10i scrollback FILE_VERSION bump 1→2 + FILE_MIN_COMPAT 1→2:user 看到 install 后翻历史仍大片黑(F3+10h Grid::scroll_up skip-blank 只防新数据污染,老 scrollback.bin 文件里 F3+8 mmap-write era 留下的 past-EOF idx tail + 中段 blank pushes 没擦掉).user 授权"新的历史没问题,老的全都不要了都可以".bump version 让 open() header check fall 到既有的 rename_corrupt + recurse fresh start 路径,所有 12 pane execv 后第一次 push 自动 reset.未来 scrollback shape 改动只 bump VERSION 不动 MIN_COMPAT 即可保 back-compat

### F3+10h Grid

F3+10h Grid::scroll_up 跳过 fully-default 行的 scrollback push:claudecode spinner UI 每 tick `cursor-up; redraw; many \n` 让 visible top row blank,过去那条 blank 行被 push 进 scrollback,user 翻几页就看到大片黑.fix:`row.iter().all(|c| *c == Cell::default())` 命中则 skip push + skip scroll_push_count,blank row 永不污染历史.同时把 crates/marspot-term/tests/scrollback_display.rs (user-perspective integration harness,6 scenarios:scroll-down-visible / blank-tail-doesnt-block / scroll-cap-at-oldest / torn-write-recovery / resize-no-corruption / clean-restart-preserves-all) 加进 bin/test.sh 用 --all-targets gate.558/558 PASS (lib + integration).clear_scrollback_zeroes_len_in_o1 test 改成在 set_cell 之后 scroll 来产 5 条非 blank 记录,适配新行为

### F3+9 split-arch 右键 ContextMenu 接通

F3+9 split-arch 右键 ContextMenu 接通:`MsgType::MouseRightDown=18` wire 串好 (per `feedback-frame-forward-compat`:v2 receiver silently skip 新 msg_type),shell `mouse_right_down` impl 走 `encode_mouse` 转发,core decode → `CoreEvent::MouseRightDown(x,y,mods)` → `CoreApp::mouse_right_down`.CoreApp 新加 `context_menu: Option<ContextMenuState>` field + 三块 enum(ContextRegion/ContextMenuAction/ContextMenuState)+ 4 个新方法(`mouse_right_down`/`resolve_context_region`/`build_menu_items`/`dispatch_context_action`).左键 mouse_down 加 menu-modal intercept (Item dispatch+关 / Frame 吞 / Outside 关 + fall-through);mouse_moved 跟踪 hovered_idx;key Esc 关菜单.render path 加 `set_context_menu`,publish ContextMenuRender 给 renderer.split-arch + 单 binary 行为对齐 (跟 [[project-amendment-16-self-execv]] 一样的 helper enum 重复在 core.rs / main.rs,但 ContextMenu 组件本身共享 src/ui/components/context_menu.rs).8 个 action 跟 main.rs 同样路径,Pane 输入走 `pane.session_mut().forward_paste()/forward_inject_input()` (而不是 Pane.write 那条 L3 backend 返 Ok(0) 的死路).主仓 build PASS

### F3+10 紧急回滚 0.10.49 mmap-write + 加 reopen 数据修复

F3+10 紧急回滚 0.10.49 mmap-write + 加 reopen 数据修复:scrollback history 被 0.10.49 引入的 mmap-write 写花(idx chunked over-allocation 留中间 zero 洞,bin ftruncate-up 让 append-mode 在 zero tail 之后继续写,最终中间一段 logical idx 指向 off=0 = 读回来全空,跨多个 pane 同时观测到大片纯黑+表格行间分隔被吞).user 拉响"绝对不能错": ① 把 `crates/marspot-term/src/scrollback.rs` 整文件 `git checkout e6fb85a^` 回到 BufWriter 版本 ② 加 `FileScrollback::flush_for_handoff()` 公开 hook 给 do_l3_execv_swap 在 execv 前调,代替 mmap-write 的"page cache 自动同步"功能,保留 snapshot v3 自描述当 belt-and-suspenders ③ 加 `rebuild_idx_walking_bin` 在每次 open() 都跑 — 前向扫 bin 记录,验 rec_len/cols sanity,撞 rec_len=0 / >4MiB / cols>4096 即停;返回真实数据末尾;按这个值 ftruncate bin 把过分配尾甩了,然后 truncate idx 文件重写 — 一次 open ~1s/100MB,跟 mmap 比慢一截但永远不会让 idx 指向 off=0 这种 garbage.snapshot v3 不动(跨版本 receiver dedup 鲁棒).已 terminal-crate 382/382 PASS

### F3+8 scrollback execv gap 根治

F3+8 scrollback execv gap 根治:① snapshot v3 自描述(`start_logical_idx: u64`),`apply_snapshot` 按索引去重而不是 F2+4 的"disk 有就 skip" 启发式,SNAPSHOT_VERSION 2→3 silent 兼容 v2 sender → v3 receiver(start_idx 缺失就 fall 回老 skip 路径)② `FileScrollback` BufWriter 彻底废,bin/idx 各一个 R/W fd + `MAP_SHARED` mmap + chunked ftruncate(BIN=64KB、IDX=4KB);push_line = ensure_capacity + memcpy(零 syscall amortised),`Drop` 不再 flush 因为本来就没 buffer;execv 跨过去 kernel page cache 是 single source of truth,数据按构造不丢.重新设计的 reopen 走 idx mmap 反向扫找最后一个非零 u64 找真实尾,处理上次进程死前没 Drop 留下的 over-allocated 尾巴;`ensure_flushed` / `has_unflushed` / `bin_for_read` 整套全删,terminal-crate 测试 382/382 + 主仓 524/524 全过

### F3+3.8 ① UI rect SDF 改内描边(box-sizing

F3+3.8 ① UI rect SDF 改内描边(box-sizing: border-box):shader cells.metal 原 `|d| < border_width/2` 居中描边让 1px border 在容器外延 0.5px,跟"border 是容器内的东西"的 UI 直觉相悖,所有 card / view frame 视觉上比 rect 大半像素.改成 `outside_band * beyond_inner` 把 border 完全锁进 `-border_width < d < 0` 的内侧带,容器尺寸是真的尺寸,border 是从内侧吃掉的几像素 ② LayoutModal card 改扁:CARD_ASPECT 4/3 → 2.4,CARD_MIN_H 56 → 44,新加 CARD_MAX_H=96 防 1-col 变成巨大 banner.3-col 现在 card_h ≈ 54pt,跟 row 间距 8pt 形成 list-of-slots 视觉而不是 mini-terminal 视觉

### F3+3.7-fix LayoutModal rows=4 (或任意把 modal_h 推过 window_h*0.95 的 cols×rows) 时 M…

F3+3.7-fix LayoutModal rows=4 (或任意把 modal_h 推过 window_h*0.95 的 cols×rows) 时 ModalFrame::layout 会硬剪 frame_h,但 LayoutModal 原版按"理想 modal_h 一定到手"算 card_region,所有剪刀差全压在 card 区:region.h < card_block_h → place(Center) 让 card_block 上下溢出 → 最后一行 card 底边落到 total_label 区,边框视觉上"消失".fix:把 body_h 拆成 fixed_body_h (steppers/labels/apply/pads/gaps,跟 rows 无关) + ideal_card_block_h;ModalFrame 拿回真实 frame.body.h 之后,actual_card_block_h = body.h - fixed_body_h,若 < ideal 就反推 card_h 收缩,card_block 永远塞进 region.加 2 个回归测试:short-window (660pt) last_row 不溢出 total_label;tall-window (1080pt) card_h 保持 ~98pt ideal 不缩水

### F3+6 session 持久化

F3+6 session 持久化:~/Library/Caches/marspot/shell-state.bin (MAGIC=0xA5505010, VERSION=1, atomic .tmp+rename).新 src/state.rs 模块 LE-binary 编码 grid_cols/rows + focused_idx + pane_count + per-pane (sid, custom_title, last_cwd) + (reserved) window frame for F3+6.1.MAX_PANES=128 / MAX_STR_BYTES=4096 防 OOM trigger.3 个 round-trip / bad-magic / runaway-pane-count unit test.5 个 save trigger:spawn_session / close_session / commit_title_edit / LayoutModal Apply / boot 末尾.Boot 路径:state::read() 决定 grid_cols/rows + n_sessions;alive_ids 按 saved.panes 顺序 reorder,saved 没记的 alive id 排到尾后续 SIGKILL 清掉;fresh-spawn 那条用 saved 的 last_cwd 走新的 spawn_l3_pane_with_cwd(initial_cwd) → MARSPOT_INITIAL_CWD env → L3 main.rs 读 → LocalSession::spawn(cwd_override=&path) chdir before forkpty.custom_titles_init 优先 saved.panes.custom_title,fallback entry.toml session_titles.focused_idx 从 saved 恢复并 clamp 到 panes.len()-1.MARSPOT_STATE_DIR env 仍走 dev sandbox.window 字段 reserved 但 None until F3+6.1 wire L1 NSWindow frame

### F3+5.1 fix latent bug

F3+5.1 fix latent bug:refresh_pane_cwd_for 失败路径不更新 last_cwd_refresh,build_views 的 lazy_fill 用 force=true → 任何永远抓不到 cwd 的 pane (entry.toml 缺 shell_child_pid / sandbox 挡 proc_pidinfo / pane 刚 spawn 那几毫秒) 都每帧 force retry,60 fps × N × ~15us = 0.8% CPU 白送.fix:① refresh_pane_cwd_for 在 syscall 之前就 last_cwd_refresh.insert(sid, now),success/fail 都进 debounce 时钟 ② lazy_fill 改 force=false,跟新 debounce 配对 — 失败 pane 最多每 CWD_REFRESH_DEBOUNCE 重试一次.steady state 全 contains_key 命中,跟之前一样

### F3+5 pane title cwd 改 hybrid 被动+懒填

F3+5 pane title cwd 改 hybrid 被动+懒填,5 个 trigger 零轮询:① pane spawn 时 refresh_pane_cwd_for(new_idx, force=true) ② mouse_down focus 切到新 pane 时 (focused_idx 变化) refresh_pane_cwd_for(idx, force=false 走 150ms debounce) ③ key_event 字节含 \r 或 \n (Enter / Return)到 focused pane 时 refresh_pane_cwd_for(focused, false) ④ LayoutModal 打开仍走 refresh_pane_cwds 全 pane 批量 ⑤ build_views 头调 lazy_fill_missing_cwds() — 任何 sid 没在 pane_cwds 就当场抓一次,steady-state 全 cache hit 不进 syscall.CWD_REFRESH_DEBOUNCE=150ms HashMap<sid, Instant> last_cwd_refresh dedup 多行粘贴 N 个 Enter 收成 1 个 syscall.close_session 时清 pane_cwds + last_cwd_refresh + pane_badges per-sid 防泄漏.idle 0 syscall/s,active 打字 < 1 syscall/s,title 一直可用

### F3+4.1 新 Table 组件 + Process Monitor 切到 Table

F3+4.1 新 Table 组件 + Process Monitor 切到 Table.src/ui/components/table.rs 新组件:TableColumn (width: Px(f64)|Flex(f64), align: Alignment, sort: Option<SortDir>, sortable: bool) + TableRow {cells: Vec<String>, depth: u8, kind: Data|Section} + TableStyle (header_bg/fg + row_bg/alt/selected + fg/section_fg).API:Table::paint(p), column_x_widths(), row_rect(i), body_rect(), hit_test_row(x,y), hit_test_header(x,y).devops-grade:Flex/Px 列混排、列对齐、tree-style depth indent、sort 指示 ▲/▼、section row 不可选.3 个 unit test 验证 flex 分配/hit_test 边界/header sortable 过滤.Process Monitor master + detail 都用 Table 实例渲染,kill[×] 叠在 detail 最后一列 cell 上.row_h/header_h 一组样式集中在 TableStyle 默认 sane defaults,marspot 配 PROCESS_PANEL_* 色板覆盖

### F3+4 Process Monitor 大重写

F3+4 Process Monitor 大重写,信息+UI 专业化.tabs 死亡 → master/detail.master 列 (left 38%):pane name(custom>cwd basename>序号) / #pids / CPU% / RSS,按 CPU% 降序排.detail 列 (right 62%):选中 pane 的进程树,缩进=depth,列 comm/pid/CPU%/RSS+×.新加 pidtree::proc_stat(PROC_PIDTASKINFO) → 拿 rss_bytes + total_cpu_ns + threads;refresh_process_panel 每 2s 跑一遍 walk 所有 pane 的 pid 树,sample 当前 stats,prev_pid_stats HashMap delta 出 CPU%,把 aggregate (n_pids/cpu_pct/rss_kb/busy) 存到 PanePidTree.master 行 hit_test 切 selected_pane.column header 用 ViewPainter::text_in + Alignment::CenterRight 拍齐数字列.ProcessPanelRow 新字段 pid/comm/cpu_pct/rss_kb 替代原 text.kill[×] 不变,scroll 仍只 detail 列.format_rss helper KB→K/M/G

### F3+3.7 LayoutModal 加大 + card 加大 + grid 居中

F3+3.7 LayoutModal 加大 + card 加大 + grid 居中.MODAL_W 320→440 / MODAL_MIN_H 280→380 / CARD_MIN_W 44→80 / CARD_MIN_H 32→56 / CARD_GAP 6→8.原 card_grid_pad 删了,改用 Rect::place(card_block_w, card_block_h, Alignment::Center) 把 card block 居中放进 row2 底部到 total_label 顶部的 region — 任何 vertical flex 平均落在 cards 上下,而不是堆在 modal 下半.原来 cwd 短 (8-9 chars) 就能塞下,但典型 workdir basename 10-15 chars 会被裁;新 size + Alignment helper 一并解决# L2 — F3+3.6 撤掉 OSC 7 push-based cwd 链,改 pull-based.原 F3+2.x 整套:Terminal.cwd/cwd_dirty + osc_dispatch OSC 7 parse + take_cwd_dirty + L3 publish_and_poke PaneCwd frame 发送 + MsgType::PaneCwd(54) + CoreEvent::PaneCwd + l3_reader_loop dispatch + ZDOTDIR shim __marspot_osc7 hook + TERM_PROGRAM=marspot 全删.改成 fn refresh_pane_cwds(walk panes → shelld_session_id → read_shell_child_pid → proc_cwd → 写 pane_cwds HashMap),在 layout button 翻 false→true 时调一次.title strip placeholder + LayoutModal preview 都读 pane_cwds — 数据在每次 modal 打开时刷新,关闭后 stale 直到下次打开.PaneCwd discriminant=54 留洞 silent-skip 兼容老 L3 frame

### F3+3.4 view 定位能力一组新 API

F3+3.4 view 定位能力一组新 API.Rect 加 inset(padding) + place(w,h,align).Alignment enum 9 anchor(TopLeft/TopCenter/TopRight/CenterLeft/Center/CenterRight/BottomLeft/BottomCenter/BottomRight),factors() 返 (hx,vy) ∈ {0,0.5,1}².ViewStyle 加 padding: f64 + View::content_rect()=rect.inset(padding).ViewPainter::text_in(rect,s,color,align)=Rect::place 算单字符宽 chars*cell_w → 居中 / 锚边等.LayoutModal paint 所有手算 `(rect.h-cell_h)*0.5+ascent` / `(rect.w-text_w)*0.5` 全删,用 text_in + Alignment 一行替代.Grid 新组件:GridStyle enum::Gap{width:f64}|Seam(SeamStyle) 互斥,Gap mode paint 是 no-op(空隙 IS 视觉),Seam mode 委托给 GridSeams

### F3+3.3 LayoutModal V2.0 加 card grid + drag-drop reorder

F3+3.3 LayoutModal V2.0 加 card grid + drag-drop reorder.modal 高度自适应放下 cols×rows 个 card,每张 card 圆角 + 居中显示该 slot 当前 pane 的 title.mouse_down 在 card 上启动 drag(grab_offset 锚视觉跟手指),mouse_drag 实时更新位置,mouse_up 用 nearest_card 找最近 slot 中心做 swap.Apply 时 card_slots permutation 应用到 self.panes + custom_titles(同步重排,title 跟着 pane 走).cols/rows 改变 reset card_slots 到 identity.drag 期间画 4 层:base card(原 slot 变暗) → drop target slot 蓝色高亮 → floating dragged card(浮在最上).V2.1 留 bounce 动画 + magnetic snap visual

### F3+3.2.1 prune path SIGTERM → SIGKILL

F3+3.2.1 prune path SIGTERM → SIGKILL.根因 #2 (#1 是 connect retry):L2 boot 时 alive_ids.skip(n_sessions) 想清掉多余 sessions 用 SIGTERM,但 RFC-003 Amendment 16 把 SIGTERM 重载成"如果 fingerprint 差就 self-execv",L3 收到 SIGTERM 后看 current/marspot-session fingerprint 跟自己不一样就 execv,然后 sit idle 永远没 L2 client → 进程泄漏.改 SIGKILL bypass handler 必死.reattach 失败那条同理改 SIGKILL.signal 语义清晰化:SIGTERM=请你升级(self-execv 或 clean exit) / SIGKILL=立刻死

### F3+3.2 fix L2 reattach race during 9-L3-simultaneous-execv install storm

F3+3.2 fix L2 reattach race during 9-L3-simultaneous-execv install storm.根因:L2 cold start 时跑 kill 0 alive_check pass (L3 process 还在),立刻 connect() → L3 已 SHUTDOWN_EXECV 但还在 execv() → from_handoff() 之间,accept loop 尚未 spawn → ECONNREFUSED.过去 connect_with_handshake 一次失败就 prune+spawn fresh,用户看到"少一格".uds_session_client::wait_and_connect 现在 ECONNREFUSED retry 20ms 直到 timeout(2s),让 execv handoff 完成.spawn 路径无影响(brand-new L3 第一次 connect 就成).典型 execv <100ms,install 抢资源时 9 L3 一起 swap 拖到 500ms — 2s 余量充足

### F3+3.1 SESSION_COUNT_HARD_CAP 9→36 (匹配 LayoutModal GRID_MAX² = 6×6)

F3+3.1 SESSION_COUNT_HARD_CAP 9→36 (匹配 LayoutModal GRID_MAX² = 6×6).空 cell 点击 → spawn_session + 落焦点,鼠标一气呵成"增 grid → 填 pane".之前 9 锁死了 Nine 固定 grid 假设,F3+3.0 LayoutModal 给了任意 cols×rows 但 cap 没动 → 用户改 12 cells 后 sidebar [+] 灰 / 空 cell 点击只是 set_focus_to_dead_idx

### F3+3.0 删 7 项固定 popup picker (Single/SplitH/

F3+3.0 删 7 项固定 popup picker (Single/SplitH/.../Nine),改成 LayoutModal:任意 cols × rows 的 +/- stepper + Total 行 + Apply 按钮.toolbar layout button 一击切 modal 开/关.modal 关时所有 click 默认行为(grid/sidebar/chrome 都可命中);modal 开时拦截整窗 click,modal 内分发 (Close/Apply/ColsDec/+/RowsDec/+/TitleBar/Frame),click 框外关 modal.shrink-guard:Apply 后若 focused_idx >= new cells_count,focus 落到 last cell.溢出 panes 继续 sidebar fallback.LayoutMode enum + PICKER_LAYOUTS const 全删,grid_dims 现在是 (cols: usize, rows: usize) tuple.Layout.picker_panel_rect / picker_option_rects / picker_option_dims fields + hit_test_picker_option / hit_test_picker_panel methods 全删.GRID_MIN=1 / GRID_MAX=6.layout_modal_state passed to build_instances → push_layout_modal_via_view → paint_layout_modal_content,overlay scratches 走 extra pass 永远在最顶

### F3+2.1 pane title placeholder 改成被动 OSC 7 链

F3+2.1 pane title placeholder 改成被动 OSC 7 链.之前 F3+2 是每帧 proc_pidinfo + entry.toml read,违反"hot path 神圣 zero alloc"原则,PTY 喷流 1-2% CPU 白费.改成:macOS 自带 zsh /etc/zshrc 已注册 OSC 7 hook `\e]7;file://host/path\07` (需 TERM_PROGRAM 非空) → marspot Terminal parser 的 osc_dispatch 解析 → set self.cwd + cwd_dirty=true → L3 publish_and_poke 读 take_cwd_dirty → 发 PaneCwd(54) wire frame → L2 l3_reader_loop decode → CoreEvent::PaneCwd → app.pane_cwds: HashMap<sid, String> 写入.build_views 拿 pane_cwds.get(sid) → Path::file_name 当 placeholder.零 hot-path syscall,event-driven.spawn 时 set TERM_PROGRAM=marspot 让 hook 自动激活

## L3  marspot-session

Current: **0.11.82**

### 0.11.82

The wrap check stops being a function call.

`take_pending_wrap` runs once per printable character and does work
once per row.  Both halves lived in one function, so the check — a
single bool — was reached through a call two million times in an 8 MB
emoji corpus.  A leaf profile showed it as its own **5.1 %** symbol,
which is what a function that should have been a branch looks like.

Split: an `#[inline(always)]` test with the rare half `#[cold]` and
`#[inline(never)]` behind it.  The same shape the CJK scanner work
used, for the same reason — a hot loop should not carry the code its
uncommon case needs.

mini, interleaved A/B, seven trials each and two rounds: `cat-cjk`
289 → 302 (**+4.5 %**), `cat-emoji` 187 → 194 (**+3.7 %**).  `cat-ascii`
gained too (429 → 442 on the dev box); a single earlier reading had
shown it losing 2 %, which two rounds of interleaving showed to be
noise.

Two things measured on the way and NOT taken, recorded so they are not
tried again blind: deferring `cluster_buf` materialisation to the rare
path (a String clear plus a UTF-8 encode per glyph — worth nothing,
because `String::push` keeps its allocation and the clear is a length
store), and encoding scrollback records straight into the writer's
buffer (slower: `Vec::resize` zero-fills before the closure overwrites,
so two passes replaced one).

### 0.11.81

A glyph is committed as soon as its width is known.

The terminal used to hold each codepoint back for one round, so that a
variation selector, a ZWJ or a combining mark arriving next could join
it before anything was drawn.  That is one round of latency per
character, and it is not how the reference implementation does it:
Alacritty's `Term::input` looks the width up once and writes, and a
zero-width codepoint amends the cell the previous one landed in
(`push_zerowidth`) — no lookahead at all.

Priced before it was built, by ablation on `cat-emoji`: removing the
lookahead was worth **+22.6 %**, three times the entire remaining width
machinery (+5.1 %) and twice the scrollback row copy (+11.0 %).

An earlier ablation had reported this as NEUTRAL and that was wrong —
it only short-circuited when the buffer was empty, which after the
first character it never is, so the branch it was measuring almost
never ran.  A measurement device that fails looks exactly like data
(methodology §9); this one had no independent witness and should not
have been believed.

**It also fixes a correctness bug, which is how it was found.**  The
new cluster table (see the commit before this) showed that a cluster
split between two pty reads came apart: the end-of-feed flush wrote
the base and moved the cursor, so the codepoint that would have
extended it took a cell of its own — `a⚠️b` split after `⚠` produced
`['a', '⚠', VS16, pad]`.  The comment beside that flush claimed the
segmenter's saved state resolved it; it did not, and a pty ends its
reads wherever the kernel had a break.  With nothing held back there
is nothing to flush and nothing to lose, so the flush is gone and the
cluster survives the boundary.

The fast class still skips the segmenter — the boundary before one of
those is unconditional — but only while the cluster already open is
itself of that class.  After a ZWJ it is not: GB11 joins ZWJ to the
pictograph after it, so `👨 ZWJ 👩` reaches the segmenter even though
`👩` qualifies alone.  Getting that wrong split the family, and the
table caught it in the first build.

Widening is the other half: `⚠` is one cell and `⚠️` is two, so a
codepoint that grows its cluster claims the cell after the anchor and
pushes the cursor along.  A glyph already at the right edge has
nowhere to grow and keeps the width it was drawn at.

Measured on mini, interleaved A/B with a rebuild between runs and the
two final screens compared cell by cell first:

| scenario | before | after | |
|---|---:|---:|---|
| cat-emoji | 157.9 | 183–193 | **+16–22 %** |
| cat-cjk | 242.0 | 288–295 | **+19 %** |
| cat-mixed | 252.8 | 272.4 | +7.8 % |
| cat-ascii | 423.6 | 436.9 | +3.1 % |

Against Alacritty on identical bytes into an identical grid, screens
verified identical: `cat-emoji` 0.71x → **0.79x**, `cat-cjk` 0.96x →
**1.10x** — ahead.  Emoji is still behind; the priced item left is the
2,440-byte row copy into scrollback, which the reference avoids by
letting the grid and the history share one ring.

### 0.11.80

A pane's shell gets the user's environment, never the launcher's
session identity.

Reported as: four panes' claudecode exited without a word, and
restarting it warned `Transcript saving is off — inherited
CLAUDE_CODE_CHILD_SESSION`.

Caused by this session.  After an install that had to relaunch the app,
`install-local.sh` ran `open "$APP"` **from inside a claudecode pane**,
and `open(1)` forwards the caller's environment — verified with a
canary variable, which arrived intact in the launched app.  So L1 came
up carrying `CLAUDE_CODE_CHILD_SESSION=1`, that session's
`CLAUDE_CODE_SESSION_ID`, and its `CLAUDE_CODE_MESSAGING_SOCKET` and
token.  Only `MARSPOT_` was stripped on the way to a pane's shell, so
every pane opened afterwards inherited all of it, and agents started
in those panes believed they were children of a session that was never
theirs — pointing at one messaging socket that belonged to someone
else.

This is the 2026-07-03 scrollback incident from the other direction:
that one was marspot's own `MARSPOT_SESSION_ID` leaking DOWN into a
pane, and the fix was `env_remove_prefixes`.  The same list now names
the agent families too (`pty::SESSION_ENV_PREFIXES`), and
`install-local.sh` strips them before `open` as well, so the two do
not have to agree in order to be safe.

An agent the user starts INSIDE a pane still sets its own variables
for its own children; that happens below this boundary and is
untouched.  `crates/marspot-term/tests/pane_env_isolation.rs` drives a
real pty and pins both directions — that they are inherited by
default, and that the list stops them while an ordinary variable still
arrives.

### 0.11.79

A scroll region anchored at row 0 feeds scrollback, because that is
what the content is doing.

Asked for as: make codex's history behave like claudecode's — ordinary
page scrolling, not a special mode.

The two panes behaved completely differently in the same terminal, and
the bytes say why.  codex reserves its input box with `DECSTBM`
anchored at the top — `CSI 1;56 r`, `CSI 1;58 r`, `CSI 1;53 r`, 582 of
them in one session, with 137 `CSI S` — and `scroll_up_region`
deliberately dropped what left the region, on the reasoning that a
region is a window-internal shuffle.  claudecode uses **no scroll
region at all** (zero `DECSTBM`, zero `CSI S`), so its output went
through `scroll_up` and into scrollback like anything else.

The result was a codex pane with `scrollback_len = 0` — nothing to
scroll back through — so the wheel had to be routed into codex's own
transcript key instead, which is slow, jumps, and shows raw
uncollapsed tool output.  Every complaint about "history" was
downstream of this.

The reasoning was right for a region BELOW a header: those rows go
nowhere.  It is wrong for a region anchored at row 0, which is the
shape used to reserve rows at the BOTTOM — content leaving row 0 is
leaving the screen upward, the same event `scroll_up` records.

Measured against the reference rather than argued: the identical
sequence driven into iTerm2 (region `1..rows-8`, 120 lines scrolled
through it, read back with select-all) leaves **all 120 lines**
reachable, `LINE-001` through `LINE-120`.  So iTerm2 keeps them, and
now so does marspot.  A region below a header still keeps its old
behaviour, and there is a test for each.

### 0.11.78

SIGTERM is blocked across the execv handoff, because an install killed
five panes with it.

Found while checking an install's health, not reported: sessions 383,
390, 391, 392 and 414 were gone, L2 had failed all three reconnect
attempts to each ("pane stays on its dead stream"), and 14 L3s had
become 8.

What happened, from the log:

* **41.127** L1 promotes pending → current; **41.141** the new L2 starts.
* **~41.19** the new L2 fans SIGTERM out to every reattached L3 —
  `marspot-core` does this on the way up so stale images swap.
* **41.513–41.540** the L3s finish probing and execv, one after another.
* **~41.54** `install-local.sh` fans SIGTERM out AGAIN.  It has no idea
  L2 already did.

Between `execv` and the new image arming its handler there are about
four milliseconds with no handler installed, and the default action for
SIGTERM is to terminate.  **The five panes that died are exactly the
five that execv'd last** (pids 1738, 1601, 1574, 1661, 1963 at
.537–.540) — the only ones whose unarmed window overlapped the second
fan-out.  No log line, no crash report: the process was killed before
its new image could write anything.

The fix is not to deduplicate the fan-outs; that has to be redone the
next time someone adds a third sender.  A signal mask survives `execv`,
and a signal raised while blocked stays PENDING — so SIGTERM is blocked
before the exec and unblocked by the new image once its handler is
armed.  A second signal mid-swap becomes a delivery instead of a kill.

`crates/marspot-term/tests/sigterm_across_execv.rs` re-execs the test
binary with a SIGTERM raised at itself first, and pins both directions:
unblocked it dies by signal with the new image never reaching its exit,
blocked it comes up AND finds the signal still masked.  The assertion
is an exit code, not a printed marker — the harness captures `println!`
from inside a test, so a message would have proved nothing.

### 0.11.77

Emoji width is a table lookup again, not a binary search.

Alacritty is the reference the project's perf rules name for pure parse
throughput — same language, same platform, so a gap there is ours.
Measured head to head on identical bytes into an identical grid, with
the two final screens compared cell by cell first: on `cat-emoji`,
**both produce exactly the same screen and marspot was 1.58x slower**.
The floor for that scenario was relaxed on 2026-07-11 as "the cost of
cluster correctness"; that reason does not survive contact with an
implementation that is correct and faster.

A leaf-symbol profile put 38 % of emoji parse in the per-character
path, against 1.6 % for ASCII, which has a batch lane.  Two hypotheses
died there, both measured rather than argued: the fast path classified
each character twice (once on arrival, once after decoding it back out
of the String next time round) — carrying the width forward instead was
NEUTRAL; and ablating the one-character lookahead pipeline entirely,
the structural difference from Alacritty, was also neutral (159.6 vs
161.0 MB/s).  The ablation's first cut read an env var per character
and reported 43 MB/s, which is its own lesson about how tight this loop
is.

What was left was `has_emoji_presentation`, a binary search over 81
ranges — about seven unpredictable branches for every emoji.  The
ranges cover 1219 codepoints across two spans, so the bitmap that
replaces the search is 615 bytes, built from the same generated table
by a `const fn`: there is still one table to regenerate.  A test
compares the two answers across **all 1,114,112 codepoints**, so a
regenerated table that grows past a span boundary fails a test instead
of silently answering "no".

Interleaved A/B on mini, same binary rebuilt between runs:
`cat-emoji` 134.4 → 146.2 MB/s (**+8.8 %**), and `cat-cjk`, which
shares the path, 215.1 → 220.6 (+2.5 %).  Against Alacritty the ratio
moves 0.63x → 0.71x with the screens still identical.  Still behind;
the remaining categories are `feed` 23.8 %, `print_glyph` 15.8 %,
`scroll_up` 15.2 %, `write_glyph` 14.6 %.

### 0.11.76

A round of codex work, and the census that decided what was in it.

Rather than guess what codex needs, `examples/escape_census.rs` counts
every escape sequence in a real 22 MB session.  The tally is what
picked these three, and — as much to the point — what kept two other
ideas out.

**A color query now gets an answer.**  `OSC 10 ; ? BEL` / `OSC 11 ; ?`
is how a TUI finds out whether it is drawing on a dark terminal.
`osc_dispatch` was an empty stub that logged and returned, so the
question went unanswered — the same shape as the DA1 stall this
codebase already learned from, where a missing reply produced extra
blank rows and misaligned chrome.  The default colors moved to
`marspot_term::palette` so the emulator can reach them and the renderer
keeps reading the same two constants; there is still one copy.

**A CSI carrying an intermediate is answered before the early return,
not after.**  `csi_dispatch` returns early for any non-`?`
intermediate, which makes every arm below that point dead for those
sequences — and XTQVERSION was written below it, with a comment saying
it replied.  Measured: `CSI > 0 q` returned zero bytes.  It replies
now, and a test pins the trap rather than the one sequence.

**DECSCUSR is recognised.**  `CSI <n> SP q`, 119,823 of them in one
session, all landing in "not implemented".  The shape is recorded and
deliberately not drawn: all 119,812 of codex's own calls ask for shape
0, which means "this terminal's default", so teaching the renderer bar
and underline cursors would change nothing for the program that sends
it most.  Building that would have been inventing a gap the data does
not show.

**Not done, on purpose.**  OSC 0 / OSC 2 arrive 47,738 times and are
now stored (`Terminal::osc_title`) but nothing displays them: a pane's
label in marspot is DERIVED from its directory, deliberately, so that
nothing can disagree about which pane is which.  Putting a
program-supplied title into that chain is a product decision, not a
protocol one.  Focus reporting (`?1004`) and theme-change notification
(`?2031`) are accepted and no-op'd; both are real gaps, both need an
L2→L3 event that does not exist yet.

**Also measured, and not attacked.**  The real codex stream parses at
185 MB/s (versus 406 for synthetic `cat-ascii` — escapes cost about
2.2x per byte).  codex peaks around 100 KB/s, so parsing its entire
22 MB session takes 119 ms.  Throughput is not codex's problem, and
the numbers do not support pretending otherwise.

### 0.11.75

A batch is cut where the program said its screen was coherent.

Reported as: in codex, a `Working (…)` line appears merged into text
that was already there — `● Working` followed by the tail of a
different row, unerased.  iTerm2 does not do it.

codex draws with DEC 2026: open a synchronized update, erase the parts
that changed, write the new text, close.  Between "erase" and "write"
the screen is meaningless, and the mode is how a program says so.  The
gate that was supposed to honour that asked the terminal "are we
mid-update?" AFTER feeding a whole PTY batch — and codex closes and
re-opens the mode within a few bytes (measured p50 gap: 8 bytes, 87.7 %
of the stream inside an update), so the coherent moments live in the
middle of a batch where that question cannot see them.  The answer was
almost always "yes", the 150 ms cap fired, and what went out was a
repaint caught halfway.

Measured on the user's own 22 MB codex bytelog, replayed at five batch
sizes: **35-46 % of published frames were mid-update**.  After the fix,
0.0 % at every size — and MORE frames are published, because none are
withheld any more.

The fix is where the cut is made, not in the gate: `pump` feeds up to
and including the last close in the batch and carries the rest to the
next one, so what reaches the grid is always a screen the program
declared finished.  A pane that never uses DEC 2026 — a shell, vim —
takes an unchanged path behind one sticky bool, and an update that
never closes is flushed anyway after 150 ms or 1 MB, because a frozen
pane is worse than a partial frame.

Two things this ruled out on the way, both by replaying the real
bytelog rather than reasoning: marspot's emulation is faithful (a full
replay reproduces the correct screen), and the stray line under codex's
`… +3 lines` marker is in codex's own output, not something marspot
failed to erase.

### 0.11.74

A guess nobody answers now comes back off the screen on its own.

Local echo paints a keystroke ahead of the PTY and waits for the byte
to come back: matching confirms it, anything else rolls it back.  Both
verdicts need a byte, and a program can send none.  `sudo` is exactly
that shape — echo off, and not one byte until Enter — so every
character of a password stayed painted where it was typed.  Found while
verifying 0.11.73 against a `stty -echo; cat > /dev/null` stand-in: ten
keystrokes, ten strays, no confirmation and no rollback.

A prediction now carries the moment it was made and expires unanswered.
Expiry deliberately does not decide anything: the byte moves to a
shadow queue, and what arrives next settles it.  Arriving late and
matching means the program does echo, just slower than we waited —
that is not a miss, and it widens the window rather than shutting
prediction off.  Anything else means the echo was never coming, which
is the miss 0.11.73 counts, so three characters into a password prompt
the pane stops guessing entirely.

The window is measured, not assumed: an EWMA of observed round trips,
floored at 50 ms and capped at 1 s.  Against a real pty (2026-09-07) a
zsh echo takes p50 0.16 ms / max 1.12 ms idle, and 0.16 ms / 8.18 ms
with the shell flooding the pipe at the same time — the floor is six
times the worst of that.  The cap is what a slow ssh link is allowed to
grow the window to, and also what bounds how long an unechoed keystroke
can linger.

Nothing waits on a byte that may never come: L3 wakes on a 10 ms tick
while a guess is outstanding and sleeps its full second otherwise, and
mcli arms exactly one wake-up per predicted keystroke.  Idle cost is
unchanged.

Two tests drive a real pty: the silent program leaves nothing on
screen, and an echoing one keeps its local echo with nothing taken
back.  The first version of the second test was green and proved
nothing — `stty -echo` alone leaves ICANON on, so `cat` echoed nothing
until Enter and the "echoing" stand-in was not echoing.

### 0.11.73

Prediction reads this pane's own tally instead of guessing what is
running.

Local echo was on for everything that was not the alt screen, which is
right for a shell and wrong for codex: codex draws its own input line
somewhere other than the cursor, so every predicted character landed in
the wrong place and was wiped by the next repaint — the input the user
watched get overwritten.

The first fix proposed was a termios gate (`ICANON|ECHO`), straight
from the code's own comment.  Measured and refuted: a zsh prompt is
`-icanon -echo` exactly like codex, because ZLE draws its own line too.
The difference is only WHERE, and termios cannot see it.

So the terminal stopped guessing the kind of program and started
reading what happened to its last few guesses.  Three misses in a row
turns prediction off for that pane; one keystroke in 128 is let through
afterwards to find out whether the program changed underneath — the
pane a program was quit in becomes a shell again, and nothing else
announces that.

Measured on the real thing: at a zsh prompt 16 keystrokes gave 15 hits
and 1 miss; inside codex, 23 keystrokes gave 0 hits and 23 misses.

### 0.11.72

The prediction tally is written down.

The hit and miss counters existed and were never reported, so "does
local echo help in this pane" had no answer.  `L3_PREDICT_TALLY` now
logs the 60 s deltas.  It is what produced the 15/1 and 0/23 numbers
above; before it, the codex behaviour was a report and a guess.

### 0.11.71

The image probe is bounded, and a wedged one can be retried.

A pane could sit on an old image forever, silently, from two holes at
once.  `can_start` waited on `.status()` with no deadline — and the
caller reads "probe outstanding" as "this signal is already handled",
so one probe that never answers does not delay a check, it retires that
pane's self-update for good.  And the flag was cleared only when a
probe FAILED, so a probe that never returned left it set.

The probe now gives up after 20 s and says so; the flag became a
timestamp, and a probe with no answer after 45 s may be started again
(`l3.execv.probe_stuck`).

The first test written for this was green and proved nothing:
`/bin/sleep --version` rejects the argument and exits at once, taking
the answered-non-zero path.  It now runs a script that genuinely hangs,
against an injected deadline — 0.30 s instead of 0.03 s, through the
branch that matters.

### 0.11.70

`PaneResetAttrs` — put a pane's pen back to plain.

A style belongs to the program that set it, and normally that program
or the shell's next prompt turns it off.  A full-screen program can run
for hours emitting no `CSI 0 m` at all, so a style switched on by
anything ELSE rides every new cell until the program exits.  Telling
someone to quit their session is not a fix.

### 0.11.69

A style left on by a dead program does not reach the new shell.

`reset_process_owned_modes` already clears mouse reporting, bracketed
paste and application cursor keys on the grounds that the program which
set them is gone.  SGR attributes are the same debt and worse in one
way: a mode usually meets a prompt that turns it off, a style can ride
every new cell for hours.

### 0.11.68

The probe says why it failed.

It compressed "the binary answered non-zero" and "the system refused to
spawn it" into one `false`, and only the first means the image is bad.
Every L3 refused a perfectly good image for half an hour and the log
said `could not start`.

### 0.11.67

The markup declaration logs the change, not the heartbeat.

### 0.11.66

`<u>…</u>` is drawn only where a plugin says the program prints markup
it does not render (`MsgType::PaneRenderMarkup`).

As a global setting this ate `<u>` out of any conversation about
markup — which is what a terminal is often for.  `appearance
.render_u_tags` remains as a manual override and defaults off.

### 0.11.65

`<u>` rendering defaults off.

The cost was larger than "someone cats an HTML file": switched on
globally it ate the tags out of the conversation specifying the
feature, including the user's own words coming back on screen.

### 0.11.64

Only a MATCHED `<u>…</u>` styles anything.

Turning underline on at the opening tag underlines everything after an
unmatched one — and text that merely mentions the tag is most of any
conversation about this feature, so within minutes a whole pane came
back underlined.

The span is withheld until `</u>` arrives.  If it never does — a line
feed, an escape sequence, or more than 1 KiB — the opening tag and
everything after it are printed exactly as they came, which is the
behaviour from before the feature existed.  A matched pair occupies no
cells.

### 0.11.63

`<u>…</u>` drawn as underline (`appearance.render_u_tags`).

Not a terminal convention — a concession to what the models on the
other end emit.  Recognised on the character path, not over the byte
stream: a byte pass cannot tell text from the inside of an escape
sequence, and `CSI < u` carries the same characters.

### 0.11.62

DEC 1007 (alternate scroll) is honoured and published to L2.

The mode is the program stating that on this screen the wheel is the
arrow keys.  Carried across an execv, because a program says it once on
entering a view it is still in — and a pane whose plugin enters with a
TOGGLE would otherwise press that toggle on an open view and shut it.
Cleared on leaving the alternate screen and on process handover.

### 0.11.61

A completely blank screen waits 50 ms before it is shown.

This is the black flash on opening codex's transcript, and it was NOT
what synchronized output fixed.  Reading the bytes shows why:

    CSI ? 2026 l   ← the previous frame's batch closes
    CSI J          ← the screen is wiped, outside any batch
    CSI ? 2026 h   ← only now does the transcript's batch open

codex wipes the screen OUTSIDE the batch it uses to protect the
repaint.  We publish once per PTY read, the wipe and the paint arrive
in separate reads, and the empty grid between them reached the display.
iTerm2 does not flash because it presents on a display cadence, so a
wipe and the repaint a millisecond later land in the same shown frame.

A COMPLETELY blank screen is almost always in transit — a repaint under
way, or a `clear` about to be followed by a prompt — so it waits.  If
content arrives it is published instead and the blank frame is never
seen; if the screen really is meant to be empty it goes out 50 ms
later, which nobody can perceive.  The test is "not one printable
cell", which no screen with content passes.

Measured on a live codex, dense screen, opening the transcript:

    before   38% → 0% → 44%    one frame fully blank
    after    38% → 44% → 44%   emptiest frame 38.1%, none under 10%

and the same on the way back out.  A withheld frame is *owed* until
something is published, so a program that wipes the screen and then
goes quiet cannot leave the pane showing what it wiped; the debt clears
on discharge, so a quiet pane still sleeps (idle CPU measured 0.0% for
core and session).

### 0.11.60

DEC mode 2026 — synchronized output — is honoured.

A program brackets a repaint with `CSI ? 2026 h` … `l` to say "do not
show anyone what is on the way".  It was on the explicit accept-and-
ignore list.  codex uses it for every frame — 8,493 pairs in one
session's byte log — and so do most modern TUIs.

Frames are now withheld between the two, under a 150 ms cap: the
terminal cannot make a program close what it opened, and a torn frame
is a blemish while a frozen pane is a bug.  A dangling update is also
cleared on process handover, so a program that dies mid-repaint cannot
hold the next one's first frame hostage.

Measured honestly: on a short repaint this changes nothing observable
(one PTY read already carried the whole frame, and `publish_if_changed`
already collapsed it into a single publish — 3 published screens either
way).  It earns its place on frames that span several reads, which is
where a tall pane on a loaded machine lives.  It is NOT the cause of
the black flash reported when opening codex's transcript; that remains
open.

### 0.11.59

Publishes whether the session is in the alternate screen.

L2 could not tell a redraw-in-place TUI from a fresh shell: both
report an empty scrollback, and the wheel has to do opposite things
in the two cases.  `FLAG_ALT_SCREEN` is additive — an older L2 masks
it off, an older L3 never sets it and reads as "not alt", which is
the pre-existing path.

### 0.11.58

The by-name shm branch is removed with RFC-007.

It existed so a `launchd`-started L3 could attach without an inherited
fd.  With that path reverted it has no caller, and an unreachable
second way to acquire the framebuffer is worth less than the clarity
of having one.

### 0.11.57

The grid region can be taken by name, not only by inherited fd.

A `launchd` job (RFC-007) inherits no descriptors, so `setup_shm`
gains a `MARSPOT_SHM_NAME` branch over `grid_shm::open_region`.  The
inherited-fd branch stays first and unchanged, so `mcli`, the tests and
any L2 that still passes an fd are on exactly the path they were on.

### 0.11.56

Publishes when a mode changes without any PTY byte arriving.

The publish at the bottom of the loop keys off "did the PTY do
something" — bytes pumped, a local echo painted, a resize, a scroll.
Mouse reporting is the one mode that stops because L1 said so rather
than because the program said so, so 0.11.55 reset it correctly and
L2 never found out: the flag it renders from stayed stale until the
next byte happened to arrive, which on a pane whose program was just
killed can be never.

Found by the end-to-end test rather than by any of the unit tests,
which is the point of having one — the reset was visible in L3's own
log the whole time.

### 0.11.55

Understands `PaneResetMouseReporting`: L1 took this pane's foreground
program down, so stop reporting mouse tracking as on.

`Terminal::reset_mouse_reporting` is deliberately narrower than the
`reset_process_owned_modes` used by cold resurrection.  That one runs
against a brand-new shell where nothing on screen set anything; here
the shell is the same shell it always was — it owns its own bracketed
paste and application cursor keys and re-asserts them per prompt, so
clearing those would break a paste already in flight for no gain.
Mouse reporting is the one mode a shell never sets and a dead TUI
never clears.

Logged (`L3_MOUSE_REPORTING_RESET`) only when it changed something:
most terminated programs never asked for mouse reports at all.

### 0.11.54

**孤立的可打印字符不再过状态机。**

emoji 散文长这样:`🚀 ✨ 🎉` —— 每个字形之间**恰好一个空格**。于是 ASCII 批量车道
(要 ≥2 个字符)永远接不住那些空格,每一个都掉回逐字节状态机;而 emoji 语料里空格的
数量与 emoji 一样多。profile 里 `Terminal::feed` 自身占 emoji 解析的 **29.9%**,是
最大的一项。

等价性是可论证的,不是赌的:这段代码只在 `parser.in_ground_plain() &&
predictions.is_empty()` 时才运行,而在 Ground 状态下,一个 0x20..=0x7E 的字节除了
派发到 `print` 不做别的。所以直接调 `print` 是同样的工作**减去**那次派发。

实测(mini,headless parse,min-of-3):

| 语料 | 之前 | 之后 |
|---|---:|---:|
| ascii | ~423 | **432.6** |
| cjk | ~236 | **245.9** |
| emoji | ~154.6 | **158.6** |

三条语料一起涨,因为「孤立可打印字符」不是 emoji 独有的形状 —— 任何标点、空格、单
字符提示符都走这里。

### 0.11.53

**RAM ring 不再为写入补齐整行。**

ring 存的是定宽行,但行本身不是:122 列的网格显示 emoji 散文时只填约 47 列,剩下的
75 列在每次推行时被 memset 成空白 —— **在解析线程上**,为一个「本来就该很少被读」的
前排缓存(miss 只是回落到文件)。

改成记录每个槽位的实际长度(`ram_lens`),写入只拷有内容的部分;读取端按长度截断,越界
返回空白而不是上一任占用者的残留。工作量从写侧挪到读侧,而读侧付得起。

顺带用消融确认了 trim **不能**去掉:把它关掉(整行照写)后跑,磁盘写量翻倍,一趟直接
跑到 10 分钟超时。trim 省的不只是磁盘,是整条链的量;该攻的是让它更快,不是让它消失。

mini(load 2.12,三次读数几乎相同):cjk 165.7 → **172.1**、emoji 123.1 → **125.4**
MB/s。emoji 对 ghostty 的余量从 +6.4% 到 **+8.4%** —— 仍是四条里最薄的一条。

### 0.11.52

**磁盘让开之后,轮到解析线程自己的两处浪费。**

`async_writer` 把写盘挪走后,profile 里最大的一项变成 `FileScrollback::push_line`
本身(解析线程 32%)。两处,都是「写法」而非「算法」:

- **每行一次 `Vec` 分配。** `push_into_ring` 走 `pad_or_clip`,那个函数返回一个新
  `Vec` —— 于是每推一行就是一次 malloc 加一次多余的整行拷贝,再 `copy_from_slice`
  进 ring。CLAUDE.md 对热路径的要求是**零分配**,而一次 bulk `cat` 从这里推过去
  二十多万行。改成直接写进 ring 槽位(`copy_from_slice` + `fill`),中间那个 `Vec`
  消失。
- **每个 cell 两次 `extend_from_slice`。** 记录编码原本是逐 cell 追加 4+4 字节,
  每次都要查容量、改长度;一行 122 列就是 244 次。改成先 `resize` 到记录大小,再在
  切片上 `chunks_exact_mut` 平铺填充。

定价(本机,同一批测法):

| | cjk | emoji | ascii |
|---|---:|---:|---:|
| async_writer 之后 | 108.2 | 93.2 | 125.4 |
| + 扁平编码 | 115.7 | 95.9 | — |
| + ring 零分配 | **129.1** | **108.2** | **156.1** |

### 0.11.51

**磁盘不再占着解析线程 —— 新增 `async_writer`,bytelog 与 scrollback 都改异步。**

每个字节要落两次盘:原样进 bytelog(为静默更新后重放),编码后进 scrollback(为历史
活过 RAM ring)。两者都是产品特性,都不能删。**能改的是谁在等磁盘** —— 此前是解析
线程,bulk `cat` 下它 46% 的样本在 `write`。

`async_writer::AsyncWriter`:

- **双缓冲,不是小写入队列**。生产者填一个 `Vec`,满了整个交给写线程,自己从空闲表
  取一个新的。缓冲在两个线程间循环,稳态零分配。
- **有界,所以不会变成内存泄漏**。通道容量固定;磁盘落后时生产者阻塞在 `send`,背压
  沿着 pty 队列传回子进程 —— 写得比磁盘快的 pane 会变慢,但不会变胖(CLAUDE.md 的
  「不能越跑越慢」)。
- **`flush()` 是屏障不是提示**。要回读文件的调用方(冷读 scrollback、execv 前的
  handoff)必须看见字节,所以 flush 等写线程确认。
- `swap_file` / `truncate` 也走队列,所以轮换与清空不会插到已排队的写前面。

定价(本机,cjk,同一批测法):83.9 → 98.7(bytelog 异步)→ 108.2(scrollback 也异步)。

**契约变化,已在测试里钉住**:写入在 flush 之前对读者不可见。`bytelog` 的两个测试
原本在 flush 前统计文件大小、flush 后读内容再断言两者相等 —— 同步写时恒成立,异步
下就是时序假设错了。`file_torn_write_no_past_eof_orphans` 则暴露另一件事:
`mem::forget` **不会**终止写线程(真正的 `execv` 才会),所以它撞的是竞态而非缓冲
丢失;测试等写线程排空后再 reopen,并把这层区别写进注释 —— `flush_for_handoff` 正是
为真 execv 那条路存在的。

### 0.11.50

**同 L2 0.12.143:scrollback 索引改缓冲,产品路径 2.1-2.8×。**

引擎共用,但这条收益整个落在 L3 —— 文件 scrollback 只在有 `MARSPOT_SESSION_ID` 时
启用,也就是只在真实 pane 里。mcli 与单测走的是内存变体,**看不见这个瓶颈**,这也是
它藏到今天的原因。

### 0.11.49

**程序问终端「你是谁」「光标在哪」,在出厂架构下从来没人回答。**

`marspot_term::session::Session::pump` 一直会把 parser 排队的能力查询回复
(DA1 `CSI c`、DA2、XTQVERSION、以及刚补的 DSR)写回 PTY。**L3 的
`LocalSession::pump` 从来没做过这件事。** 而 L3 自 2026-06-13 起就是默认架构 ——
它自己持有 PTY,这里不发就等于没发。

于是形成一个最难发现的组合:mcli、单元测试、bench harness 走的都是 in-process 那条
路(会回答),而**用户的每一个 pane 走的是不会回答的那条**。终端里的注释早写了代价:
「apps that stall waiting for a DA response fall back to degraded rendering paths
(extra blank rows, misaligned chrome)」—— 不是少个功能,是**卡住**:程序问完就等,
等到自己超时,然后退化渲染。

修法是 `pump` 末尾把回复交给 PTY,并且**不放在 `total > 0` 的条件里** —— 回复可能
来自一次没有新字节的 feed(held grid 释放就是),而空回复的写入是免费的。释放 hold
的那条路径同样补上。

怎么发现的:重建 `bin/measure-l3.sh` 的探针时,让 session 在 cat 完语料后回一个 DSR
自证「我确实消费完了」,结果**它永远不回**。装置照出了产品的洞。

### 0.11.48

**读 pty 的线程,85% 的时间花在 poll 上,不是在搬字节。**

macOS 的内核 tty 输出队列**每次 read 最多给主端约 1 KiB**,无论你递多大的缓冲区
(实测 1009.2 B/read,209066 次 read 的平均)。于是一条 bulk 输出流就是几千次 read,
中间夹着微秒级的补货间隙。原来的写法在第一次空探测(`poll(0)` 返回 0)时就回到阻塞
`poll` 去睡,等于为每个间隙付一次睡眠+唤醒。采样直接把这件事摆出来:

    reader 线程   poll 84.8%   read 15.2%
    reader 线程   poll 81.0%   read 14.3%

改成:流已经攒够一整个内核队列(≥1 KiB,说明是 bulk 而非交互 trickle)时,空探测最多
重试 16 次再去睡。交互输入按定义够不到这个门槛,一次也不 spin。

实测(mini,交错 5 轮取最小):**gather 批次 4362 → 677(6.4×)**,吞吐 cjk +4.0% /
emoji +5.6% / ascii +6.3%。

诚实说一句:ghostty 在同一处的注释写着这招"几乎让饱和排空速率翻倍",我们只拿到
+5%。**睡眠次数确实降了 6.4 倍,吞吐却没跟着动** —— 说明在我们这条路径上,睡眠不是
主成本(见 L2 0.12.139 的分段:那 5.7 ms/MB 绝大部分是内核与 `cat` 的共通开销)。
+5% 留下,是因为它是真的且零风险;但它不是 cjk/emoji 那 24-43% 的答案。

`MARSPOT_PTY_BRIDGE_SPIN` 可调(0 = 旧行为),用来复测这个取舍。

### 0.11.47

**补上 DSR / CPR(`CSI 5 n` / `CSI 6 n`)—— 见 L2 0.12.138。**

引擎在 L2/L3 共用;L3 是每个 pane 里跑 PTY 的那一侧,所以「程序问光标在哪」这件事
实际发生在这里。理由与实现记在 L2 0.12.138。

### 0.11.46

**parser 的 emoji 路径 2.17× —— 引擎在 L2/L3 共用,见 L2 0.12.137。**

改动全在 `marspot-term`(parser 车道分派 + fast class + `char_width`),L3 是
它在每个 pane 里的宿主,所以同一份收益直接落在 PTY → grid 这条线上。完整的发现
过程、五项定价表与安全性论证记在 L2 0.12.137,不在此重复。

### 0.11.45

**设置改动在 L3 生效的延迟:最多 5 秒 → 最多 1 秒。**

量了一遍三层各自的传播:L1 每次事件循环都 `stat`(实际即时),L2 自己写自己
读(即时),只有 L3 把这次 `stat` 搭在 5 秒的可达性检查上 —— 而 L3 拥有的正是
「圈圈数字占两格」那条(字符宽度)。**一个设置是靠「翻完立刻用」来被感知
的**,5 秒是能看出来的。

给它自己的 1 秒节拍,并放在**事件处理之前** —— 于是新到的字节就是用新规则解析
的。每个 session 每秒一次 `stat`,在它旁边那个解析循环面前不值一提。

**没有造 wire 帧,是有意的**:文件是唯一真相(它也是手工可编辑的),而一条
携带数值的帧就是第二条可能与文件不一致的路径。这个代码库这两天所有的单位
bug,都是「两条路回答同一个问题」。

### 0.11.44

随 `marspot-term`:新增 `WindowChrome` 消息(chrome 几何自己的轻量帧)。

### 0.11.43

随 `marspot-term`:`SurfaceAttachWindow` 追加红绿灯边界字段 + `layout` 签名变化。

### 0.11.42

设置结构多两个浮点字段(`dim_scale` / `scroll_factor`)+ 浮点键的解析与
回写。L3 自己不读这两项 —— 但 `marspot-term` 是三层共用的那一份,所以它
的二进制跟着变。

### 0.11.41

`char_width` 读设置改用**原子镜像**,不是 `get()`。

第一版直接在 `char_width` 里 `settings::get()` —— 一次 RwLock 读加一次
`Arc` 引用计数,**每个解析到的字符一次**。mini 上 `cat-cjk` 195.2 /
`cat-emoji` 58.8,双双跌破地板。改成一个 relaxed 原子(设置变更时重新发布),
同一台机器空闲复测 231.0 / 72.4,**GATE 11/11**。

"便宜"是相对的:一把锁每帧一次没问题,每字节一次就是回归。

### 0.11.40

字符宽度跟着 `appearance.circled_wide` 走,**L3 自己重读**。

设置文件搬进了 marspot-term(石头层),三层共读一份、只有一处解析它。L3
在它本来就有的 5 秒可达性检查上顺带 `stat` 一次 —— 新解析的内容按新规则,
已经在屏幕上的格子保持它被排版时的宽度,随程序重画自然痊愈。

env 覆盖仍然读一次就定死:它说的是「这个进程、这一次运行」,不会在底下变。
设置是活的,因为面板有权在终端开着的时候改它。

### 0.11.39

**回退 0.11.38。** 圈圈数字改回 1 格。

预测是「错位只落在含 ① 的那几行」—— 有边界,烦但可忍。真机上不是这样:
宽度分歧会移动**换行点**,于是**只要滚动经过一个圈圈数字**,整段回来时就有
字被落在左边的缝里,输入框自己画在自己身上。终端丢掉换行不是「拿一点对齐
换一个好看的字形」,是坏了。

`Bun.stringWidth('①')` 是 1 —— 跟画屏幕的那个程序保持一致,比任何字形大小
都值钱。孤立的那个交给溢出(core 0.12.95)去救,零分歧;连着的那几个继续
小,那是一块正确的屏幕的价钱。

`MARSPOT_AMBIGUOUS_WIDE=circled` 保留为显式开关。

### 0.11.38

**圈圈数字默认占 2 格 —— 连着写也一样大。**

溢出(core 0.12.95)治好了**孤立**的那个,治不了**连着**的:`①②③` 里每个
邻居本身就是字形,没有空白可借,于是三个大小不一,取决于后面跟着什么。而
N 个方形字形塞进 N 个 1 格的槽里想要统一的大小,是做不到的 —— 会互相压
4.5 px。**两格是唯一的路,而那按定义就是一次分歧。**

分歧的代价这次是量出来的,不是听说的:

```
Bun.stringWidth('①')  →  1        (claudecode 是 Bun 打的包,用的就是它)
Bun.stringWidth('中')  →  2
```

所以确实会错位。值得赌的理由是**赌注很小**:错位只落在**含 ① 的那几行**,
而 `°` `±` `→` `★` 整张表继续保持窄、继续跟所有人一致 —— 它们由溢出那条
路照顾,不需要任何分歧。

`MARSPOT_AMBIGUOUS_WIDE=0` 完全退回原样。

### 0.11.37

`is_ambiguous_width` 从 `char_width` 里提出来公开。

它命名的正是**会被压小的那批**:我们按 1 格渲染、而拥有字形的那个字体按
2 格 em 设计。渲染侧用它决定谁可以溢出到右边的空白格(见 core 0.12.95)。

### 0.11.36

**圈圈数字为什么那么小 —— 量清楚了,以及唯一的那个杠杆。**

```
char_width('①')            = 1 格      ← MARSPOT_AMBIGUOUS_WIDE 默认关
解析到的字体                 PingFang SC,ink 11.71 × 11.71
格子                        7.20 × 16.00
rasterise_glyph             11.71 > 7.20 → oversized → 缩到 61%
```

**换字体救不了。** 圈圈数字是方的,缩放的约束边永远是格子宽,所以在 1 格里
它最多 ~7.2 px;这台机器上最窄的 `①`(STIXGeneral,8.21)落到屏幕上还是
那个 ~7.2 px。而汉字占 2 格 = 14.4 px。**要跟 CJK 同体量就必须占 2 格,
没有别的杠杆。**

而 2 格正是默认关掉的东西:别的 wcwidth(zsh / less / claudecode 的
`string-width` / Python `wcwidth`)都把歧义宽度当窄的,marspot 认宽就会
累积 CUP 偏移,画面几次编辑之后就对不上了。

所以开关多一档中间值,只放宽**这一族**:

```
MARSPOT_AMBIGUOUS_WIDE=0        (默认) 全窄,什么都不变
MARSPOT_AMBIGUOUS_WIDE=circled  只有 ①②③ ❶❷❸ ⓪ … 变宽
MARSPOT_AMBIGUOUS_WIDE=1        整张歧义表变宽
```

`circled` 的错位面只剩「含 ① 的那几行」,比每个 `°` `→` `★` 都参与要小得
多。这一族的范围也比歧义表自己那一段(`0x2460..=0x24E9`)宽:`⓪` 和到
U+24FF 的尾巴是同一家、同一个抱怨,`❶..➓` 也是同样的字面度量。

### 0.11.35

hold 的上限从 512 KB 提到 4 MB,并在触顶时留一行 WARN。

上限是为了不让缓冲无限长(CLAUDE.md §3),不是为了替一次重画拿主意 ——
而这两件事的量差着两个数量级:**停放**中的 pane 是一个没人敲的提示符,
几百字节;**唤醒**中的 pane 是一个程序把整个会话从头画一遍,而那整段都
得压住,才能保证用户只看见「旧帧 → 画完的帧」这一次跳变。触顶会在半程
放开冻结 —— 正是冻结本身要避免的那一下闪,所以现在它会说出来。

### 0.11.34

**grid hold** —— 收到 `PaneHoldGrid` 后,PTY 照读、字节照记 bytelog,但不喂
给终端解析器,所以这个 pane 的画面停在原处;释放时把攒下的字节一次喂完。

为什么在这一层:回收一个 idle 的 claude 时要让画面停在它最后一帧,而 L2 的
冻结活不过静默更新(每次更新都重启 core)。L3 活得比 core 长。

两条自保:攒够 512 KB 自动放弃 hold(pane 里显然有活物,`CLAUDE.md` §3 要求
每个队列有界);以及**用户按键一旦到达 L3 就立刻释放** —— 键能走到这里说明
上游没人锁着键盘,也就没人会来解冻,冻着不如让他看见真相。

### 0.11.33

日志目录被整个删掉之后能自愈。

今天 09:34 UTC 我去翻 hibernate 日志,`~/Library/Logs/Marspot` **整个不
见了** —— 而 marspot 还在跑,`lsof` 显示它的 fd 仍指着那个已经不存在的
路径,4.8MB,还在往一个 unlink 掉的 inode 里写。也就是说:从那一刻起,
这个进程剩下的日志谁也读不到。

不是我们删的:`~/Library/Logs/LemonMonitor.log` 里 18:34(=09:34 UTC)
有一次 `cleanResultDidEnd: totalSize: 36003690256` —— 腾讯柠檬清理跑了
一遍,清掉约 36GB,顺手带走了 app 的日志目录。这类清理工具在这台机器上
是常驻的,所以这不是一次意外,是环境。

我们这边的缺口是真的:`rotate::check_and_maybe_rotate` 早就处理了「文件
被 unlink」(stat 失败就 `reopen`),但 `reopen` 只是重新 `open(path)` ——
父目录没了的话它必然失败,而这个错误被吞掉,于是继续写老 fd,永远。

改一行:`reopen` 先 `create_dir_all(dir)`。对一个要跑几周的终端来说,
「日志从某一刻起永久静默」跟「没有日志」是一回事。

回归测试就照现场复现:开 sink、写一行、**把整个目录删掉**、再写够一个
stat 周期,断言目录和文件都回来了、而且新写的行在里面。

### 0.11.32

PTY teardown 不再被一个挂起的作业卡死。

现场是测试里撞出来的:pane 里 `^Z` 一个作业再 drop `Pty`,`waitpid` 在
`SIGKILL` 之后**永不返回**,子进程停在内核的 `E`(exiting)态,测试挂了
13 分钟直到 runner 把它杀掉。真机上这条路是**关 pane**,也就是整个 app
卡住。

根因顺序问题:master fd 关在最后。只要它还开着而且没人读,子进程拆自己
的控制终端就可能永远拆不完。

三处改动,都是量出来的:

1. **先关 master**。最后一个 reader 一走,slave 侧拿到 EOF,退出流程才
   走得完。
2. **SIGCONT 再 SIGHUP**。停止的进程不会跑信号处理器 —— shell 得先活过来
   才谈得上冲 history、给自己的作业发 HUP。
3. **每一步等待都有界**(polite 100ms,SIGKILL 之后 500ms,都是 WNOHANG
   轮询)。留一个僵尸片刻是有界的局部代价;把调用方永远堵住不是 —— 而这
   条路跑在关 pane / L3 teardown 上。

回归测试断言的是**时间上界**(3s),因为"最终会好"从来不是问题所在:
真 zsh + `sleep 300` + `^Z` + drop,实测 6.4ms。

### 0.11.31

只重编:`marspot-term::layout` 的 chrome 几何变了(见 L2 0.12.68)。
session 侧行为不变 —— 它用 layout 里的 grid 计算,不碰 chrome rect。
bump 的理由同 L1 0.7.33。

### 0.11.30

可达性哨兵查完整契约,不只 entry.toml。

2026-07-28 加的 deadman 只问一件事:`entry.toml` 还在吗。但成为"谁都
找不到我"有三条路,它只堵了一条:

1. `entry.toml` 被删 —— 原有检查
2. **socket 被 unlink** —— L2 的 fresh-spawn 路径会清槽,若那次 spawn
   没落地(或 core 先死了),现役 L3 就抱着一个没人能拨的 socket
3. **entry 改名到了别的 pid** —— 一个真正落地的替代 L3 会用自己的
   pid 重写 entry.toml,从那一刻起本进程才是过期的那个

三者是同一个条件,现在一起查。读 entry 失败**不**算被驱逐 —— 正在被
重写的 entry 撕裂读不该看起来像换主。

### 0.11.28

BCE 只继承背景色 —— 擦除/滚动填充的空白此前 stamp 整套 SGR(含 underline):\e[4m 的 URL 在底行折行触发滚动时,新行空白"出生即带下划线",行尾拖出一条横线直到窗缘(omz update 横线穿版现场);bold/italic/underline/reverse/dim 是字形属性,空白无字形.

### 0.11.27

共享 crate(shell_proto drag/up/open-at 尾巴)重链接.

### 0.11.26

共享 crate(shell_proto encode/decode_mouse_up)重链接.

### 0.11.25

共享 crate(shell_proto WindowCloseRequest=65)重链接.

### 0.11.24

CSI E/F(CNL/CPL)—— brew 并发下载进度用 \\033[nF 上移重绘,缺 F 分支时序列被静默丢弃、光标不动,每轮刷新往下追加 = "brew 进度无限刷屏"现场报告;负向验过(去掉分支测试重现追加).

### 0.11.23

2026-07-28 事故防线:registry 哨兵 —— L3 每 5 秒 stat 自己的 entry.toml,条目消失即自杀.L3 设计上比 L1/L2 长寿(silent update / 崩溃重连的根基),entry 是它唯一的所有权记录;entry 没了 = 谁都永远找不到它 = 当晚 176 个永生孤儿的由来.

### 0.11.22

共享 crate(shell_proto WindowOpenRequest=64)重链接.

### 0.11.21

复活的会话不再把旧进程的终端模式套在新 shell 上 —— 鼠标追踪残留会让滚轮变成 `CSI < 64;x;y M` 打进 zsh 提示符("command not found: 29M64").

### 0.11.20

共享 crate(shell_proto window_id)重链接.

### 0.11.19

主循环看门狗 + 共享 crate 重链接.

### 0.11.18

共享 crate(StallReport::summary)重链接.

### 0.11.17

共享 crate(握手超时受 deadline 约束)重链接.

### 0.11.16

PtyWriter 退化成 BoundedWriter 特化 + SnapshotWriter 独立成模块.

### 0.11.15

快照写盘不再白拷一份 body 只为打日志;队列容量改引用共享 cap.

### 0.11.14

共享 crate(uds_session_client 单一 deadline)重链接.

### 0.11.13

周期快照的写盘搬出主循环 — 现场实测 115KB 快照在磁盘忙时写了 2.87 秒,整条循环冻住.

### 0.11.12

FrameWriter 提到共享 crate(ControlWriter 改为薄封装)+ .next_id 非阻塞 flock 重链接.

### 0.11.11

控制 socket 写搬出主循环(实测 macOS AF_UNIX SO_SNDBUF 仅 8192,L2 一停读 L3 主循环即钉死)+ 主循环停顿探测器 l3.loop.stall.

### 0.11.10

bytelog 轮转取代拷贝压缩(共享 crate 重链接)— 越过 100MB 上限时不再在主循环上拷 50MB,现场实测冻住 pane 10 秒以上;改为一次 rename.

### 0.11.9

PTY 写路径搬出主循环 — raw 模式子进程不读 stdin 时 write(2) 阻塞整条 L3 主循环(单 pane 卡输入数分钟后自愈的根因);改为专用 writer 线程 + 256KB 有界队列 + 短写补齐.

### 0.11.8

共享 crate(layout)重链接.

### 0.11.7

linkify 孵化重链接.

### 0.11.3

v5 live 验证轮 — execv 场景 alt 屏原样恢复(fold 语义误用导致输入框消失/重绘慢的现场回归修复)+ execv 恢复后 SIGWINCH 前台进程组兜底.

### 0.11.1

C.1 快照升 INFO — session 目录 flock(A.3)、周期快照 30s 防抖(C.1,硬宕机窗口有界)、快照 v4 alt 折叠(B13)+ B14 tail 序反修复、scrollback 损坏隔离重建(A.4)、.next_id 自愈(A.1)、Drop 去毁灭化.

### 0.10.21

笔记见 git blame.

### 0.11.29

加 `--version` 早退 + execv 自更新前先 probe。SIGTERM 要求换 image
时,不再立刻停服去准备 fd handoff,而是在后台 exec 一次候选,主循环
继续驱动 PTY 直到裁决回来(`SessionEvent::ExecvProbeDone`)。候选起
不来就留在现役 image 上继续服务 —— 与 L1/L2 同一口径:退休换不来的
升级,不值得赔上这个 pane。

2026-07-29 二十个 L3 在 Gatekeeper 评估里各卡 114 秒、pane 全冻,正
是因为它们已经先停服了。

### 0.11.4

CSI 3 J(`clear`)清到持久层。旧行为只清 RAM 视图、故意保留盘上
文件("怕用户想留着"),结果 `clear` + 关闭重开 = 历史全量复活
(现场报告)。3J 是用户显式的清历史指令,按 RFC-004 不变式 4
(毁数据恰在用户显式要求时发生)修正:hot pair 截断到 header、
cold 对删除、读侧 mmap 失效、计数归零;bytelog 保留(灾难恢复
ground truth,重放会重现同一 3J 收敛到相同的清空态)。0.11.3:
v5 live 无缝 execv 验证轮。

### 0.11.2

快照 v5 — LIVE 变体(execv 专用)。v4 的 alt 折叠语义只适用于死亡
快照(冷启复活,进程已死);execv 时 TUI 还活着还在 alt 里,折叠
把可见屏换成主屏,TUI 增量重绘落在错误基底上(现场回归:reinstall
后 claudecode 输入框消失、恢复慢)。v5 live:alt 屏 verbatim 序列
化(尺寸/光标/滚动区/ring/cells),apply 重建 saved_main + alt 现
场,bit 级延续;恢复后向 PTY 前台进程组发 SIGWINCH 兜底全量重绘。
死亡快照(periodic/SIGTERM)保持 fold。0.11.1:首次周期快照升
INFO 级便于真机验证 C.1 存活。

### 0.11.0

RFC-004 L3 侧。A.3 session 目录 flock(双 L3 同 id 物理不可能,347
类交错写根绝);C.1 周期快照(30s 防抖 + generation dirty 门,硬宕
机丢失窗口从"自 boot"缩到 30s,idle 零写);快照 v4 alt 折叠
(B13:claudecode 最终屏 + alt ring 落 scrollback)+ B14 修复(v3
tail 两轴皆反,execv 补 gap 一直在补最老行的复制品);A.4 scroll-
back 损坏隔离 `.corrupt-<ts>` 重建,不再静默 RAM 降级;A.1
`.next_id` 自愈以目录 max(id) 为下限;SessionListener Drop 去毁灭
化(panic unwind 不再删 session 目录)。

### F3+12.1 marspot-term 撤 blank-skip 一并进 L3

F3+12.1 marspot-term 撤 blank-skip 一并进 L3

### F3+12 marspot-term Grid

F3+12 marspot-term Grid::scroll_up `Cell::default()` blank-skip 一并进 L3

### F3+11.2 RAM ring load tolerant 单 record 失败一并进 L3

F3+11.2 RAM ring load tolerant 单 record 失败一并进 L3

### F3+11.1 marspot-term trim 恢复 + idx 改 raw File + grid_shm 滚动时 mask 掉 cursor_vi…

F3+11.1 marspot-term trim 恢复 + idx 改 raw File + grid_shm 滚动时 mask 掉 cursor_visible 一并进 L3

### F3+11 marspot-term scrollback dumb-store 重做随 lib 进 L3

F3+11 marspot-term scrollback dumb-store 重做随 lib 进 L3.do_l3_execv_swap 仍 flush_for_handoff (bin BufWriter tail 进 kernel),idx 现在 push-time 已 flush 不需要额外步

### F3+10i-3 marspot-term Grid blank-check 撤 `\0` 分支随 lib 进 L3

F3+10i-3 marspot-term Grid blank-check 撤 `\0` 分支随 lib 进 L3

### F3+10i-2 marspot-term Grid blank-skip 改 ch-only 随 lib 进 L3

F3+10i-2 marspot-term Grid blank-skip 改 ch-only 随 lib 进 L3

### F3+10i marspot-term scrollback FILE_VERSION 2 随 lib 进 L3

F3+10i marspot-term scrollback FILE_VERSION 2 随 lib 进 L3.execv 后第一次 push 触发 open() header check,旧 v1 文件 rename 成 `.corrupt-<ts>` 让出位置给新 v2 fresh file

### F3+10h marspot-term Grid 行为更新(blank-row skip)随 lib 进 L3

F3+10h marspot-term Grid 行为更新(blank-row skip)随 lib 进 L3.do_l3_execv_swap 已经在 F3+10 调 scrollback_flush_for_handoff 锁住跨 execv 持久化,本次 L3 binary 没动逻辑只跟着 lib bump

### F3+6 LocalSession

F3+6 LocalSession::spawn 现在读 MARSPOT_INITIAL_CWD env (L2 cold restart 时塞进去),传给 cwd_override 参数 → forkpty 前 chdir 到 user 上次的 workdir,避免每次 fresh spawn 都掉回 $HOME

### F3+3.6 cwd push-based 链路退役

F3+3.6 cwd push-based 链路退役.publish_and_poke 里 take_cwd_dirty / PaneCwd frame 发送移除;Terminal 的 cwd/cwd_dirty fields + osc_dispatch OSC 7 parse + parse_osc7_path/hex_nibble 全删.ZDOTDIR shim 的 __marspot_osc7 函数 + TERM_PROGRAM env 也撤.L2 通过 proc_pidinfo 主动查 cwd,L3 不再有 cwd 这门事

### F3+3.5 修 scrollback `Disk

F3+3.5 修 scrollback `Disk::push_line` / `Memory::push_line` 在 release build 上的 UB.根因:`debug_assert_eq!(source.len(), self.cols)` 只在 debug 生效,release 直接跑 `copy_nonoverlapping(source.as_ptr(), …, line_bytes)`.当 source 为空 (Vec<Cell> 长度 0) 时 as_ptr() 返回 NonNull::dangling()=align_of::<Cell>()=0x4,memmove 读 line_bytes 字节即在 0x4 SIGSEGV — 精确对应 sid 248 crash report KERN_INVALID_ADDRESS@0x4 in `marspot_term::grid::Grid::push_historic_scrollback_line`.fix:width 不匹配时用 scratch Vec 填 Cell::default() / 截断到 self.cols,确保 copy_nonoverlapping 总有 line_bytes 真实字节.terminal::apply_snapshot 也加 sb_count<=1M / line_cols<=4096 cap,挡掉 corrupt u32 触发巨大 Vec::with_capacity → allocator abort 那条路

### F3+3.2 marspot-term lib bump (wait_and_connect retry)

F3+3.2 marspot-term lib bump (wait_and_connect retry).L3 binary 没改但走同 lib,版本号同步 promote 信号

### F3+2.2 fix

F3+2.2 fix:Apple /etc/zshrc 默认不 source zshrc_Apple_Terminal(那是 Terminal.app 内部加载),所以光设 TERM_PROGRAM 不会自动 activate update_terminal_cwd hook.改成把 OSC 7 hook (__marspot_osc7 func + add-zsh-hook precmd)直接写进 marspot 的 ZDOTDIR shim ~/.cache/marspot/zdot/.zshrc.每个新 pane 的 shell 都自动有 hook.shim init 时也 fire 一次拿到 cwd.F3+2.1 — L3 wire frame PaneCwd(54) + Terminal.cwd_dirty pipeline 保留 OSC 7 → PaneCwd wire.Terminal 加 cwd: Option<String> + cwd_dirty.osc_dispatch 识别 `7;file://host/path`,strip file://,定位首个 `/` 截 path,percent-decode 写入 self.cwd 同时 dirty=true.publish_and_poke 每次都 take_cwd_dirty,真 dirty 时 Frame::new(MsgType::PaneCwd, cwd.as_bytes().to_vec()) 写 control socket.独立于 grid_changed 分支(cd 不一定立刻产 grid 字节).local_session 启 shell 时 set TERM_PROGRAM=marspot 让 macOS /etc/zshrc 的 update_terminal_cwd 自动 hook 上去.parse_osc7_path 容错:无 file:// / 无 host(只 /path) / 路径含 percent-encoding 全 OK,malformed 不 nuke 已知 cwd hot/cold/delete 三层 scrollback.scrollback.bin 当超过 MARSPOT_SCROLLBACK_HOT_CAP_MB(默认 128MB)就 rotate 成 scrollback.cold.bin/.idx(覆盖前任 = "delete"),开新空 hot.cell_at/read_line/wrapped_at fall through cold tier:line_idx >= hot_first_line 走 hot,cold_first_line<= line_idx < hot_first_line 走 cold(pread,不 mmap,低频访问不值得),line_idx < cold_first_line 返 None(已删).total_lines 跨 L3 restart 重置(line_idx per-process,不持久).RAM ring 只装 hot tail.regression test rotation_writes_cold_and_keeps_old_rows_readable(cap=1MB 推 12000 行验证 rotate 发生 + 早行通过 cold 仍可读 + 晚行通过 hot).search 当前只看 hot,cold search 留 F2+6.单 pane 上限 ≤ 256MB,9 pane ≤ 2.3GB(对照之前 54GB)

