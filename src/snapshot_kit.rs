//! Pixel-snapshot machinery shared by every test that renders
//! offscreen and compares with a committed PNG: the mode switch
//! (`MARSPOT_FONT_SNAPSHOT`), the SSIM gate, and the PNG reader and
//! writer.  A baseline's own directory decides whether an unset
//! variable compares or skips, so each family of snapshots keeps its
//! baselines in one place.

/// Phase 9 (extended) — pure-Rust SSIM (Structural Similarity
/// Index) over the luma channel.  No new deps — `png` is already
/// in the workspace, the rest is arithmetic over RGBA bytes.
///
/// 8×8 non-overlapping windows; standard SSIM weights
/// (`k1 = 0.01`, `k2 = 0.03`, `L = 255`).  Luma per pixel uses
/// BT.709 weights (`0.2126 R + 0.7152 G + 0.0722 B`).  Returns
/// the unweighted mean SSIM across every window — 1.0 = identical,
/// `< 0.98` is the project's visual-regression gate.
///
/// Input bytes are RGBA (the snapshot harness already does the
/// BGRA→RGBA swap before calling this).  Sizes must agree;
/// caller checks dims via the `png::Decoder` header.
pub(crate) fn ssim_luma(a: &[u8], b: &[u8], w: u32, h: u32) -> f64 {
    const W: usize = 8;
    let stride = (w as usize) * 4;
    let luma = |bytes: &[u8], x: usize, y: usize| -> f64 {
        let i = y * stride + x * 4;
        0.2126 * bytes[i] as f64
            + 0.7152 * bytes[i + 1] as f64
            + 0.0722 * bytes[i + 2] as f64
    };
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let mut total: f64 = 0.0;
    let mut count: usize = 0;
    let mut y = 0;
    while y + W <= h as usize {
        let mut x = 0;
        while x + W <= w as usize {
            let mut mu_a = 0.0;
            let mut mu_b = 0.0;
            for dy in 0..W {
                for dx in 0..W {
                    mu_a += luma(a, x + dx, y + dy);
                    mu_b += luma(b, x + dx, y + dy);
                }
            }
            let n = (W * W) as f64;
            mu_a /= n;
            mu_b /= n;
            let mut var_a = 0.0;
            let mut var_b = 0.0;
            let mut cov = 0.0;
            for dy in 0..W {
                for dx in 0..W {
                    let da = luma(a, x + dx, y + dy) - mu_a;
                    let db = luma(b, x + dx, y + dy) - mu_b;
                    var_a += da * da;
                    var_b += db * db;
                    cov += da * db;
                }
            }
            var_a /= n;
            var_b /= n;
            cov /= n;
            let s = ((2.0 * mu_a * mu_b + C1) * (2.0 * cov + C2))
                / ((mu_a * mu_a + mu_b * mu_b + C1) * (var_a + var_b + C2));
            total += s;
            count += 1;
            x += W;
        }
        y += W;
    }
    if count == 0 { 1.0 } else { total / count as f64 }
}

/// Where the font and dev-panel baselines live.
pub(crate) const FONT_SNAPSHOT_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/bench/font-rendering/snapshots");
/// Where the panel baselines live.
pub(crate) const UI_SNAPSHOT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/bench/ui/snapshots");

/// A baseline's name under the theme in force: `<name>` in the dark
/// theme, `<name>.light` and `<name>.contrast` in the others.
pub(crate) fn themed(name: &str) -> String {
    match marspot_term::palette::current_id() {
        marspot_term::palette::ThemeId::Dark => name.to_string(),
        other => format!("{name}.{}", other.as_str()),
    }
}

/// Run a snapshot test with `theme` in force.  It is put in force the
/// way the app does it, through the settings, so that nothing the
/// test goes on to read from them puts the dark theme back.
pub(crate) fn in_theme(theme: marspot_term::palette::ThemeId, shot: fn()) {
    crate::settings::set_for_test(crate::settings::Settings {
        appearance_theme: Some(theme),
        ..Default::default()
    });
    shot();
}

/// The same scenes under the light and the high-contrast theme: each
/// `light, contrast = scene;` is two tests that run `scene` again.
macro_rules! snapshots_in_other_themes {
    ($($light:ident, $contrast:ident = $shot:ident;)*) => {$(
        #[test]
        fn $light() {
            crate::snapshot_kit::in_theme(marspot_term::palette::ThemeId::Light, $shot);
        }
        #[test]
        fn $contrast() {
            crate::snapshot_kit::in_theme(marspot_term::palette::ThemeId::Contrast, $shot);
        }
    )*};
}
pub(crate) use snapshots_in_other_themes;

/// What a pixel snapshot test does this run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SnapshotMode {
    /// Compare against the committed baseline; a missing one fails.
    Check,
    /// Write the baseline (`MARSPOT_FONT_SNAPSHOT=1`).
    Record,
}

/// `MARSPOT_FONT_SNAPSHOT`: unset or `check` compares, `1`
/// records, `0` skips.  Unset skips only where there are no
/// baselines to compare with, and says so.  `None` = skip.
pub(crate) fn snapshot_mode_in(dir: &std::path::Path) -> Option<SnapshotMode> {
    let var = std::env::var("MARSPOT_FONT_SNAPSHOT").ok();
    let mode = snapshot_mode_for(var.as_deref(), dir.is_dir());
    if mode.is_none() && var.is_none() {
        eprintln!("SKIPPED: no baselines at {}", dir.display());
    }
    mode
}

/// The mode for the baseline at `path`: its directory is the one
/// that has to exist for an unset variable to compare.
fn mode_for_baseline(path: &std::path::Path) -> Option<SnapshotMode> {
    snapshot_mode_in(path.parent().unwrap_or(path))
}

/// The decision itself, apart from the environment: the snapshot
/// tests read the variable while this one's test would be changing
/// it, and a `1` read by the wrong test overwrites a baseline.
fn snapshot_mode_for(var: Option<&str>, have_baselines: bool) -> Option<SnapshotMode> {
    match var {
        Some("1") => Some(SnapshotMode::Record),
        Some("check") => Some(SnapshotMode::Check),
        Some("0") => {
            eprintln!("SKIPPED: MARSPOT_FONT_SNAPSHOT=0");
            None
        }
        Some(other) => panic!("MARSPOT_FONT_SNAPSHOT={other:?}: expected 1, check or 0"),
        None if !have_baselines => None,
        None => Some(SnapshotMode::Check),
    }
}

/// With baselines in the tree, a run that says nothing compares:
/// skipping is asked for, never the default.
#[test]
fn snapshots_are_compared_unless_told_otherwise() {
    assert!(std::path::Path::new(FONT_SNAPSHOT_DIR).is_dir(), "the baselines are in the tree");
    assert_eq!(snapshot_mode_for(None, true), Some(SnapshotMode::Check), "unset");
    assert_eq!(snapshot_mode_for(Some("check"), true), Some(SnapshotMode::Check));
    assert_eq!(snapshot_mode_for(Some("1"), true), Some(SnapshotMode::Record));
    assert_eq!(snapshot_mode_for(Some("0"), true), None);
    assert_eq!(snapshot_mode_for(None, false), None, "nothing to compare with");
    assert_eq!(
        snapshot_mode_for(Some("check"), false),
        Some(SnapshotMode::Check),
        "asked to compare, it compares, and a missing baseline fails"
    );
}

/// Phase 9 — load a baseline PNG as raw RGBA bytes (matches what
/// the snapshot harness produces post-channel-swap), or say why
/// there is none to compare with.
fn load_baseline_rgba(
    path: &std::path::Path,
    expected_w: u32,
    expected_h: u32,
) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open it: {e}"))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().map_err(|e| format!("not a png: {e}"))?;
    let info = reader.info();
    if info.width != expected_w || info.height != expected_h {
        return Err(format!(
            "it is {}x{}, the render is {expected_w}x{expected_h}",
            info.width, info.height
        ));
    }
    let mut buf = vec![0u8; reader.output_buffer_size().ok_or("no buffer size")?];
    let frame = reader.next_frame(&mut buf).map_err(|e| format!("bad frame: {e}"))?;
    buf.truncate(frame.buffer_size());
    // Force RGBA shape — decoder honours the encoder's RGBA8
    // setup from `write_snapshot_png` below.
    if frame.color_type != png::ColorType::Rgba {
        return Err(format!("it is {:?}, not RGBA", frame.color_type));
    }
    Ok(buf)
}

/// The SSIM gate has to be able to fail.
///
/// Every snapshot reading is a number near 1.0, and a comparison
/// that silently compares an image with itself reads exactly the
/// same.  Two images that differ have to come out under the
/// threshold the gate uses.
#[test]
fn ssim_separates_two_different_images() {
    let (w, h) = (64u32, 64u32);
    let n = (w * h * 4) as usize;
    let flat = vec![128u8; n];
    assert_eq!(ssim_luma(&flat, &flat, w, h), 1.0, "an image against itself");

    // Same mean, different structure: half the pixels dark, half
    // bright.  A gate that only looked at averages would miss it.
    let mut split = vec![0u8; n];
    for (i, px) in split.chunks_mut(4).enumerate() {
        let v = if (i / w as usize).is_multiple_of(2) { 30 } else { 226 };
        px.copy_from_slice(&[v, v, v, 255]);
    }
    let s = ssim_luma(&flat, &split, w, h);
    assert!(s < 0.98, "two different images must fall under the gate, got {s}");
}

/// Write a baseline PNG — but only when recording one.
///
/// These tests used to write their own baseline on every run,
/// including the run that had just compared against it.  A render
/// that drifted therefore updated the file it was being judged
/// by, and the SSIM gate could never fail twice.  Recording is now
/// something you ask for.
pub(crate) fn write_snapshot_png(path: &std::path::Path, rgba: &[u8], w_px: u32, h_px: u32) {
    if mode_for_baseline(path) != Some(SnapshotMode::Record) {
        return;
    }
    encode_png(&baseline_for(path), rgba, w_px, h_px);
}

/// The macOS major version, `27` on 27.0.1.
fn macos_major() -> String {
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    let rc = unsafe {
        libc::sysctlbyname(c"kern.osproductversion".as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0)
    };
    assert_eq!(rc, 0, "kern.osproductversion");
    let v = std::str::from_utf8(&buf[..len]).unwrap_or_default().trim_end_matches('\0');
    v.split('.').next().unwrap_or(v).to_string()
}

/// `name.png` → `name.macos<N>.png`: the baseline for this macOS
/// release alone.  A snapshot that draws with system fonts can
/// look different on another release -- the CJK fallback fonts do
/// between macOS 15 and 27 -- and gets a baseline per release; the
/// rest share one.
fn versioned(path: &std::path::Path) -> std::path::PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    path.with_file_name(format!("{stem}.macos{}.png", macos_major()))
}

/// The baseline this machine is held to: this release's own when
/// there is one, the shared one otherwise.
fn baseline_for(path: &std::path::Path) -> std::path::PathBuf {
    let v = versioned(path);
    if v.is_file() { v } else { path.to_path_buf() }
}

fn encode_png(path: &std::path::Path, rgba: &[u8], w_px: u32, h_px: u32) {
    let file = std::fs::File::create(path).expect("create png");
    let buf = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(buf, w_px, h_px);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(rgba).expect("png data");
}

/// The SSIM gate, run after each snapshot test has its fresh RGBA
/// bytes and before anything is written: under `Check` the render
/// has to reach `threshold` against the committed baseline, and a
/// baseline that is missing or the wrong size fails -- it is not
/// a pass.
pub(crate) fn assert_snapshot_ssim(
    rgba: &[u8],
    baseline_path: &std::path::Path,
    w_px: u32,
    h_px: u32,
    threshold: f64,
) {
    if mode_for_baseline(baseline_path) != Some(SnapshotMode::Check) {
        return;
    }
    let path = baseline_for(baseline_path);
    let baseline = load_baseline_rgba(&path, w_px, h_px).unwrap_or_else(|why| {
        let kept = keep_render(baseline_path, rgba, w_px, h_px);
        panic!("no usable baseline at {}: {why}; record one with MARSPOT_FONT_SNAPSHOT=1{kept}", path.display())
    });
    let s = ssim_luma(rgba, &baseline, w_px, h_px);
    eprintln!("[ssim] {} = {:.4} (threshold {:.4})", path.display(), s, threshold);
    if s < threshold {
        let kept = keep_render(baseline_path, rgba, w_px, h_px);
        panic!("SSIM {s:.4} < {threshold:.4} — visual regression vs {}{kept}", path.display());
    }
}

/// A render that did not match, written under `MARSPOT_SNAPSHOT_OUT`
/// with the name this release's own baseline would have, so it can
/// be looked at -- and, when it is right, committed as that
/// baseline.  CI uploads the directory when the tests fail.
fn keep_render(baseline_path: &std::path::Path, rgba: &[u8], w_px: u32, h_px: u32) -> String {
    let Some(dir) = std::env::var_os("MARSPOT_SNAPSHOT_OUT") else { return String::new() };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("create MARSPOT_SNAPSHOT_OUT");
    let out = dir.join(versioned(baseline_path).file_name().expect("a file name"));
    encode_png(&out, rgba, w_px, h_px);
    format!("; this render is at {}", out.display())
}
