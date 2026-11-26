//! The accounts panel's reset buttons: a press on a line of an account's
//! stored resets arms it, a second press on it confirms, and the claim
//! goes to the shell as `ResetPanelAct`.  Only this pointer handling
//! sends that frame.

use super::*;
use marspot::agent_resets::{AccountResets, ResetLine, Resets};
use marspot::cc_usage::CcWindow;
use marspot::ui::components::cc_usage_reset_lines::RESET_LINE;

/// The account the panel draws at card `i`: the Claude feed's accounts
/// first, then the Codex and Kimi feed's, as the cards are laid out.
fn account_at(modal: &CcUsageModalState, i: usize) -> Option<&AccountResets> {
    let claude: Vec<&str> = modal.data.iter().flat_map(|d| d.accounts.iter()).map(|a| a.name.as_str()).collect();
    let rest: Vec<&str> = modal.codex.iter().flat_map(|d| d.accounts.iter()).map(|a| a.name.as_str()).collect();
    let (name, agents): (&str, &[&str]) = match i.checked_sub(claude.len()) {
        None => (claude[i], &["claude"]),
        Some(j) => (rest.get(j)?, &["codex", "kimi"]),
    };
    modal.resets.iter().find(|a| a.name == name && agents.contains(&a.agent.as_str()))
}

/// When what `restores` names would come back by itself, from the
/// account's own windows.
fn renews(windows: &[CcWindow], restores: &[String]) -> Option<i64> {
    let named = |w: &CcWindow, r: &str| match r {
        "five_hour" => w.label == "5h",
        "seven_day" => w.label == "7d",
        "seven_day_overage_included" => w.label.eq_ignore_ascii_case("fable"),
        _ => false,
    };
    windows.iter().filter(|w| restores.iter().any(|r| named(w, r))).filter_map(|w| w.reset).min()
}

fn when(t: i64) -> String {
    let (month, day, hour, minute) = marspot::cc_usage::local_mdhm(t);
    format!("{month}/{day} {hour:02}:{minute:02}")
}

/// The reset lines of `account` on the open panel, the armed one saying
/// what a second press does.
pub(super) fn lines_for(modal: &CcUsageModalState, account: Option<&AccountResets>, windows: &[CcWindow], now: i64) -> Vec<ResetLine> {
    let Some(a) = account else { return Vec::new() };
    let armed = modal.buttons.armed_at(now).filter(|x| x.agent == a.agent && x.profile == a.profile).map(|x| x.line);
    let claims = matches!(a.agent.as_str(), "claude" | "codex");
    marspot::agent_resets::lines(&a.resets, now, when)
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            let grant = claims.then(|| marspot::agent_resets::claimable(&a.resets, i)).flatten();
            match (grant, armed == Some(i)) {
                (Some((_, g)), true) => ResetLine { text: marspot::agent_resets::armed_text(g, renews(windows, &g.restores), when), pressable: true, armed: true },
                (g, _) => ResetLine { text, pressable: g.is_some(), armed: false },
            }
        })
        .collect()
}

/// The frame claiming line `line` of `a`'s resets.
fn act(a: &AccountResets, r: &Resets, line: usize) -> Option<marspot::shell_proto::ResetPanelAct> {
    let (i, g) = marspot::agent_resets::claimable(r, line)?;
    Some(marspot::shell_proto::ResetPanelAct {
        provider: a.agent.clone(),
        profile: a.profile,
        kind: g.kind.clone(),
        grant_ref: format!("{}:{}:g{}", a.agent, a.profile, i + 1),
        starts_at: g.starts_at,
        ends_at: g.ends_at,
        left: g.left,
        request_id: marspot_peer::rand::uuid_v4(),
    })
}

impl CoreApp {
    /// A press on the accounts panel's content.
    pub(super) fn reset_panel_press(&mut self, wi: usize, x_phys: f64, y_phys: f64) {
        let on = self.tree_panel_action_at(wi, SurfaceKind::Accounts, x_phys, y_phys);
        let now = unix_now_s();
        let Some(modal) = win!(self, wi).cc_usage_modal.as_mut() else { return };
        let Some(action) = on.filter(|a| a.id == RESET_LINE) else {
            modal.buttons.disarm();
            return;
        };
        let (card, line) = (action.arg as usize / marspot::agent_resets::MAX_LINES, action.arg as usize % marspot::agent_resets::MAX_LINES);
        let Some(a) = account_at(modal, card).cloned() else { return };
        if marspot::agent_resets::claimable(&a.resets, line).is_none() || !matches!(a.agent.as_str(), "claude" | "codex") {
            return;
        }
        let Some(confirmed) = modal.buttons.press(&a.agent, a.profile, line, now) else { return };
        let Some(m) = act(&a, &a.resets, confirmed.line) else { return };
        match marspot::shell_proto::encode_reset_panel_act(&m) {
            Ok(payload) => self.pending_to_shell.push((MsgType::ResetPanelAct, payload)),
            Err(e) => marspot::lx_warn!("core.reset_panel.encode", &e.to_string()),
        }
        marspot::lx_info!("core.reset_panel.confirmed", &format!("provider={} profile={} kind={} request={}", m.provider, m.profile, m.kind, m.request_id));
    }
}

fn unix_now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
