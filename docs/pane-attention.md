# Pane attention — 亮度、回收、以及两者的边界

一个 pane 有多「在场」由两件独立的事决定:

- **亮度**(scrim):所有 pane 都适用,纯粹是画面,不动任何进程。
- **回收**(reclaim / hibernate):**只有 claudecode pane**,会真的把
  claude 停掉再按需恢复。

两者共用一条状态机(`pane_status.rs`),但门槛不同、后果不同。下面是全部
规则,数字都是代码里的常量,改代码就要改这张表。

---

## 1. 亮度 —— 每个 pane 都适用

scrim 是盖在 pane 上的一层黑,`0.25` 表示不透明度 75%。

| 情况 | recede | scrim | 不透明度 | 常量 |
|---|---|---|---|---|
| **你正在用的那个 pane** | — | 0.00 | **100%** | — |
| 其他 pane,一般情况 | 0 | 0.25 | 75% | `UNFOCUSED_SCRIM` |
| 其他 pane,**程序答完一轮并静置 ≥ 5 分钟** | 1 | 0.50 | 50% | `RESTING_SCRIM` |
| 其他 pane,**已被回收**(dormant) | 2 | 0.75 | 25% | `PARKED_SCRIM` |
| 正在被拖动的源 pane | — | 0.38 | 62% | `DRAG_SOURCE_SCRIM` |
| 空位(pane 被移走留下的坑) | — | 0.22 | 78% | `EMPTY_SEAT_SCRIM` |

三条规则:

1. **有焦点的 pane 永远不暗。** 状态机怎么想都不算数 —— 你正在看的东西
   不该从你面前退场。
2. **变暗 220 ms 缓动,变亮瞬间。** 不是定时器:值是时钟的纯函数,到位后
   窗口回到「只在有变化时才画」。
3. **画面被冻住的 pane,亮度也冻住。** 回收期间格子是停的,但底下的字节
   流没停(杀 claude 会产出输出),不冻亮度的话 pane 会先亮到满再暗两格
   —— 内容一动没动,pane 自己在那儿闪。

### 哪些状态对应 recede 1

只有 `AwaitingUser`(**一个程序绑在这个 pane 上、答完了、在等你**)才会
走到 1。`RESTING_AFTER = 300s`。

**裸 shell 停在提示符是 `Empty`,不是 `AwaitingUser`,永远停在 0。**
终端等它的用户,等多久都是本分 —— 按闲置计时会把 18 个 pane 里的 15 个
标暗,等于一个都没标。

---

## 2. 回收 —— 只有 claudecode pane

回收 = 把 claude 停掉(SIGTERM),画面冻在停之前那一帧,你回来时用
`claude --resume <uuid>` 原样接回来。目的是收回内存,不是收回你的会话。

### 全部条件(全部满足才回收)

| # | 条件 | 为什么 |
|---|---|---|
| 1 | 插件认得出这个 pane 的 claude:**profile 和会话 uuid 都知道** | 不知道 uuid 就没法 resume,不知道 profile 就可能用错账号回来 —— 两种都是不可逆的错 |
| 2 | 状态 = `AwaitingUser`,且已稳定保持(`CONFIRM_TICKS`) | 有东西在飞就不是闲置 |
| 3 | **你的光标不在这个 pane 里** | 唯一一条关于「人」的条款,见下 |
| 4 | **会话闲置 ≥ 30 分钟** —— 按**会话记录文件的年龄**算 | pane 自己的安静时钟不能用:claude 每 30 分钟写一次 `Checking for updates`,把它清零,实测永远差 33 秒 |
| 5 | 没有它自己的进程在跑(后台任务 / 构建 / watcher) | CPU 看不出来:实测 `zsh → cargo-fuzz + tail` 是 0.0% |
| 6 | 不是在等自己设的定时器(`/loop` autorun) | 那是「在两步之间」,不是闲置 |
| 7 | CPU 增量 ≤ 容差,且采样跨度 ≥ 15 秒 | 一次采样得先跨过真实时间才有意义 |

阈值 30 分钟可调:`MARSPOT_CC_IDLE_HIBERNATE_S`,设 `0` 完全关掉。

### 条件 3 是 2026-08-03 补的

在此之前唯一的时钟是**会话的**(条件 4),而它不知道人在哪。你坐在一个
pane 里读上一轮的回答,它的记录文件已经 30 分钟没动 —— 于是这个 pane 在
你眼皮底下被停掉,你的下一次击键要等三秒才有反应。而回收是**刻意做成看
不见的**,所以这三秒没有任何解释,读起来就是 marspot 卡住了。

现在:**有焦点的 pane 永不回收。** 你离开时坐的那个位子一直留着。

### 非 cc pane 呢

**完全不参与。** 回收是 claudecode 插件的策略,它只认自己绑上的
claude 进程。裸 shell、vim、ssh、任何别的东西:

- 不会被回收,不会被 SIGTERM,画面不会被冻
- 亮度只有「有焦点 100%」和「没焦点 75%」两档

---

## 3. 回收看得见吗

**不。** 这是明确的产品要求:画面固定在回收前那一帧完全静止,静默回收,
恢复时先恢复完再放开静止。具体到每一处:

| 会动的东西 | 处理 |
|---|---|
| 格子内容 | L3 侧 hold:PTY 照读、字节照进 bytelog,但不喂给解析器 |
| 亮度 | 冻结期间不更新 recede 等级 |
| 徽章 / 标题 | 冻在回收前那一个,期间照常重新断言(核心重启也戴着) |
| 唤醒的收尾 | 等它真画完(静 1.5 秒)再放开,不在半张屏幕上放开 |

代价是**没有任何提示说这个 pane 是停放着的**。所以条件 3 很重要:能被
停放的,只该是你确实离开了的 pane。

---

## 4. 唤醒要多久

约 3 秒,几乎全花在 claude 自己身上(启动 + 读完整个记录 + 画第一帧)。
拆开:

| 段 | 时间 |
|---|---|
| 打出 resume 那一行、claude 进程出现 | ~30 ms |
| claude 读记录 + 画第一帧 | 1–2 s(记录越大越久) |
| 等它画完的静默窗口 `WAKE_QUIET_FOR` | 1.5 s |
| 上限 `WAKE_WATCHDOG` | 30 s |

这段时间画面是冻着的 —— 打字进得去,但你看不到,直到一次跳到画完的那帧。
缩短静默窗口会换来「冻结在半张屏幕上放开」,那是更糟的一种可见。

---

## 相关常量

| 常量 | 值 | 文件 |
|---|---|---|
| `RESTING_AFTER` | 300 s | `src/bin/marspot-shell/pane_status.rs` |
| `SWEEP_INTERVAL` | 1 s | 同上 |
| `PTY_QUIET_AFTER` | 30 s | 同上 |
| `UNFOCUSED/RESTING/PARKED_SCRIM` | .25 / .50 / .75 | `src/render_metal.rs` |
| `hibernate_after()` 默认 | 1800 s | `src/bin/marspot-shell/plugins/claudecode.rs` |
| `WAKE_QUIET_FOR` | 1500 ms | 同上 |
| `HOLD_SETTLE` | 250 ms | 同上 |
| `WAKE_WATCHDOG` | 30 s | 同上 |
| `HOLD_CAP` | 4 MiB | `crates/marspot-session/src/local_session.rs` |

## 怎么看它有没有按表干活

```
grep -aE "hibernate\.(start|waiting)|reclaim\.(woken|outcome)|hold_released" \
  ~/Library/Logs/marspot/marspot.log | tail -20
```

- `hibernate.waiting … — <原因>` 说的是**为什么还不回收**,每个 pane 每次
  原因变了才打一行(`user_here` / `work_in_flight` / `own_timer` /
  `below_threshold` / `short_cpu_sample` / `cpu_busy`)
- `hibernate.start … idle=Ns` 里的 `idle` 是**决定的那个时钟**(会话记录
  年龄);括号里的 `pane state held` 是 pane 自己的状态时钟,只作参考
- `hibernate.start` 到 `hold_released` 之间,那个 sid **不该**出现任何
  `pane_recede.changed` 或 `session.unbound` —— 出现了就是还有一条漏光的路
