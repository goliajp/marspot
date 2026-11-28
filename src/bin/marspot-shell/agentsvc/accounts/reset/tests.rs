//! One claim per account, one answer per request id, and nothing done
//! unless devops confirms.  No test here gets past the confirmation:
//! nothing is ever claimed.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use super::ledger::{HEADER, Ledger};
use super::*;

mod again_tests;
mod curl_tests;
mod no_devops_tests;
mod status_tests;

const NOW: i64 = 1_795_740_000;

fn claim(provider: Provider, profile: u8, id: &str) -> Claim {
    let kind = if provider == Provider::Claude { "session" } else { "codex_full" };
    let grant = Grant { kind: kind.into(), grant_ref: format!("codex:{profile}:g1"), ..Grant::default() };
    Claim { provider, profile, grant, request_id: id.into(), reason: "week spent".into(), by: By::DevopsUi }
}

fn ledger(dir: &marspot_term::test_env::TempDir) -> Arc<Mutex<Ledger>> {
    Arc::new(Mutex::new(Ledger::open(dir.join("reset_claims"), NOW)))
}

fn refused() -> Result<Grant, Outcome> {
    Err(Outcome::of(Res::Error, "devops did not confirm this claim (HTTP 410)"))
}

fn fact_rows(dir: &marspot_term::test_env::TempDir) -> Vec<String> {
    let files = std::fs::read_dir(dir.join("reset_claims")).map(|d| d.flatten().map(|f| f.path()).collect::<Vec<_>>()).unwrap_or_default();
    files.iter().flat_map(|f| std::fs::read_to_string(f).unwrap().lines().map(str::to_string).collect::<Vec<_>>()).filter(|l| format!("{l}\n") != HEADER).collect()
}

#[test]
fn reset_a_request_id_asked_again_gets_its_first_answer_and_is_checked_once() {
    let dir = marspot_term::test_env::TempDir::new("reset-again");
    let l = ledger(&dir);
    let first = run(&l, claim(Provider::Codex, 1, "aa-1"), |_| refused());
    assert_eq!(first.result, Res::Error);
    let again = run(&l, claim(Provider::Codex, 1, "aa-1"), |_| panic!("the token is never checked twice"));
    assert_eq!(again, first);
    let rows = fact_rows(&dir);
    assert_eq!(rows.len(), 1, "one attempt, one fact: {rows:?}");
    assert!(rows[0].contains("\taa-1\tcodex\t1\tcodex_full\tcodex:1:g1\tdevops-ui\terror\t\t"), "{}", rows[0]);
    assert!(!rows[0].contains("tok"), "no token is written");

    // a restart answers from the facts
    let l2 = ledger(&dir);
    assert_eq!(run(&l2, claim(Provider::Codex, 1, "aa-1"), |_| panic!("answered from the facts")), first);
}

#[test]
fn reset_one_claim_per_account_at_a_time_and_busy_is_not_an_answer() {
    let dir = marspot_term::test_env::TempDir::new("reset-lock");
    let l = ledger(&dir);
    let held = Ledger::hold(&l, &claim(Provider::Claude, 3, "bb-1")).expect("the account is free");
    assert!(Ledger::hold(&l, &claim(Provider::Codex, 4, "bb-1")).is_none(), "the same request id elsewhere waits too");

    let busy = run(&l, claim(Provider::Claude, 3, "bb-2"), |_| panic!("not checked while the account is held"));
    assert_eq!(busy.result, Res::Busy);
    let other = run(&l, claim(Provider::Codex, 3, "bb-3"), |_| refused());
    assert_eq!(other.result, Res::Error, "another provider's account of the same number is free");

    drop(held);
    let mut checked = false;
    let later = run(&l, claim(Provider::Claude, 3, "bb-2"), |_| {
        checked = true;
        refused()
    });
    assert!(checked && later.result == Res::Error, "busy was not kept as the answer to bb-2");
    assert_eq!(fact_rows(&dir).len(), 3, "every attempt is a fact, the busy one included");
}

/// A devops stand-in on a local port: each connection gets the next
/// answer, and the request heads are handed back.
fn devops(answers: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut heads = Vec::new();
        for (status, body) in answers {
            let (mut s, _) = listener.accept().unwrap();
            let mut head = String::new();
            let mut r = BufReader::new(s.try_clone().unwrap());
            while r.read_line(&mut head).unwrap() > 2 && !head.ends_with("\r\n\r\n") {}
            heads.push(head);
            let reply = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            s.write_all(reply.as_bytes()).unwrap();
        }
        heads
    });
    (base, server)
}

const CONFIRMED: &str = r#"{"provider":"codex","profile":2,"kind":"codex_full","grant_ref":"codex:2:g1","starts_at":1793478363,"ends_at":"2026-11-30T20:26:03Z","request_id":"cc-1","reason":"week spent"}"#;

#[test]
fn reset_devops_confirms_once_and_only_the_claim_it_issued() {
    let wrong_kind = CONFIRMED.replace("codex_full", "full");
    let (base, server) = devops(vec![(200, CONFIRMED.into()), (410, r#"{"error":"used"}"#.into()), (200, wrong_kind)]);
    let access = verify::Access { base, key: "dk_test_1".into() };
    let c = claim(Provider::Codex, 2, "cc-1");
    let g = verify::verify(&access, &c, "tok/+=1").expect("a 200 naming this claim");
    assert_eq!((g.starts_at, g.ends_at, g.grant_ref.as_str()), (Some(1_793_478_363), Some(1_796_070_363), "codex:2:g1"));

    let second = verify::verify(&access, &c, "tok/+=1").unwrap_err();
    assert_eq!((second.result, second.detail.as_str()), (Res::Error, "devops did not confirm this claim (HTTP 410)"));
    let other = verify::verify(&access, &c, "tok/+=1").unwrap_err();
    assert!(other.detail.contains("different"), "{}", other.detail);

    let heads = server.join().unwrap();
    let first = heads[0].to_ascii_lowercase();
    assert!(
        heads[0].starts_with("GET /api/ai/accounts/codex/2/reset/verify?confirm=tok%2F%2B%3D1&request_id=cc-1&kind=codex_full HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(first.contains("authorization: bearer dk_test_1"), "{first}");
}

#[test]
fn reset_nothing_happens_without_devops_or_its_credential() {
    let c = claim(Provider::Codex, 2, "dd-1");
    // nothing listens there once the listener is gone
    let base = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let o = verify::verify(&verify::Access { base, key: "k".into() }, &c, "t").unwrap_err();
    assert_eq!(o.result, Res::Error);

    // a stand-in home: the user's own devops files are never read here
    let home = marspot_term::test_env::TempDir::new("reset-devops-home");
    let none = verify::access_in(home.path()).err().expect("no files");
    assert_eq!((none.result, none.detail.as_str()), (Res::Unavailable, verify::NO_DEVOPS));
    std::fs::create_dir_all(home.join(".config/devops")).unwrap();
    let half = |home: &std::path::Path| verify::access_in(home).err().map(|o| (o.result, o.detail));
    let incomplete = Some((Res::Error, verify::INCOMPLETE.to_string()));
    std::fs::write(home.join(".config/devops/api-key"), "dk_test_2\n").unwrap();
    assert_eq!(half(home.path()), incomplete, "a key and no address");
    std::fs::remove_file(home.join(".config/devops/api-key")).unwrap();
    std::fs::write(home.join(".config/devops/api-url"), "http://127.0.0.1:1\n").unwrap();
    assert_eq!(half(home.path()), incomplete, "an address and no key");
    std::fs::write(home.join(".config/devops/api-key"), "  \n").unwrap();
    assert_eq!(half(home.path()), incomplete, "an address and an empty key");
}

#[test]
fn reset_the_devops_files_give_the_address_and_the_key_goes_only_in_the_header() {
    let home = marspot_term::test_env::TempDir::new("reset-devops-files");
    let (base, server) = devops(vec![(200, CONFIRMED.into()), (403, r#"{"error":"no"}"#.into())]);
    std::fs::create_dir_all(home.join(".config/devops")).unwrap();
    std::fs::write(home.join(".config/devops/api-url"), format!("{base}/\n")).unwrap();
    std::fs::write(home.join(".config/devops/api-key"), "dk_secret_9\n").unwrap();
    let access = verify::access_in(home.path()).expect("both files");
    let c = claim(Provider::Codex, 2, "cc-1");
    verify::verify(&access, &c, "tok-9").expect("confirmed");
    let refused = verify::verify(&access, &c, "tok-9").expect_err("403");
    assert!(!refused.detail.contains("dk_secret_9") && !refused.reply("cc-1").to_json().contains("dk_secret_9"));
    let heads = server.join().unwrap();
    for head in &heads {
        let (request_line, headers) = head.split_once("\r\n").unwrap();
        assert!(request_line.starts_with("GET /api/ai/accounts/codex/2/reset/verify?confirm=tok-9&"), "{request_line}");
        assert!(!request_line.contains("dk_secret_9"), "the key is not in the address");
        let with_key: Vec<&str> = headers.lines().filter(|l| l.contains("dk_secret_9")).collect();
        assert_eq!(with_key, ["authorization: Bearer dk_secret_9"], "{head}");
    }
}

#[test]
fn reset_nothing_is_tried_once_the_claim_s_time_has_run_out() {
    let dir = marspot_term::test_env::TempDir::new("reset-deadline");
    let l = ledger(&dir);
    let slow = |c: &Claim| {
        std::thread::sleep(Duration::from_millis(80));
        Ok(c.grant.clone())
    };
    let o = run_within(&l, claim(Provider::Codex, 6, "gg-1"), Duration::from_millis(40), slow);
    assert_eq!((o.result, o.detail.as_str()), (Res::Error, "the claim's time ran out before anything was tried"));
}

#[test]
fn reset_a_request_that_may_be_tried_again_is_not_confirmed_twice() {
    let dir = marspot_term::test_env::TempDir::new("reset-again-key");
    let l = ledger(&dir);
    let mut c = claim(Provider::Codex, 7, "hh-1");
    c.grant.ends_at = Some(1_796_165_597);
    let lost = Outcome::again(Res::Error, "the app-server did not answer the consume request; ask again with the same request id");
    l.lock().unwrap().record(&c, &lost, NOW).unwrap();
    let asked = Claim { grant: Grant { ends_at: None, ..c.grant.clone() }, ..c.clone() };
    assert_eq!(l.lock().unwrap().confirmed_before(&asked, NOW).and_then(|g| g.ends_at), Some(1_796_165_597), "the grant as devops confirmed it");
    assert!(l.lock().unwrap().answered("hh-1").is_none(), "not an answer");
    let elsewhere = Claim { profile: 8, ..asked.clone() };
    assert_eq!(l.lock().unwrap().confirmed_before(&elsewhere, NOW), None, "another account is another request");

    // a restart forgets the confirmation, and the row reads back as not final
    let l2 = ledger(&dir);
    assert_eq!(l2.lock().unwrap().confirmed_before(&asked, NOW), None);
    assert!(l2.lock().unwrap().answered("hh-1").is_none());
    assert!(fact_rows(&dir)[0].contains("\t1\t"), "{:?}", fact_rows(&dir));

    l.lock().unwrap().record(&c, &Outcome::of(Res::Reset, "reset"), NOW).unwrap();
    assert_eq!(l.lock().unwrap().confirmed_before(&asked, NOW), None, "an answer ends it");
    assert_eq!(l.lock().unwrap().answered("hh-1").map(|r| r.outcome.result), Some(Res::Reset));
}

#[test]
fn reset_the_accounts_panel_reads_each_account_s_last_claim_from_the_facts() {
    let dir = marspot_term::test_env::TempDir::new("reset-panel-reads");
    let l = ledger(&dir);
    let c = claim(Provider::Codex, 3, "ii-1");
    l.lock().unwrap().record(&c, &Outcome::of(Res::NotLimited, "nothingToReset"), NOW).unwrap();
    let o = Outcome { left_after: Some(2), ..Outcome::of(Res::Reset, "reset") };
    l.lock().unwrap().record(&claim(Provider::Codex, 3, "ii-2"), &o, NOW + 5).unwrap();
    let shown = marspot::agent_resets::last_claims(&dir.join("reset_claims"));
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!((shown[0].at, shown[0].agent.as_str(), shown[0].profile, shown[0].kind.as_str()), (NOW + 5, "codex", 3, "codex_full"));
    assert_eq!(marspot::agent_resets::claimed_text(&shown[0]), "full reset");
}
