//! The core must not know what a graphics API is.
//!
//! The boundary is supposed to be mechanical — this crate depends on
//! nothing, so nothing can leak in through a dependency — but a type
//! named after one API, or a comment that assumes one, leaks it just
//! as well and compiles fine.  So: read the source and look.

use std::fs;
use std::path::Path;

const FORBIDDEN: &[&str] = &[
    "MTLPixelFormat", "MTLBuffer", "MTLTexture", "MTLDevice",
    "D3D12", "DXGI", "ID3D",
    "VkBuffer", "VkImage", "VkDevice", "vkCreate",
    "objc2", "CoreGraphics", "CGRect",
];

fn offenders(source: &str) -> Vec<&'static str> {
    FORBIDDEN.iter().copied().filter(|w| source.contains(w)).collect()
}

fn walk(dir: &Path, out: &mut Vec<(String, Vec<&'static str>)>) {
    for entry in fs::read_dir(dir).expect("read crate source") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path).expect("read source file");
            let hits = offenders(&text);
            if !hits.is_empty() {
                out.push((path.display().to_string(), hits));
            }
        }
    }
}

#[test]
fn no_graphics_api_vocabulary_in_the_core() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    walk(&src, &mut found);
    assert!(
        found.is_empty(),
        "a backend's vocabulary reached the core: {found:?}"
    );
}

#[test]
fn the_check_can_fail() {
    // A scan that has never seen a hit is a scan nobody has tested.
    assert_eq!(
        offenders("let fmt = MTLPixelFormat::BGRA8Unorm;"),
        vec!["MTLPixelFormat"]
    );
    assert!(offenders("let x = 1;").is_empty());
}

#[test]
fn the_scan_actually_reads_files() {
    // If the walk stopped finding files — a renamed directory, a
    // changed extension — the first test would pass by seeing nothing,
    // which is the same answer as being clean.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut seen = 0usize;
    fn count(dir: &Path, seen: &mut usize) {
        for entry in fs::read_dir(dir).expect("read crate source") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                count(&path, seen);
            } else if path.extension().is_some_and(|e| e == "rs") {
                *seen += 1;
            }
        }
    }
    count(&src, &mut seen);
    assert!(seen >= 4, "expected the core's modules, found {seen} source files");
}
