# RFC-004 — Session identity: pane 永不漂移

> Status: EXECUTING (autorun).  2026-07-17.
> 起因:宕机重启后 12 pane 变 10、history 按位置错位。审计结论:
> pane↔session 绑定是位置制,装配失败压缩空位,错误处理毁灭式
> (SIGKILL + delete_session)。本 RFC 把绑定改成身份制并修掉审计
> 中列出的全部 12 项缺陷(B1-B12)。

## 设计不变式(修完后永真)

1. **pane = 虚拟独立终端,身份 = session_id。** 槽位、标题、cwd、
   history 全部跟着 sid 走;槽位顺序仅由 `shell-state.bin` 的
   `panes[]` Vec 序决定,每格记录自己的 sid。
2. **装配永不压缩。** 一格装不出来就放占位 pane(保留 sid,可重
   试复活);boot 后回写的 state 保留原 sid,不固化损坏。
3. **永不向未验证身份的 pid 发信号。** 判活 = `kill(pid,0)` 成功
   **且** `proc_pidpath` 指向 marspot-session 镜像。
4. **`delete_session`(毁数据)只发生在用户显式关 pane。** 其余
   一切失败路径最多把目录移入回收站 `retired/`,绝不 rm。
5. **宕机窗口有界。** L3 周期快照(30s 防抖)+ `.idx` 逐条直写
   + BufWriter 64KiB 尾巴 = 硬宕机最多丢 30s 可见网格演化。

## 线性 checklist(中途不决策,顺序执行)

### Phase 0 — 数据抢救(先于一切代码改动)
- [x] 0.1 用 `rebuild_scrollback_from_bytelog` 重放孤儿 369 / 373 /
      374 / 380 / 381 的 bytelog → 重建 scrollback.{bin,idx} 放回
      各自 session 目录(它们已死,无需 SIGSTOP 协议)
      **执行记录 2026-07-17**:工具加了第 6 参数 dump-text(TUI 会
      话内容在最终网格不在 scrollback);5 个孤儿的文本转储落在
      `~/Library/Caches/marspot/rescue-2026-07-17/session-*.txt`;
      369/373 重建的 bin/idx 已放回。重放过程中发现 **B13**(见下)
      —— alt-screen 历史本就从未落盘,重放能拿回的就是文本转储。

### Phase A — 注册表与身份基础(infra)
- [ ] A.1 (B5) `allocate_next_session_id` 自愈:counter 值与现存
      目录 max(id) 取大者 +1;`.next_id` 缺失/损坏时从目录重建
- [ ] A.2 (B1 前置) `session_registry::pid_is_live_session(pid)`:
      `kill(0)` + `proc_pidpath` 含 `marspot-session` 才算活
- [ ] A.3 (B6) L3 `SessionListener::bind` 前对 `session_dir/.lock`
      `flock(LOCK_EX|LOCK_NB)`;拿不到 = 同 id 已有 L3 → 退出。
      锁 fd 跨 execv 传递(clear CLOEXEC)
- [ ] A.4 (B7+B10) `FileScrollback::open` 失败:实现 doc 声称的
      行为 — 改名 `.corrupt-<ts>` + 重建空文件,保持 File 模式;
      `Terminal::new` 的 eprintln 换 lx_warn(L3 stderr 是 null)

### Phase B — 装配身份制(infra,核心手术)
- [ ] B.1 重写 L2 boot 装配为按格身份制:遍历 `saved_state.panes`,
      每格:sid 活(A.2 验证)→ reattach;reattach 失败 → 降级
      复活(先 SIGKILL 已验证的本家进程,不删目录);sid 死且目
      录在 → 同 id resurrect;目录无/sid=0 → 新 id fresh spawn;
      spawn 失败 → 占位 pane(保 sid)。不在 saved 里的活 session
      → append 到尾部(封 HARD_CAP,超出 SIGKILL + retire)
- [ ] B.2 占位 pane:无进程 Pane 变体,渲染显示 session lost 提示,
      Enter/点击触发同 id 复活重试;save_session_state 保留其 sid
- [ ] B.3 标题恢复按 sid 匹配(saved 格 sid == 实装 sid 才用位置
      项,否则全表按 sid 查);cwd 同理
- [ ] B.4 (B9) boot 尾 GC:既不在 saved 又不活的死目录 → move 到
      `retired/<id>-<ts>`;`retired/` 内 >14 天的删除
- [ ] B.5 e2e 装配测试:沙箱造多 session → SIGKILL 全部 → 冷启 →
      断言每格 sid/标题/history 对位;spawn 失败 → 占位不压缩;
      乱序 readdir 不影响槽位
### Phase C — 宕机窗口(infra,L3)
- [ ] C.1 (B4+B13) L3 周期快照:自上次快照后有 feed 才写,30s 防
      抖,tmp+rename 原子;与 one-shot apply 语义兼容(apply 后删,
      周期性重建)。**快照 v4**:在 alt-screen 时同时序列化
      saved_main(主 grid + 主 scrollback 指针)与 alt grid + alt
      ring tail(cap 同 20k 行),恢复时重建 saved_main 结构 ——
      没有这个,claudecode pane 宕机快照恢复只剩 alt 表面一屏。
- [ ] C.2 (B2) `wait_and_connect` 重试面扩大:握手 EOF /
      InvalidData 也在 deadline 内重试;reattach 超时 2s → 5s

### Phase D — 状态根迁移(infra)
- [ ] D.1 (B8) `state_root()` 默认迁 `~/Library/Application
      Support/marspot`;L1 最早入口做一次性迁移:rename 旧根 →
      新根 + 旧位置留 symlink(兜漏网引用);MARSPOT_STATE_DIR
      沙箱不受影响;迁移后对活 L3 SIGTERM fanout 触发 execv 以
      新路径重开文件

### Phase E — 杂项 + 收尾
- [ ] E.1 (B11) `render_metal.rs` 私有 `SESSION_COUNT_HARD_CAP=9`
      与 `ui::SESSION_COUNT_HARD_CAP=36` 统一(查清 9 的语义:
      若是 buffer 预算需按实际 panes.len() 扩)
- [ ] E.2 (B12) 验证 sid=0 格在新装配下独立处理不塌缩(B.1 覆盖,
      加测试钉)
- [ ] E.3 全量 lib 测试 + 新增单测全绿;`bin/bench.sh` 本机粗筛
      (idle CPU 不回归 — C.1 的周期快照必须 idle 零写)
- [ ] E.4 version bump(shell/core/session 按实际触面)+
      CHANGELOG + install-local 上真机 + 三栏交割

## 审计缺陷 → checklist 映射

| Bug | 修复步 |
|---|---|
| B1 pid 复用误杀 | A.2 + B.1 |
| B2 reattach 瞬时失败即删 | B.1(降级复活)+ C.2 |
| B3 压缩固化 | B.1 + B.2 |
| B4 硬宕机丢可见网格 | C.1 |
| B5 .next_id 归零碰撞 | A.1 |
| B6 session 目录无锁 | A.3 |
| B7 scrollback 静默降级 | A.4 |
| B8 状态根在 Caches | D.1 |
| B9 孤儿累积 | B.4 |
| B10 doc drift(.corrupt) | A.4 |
| B11 常量分叉 | E.1 |
| B12 sid=0 塌缩 | B.1 + E.2 |
| **B13 alt-screen 历史不持久化**(Phase 0 重放中发现:`terminal.rs:1790` alt grid 用 Memory scrollback,claudecode 等 TUI 的滚动历史从不落盘;唯一载体 = 优雅退出的 state.bin → 硬宕机全灭。现网 369/374/381 scrollback.bin 只有 32B header 即此因) | C.1(快照 v4 覆盖 alt) |

## 审计原始报告

见 2026-07-17 会话审计(三条子链路:cold-start 装配 / history
持久化 / 掉线路径),关键坐标:

- 装配循环 `src/bin/marspot-core.rs:4204-4533`(重写对象)
- 判活 `marspot-core.rs:4305`;毁灭臂 `4341-4367`
- readdir 无序 `session_registry.rs:291`
- 标题位置制 `marspot-core.rs:4514-4529`
- 快照只在优雅退出写 `marspot-session/main.rs:1745-1747`
- scrollback 静默降级 `terminal.rs:245-250`
