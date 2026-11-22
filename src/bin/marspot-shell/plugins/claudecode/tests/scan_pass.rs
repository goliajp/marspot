//! What one scan pass reads: banners, cleared sessions, and every profile under its own config dir.

use super::*;

/// The banner is the only place a just-opened pane says its model.
///
/// The sample is the real thing off a screenshot (2026-08-11) — a
/// pane that had been open for a minute with a bare `P1` badge
/// while `Opus 5` sat on its own first line.
#[test]
fn a_startup_banner_names_the_model_before_the_transcript_can() {
    let screen = "\
 devops                                                    P1

   Claude Code v2.1.227
   Opus 5 (1M context) with high effort · Claude Max
   ~/workspace/goliajp/devops

";
    assert_eq!(rendered(parse_banner_model(screen)).as_deref(), Some("opus-5\u{b7}high"));

    // The remark after `·` is a plan, not a model.
    assert_eq!(
        rendered(parse_banner_model("Claude Code v2.0.1\nSonnet 4.5 · Claude Pro\n")).as_deref(),
        Some("sonnet-4-5"),
    );
    // No parenthesised remark, no separator — still just the name.
    assert_eq!(
        rendered(parse_banner_model("Claude Code v9\nFable 5\n")).as_deref(),
        Some("fable-5"),
    );
    // A screen with no banner says nothing rather than guessing —
    // most panes are not claude, and every one of them reaches
    // here while its badge is being decided.
    assert_eq!(parse_banner_model("$ ls -la\ntotal 0\n"), None);
    assert_eq!(parse_banner_model(""), None);
    // A version line with nothing under it must not read the next
    // screenful as a model name.
    assert_eq!(parse_banner_model("Claude Code v1\n\n\n\n"), None);
}

/// The banner as claudecode v2.1.232 actually draws it — logo
/// glyphs sharing the row, and an effort qualifier after the
/// version.  Both broke it, in opposite directions: the logo made
/// the whole line non-ASCII and produced *no* model, and without
/// the logo the qualifier produced `fable-5-with-hig`.
#[test]
fn a_banner_with_a_logo_names_the_model_and_its_effort() {
    let with_logo = "  ▛▀▜  Claude Code v2.1.232\n  ▙▄▟  Fable 5 with high effort · Claude Max\n";
    assert_eq!(rendered(parse_banner_model(with_logo)).as_deref(), Some("fable-5·high"));

    let bare = "Claude Code v2.1.232\nFable 5 with high effort · Claude Max\n";
    assert_eq!(rendered(parse_banner_model(bare)).as_deref(), Some("fable-5·high"));

    // The parenthesised form used to work only because dropping
    // everything from `(` also dropped the qualifier.  It has to
    // keep working now that the qualifier is read on purpose —
    // and the qualifier lives past the `(`, so the effort has to
    // come from the whole line, not the trimmed name.
    let paren = "Claude Code v2.1.227\nOpus 5 (1M context) with high effort · Claude Max\n";
    assert_eq!(rendered(parse_banner_model(paren)).as_deref(), Some("opus-5·high"));

    // A two-word family name survives; the version still ends it.
    let two_word = "Claude Code v9\nClaude Opus 5 with xhigh effort · Claude Max\n";
    assert_eq!(
        rendered(parse_banner_model(two_word)).as_deref(),
        Some("claude-opus-5·xhigh")
    );

    // A model with no effort setting says so by not saying it,
    // and the badge draws the model alone rather than inventing
    // a level for it.
    let no_effort = "Claude Code v9\nHaiku 4.5 · Claude Max\n";
    assert_eq!(rendered(parse_banner_model(no_effort)).as_deref(), Some("haiku-4-5"));

    // No model line at all is still None, not a guess.
    assert_eq!(rendered(parse_banner_model("Claude Code v9\n\n\n\n")).as_deref(), None);
}

/// `/clear` moves the session on; argv does not.
///
/// 2026-08-11 report: switching profile on `torajs` resumed a
/// session from hours earlier.  argv said `--resume f7a8a54b`
/// (where the process started); the transcript actually being
/// written was `e024458b`, three minutes newer.  Every downstream
/// use of the binding was therefore aimed at the wrong
/// conversation — the model in the badge, the reclamation resume
/// line, and the profile cycle.
#[test]
fn a_cleared_session_supersedes_the_one_named_in_argv() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let mk = |uuid: &str, mtime_off: u64| SessionInfo {
        session_id: uuid.to_string(),
        project_dir: "-p-torajs".into(),
        jsonl_path: PathBuf::from(format!("/tmp/{uuid}.jsonl")),
        last_mtime: t0 + Duration::from_secs(mtime_off),
        last_size: 1,
        has_conversation: true,
    };
    let mut ctx = WorkerCtx {
        projects_root: PathBuf::from("/fake"),
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };
    // argv's session, and the one `/clear` started after it.
    ctx.seen.insert(PathBuf::from("/tmp/argv.jsonl"), mk("argv-uuid", 10));
    ctx.seen.insert(PathBuf::from("/tmp/live.jsonl"), mk("live-uuid", 200));
    let none = std::collections::HashSet::new();

    assert_eq!(
        ctx.successor_session("-p-torajs", &none, t0, "argv-uuid").as_deref(),
        Some("live-uuid"),
        "the transcript being written is the session in front of the user"
    );

    // A session that stopped before this claude started belongs to
    // an earlier run and must not be adopted.
    let later_start = t0 + Duration::from_secs(500);
    assert_eq!(
        ctx.successor_session("-p-torajs", &none, later_start, "argv-uuid"),
        None,
        "nothing here has been written since this process started"
    );

    // Already spoken for by another pane — leave it alone.
    let mut taken = std::collections::HashSet::new();
    taken.insert("live-uuid".to_string());
    assert_eq!(
        ctx.successor_session("-p-torajs", &taken, t0, "argv-uuid"),
        None,
    );

    // Two files touched in the same instant at startup must not
    // make the binding flip: within the margin, argv keeps it.
    ctx.seen.insert(PathBuf::from("/tmp/tie.jsonl"), mk("tie-uuid", 12));
    ctx.seen.remove(&PathBuf::from("/tmp/live.jsonl"));
    assert_eq!(
        ctx.successor_session("-p-torajs", &none, t0, "argv-uuid"),
        None,
        "2 s apart is the same instant, not a succession"
    );

    // Another project's session is never a successor.
    let mut other = mk("other-uuid", 900);
    other.project_dir = "-p-elsewhere".into();
    ctx.seen.insert(PathBuf::from("/tmp/other.jsonl"), other);
    assert_eq!(
        ctx.successor_session("-p-torajs", &none, t0, "argv-uuid"),
        None,
    );
}

/// A pane on a non-default profile keeps both halves of its badge.
///
/// 2026-08-10 report: `torajs` showed `P3` and never `P3@model`.
/// The tag comes from the process's `CLAUDE_CONFIG_DIR`, so it was
/// right; the transcript walk was pinned to `~/.claude/projects`,
/// so for any profile but the default it found nothing and the
/// model half was simply absent.  Two answers about the same pane
/// have to come from the same place.
#[test]
fn a_second_profile_is_scanned_under_its_own_config_dir() {
    let tmp = TempDir::new("cc-profiles");
    let base = tmp.path().to_path_buf();
    // Two profiles, the same project open under each, a different
    // session id in each — exactly the shape on the machine.
    let mk = |profile: &str, uuid: &str| -> PathBuf {
        let dir = base.join(profile).join("projects").join("-p-torajs");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{uuid}.jsonl"));
        fs::write(&p, format!("{{\"sessionId\":\"{uuid}\"}}\n")).unwrap();
        p
    };
    let default_path = mk(".claude", "11111111-1111-1111-1111-111111111111");
    let third_path = mk(".claude-profile-3", "33333333-3333-3333-3333-333333333333");

    let mut ctx = WorkerCtx {
        projects_root: base.join(".claude").join("projects"),
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };
    let mut logs = Vec::new();
    // The default root alone sees only the default profile's file.
    ctx.refresh_seen(
        [(ctx.projects_root.clone(), "-p-torajs")]
            .iter()
            .map(|(r, d)| (r.as_path(), *d))
            .collect::<Vec<_>>()
            .into_iter(),
        &mut logs,
    );
    assert!(ctx.seen.contains_key(&default_path));
    assert!(
        !ctx.seen.contains_key(&third_path),
        "the default root cannot see another profile — that is the bug"
    );

    // Both roots, one pass — which is how the scan calls it: every
    // live pane contributes its own root, and `seen` is pruned to
    // what this pass walked.  The same project under two profiles
    // is two directories and two files, not one walked twice.
    let default_root = base.join(".claude").join("projects");
    let third_root = base.join(".claude-profile-3").join("projects");
    ctx.refresh_seen(
        [
            (default_root.as_path(), "-p-torajs"),
            (third_root.as_path(), "-p-torajs"),
        ]
        .into_iter(),
        &mut logs,
    );
    assert!(
        ctx.seen.contains_key(&third_path),
        "a pane on profile 3 must find its own transcript"
    );
    assert!(
        ctx.seen.contains_key(&default_path),
        "and the default profile's pane keeps its own"
    );
}
