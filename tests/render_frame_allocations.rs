//! How many allocating constructs are on the per-frame render path.
//!
//! S5-09. `CoreApp::render_inner` runs once per window per frame, and the
//! v1 plan records it as "about 55–60 mallocs per frame per window, in no
//! instrument". This is the instrument. It does not run the function —
//! that needs a Metal device and a window — it counts the constructs in
//! its body that allocate, which is a number that can only be read off
//! the source and so cannot drift away from it.
//!
//! What it is good for: the count goes down and never up. What it is not:
//! a measurement of the heap. A `collect()` over nine panes is one
//! allocation and a `collect()` over one is also one, so the ceiling here
//! is a count of *sites*, and the per-frame heap cost is sites times what
//! each one holds. The real heap number needs a GPU and a running window,
//! which is `bin/bench-remote.sh` territory.
//!
//! Why a ceiling rather than zero: the function is 330 lines of frame
//! preparation and getting it to zero is a scratch-buffer refactor. A
//! ceiling set at today's number means the next person cannot add one
//! without noticing, and every removal tightens it.

/// Allocating constructs in the body of `render_inner`, counted today.
///
/// 2026-10-01: 21, now 20. The pane-number labels moved to a static table,
/// which removed two sites (`to_string` and `collect`) and added one --
/// the fallback that used to clone out of that `Vec` now makes its own
/// `String`, because `resolved_labels` is `Vec<String>` and an owned value
/// has to come from somewhere.
///
/// One site, and a real saving: what went away was a `Vec` plus one
/// `String` per pane, every frame, unconditionally; what remains allocates
/// only for the panes that fall through to their number. Which is the
/// caveat in the module docs made concrete -- this counts sites, and sites
/// are not the heap. Getting the rest out means `Vec<Cow<str>>` or a
/// scratch buffer on `WindowRender`, which is the next slice.
const CEILING: usize = 20;

fn render_inner_body() -> String {
    let src = std::fs::read_to_string("src/bin/marspot-core.rs")
        .expect("the core is where it was");
    let lines: Vec<&str> = src.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim_start().starts_with("fn render_inner("))
        .expect("render_inner is still called that");
    let mut depth = 0i32;
    let mut opened = false;
    let mut end = start;
    for (i, line) in lines.iter().enumerate().skip(start) {
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;
        if line.contains('{') {
            opened = true;
        }
        if opened && depth == 0 {
            end = i;
            break;
        }
    }
    assert!(end > start + 50, "render_inner came out {} lines long", end - start);
    lines[start..=end].join("\n")
}

fn allocating_sites(body: &str) -> Vec<(usize, String)> {
    const PATTERNS: [&str; 8] = [
        ".to_string()",
        ".collect()",
        "format!",
        "String::",
        "Vec::new",
        "vec![",
        ".to_owned()",
        ".clone()",
    ];
    let mut out = Vec::new();
    for (n, line) in body.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for p in PATTERNS {
            let mut from = 0;
            while let Some(at) = line[from..].find(p) {
                out.push((n + 1, p.to_string()));
                from += at + p.len();
            }
        }
    }
    out
}

#[test]
fn the_per_frame_path_does_not_grow_new_allocations() {
    let body = render_inner_body();
    let sites = allocating_sites(&body);
    assert!(
        sites.len() <= CEILING,
        "render_inner has {} allocating constructs, ceiling is {CEILING}. Every one of \
         these runs once per window per frame:\n  {}",
        sites.len(),
        sites
            .iter()
            .map(|(n, p)| format!("+{n}: {p}"))
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// And the ceiling tracks reality rather than sitting above it.
///
/// A ceiling left well above the count stops being a ratchet: someone adds
/// three allocations and nothing says so. This fails when the count drops,
/// which is the prompt to lower it.
#[test]
fn the_ceiling_is_not_slack() {
    let n = allocating_sites(&render_inner_body()).len();
    assert!(
        n >= CEILING,
        "render_inner is down to {n} allocating constructs and the ceiling still says \
         {CEILING} -- lower it, or the next three additions go unnoticed"
    );
}

/// The counter finds things, so a pattern list that stopped matching
/// cannot read as "no allocations".
#[test]
fn the_counter_can_see() {
    let found = allocating_sites("let a = x.to_string();\nlet b: Vec<_> = y.collect();");
    assert_eq!(found.len(), 2, "the counter missed an obvious allocation");
    assert!(
        allocating_sites("// let a = x.to_string();").is_empty(),
        "the counter reads commented-out code as live"
    );
}
