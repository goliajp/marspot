# RFC-003 — L3 拥有 PTY,删除 L4 shelld

Status: approved 2026-06-16, ready to execute. No defer, no mid-stream decisions —
issues encountered during execution are resolved by amending this document, not by
working around it.

## 1. 终局架构(3 层)

```
L1 marspot-shell    NSWindow + supervisor + install-local 触发器
                    只 spawn L2,不参与 session 生命周期
        ↓ spawn
L2 marspot-core     Metal renderer / 布局 / pane 管理 / 输入分发
                    - 启动:扫 state/sessions/*.toml 重连健康 L3
                    - 新建 pane:fork+exec marspot-session
                    - 关闭 pane:对 L3 发 Quit RPC
        ↓ spawn 1:N
L3 marspot-session  Owns: PTY master + shell child + parser + grid +
                          scrollback + bytelog + shm frame buffer
                    UDS listener 接 L2 RPC(Attach/Resize/SendKeys/
                    SendSignal/GetScrollbackPage/SubscribeFrameSeq/
                    Quit + Title/Bell/OSC7/OSC8 events 上报)

(L4 marspot-shelld 整体删除)
```

## 2. Audit findings(execution 前最后一遍)

- **延迟性能并非来自"少一道进程跳"**。输入路径前后都是 2 个 IPC 跳。性能赢面来自:
  - 输出路径少一次 bytelog ring 中转(`cat bigfile` 类高吞吐场景实赢)
  - idle 时少一个 daemon select 循环
  - 整机少一个 ~10-20 MB 进程
- **L3 启动顺序**:先写 toml(原子 tempfile+rename)再 fork shell,避免 L2 看不到注册的竞态。
- **session id 分配**:`state/sessions/.next_id` 文件 + atomic rename 单调递增。
- **UDS path**:`state/sessions/<id>.sock`,L3 启动 unlink stale + 新建。
- **注册表 toml schema**:`pid / socket / cols / rows / title / cwd / proto_version / created_at_unix`。
- **bytelog 格式**:L3 owns 后格式与 L4 老 bytelog **不兼容**(self-use 接受一次割断)。
- **install-local 安全性**:基于 `install` 命令原子 rename(新 inode),理论上不触发 AMFI;保留 liveness skip 双保险。
- **Output 事件通道**:Title / Bell / OSC7(cwd)/ OSC8(hyperlink)需要新的 RPC events,不能只靠 shm frame 传。
- **L2 swap 期间 L3 reparent**:L2 死后,L3 children 被 launchd 收养,L2 重启从 toml 扫回。验证 reparent 不杀 L3。

## 3. 已知风险(已接受)

- Phase 1-3 期间 L4+L3 双写 bytelog(浪费几 MB 盘),不影响行为
- 上线日所有活会话不能跨架构延续,需主动关掉重开
- Phase 1 内部多 commit 期间有 dead-path 代码,但每个 commit 必须 build green + install-local 可工作

## 4. 计划修改协议

执行中遇任何"原计划站不住"的情况:
1. **停手不绕**
2. 在本文件 §6 追加 `Amendment N — <短描述>`,写清:卡在哪 / 新决定 / 影响哪些步骤
3. 必要时修改后续 checklist 项,但不退却 / 不绕过 / 不 defer
4. 继续按更新后的 checklist 推

## 5. 线性 checklist

每个 commit 必须 build green;每个 Phase 完成 gate 必须达成才进下一个。

### Phase 0 — Baseline 锁定(no commits)

- [ ] 0.1 跑 `bin/bench.sh --full`,数字写入 `bench/rfc-003-baseline.json`
- [ ] 0.2 12-pane idle 1h soak,记 per-pane RSS 上限 + 整机 idle CPU,写入同文件
- [ ] 0.3 当前 `docs/architecture.md` 快照存为 `docs/architecture-pre-rfc003.md`

### Phase 1 — L3 拿到 PTY 直连(L4 留存)

- [ ] 1.1 grep 列出 L4 中 openpty / fork / execve / TIOCSCTTY / waitpid / SIGHUP / bytelog 写入的源位置
- [ ] 1.2 commit `infra: RFC-003 step 1a — L3 加 --owns-pty mode flag(默认 off)`
- [ ] 1.3 commit `infra: RFC-003 step 1b — L3 --owns-pty 路径下 openpty + fork shell + bytelog 自写`
- [ ] 1.4 commit `infra: RFC-003 step 1c — L3 主循环新增 pty_master poll + parser direct feed`
- [ ] 1.5 cargo test + `bin/test.sh` 全绿
- [ ] 1.6 手动 launch L3 with `MARSPOT_L3_OWNS_PTY=1`(单 pane sandbox),L2 仍走 L4,L3 独立跑通 `echo hello`
- **gate**:`MARSPOT_L3_OWNS_PTY=1` 单 pane echo/cat 跑通,L3 进程不崩

### Phase 2 — UDS 控制面 + 注册表

- [ ] 2.1 commit `infra: RFC-003 step 2a — sessions/.next_id atomic id 分配`
- [ ] 2.2 commit `infra: RFC-003 step 2b — toml 注册表 atomic write/unlink + stale socket 清理`
- [ ] 2.3 commit `infra: RFC-003 step 2c — UDS listener + Hello/Attach RPC + SessionSnapshot 返回`
- [ ] 2.4 commit `infra: RFC-003 step 2d — Resize/SendKeys/SendSignal/Quit RPC`
- [ ] 2.5 commit `infra: RFC-003 step 2e — GetScrollbackPage + SubscribeFrameSeq RPC`
- [ ] 2.6 commit `infra: RFC-003 step 2f — Title/Bell/OSC7/OSC8 events 上报通道`
- [ ] 2.7 写一个 cargo example demo client,attach 一个 L3 + 拿 snapshot
- **gate**:demo client 跑通 RPC 往返,L3 端 unit test 覆盖各 RPC

### Phase 3 — L2 切到 L3 直连

- [ ] 3.1 grep 列出 L2 里所有 shelld_client 调用点
- [ ] 3.2 commit `basic: RFC-003 step 3a — L2 新增 session_client 模块直连 L3 UDS`
- [ ] 3.3 commit `basic: RFC-003 step 3b — L2 新建 pane 改走 fork+exec marspot-session(--owns-pty 隐式)`
- [ ] 3.4 commit `basic: RFC-003 step 3c — L2 启动扫 sessions/*.toml + kill 0 + 重连`
- [ ] 3.5 commit `basic: RFC-003 step 3d — L2 input/output/resize/scrollback/title 全部 reroute 到 session_client`
- [ ] 3.6 commit `infra: RFC-003 step 3e — install-local 跑通 9-pane,L4 进程残留但 L2 0 流量`
- **gate**:install-local 后 9 pane 全走 L3 直连,L4 socket 流量计数为 0(log 验证),目视无回归

### Phase 4 — Frozen reattach

- [ ] 4.1 验证 bytelog 文件可 reopen-append(读源,如不行修补)
- [ ] 4.2 commit `basic: RFC-003 step 4a — L3 --rehydrate mode:replay bytelog 到 grid 不 fork shell`
- [ ] 4.3 commit `basic: RFC-003 step 4b — L2 死 pid toml 渲染半透明遮罩 "Session lost — Enter to revive"`
- [ ] 4.4 commit `basic: RFC-003 step 4c — Enter 触发 L2 spawn 新 L3 接管 pane id + 继承 bytelog 文件`
- **gate**:`kill -9 <L3-pid>` 后那个 pane "Session lost",按 Enter 起新 shell 继续输入正常

### Phase 5 — Bench gate(中检)

- [ ] 5.1 跑 `bin/bench.sh --full`,数字写入 `bench/rfc-003-mid.json`
- [ ] 5.2 vs `bench/rfc-003-baseline.json` 四项(idle CPU / render lat / parse tput / per-pane RSS)全部 ≤ baseline
- [ ] 5.3 任一项退化 → 不进 Phase 6,在 §6 写 Amendment 定位修复
- **gate**:四项全绿底

### Phase 6 — 删 L4

- [ ] 6.1 commit `infra: RFC-003 step 6a — 删 src/bin/marspot-shelld.rs + 单元测试`
- [ ] 6.2 commit `infra: RFC-003 step 6b — 删 shelld-only wire 协议代码(Frame/MsgType 中仅 L4 用项)`
- [ ] 6.3 commit `infra: RFC-003 step 6c — 删 com.marspot.shelld.plist + L1 supervisor 中 shelld 拉起逻辑`
- [ ] 6.4 commit `infra: RFC-003 step 6d — install-local.sh 去 shelld 段 + 新增 marspot-session liveness skip`
- [ ] 6.5 commit `infra: RFC-003 step 6e — Cargo workspace 删 shelld crate + version-vector.toml 删 shelld 行`
- [ ] 6.6 commit `infra: RFC-003 step 6f — bin/run.sh + bin/_dev-sandbox.sh + 其他脚本去 shelld 启动`
- [ ] 6.7 commit `infra: RFC-003 step 6g — logx 类别清理(shelld.* 类别下落)`
- [ ] 6.8 `cargo test` + `cargo nextest run --lib` + `bin/fuzz.sh` + `bin/lint-deps.sh` 全绿
- [ ] 6.9 `grep -ri shelld src/ Cargo.toml bin/` 无残留(除本 RFC 引用)
- **gate**:仓库无 shelld 残留,所有 gate 测试绿

### Phase 7 — Bench gate(终检)

- [ ] 7.1 跑 `bin/bench.sh --full`,数字写入 `bench/rfc-003-final.json`
- [ ] 7.2 目标:idle CPU ↓ + render lat 持平/↓ + 整机 RSS ↓ ≥ 一个 shelld 的量
- [ ] 7.3 12-pane 24h soak,RSS 不漂移
- **gate**:final.json 优于 baseline,24h 无回归

### Phase 8 — 故障注入

- [ ] 8.1 `kill -9 <L3-pid>` → 仅该 pane 进 frozen,其他 8 无感
- [ ] 8.2 `pkill marspot-core` → L2 自动重启,scan toml 后 9 pane 全部重连
- [ ] 8.3 完整 install-local(L2 + L3 二进制都变)走一遍,目视 0 闪、0 黑
- [ ] 8.4 reboot 模拟:正常 quit marspot → 重启系统 → 重开 marspot,所有 sessions/ toml stale,全部 "Session lost",按 Enter 进 frozen 回放
- [ ] 8.5 install-local 进程被 kill -9 中断 → 验证 L3 全活,L2 健在或自动恢复

### Phase 9 — 文档 + memory 同步

- [ ] 9.1 改 `docs/architecture.md` 为 3 层
- [ ] 9.2 改 `CLAUDE.md` commit scope policy `infra` 描述去 shelld 字样
- [ ] 9.3 memory:`project_four_layer_split.md` 标 superseded
- [ ] 9.4 memory:新建 `project_three_layer_arch.md`
- [ ] 9.5 memory:`project_l3_default.md` 加"L3 also owns PTY master"
- [ ] 9.6 memory:`project_shelld_amfi_kill.md` 标 resolved(根因消失)
- [ ] 9.7 memory:`project_install_local_fingerprint.md` 验证 fingerprint 逻辑不依赖 L4 binary

### Phase 10 — Release

- [ ] 10.1 `version-vector.toml`:L1 bump、L2 major bump、L3 bump、shelld 行删除
- [ ] 10.2 `git flow release start`
- [ ] 10.3 release commit `infra: RFC-003 — three-layer marspot, PTY owned by L3`
- [ ] 10.4 `git flow release finish` + tag
- [ ] 10.5 ff master

### Phase 11 — 上线 + 观察

- [ ] 11.1 install-local 到 production marspot.app
- [ ] 11.2 主动关闭所有活会话,重新开(bytelog 格式割断)
- [ ] 11.3 24h 观察 idle CPU + RSS 漂移 + crash log
- **gate**:24h 无回归 → RFC-003 收官

## 6. Plan amendments

### Amendment 1 — Phase 0.1 / competitors_snapshot 容忍 9 天 stale

`bench/baseline.json` 的 `competitors_snapshot.captured_at = 2026-06-07`(9 天前,限 7 天),
`bin/bench.sh --full` 默认 refuse。选择不跑 `bin/measure-other.sh` 刷新(会强制
focus 用户的 iTerm2/Warp 派 AppleScript,打扰当前工作流),改设
`MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1` 跑 baseline 锁定。

理由:本次锁的是 marspot 自身的 parse/render/scroll/idle-RSS,competitor 数字仅
作 vs-best-other 参考。RFC-003 完成后的 final gate(Phase 7)会再跑一次;到那时
若 competitors 仍 stale,再考虑刷新。

影响:`bench/rfc-003-baseline-bench.txt` 头部需标注 stale flag,以便后续读懂数字
对比时知道 vs-best 列的 reference 不是最新的。

### Amendment 2 — Phase 0.2 "1h soak" 替换为 `bin/soak-l3-drift.sh DURATION_S=1800`

原文写的 "12-pane idle 1h soak" 是我拍脑袋数字 + 不存在的具体 harness。现状:
- `bin/soak-l3-drift.sh` 是 release/nightly 级 sustained-load 漂移闸,DURATION_S=1800
  是它定义的"扩展"档(默认 300、release 1800)
- `bin/soak-l3-rss-scaling.sh` 跑 N=1 / N=9 per-session RSS scaling check
- 没有现成的 "N-pane idle 全 GUI"  soak 脚本

调整 0.2 为跑 release-grade 的 drift(1800s 持续 sustained load)+ scaling(N=9
idle),两者已是 perf-attack A1/C 的正式闸,远比拍脑袋的"1h idle"更有 signal。
若 RFC-003 后真发现别的漂移维度漏掉,Phase 7 / Phase 11 的 24h 观察会捕到,届时再加针对性 soak。

### Amendment 3 — Phase 0 baseline 用 dev-box 同机 A/B,不用 bench/baseline.json gate verdict

`bench/baseline.json` 的 floors 是 2026-05-05 在 ssh mini clean machine 上锁的(perf-attack F)。
本机 dev-box 跑 `bench.sh --full` 时 headless parse 会落到 27-44 MB/s,而 floor 是 163-199 MB/s
—— **同代码、不同机器**,gate 必然 FAIL。这不是 RFC-003 引入的问题,也不是 RFC-003 要修的问题。

RFC-003 的 Phase 0 baseline 改为捕获**同 dev-box 同时段**的当前数字到
`bench/rfc-003-baseline.json`,Phase 5 / Phase 7 同样在 dev-box 上跑 `bench.sh --full` 并和
本 JSON 对比 — A/B 同机,看 delta。

如需 vs-baseline.json 的官方 gate 通过,要 ssh mini 跑 `bin/bench-remote.sh`。RFC-003 不强求,
Phase 10 release 阶段决定是否补这一步。

附:`bench/baseline.json` 的 `binary_size_bytes_max` 和 `memory_idle_kb_max` 也是历史值
(marspot 778 KB floor vs 实际 1.1 MB),这是几个月正常开发增长堆积,需要单独 `--update-baseline`
跑一次,跟 RFC-003 解耦。

### Amendment 4 — Phase 1.1 audit 收集(read-only,不算 commit)

`bin/bench.sh --full` 完成后、drift 后台跑期间利用空闲做 1.1 grep。结果记录在此:

**L4 spawn / bytelog 代码位置**(`src/bin/marspot-shelld.rs` 1896 行):

| 位置 | 内容 |
|------|------|
| 204-330 | bytelog open/append/delete(`ByteLog::open`、`delete_bytelog`) |
| 350-441 | `Session` struct + bytes fan-out(PTY 读 → bytelog 追加 + 广播给 subscribers) |
| 887-941 | `execv.rehydrate` 在 shelld 自更新时从 bytelog 重建 session 状态 |
| 1448-1495 | `MsgType::NewSession` 处理:`pty::spawn` + `ByteLog::open` + register |
| 1682-1686 | `MsgType::KillSession` = `delete_bytelog` |

**PTY 原语**:`crates/marspot-term/src/pty.rs` 727 行,已是共享 crate;`libc::forkpty` +
`libc::execvp` 包装在 pty.rs 里。**结论:不需要剥离,L3 直接 use 即可**。

**L3 现有形态**(`crates/marspot-session/src/main.rs` 628 行):
- `fn main()` 在 337 行
- `MARSPOT_SESSION_ID` env / `ENV_CONTROL_FD` / `ENV_SHM_FD` 三个 env 入口
- `setup_control_socket` 接 L4 控制 socket
- `setup_shm` 接 L2 给的共享内存 fd
- 当前 L3 是 L4 的 worker:从 L4 收 bytes → parser → grid → shm publish

**Phase 1 实施切片**(小,远小于我先前估计):
1. `--owns-pty` 路径下 L3 不连 L4 control socket,改为自己 `pty::spawn` + 维护一个 PTY master fd
2. bytelog 写入路径搬到 L3:在 marspot-term 里抽一个轻量 `bytelog` 模块(同格式),
   L3 / L4 各自调用。L4 在 Phase 6 删,但 Phase 1-5 共存期间双写无害。
3. 主循环新增 PTY master poll(`poll` / `epoll_create`,macOS 上用 `kqueue` 或 `select`)
4. shm publish 路径完全不动 — L2 在 Phase 3 才切

预计 commit 数维持 3 个(1a/1b/1c),复杂度比预想低 30-40%。

### Amendment 5 — registry 用 per-session 子目录,不是 top-level files

原 §2 第 4 条规划 `sessions/<id>.toml` + `sessions/<id>.sock` 作为 top-level 文件,
但 bytelog 已经在 `sessions/<id>/bytelog`。混存会让同一 session id 在 sessions/ 同时
以 dir 和 filename prefix 出现,扫描和清理都别扭。

实际采用:**per-session 单一子目录,所有 session 资产打包**:

```
sessions/
  .next_id
  <id>/
    bytelog       # 字节日志(自 1a 起已在)
    entry.toml    # 注册表元数据(本 step)
    sock          # UDS 控制 socket(Phase 2.3)
```

`list_sessions()` 扫 `sessions/` 子目录、对每个子目录读 `entry.toml`。删 session = `rm -rf <id>/`,一次清干净。

UDS 路径 `sessions/<id>/sock` 在沙箱里约 35-55 字符,macOS 104 限内,安全。

(执行中后续追加。)
