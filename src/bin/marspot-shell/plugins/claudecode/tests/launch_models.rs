//! The model every claude marspot starts is named with, and where it is read from.

use super::dormant::dormant_choice;
use super::launch_model::{self, Source};
use super::*;

fn record(uuid: &str, transcript: &str, id: &str) {
    let push = model_push_dir();
    fs::create_dir_all(&push).unwrap();
    fs::write(push.join(uuid), format!("x\n{transcript}\nmedium\npids=1\nid={id}\n")).unwrap();
}

/// Nearest source first; a value claude would not take is passed over;
/// nothing known is the default, said as such.
#[test]
fn launch_model_takes_the_first_source_that_names_one() {
    let none = || None;
    let bad = || Some("opus; rm -rf ~".to_string());
    let opus = || Some("claude-opus-5-5".to_string());
    let fable = || Some("claude-fable-5-1[1m]".to_string());
    assert_eq!(
        launch_model::first_known(&[(Source::Session, &none), (Source::Pane, &fable), (Source::Account, &opus)]),
        ("claude-fable-5-1[1m]".to_string(), Source::Pane)
    );
    assert_eq!(launch_model::first_known(&[(Source::Session, &bad), (Source::Account, &opus)]).1, Source::Account);
    assert_eq!(
        launch_model::first_known(&[(Source::Session, &none)]),
        (launch_model::DEFAULT_MODEL.to_string(), Source::Default)
    );
}

/// Claude's own log names the model of every answer; an error record's
/// `<synthetic>` is not one.
#[test]
fn launch_model_reads_the_newest_answer_in_the_transcript() {
    let dir = TempDir::new("launch-model-transcript");
    let t = dir.join("u.jsonl");
    let answer = |m: &str| format!(r#"{{"type":"assistant","message":{{"role":"assistant","model":"{m}"}}}}"#);
    fs::write(&t, [answer("claude-opus-5-5"), answer("claude-fable-5-1"), answer("<synthetic>")].join("\n")).unwrap();
    assert_eq!(launch_model::transcript_model(&t).as_deref(), Some("claude-fable-5-1"));
    fs::write(&t, "{\"type\":\"user\"}\n").unwrap();
    assert_eq!(launch_model::transcript_model(&t), None);
    // the hook's record comes first, the transcript after it
    let _sandbox = sandbox_state_dir();
    fs::write(&t, answer("claude-opus-5-5")).unwrap();
    assert_eq!(launch_model::session_model("u-x", Some(&t)).as_deref(), Some("claude-opus-5-5"));
    record("u-x", "/t/u-x.jsonl", "claude-fable-5-1[1m]");
    assert_eq!(launch_model::session_model("u-x", Some(&t)).as_deref(), Some("claude-fable-5-1[1m]"));
}

/// An account's last observed model is its newest record, and only
/// records of sessions under that account count.
#[test]
fn launch_model_of_an_account_is_its_newest_record() {
    let _sandbox = sandbox_state_dir();
    let p1 = "/Users/x/.claude-profile-1";
    assert_eq!(launch_model::account_model(Path::new(p1)), None, "nothing recorded");
    record("a", &format!("{p1}/projects/-w/a.jsonl"), "claude-opus-5-5");
    std::thread::sleep(Duration::from_millis(20));
    record("b", &format!("{p1}/projects/-w/b.jsonl"), "claude-fable-5-1[1m]");
    std::thread::sleep(Duration::from_millis(20));
    record("c", "/Users/x/.claude-profile-10/projects/-w/c.jsonl", "claude-sonnet-5");
    assert_eq!(launch_model::account_model(Path::new(p1)).as_deref(), Some("claude-fable-5-1[1m]"));
}

/// A dormant row written before the model column wakes on a model all
/// the same: the session's record, else the account's, else the
/// default, with a line saying which.
#[test]
fn a_dormant_row_without_a_model_wakes_on_a_named_one() {
    let sandbox = sandbox_state_dir();
    let host = FakeHost::new(sandbox.path().to_path_buf());
    let p2 = "/Users/x/.claude-profile-2";
    let row = |choice: CliChoice| DormantRecord {
        shelld_sid: 7,
        uuid: "u7".into(),
        profile_num: 2,
        config_dir: Some(p2.into()),
        project_dir: None,
        choice,
        created_at: SystemTime::now(),
    };
    let model = |d: &DormantRecord| dormant_choice(&host, d).model;
    assert_eq!(model(&row(known())).as_deref(), Some("claude-opus-5-5"), "the row's own");
    assert_eq!(model(&row(CliChoice::default())).as_deref(), Some(launch_model::DEFAULT_MODEL));
    assert!(host.logged.lock().unwrap().iter().any(|(e, l)| e == "hibernate.model_from" && l.contains("(default)")));
    record("other", &format!("{p2}/projects/-w/o.jsonl"), "claude-fable-5-1[1m]");
    assert_eq!(model(&row(CliChoice::default())).as_deref(), Some("claude-fable-5-1[1m]"), "the account's");
    record("u7", "/t/u7.jsonl", "claude-opus-5-5");
    let back = dormant_choice(&host, &row(CliChoice { model: None, effort: Some("high".into()) }));
    assert_eq!(back, CliChoice { model: Some("claude-opus-5-5".into()), effort: Some("high".into()) }, "the session's");
    let op = reclaim_op("u7", Some(p2), None, 4200, None, &back).expect("a wake line");
    let line: String = op
        .steps
        .iter()
        .filter_map(|s| match &s.kind {
            pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .collect();
    assert!(line.contains("--model 'claude-opus-5-5'"), "{line}");
}

/// A `/model` pick is the model the session runs from then on, even
/// before an answer says so; its display name stands for claude's id,
/// and a name that is not a family and a version says nothing.
#[test]
fn launch_model_reads_a_model_pick_in_the_transcript() {
    let dir = TempDir::new("launch-model-pick");
    let t = dir.join("u.jsonl");
    let answer = r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-5-5"}}"#;
    let pick = |name: &str| {
        format!(r#"{{"type":"user","message":{{"role":"user","content":"<local-command-stdout>Set model to \u001b[1m{name}\u001b[22m and saved as your default for new sessions</local-command-stdout>"}}}}"#)
    };
    fs::write(&t, [answer.to_string(), pick("Fable 5.1")].join("\n")).unwrap();
    assert_eq!(launch_model::transcript_model(&t).as_deref(), Some("claude-fable-5-1"));
    fs::write(&t, [answer.to_string(), pick("Opus 4.6 (1M context)")].join("\n")).unwrap();
    assert_eq!(launch_model::transcript_model(&t).as_deref(), Some("claude-opus-4-6[1m]"));
    fs::write(&t, [pick("Fable 5.1"), answer.to_string()].join("\n")).unwrap();
    assert_eq!(launch_model::transcript_model(&t).as_deref(), Some("claude-opus-5-5"), "an answer after the pick");
    fs::write(&t, [answer.to_string(), pick("Default")].join("\n")).unwrap();
    assert_eq!(launch_model::transcript_model(&t), None, "no id to name");
    // quoted inside a message, the marker's quotes are escaped
    let quoted = r#"{"type":"user","message":{"role":"user","content":"see \"content\":\"<local-command-stdout>Set model to Fable 5.1"}}"#;
    fs::write(&t, [answer, quoted].join("\n")).unwrap();
    assert_eq!(launch_model::transcript_model(&t).as_deref(), Some("claude-opus-5-5"));
}

/// A pane whose status-line hook never wrote an `id=` line restarts on
/// what its transcript last names, model and effort, so the scan's
/// choice lets a switch or a reclaim go ahead.
#[test]
fn a_pane_without_a_hook_record_switches_on_its_transcript_model() {
    let _sandbox = sandbox_state_dir();
    let home = TempDir::new("launch-model-scan");
    let uuid = "u-nohook";
    let mut meta = pane_on_p1(home.path(), uuid);
    let sid = 5;
    let choices = |meta: &BindMeta| quota_inputs(&HashMap::from([(sid, meta.clone())])).1;
    assert_eq!(choices(&meta)[&sid], CliChoice::default(), "nothing recorded, no transcript");

    let t = home.join(".claude-profile-1/projects/-w").join(format!("{uuid}.jsonl"));
    fs::create_dir_all(t.parent().unwrap()).unwrap();
    let answer = r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-5-5"},"effort":"high"}"#;
    fs::write(&t, answer).unwrap();
    let got = choices(&meta)[&sid].clone();
    assert_eq!(got, CliChoice { model: Some("claude-opus-5-5".into()), effort: Some("high".into()) });
    assert_eq!(confirm_choice(Some("opus-5-5"), &got), Ok(()), "a switch may go ahead");

    meta.has_transcript = false;
    assert_eq!(choices(&meta)[&sid].model, None, "no transcript written yet: nothing read");
}
