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

## L1  marspot-shell

Current: **0.6.5**

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

Current: **0.10.67**

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

Current: **0.9.23**

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

