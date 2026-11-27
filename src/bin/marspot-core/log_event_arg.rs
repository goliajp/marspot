//! The one command line the core answers without booting.

/// `install-local.sh`'s `sup_log` shells out to
/// `marspot-core --log-event <tag> <detail>` so supervisor-side
/// decisions land in the same marspot.log as everything else.
///
/// That mechanism was added after an install-local bug took down nine
/// live sessions with no log line explaining why — and it had never
/// once worked.  Nothing parsed argv, so every call fell straight into
/// the boot path and panicked in `env_required` on the missing surface
/// id: three dead processes per install (the script tries three binary
/// paths) and not one line written.
///
/// Hence this is the first thing `main` does, before any env read.
pub(super) fn parse_log_event(args: &[String]) -> Option<(&str, String)> {
    if args.first().map(String::as_str) != Some("--log-event") {
        return None;
    }
    // A call with no tag is still a call — log it under a placeholder
    // rather than falling through to the boot path and panicking,
    // which is the failure this whole function exists to end.
    let tag = args.get(1).map(String::as_str).unwrap_or("untagged");
    Some((tag, args.get(2..).unwrap_or(&[]).join(" ")))
}
