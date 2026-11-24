//! The arguments the update path runs a staged core with before it
//! replaces the running one.

/// Answer `--version` or `--warm-shaders` and say so.  Both come before
/// anything touches the machine: a probe that migrated state, opened
/// the log or read the environment would be a probe with side effects.
///
/// `--version` exists to make the kernel exec this file while the
/// outgoing core is still on screen, so the Gatekeeper assessment is
/// paid then instead of during the swap's blackout.
///
/// `--warm-shaders` does the same for the shaders.  The system keeps
/// compiled shaders per binary, so a new core's first start compiles
/// them from source, with no core drawing while it does.  Run here, on
/// the staged binary, that compile happens behind the old core's back.
/// Prints the microseconds the library and the pipelines took.
pub(super) fn answer(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("--version") => println!("{}", env!("MARSPOT_VERSION_CORE")),
        Some("--warm-shaders") => match marspot::render_metal::pipelines::warm_shaders() {
            Ok((library_us, pipelines_us)) => println!("{library_us} {pipelines_us}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        _ => return false,
    }
    true
}

/// Each mask asked for in `list` (`symbol:point_px,...`), written to
/// stdout as a length and an `IconMask` payload.
pub(super) fn draw_icons(list: &str) {
    use std::io::Write;
    let mut out = Vec::new();
    for item in list.split(',') {
        let Some((s, p)) = item.split_once(':') else { continue };
        let (Ok(symbol), Ok(point_px)) = (s.parse::<u16>(), p.parse::<u16>()) else { continue };
        let Some((side, mask)) = marspot::ui::system::macos::icons::draw_for_core(symbol, point_px) else { continue };
        let payload = marspot::shell_proto::encode_icon_mask(symbol, point_px, side, &mask);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
    }
    let _ = std::io::stdout().write_all(&out);
}
