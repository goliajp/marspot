//! Using one stored rate-limit reset of one agent account, by hand.
//!
//! Two entry points, both confirmed twice: the control socket's
//! `reset.claim`, whose single-use token is checked with devops before
//! anything happens, and the accounts panel, whose second press comes
//! from the user's own mouse or keyboard as a core frame no pane can
//! write.  Nothing here claims by itself (`sched.reset` stays off).
//!
//! One claim per account at a time; a request id asked again gets the
//! answer it got the first time.  The account is waited on until every
//! pane on it rests, for up to two minutes, so a claim never lands in
//! the middle of a turn.  Claude goes through the official CLI's
//! `/limit-reset` in a background pane under a window marspot opens for
//! it; Codex through the app-server's consume request.

mod claude;
mod claude_screen;
mod codex;
pub(crate) mod idle;
mod ledger;
mod outcome;
mod status;
mod verify;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use marspot::shell_proto::ResetPanelAct;
use marspot_peer::control::ResetClaimArgs;
use marspot_peer::json::Value;

pub(crate) use outcome::{Outcome, Res};
pub(crate) use status::status;

/// Longest wait for every pane of the account to rest.
const IDLE_WITHIN: Duration = Duration::from_secs(120);
/// Longest the claim itself may take once the account rests.
const CLI_WITHIN: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Provider> {
        [Provider::Claude, Provider::Codex].into_iter().find(|p| p.as_str() == s)
    }
}

/// Who confirmed a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum By {
    MarspotUi,
    DevopsUi,
}

impl By {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            By::MarspotUi => "marspot-ui",
            By::DevopsUi => "devops-ui",
        }
    }
}

/// The grant a claim is about, as the confirming side saw it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Grant {
    pub kind: String,
    pub grant_ref: String,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub left: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Claim {
    pub provider: Provider,
    pub profile: u8,
    pub grant: Grant,
    pub request_id: String,
    pub reason: String,
    pub by: By,
}

fn ledger() -> &'static Arc<Mutex<ledger::Ledger>> {
    static L: OnceLock<Arc<Mutex<ledger::Ledger>>> = OnceLock::new();
    L.get_or_init(|| Arc::new(Mutex::new(ledger::Ledger::open(claims_dir(), unix_now()))))
}

pub(crate) fn claims_dir() -> std::path::PathBuf {
    marspot_term::paths::state_root().join("usage/reset_claims")
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// `reset.claim` from the control socket: checked with devops first.
pub(crate) fn from_socket(a: &ResetClaimArgs) -> Value {
    let Some(provider) = Provider::parse(&a.provider) else {
        return Outcome::of(Res::Error, "unknown provider").reply(&a.request_id);
    };
    let grant = Grant { kind: a.kind.clone(), grant_ref: a.grant_ref.clone(), ..Grant::default() };
    let c = Claim { provider, profile: a.profile, grant, request_id: a.request_id.clone(), reason: a.reason.clone(), by: By::DevopsUi };
    let confirm = a.confirm.clone();
    run(ledger(), c, move |c| match verify::access() {
        Some(access) => verify::verify(&access, c, &confirm),
        None => Err(Outcome::of(Res::Error, "marspot has no devops credential to check the confirmation with")),
    })
    .reply(&a.request_id)
}

/// A claim the user confirmed on the accounts panel, from the core's
/// frame reader: done on a thread of its own, one at a time.
pub(crate) fn from_panel(payload: &[u8]) {
    use std::sync::mpsc::{SyncSender, sync_channel};
    static TX: OnceLock<SyncSender<ResetPanelAct>> = OnceLock::new();
    let Ok(act) = marspot::shell_proto::decode_reset_panel_act(payload) else { return };
    let tx = TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<ResetPanelAct>(4);
        let spawned = std::thread::Builder::new().name("reset-claim".into()).spawn(move || {
            for act in rx {
                let id = act.request_id.clone();
                let o = panel_claim(act);
                marspot::lx_info!("svc.reset.panel", &format!("request={id} result={} {}", o.result.as_str(), o.detail));
            }
        });
        if let Err(e) = spawned {
            marspot::lx_warn!("svc.reset.thread", &e.to_string());
        }
        tx
    });
    if tx.try_send(act).is_err() {
        marspot::lx_warn!("svc.reset.panel_dropped", "a confirmed reset was not taken: one is already waiting");
    }
}

fn panel_claim(act: ResetPanelAct) -> Outcome {
    let Some(provider) = Provider::parse(&act.provider) else { return Outcome::of(Res::Error, "unknown provider") };
    let grant = Grant { kind: act.kind, grant_ref: act.grant_ref, starts_at: act.starts_at, ends_at: act.ends_at, left: act.left };
    let c = Claim { provider, profile: act.profile, grant, request_id: act.request_id, reason: "confirmed on the accounts panel".into(), by: By::MarspotUi };
    run(ledger(), c, |c| Ok(c.grant.clone()))
}

/// One claim from start to answer: the answer already given, or hold
/// the account, `check` the confirmation, wait for the account to rest,
/// claim, and write the attempt down.
fn run(ledger: &Arc<Mutex<ledger::Ledger>>, mut c: Claim, check: impl FnOnce(&Claim) -> Result<Grant, Outcome>) -> Outcome {
    if let Some(row) = ledger.lock().unwrap_or_else(|p| p.into_inner()).answered(&c.request_id) {
        return row.outcome.clone();
    }
    let Some(_hold) = ledger::Ledger::hold(ledger, &c) else {
        return record(ledger, &c, Outcome::of(Res::Busy, "a claim on this account or with this request id is running"));
    };
    let o = match check(&c) {
        Err(o) => o,
        Ok(grant) => {
            c.grant = grant;
            if idle::wait_idle(c.provider.as_str(), c.profile, IDLE_WITHIN) {
                claim(&c, Instant::now() + CLI_WITHIN)
            } else {
                Outcome::of(Res::AccountBusy, "a pane on this account kept working for two minutes")
            }
        }
    };
    record(ledger, &c, o)
}

fn claim(c: &Claim, deadline: Instant) -> Outcome {
    match c.provider {
        Provider::Claude => claude::claim(c.profile, &c.grant, deadline),
        Provider::Codex => codex::claim_profile(c.profile, &c.grant, &c.request_id, deadline),
    }
}

fn record(ledger: &Arc<Mutex<ledger::Ledger>>, c: &Claim, o: Outcome) -> Outcome {
    let line = format!(
        "provider={} profile={} kind={} grant={} by={} request={} result={}",
        c.provider.as_str(),
        c.profile,
        c.grant.kind,
        c.grant.grant_ref,
        c.by.as_str(),
        c.request_id,
        o.result.as_str()
    );
    marspot::lx_info!("svc.reset.claim", &line);
    if let Err(e) = ledger.lock().unwrap_or_else(|p| p.into_inner()).record(c, &o, unix_now()) {
        marspot::lx_warn!("svc.reset.record_failed", &e.to_string());
    }
    o
}

#[cfg(test)]
mod tests;
