#!/usr/bin/env bash
# bin/test.sh — run the lib test suite via cargo-nextest.
#
# Why nextest over `cargo test`: per-test process isolation surfaces
# panics with their owning test name, parallel scheduling cuts wall
# clock noticeably at ~140 tests, and the failure output is one line
# per failure instead of a wall of output to grep through.
#
# Passes through any extra args:
#   bin/test.sh parser::
#   bin/test.sh --failure-output immediate

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# --workspace so the marspot-term crate's tests (the terminal engine —
# parser/grid/terminal/scrollback/… extracted for target #4) run too,
# not just the GUI crate's handful of lib tests.
#
# `--all-targets` (instead of `--lib`) includes integration tests —
# in particular `crates/marspot-term/tests/scrollback_display.rs`, the
# user-perspective gate that catches scrollback display bugs (blank
# pushes leaking into scrollback, torn-write history loss, scroll
# cap mismatches, resize content corruption) BEFORE they ship.
#
# Running the suite INSIDE a marspot terminal inherits the pane's
# MARSPOT_SESSION_ID (L3 exports it to its shell).  With it set,
# every `Terminal::new` in the tests opens the REAL state dir's
# sessions/<id>/scrollback.bin and overwrites the pane's on-disk
# history (bit us 2026-07-03: session 347's scrollback truncated to
# test residue).  Unset it before any test process spawns.
unset MARSPOT_SESSION_ID
# 2026-08-10:同一类事故的另一半。`unset MARSPOT_SESSION_ID` 只挡住了
# 测试**写**真实 scrollback;测试**读**真实状态一样有害 —— settings 落地
# 之后,一条断言默认回收阈值的测试在作者从面板里把回收关掉那天开始失败,
# 而且只在他的机器上失败。测试报告的是它所在机器的状态,不是代码的状态。
# 整个测试进程树钉在沙箱状态目录上;需要真实目录的测试自己覆盖。
export MARSPOT_STATE_DIR="${MARSPOT_STATE_DIR:-/tmp/marspot-test-state}"
mkdir -p "$MARSPOT_STATE_DIR"
# 2026-07-28 事故:nextest 默认并发 = 核数(此机 14),811 个测试里
# 一批要各自初始化 Metal / CoreText 的重进程(每个 ~130MB)齐发,
# 32 个测试二进制两分钟内并发拉起,16 个同时抢一把内核 rwlock 写锁,
# tccd 跟着卡进不可中断等待,WindowServer 主线程同步等 tccd 40 秒 →
# 被 watchdogd 击杀,整机强制重启。上限压到 6(可用 MARSPOT_TEST_JOBS
# 覆盖);测试墙钟略增,换的是"跑测试不会把宿主机跑死"。
exec cargo nextest run --workspace --all-targets \
  --test-threads "${MARSPOT_TEST_JOBS:-6}" "$@"
