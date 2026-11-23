//! Reading which model and effort a pane runs at: status line, hook record, transcript, banner.

use super::*;

/// A real status-line payload, captured from claude 2.1.239 by
/// pointing `--settings` at a command that dumps stdin.  Kept
/// verbatim so a change in the payload's shape shows up here
/// rather than as a badge that quietly stops updating.
const REAL_STATUSLINE_PAYLOAD: &str = concat!(
    r#"{"session_id":"b0b0b0b0-1111-2222-3333-444444444444","#,
    r#""transcript_path":"/Users/x/.claude-profile-3/projects/-p/"#,
    r#"b0b0b0b0-1111-2222-3333-444444444444.jsonl","#,
    r#""cwd":"/Users/x/p","effort":{"level":"high"},"#,
    r#""model":{"id":"claude-opus-5[1m]","display_name":"Opus 5 (1M context)"},"#,
    r#""version":"2.1.239","exceeds_200k_tokens":false}"#,
);

#[test]
fn a_status_line_payload_yields_the_session_and_its_model() {
    let (sid, transcript, badge) =
        statusline_fields(REAL_STATUSLINE_PAYLOAD).expect("payload parses");
    assert_eq!(sid, "b0b0b0b0-1111-2222-3333-444444444444");
    assert!(transcript.ends_with("b0b0b0b0-1111-2222-3333-444444444444.jsonl"));
    // `display_name`, not `id`: the id's `[1m]` suffix is not a
    // model name and `short_model` throws the whole thing out.
    assert_eq!(badge.model, "opus-5");
    assert_eq!(short_model("claude-opus-5[1m]"), "");
    // The same payload states the effort, and the badge draws
    // both halves.
    assert_eq!(badge.effort.as_deref(), Some("high"));
    assert_eq!(badge.render(), "opus-5\u{b7}high");
}

#[test]
fn a_status_line_payload_that_names_a_path_is_refused() {
    let bad = REAL_STATUSLINE_PAYLOAD
        .replace("b0b0b0b0-1111-2222-3333-444444444444\",", "../../etc/passwd\",");
    assert_eq!(statusline_fields(&bad), None);
}

/// The reported bug, as a test.
///
/// The transcript's newest word on the subject is `opus-5` — the
/// model that served the last turn.  The user has since switched
/// to Fable, which claude knows and the transcript will not
/// record until the session answers again.  Before the hook the
/// badge read `opus-5` and stayed there; now claude's own report
/// A pane whose session file does not exist yet still shows all
/// three halves — profile, model, effort.
///
/// claude writes that file on its first turn, so for the whole
/// window in which the user is looking at a pane they just opened
/// there is nothing to tail.  The status-line hook has run by then
/// (measured: ~2 s after start), and its record is the only source
/// that carries the effort as well — the banner line prints
/// `Opus 5.5 (1M context) · Claude Max`, model and no more.
#[test]
fn a_pane_with_no_session_file_yet_still_shows_model_and_effort() {
    let sandbox = sandbox_state_dir();
    let root = sandbox.path().to_path_buf();
    let uuid = "d794e66f-bc9b-48e8-a114-70c5fc996678";
    let push = model_push_dir();
    fs::create_dir_all(&push).unwrap();
    // The shape the hook writes: model, transcript, effort, pids.
    fs::write(
        push.join(uuid),
        format!("opus-5-5\n{}/projects/x/{uuid}.jsonl\nmedium\npids=20809,1492\n", root.display()),
    )
    .unwrap();

    let mut ctx = ctx_for_test(root.join("projects"));
    assert_eq!(
        rendered(ctx.model_for_pane(None, uuid, None, 20809, 436)).as_deref(),
        Some("opus-5-5\u{b7}medium"),
        "the hook's record carries the effort the banner cannot"
    );
}

/// The model half must not blink.
///
/// Its sources answer None on some ticks — the banner read is
/// rate-limited to once every 3 s and the scan runs every 2 — so a
/// badge that takes None at face value alternates between `P7` and
/// `P7@opus-5-5` for as long as the pane has no session file
/// (2026-09-23 report, 20 badge changes in one log tail).
#[test]
fn the_model_half_survives_a_tick_that_could_not_read_it() {
    let sandbox = sandbox_state_dir();
    let root = sandbox.path().to_path_buf();
    let uuid = "d794e66f-bc9b-48e8-a114-70c5fc996678";
    let mut ctx = ctx_for_test(root.join("projects"));

    // Tick 1: the hook's record was found.
    let first = rendered(ctx.model_for_pane(
        None,
        uuid,
        Some(ModelBadge::new("opus-5-5".into(), Some("medium".into()))),
        20809,
        436,
    ));
    assert_eq!(first.as_deref(), Some("opus-5-5\u{b7}medium"));

    // Tick 2: nothing readable — no record on disk, no banner
    // (no pane registry in a test), and the pane is the same one.
    assert_eq!(
        rendered(ctx.model_for_pane(None, uuid, None, 20809, 436)).as_deref(),
        Some("opus-5-5\u{b7}medium"),
        "a tick that could not read it must not blank the badge"
    );
    // A different pane borrows nothing.
    assert_eq!(rendered(ctx.model_for_pane(None, "", None, 1, 999)), None);
}

/// is what the badge shows.
#[test]
fn claudes_own_report_beats_the_transcripts_last_word() {
    let sandbox = sandbox_state_dir();
    let root = sandbox.path().to_path_buf();

    let dir = root.join("projects");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("11111111-2222-3333-4444-555555555555.jsonl");
    fs::write(
        &path,
        "{\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"x\":1}\n",
    )
    .unwrap();

    let mut ctx = WorkerCtx {
        projects_root: dir.clone(),
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };
    assert_eq!(rendered(ctx.model_for(&path, 111, 0)).as_deref(), Some("opus-5"));

    let push = model_push_dir();
    fs::create_dir_all(&push).unwrap();
    fs::write(
        push.join("11111111-2222-3333-4444-555555555555"),
        format!("fable-5\n{}\n", path.display()),
    )
    .unwrap();

    assert_eq!(
        rendered(ctx.model_for(&path, 111, 0)).as_deref(),
        Some("fable-5"),
        "the badge follows claude, not the last completed turn"
    );

    // And a session claude has said nothing about still reads its
    // transcript rather than borrowing someone else's record.
    let other = dir.join("99999999-2222-3333-4444-555555555555.jsonl");
    fs::write(
        &other,
        "{\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"x\":1}\n",
    )
    .unwrap();
    assert_eq!(rendered(ctx.model_for(&other, 222, 0)).as_deref(), Some("sonnet-5"));

}

/// The transcript names the effort on the same record as the
/// model, so the badge gets both halves from one line.
#[test]
fn an_assistant_record_names_the_effort_it_ran_at() {
    let tmp = TempDir::new("cc-effort-tail-test");
    let dir = tmp.path().to_path_buf();
    let path = dir.join("s.jsonl");

    // Shape taken from a real record: `effort` is a sibling of the
    // record's own uuid, not a member of `message`.
    fs::write(
        &path,
        "{\"role\":\"assistant\",\"model\":\"claude-opus-5\",\
         \"uuid\":\"u\",\"effort\":\"high\"}\n",
    )
    .unwrap();
    assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("opus-5\u{b7}high"));

    // A model with no effort setting: the record simply does not
    // carry one, and the badge says the model alone.
    fs::write(
        &path,
        "{\"role\":\"assistant\",\"model\":\"claude-haiku-4-5\",\"uuid\":\"u\"}\n",
    )
    .unwrap();
    assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("haiku-4-5"));

    // `/model` output names the model and nothing else.  Saying
    // no effort is the honest reading — the model just changed,
    // and what it will run at is the next turn's news.
    fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"content\":\
         \"<local-command-stdout>Set model to \u{1b}[1mFable 5\u{1b}[22m</local-command-stdout>\"}}\n",
    )
    .unwrap();
    assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("fable-5"));
}

/// A record written before the effort line existed still reads.
///
/// The hook and the badge are separate binaries during an update
/// — the running shell reads what a just-replaced one wrote, and
/// the other way round — so the file's shape has to tolerate both
/// generations.
#[test]
fn a_model_record_without_an_effort_line_still_reads() {
    let sandbox = sandbox_state_dir();
    let root = sandbox.path().to_path_buf();
    let push = model_push_dir();
    fs::create_dir_all(&push).unwrap();
    let jsonl = root.join("11111111-1111-1111-1111-111111111111.jsonl");
    let name = "11111111-1111-1111-1111-111111111111";

    // Two lines: what 0.7.116 wrote.
    fs::write(push.join(name), "opus-5\n/some/path.jsonl\n").unwrap();
    assert_eq!(
        pushed_model(&jsonl),
        Some((ModelBadge::new("opus-5".into(), None), EffortSaid::No)),
        "two lines is a record that could not say, not one saying no"
    );

    // Three lines, third blank: claude reported no effort, and
    // that is an answer — nothing falls through to fill it in.
    fs::write(push.join(name), "opus-5\n/some/path.jsonl\n\n").unwrap();
    assert_eq!(
        pushed_model(&jsonl),
        Some((ModelBadge::new("opus-5".into(), None), EffortSaid::Yes))
    );

    // Three lines with one.
    fs::write(push.join(name), "opus-5\n/some/path.jsonl\nxhigh\n").unwrap();
    assert_eq!(
        pushed_model(&jsonl).map(|(b, _)| b.render()).as_deref(),
        Some("opus-5\u{b7}xhigh")
    );
}

/// A parked pane still gains the effort it never got pushed.
///
/// The reported case: claude re-runs the status-line hook only
/// when it redraws, so a pane nobody is in keeps whatever record
/// it last wrote — here one from a build with no effort line at
/// all.  Short-circuiting on it would hide the effort the pane's
/// own transcript is stating plainly, for as long as the pane
/// stays parked.
#[test]
fn a_record_too_old_to_carry_the_effort_takes_it_from_the_transcript() {
    let sandbox = sandbox_state_dir();
    let root = sandbox.path().to_path_buf();
    let dir = root.join("projects");
    fs::create_dir_all(&dir).unwrap();
    let name = "22222222-2222-2222-2222-222222222222";
    let path = dir.join(format!("{name}.jsonl"));
    fs::write(
        &path,
        "{\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"effort\":\"high\"}\n",
    )
    .unwrap();
    let push = model_push_dir();
    fs::create_dir_all(&push).unwrap();

    let mut ctx = WorkerCtx {
        statusline_state: None,
        projects_root: dir.clone(),
        caches: Default::default(),
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };

    // The old two-line shape.
    fs::write(push.join(name), format!("opus-5\n{}\n", path.display())).unwrap();
    assert_eq!(
        rendered(ctx.model_for(&path, 111, 0)).as_deref(),
        Some("opus-5\u{b7}high"),
        "the pushed model stands, and the transcript fills the half it could not carry"
    );

    // Claude having actually reported no effort is different, and
    // is left alone — the transcript's older turn does not get to
    // overrule what claude said a moment ago.
    fs::write(push.join(name), format!("opus-5\n{}\n\n", path.display())).unwrap();
    ctx.last_model.clear();
    assert_eq!(rendered(ctx.model_for(&path, 111, 0)).as_deref(), Some("opus-5"));

    // Nor is an effort borrowed across a model change: the
    // transcript's turn was served by something else.
    fs::write(push.join(name), format!("fable-5\n{}\n", path.display())).unwrap();
    ctx.last_model.clear();
    assert_eq!(rendered(ctx.model_for(&path, 111, 0)).as_deref(), Some("fable-5"));

}

/// The profile-switch case, from the other side.
///
/// The transcript still says `opus-5` — the model the profile that
/// was just cycled away from served with — and the fence correctly
/// refuses to read it.  The answer is on the pane's own screen:
/// the resumed claude reprinted its banner and the banner names
/// Fable.  Before the reorder `last_model` answered first and the
/// badge kept saying `opus-5` for as long as the pane stayed
/// parked.
#[test]
fn a_resumed_pane_reads_its_banner_before_falling_back() {
    use marspot_term::session_registry::{write_session_entry, SessionEntry};
    let sandbox = sandbox_state_dir();
    let root = sandbox.path().to_path_buf();

    let dir = root.join("projects");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.jsonl");
    fs::write(
        &path,
        "{\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"x\":1}\n",
    )
    .unwrap();

    let sid = 4242u64;
    write_session_entry(&SessionEntry {
        id: sid,
        pid: 1,
        socket: root.join("s.sock"),
        cols: 100,
        rows: 30,
        title: "t".into(),
        cwd: "/tmp".into(),
        proto_version: marspot_term::shell_proto::PROTO_VERSION,
        created_at_unix: 0,
        shm_name: String::new(),
        shell_child_pid: 0,
        build: String::new(),
        stamps_regions: false,
        shadow_of: 0,
    })
    .unwrap();
    fs::write(
        marspot_term::paths::sessions_dir().join(sid.to_string()).join("bytelog"),
        "  \u{2599}\u{2584}\u{259f}  Claude Code v2.1.239\r\n\
         \u{2599}\u{2584}\u{259f}  Fable 5 with high effort \u{b7} Claude Max\r\n\
         ~/workspace/goliajp/marspot\r\n",
    )
    .unwrap();

    let mut ctx = WorkerCtx {
        projects_root: dir.clone(),
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };
    // First sighting: the transcript is this process's own.
    assert_eq!(rendered(ctx.model_for(&path, 111, sid)).as_deref(), Some("opus-5"));

    // The switch: new pid, fence at end of file, and the records a
    // resumed process writes at startup name no model.
    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(f, "{{\"type\":\"mode\",\"mode\":\"default\"}}").unwrap();
    drop(f);

    assert_eq!(
        rendered(ctx.model_for(&path, 222, sid)).as_deref(),
        Some("fable-5·high"),
        "the banner on the pane's own screen outranks the model \
         the previous profile happened to leave behind, and it \
         names the effort while it is there"
    );

}

/// The profile-switch case.
///
/// Switching profiles re-runs `claude<N> --resume <uuid>`, so the
/// same jsonl keeps growing and its newest assistant record still
/// names the *previous* profile's model.  Reading it back made the
/// badge state the wrong model until the next turn.  Fencing the
/// read at the switch offset makes it report nothing instead —
/// the badge then omits `@model`, which is honest.
#[test]
fn model_read_is_fenced_at_a_profile_switch() {
    let tmp = TempDir::new("cc-model-fence-test");
    let dir = tmp.path().to_path_buf();
    let path = dir.join("s.jsonl");

    // Pre-switch history: P1 was answering with fable-5.
    let pre = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-fable-5\"}}\n";
    fs::write(&path, pre).unwrap();
    let switch_at = fs::metadata(&path).unwrap().len();

    // Unfenced, the old model is what you get — this is the bug.
    assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("fable-5"));

    // Fenced at the switch: nothing to report yet.
    assert_eq!(
        tail_model_short(&path, switch_at),
        None,
        "a fenced read must not surface the previous profile's model"
    );

    // The new profile answers a turn; now it reports again.
    let post = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-opus-4-8\"}}\n";
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    use std::io::Write;
    f.write_all(post.as_bytes()).unwrap();
    drop(f);
    assert_eq!(
        rendered(tail_model_short(&path, switch_at)).as_deref(),
        Some("opus-4-8"),
        "post-switch records must still be read"
    );

}
