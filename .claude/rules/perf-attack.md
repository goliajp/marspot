# Perf attack —— marspot 专属绑定

通用方法论 = `[[perf-decomposition-vs-polish]]`(全局 methodology,host CLAUDE.md 已 @import)。本文只补 marspot 项目层面的固定锚点 —— 触发词清单 / 两步 dance / atomic op 表 / decomposition 模板都看通用版,这里不复述。

---

## 1. Marspot 的"成熟参考实现"是谁

跟 SPG 之于 PostgreSQL 类似,marspot perf 红线的对照对象固定就这三家:

| 角色 | 项目 | 用法 |
|---|---|---|
| 主对照 | **iTerm2**(macOS,Objective-C/Cocoa) | 12 session idle CPU + active CPU 是 marspot 要打的痛点(见 [[project-iterm2-baseline]]) |
| 副对照 | **Warp**(macOS,Rust+WebView+Metal) | 同 platform 同 GPU 路径,跨场景对照 |
| 极速对照 | **Alacritty**(macOS,Rust+OpenGL) | 纯渲染对照(无 chrome / no overlay) |

**源码读位置**(read 不是 grep,函数入口到出口跟 callees):

- iTerm2 主仓:`iterm2/iTerm2`(Objective-C,renderer 在 `sources/Metal/`,parser 在 `sources/iTerm/VT100Parser.m`)
- Warp:闭源 → 黑盒 profile + 公开 blog 推断(per 通用版 §6 FAQ "对手是闭源" 路径)
- Alacritty 主仓:`alacritty/alacritty`(crates `alacritty_terminal/src/vte/`、`alacritty/src/display/`)

声称"language ceiling" / "user-space 不可触" / "syscall residual" 前,必须先 read 完对方等价路径并写进 decomposition doc。Alacritty 同语言同 platform,任何"Rust idiomatic 差距"借口对它无效。

## 2. Bench gate 怎么算"动针"

Marspot 的 perf gate 是双层:

- **本机 `bin/bench.sh`** —— variance ~10-15%,**不能**当 perf 真值(见 [[feedback-bench-mini-only]])。能用来粗筛 polish 路径有没有错,但任何"动针"声明不接受本机数据。
- **远端 `bin/bench-remote.sh`(default mini)** —— clean idle Apple Silicon,默认 n=5 trials × median,baseline 在 `bench/baseline.json` 锁。这是唯一算数的 perf 真值。

"动针"门槛: mini 上累计 ≥ baseline margin(parse 7% / render 50% / scroll 30%,在 baseline.json 注释里)。在这之下 = 没用,不是"应该有用只是看不出来"。

要 5μs 级 wins 直接拒收 —— marspot 没投资到 n=1000+ 那个精度。**5μs 级 wins 必须 3-5 个累计同批上**,单独 ship 不验。

## 3. Hot path / 攻击面 已知优先级

marspot 的红线 endpoint 按 [[project-iterm2-baseline]] + [[feedback-perf-over-feature]] 排:

| 优先 | 红线 | bench scenario | 当前位置 |
|---|---|---|---|
| 1 | idle CPU 0% | `bench --rss-watch` + 手测 | marspot 目前 ~0%,iTerm2 12-session 23%。**不能退** |
| 2 | parse 字节吞吐 | `cat-ascii / cjk / emoji / mixed` | mini floor ascii 173 / mixed 167 / cjk 208 / emoji 61(2026-07-11 emoji relock:cluster 正确性成本,见 baseline _note)|
| 3 | scroll p99 | `--bench scroll` + `scroll-cold` | mini ≤4µs / ≤4µs(2026-07-11 relock:中位 3.083µs ×1.3,见 baseline _note)|
| 4 | render p99 | `--bench render` | mini ≤1301µs(2026-06-21 lock) |
| 5 | RSS idle | `--bench` idle 段 | mini ≤95MB |
| 6 | binary size | strip + lto | mcli ≤1.37MB / marspot ≤1.54MB(2026-07-11 relock:feature 月成本入账,user 拍板 size 不卡紧;职能=抓意外大跳)|

任何 perf attack 影响 idle CPU = **直接拒**。"hot path 神圣" 是产品调性(`[[feedback-perf-over-feature]]`),不是 negotiable budget。

## 4. Decomposition 起手位置

marspot 18+ stage 的拆解,典型起点(对照 PTY → screen 一帧的生命周期):

1. PTY byte 进 L3(`read()` syscall 大小 / vDSO / kqueue 事件)
2. byte slice → parser feed
3. `marspot_term::parser` C0/CSI/OSC dispatch
4. Cell 写入 grid(`Grid::print` / `take_pending_wrap` / `scroll_up`)
5. SGR carry / attr packing
6. scrollback push_line(trim / BufWriter / mmap-write)
7. wrapped 标记 + sb_wrapped mirror
8. shm publish(grid_shm header + cells)
9. L2 reader poke / coalesce / read grid snapshot
10. L2 build_instances(render_metal 的 cell + glyph push)
11. Metal pipeline 4 passes + UI rect + glyph FG
12. CAMetalLayer present
13. ... (continue, 目标 18+ 段)

Read 对手等价路径(iTerm2 `MTKView` + `iTermMetalDriver.mm`、Alacritty `alacritty/src/display/window.rs`)。**预测 < 实测**,翻盘的项标红 —— 是真正会闭红线的位置。

## 5. 反油条触发点(marspot 专版补充)

通用版 `§1 自欺触发词清单` 之外,marspot 项目里出现下列任意一句也 abort:

- "iTerm2 是 Objective-C / Cocoa 走老路所以慢,我们底层结构上不会有这个 overhead" —— 没读 iTerm2 source 不允许声称
- "Metal 渲染没法再快 / GPU 已经满载" —— flamegraph 跑了吗?Alacritty 怎么省的?
- "scrollback 文件 IO 是磁盘 bound" —— mmap-write / page cache 真正 bound 在哪?osfile / iostat 看了吗
- "marspot 已经赢了 iTerm2 大部分 metric,这一项可以放" —— 砍 endpoint 不在我职权范围(`[[guideline/utility-judgment-is-not-mine]]`)

## 6. 关联

- [[perf-decomposition-vs-polish]] —— 通用 methodology(全局 @import)
- [[steel-cement-stone.md]] —— 三分类模型,perf 工作主要在"石头"层
- [[feedback-bench-mini-only]] —— mini 才算数的来历
- [[feedback-perf-over-feature]] —— "perf > feature" 是产品调性
- [[feedback-ceiling-first]] —— LOC 不在意但 perf budget 是硬天花板
- [[project-iterm2-baseline]] —— iTerm2 12 session idle 23% / active 44% 的对照数据
