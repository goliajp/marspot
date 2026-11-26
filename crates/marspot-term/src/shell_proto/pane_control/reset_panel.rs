//! L2 → L1: the user confirmed, on the accounts panel, the use of one
//! stored reset of one account.  Sent only from the panel's own press
//! handling; nothing a pane writes reaches it.

use std::io;

use super::wire::{Rd, finish, put_text};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetPanelAct {
    /// `claude` or `codex`.
    pub provider: String,
    pub profile: u8,
    pub kind: String,
    /// The grant as the readings name it (`codex:1:g2`).
    pub grant_ref: String,
    /// Unix seconds; `None` when the reading did not say.
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    /// How many of that grant the panel showed as left.
    pub left: Option<u32>,
    /// Made by the core when the user confirmed; the same act sent twice
    /// claims once.
    pub request_id: String,
}

const NONE_I64: i64 = i64::MIN;
const NONE_U32: u32 = u32::MAX;

pub fn encode_reset_panel_act(m: &ResetPanelAct) -> io::Result<Vec<u8>> {
    const WHAT: &str = "reset_panel_act";
    let mut v = Vec::with_capacity(64);
    v.push(m.profile);
    v.extend_from_slice(&m.starts_at.unwrap_or(NONE_I64).to_le_bytes());
    v.extend_from_slice(&m.ends_at.unwrap_or(NONE_I64).to_le_bytes());
    v.extend_from_slice(&m.left.unwrap_or(NONE_U32).to_le_bytes());
    for s in [&m.provider, &m.kind, &m.grant_ref, &m.request_id] {
        put_text(&mut v, s, WHAT)?;
    }
    finish(v, WHAT)
}

pub fn decode_reset_panel_act(p: &[u8]) -> io::Result<ResetPanelAct> {
    let mut r = Rd::new(p, "reset_panel_act");
    let profile = r.u8()?;
    let time = |n: u64| (n as i64 != NONE_I64).then_some(n as i64);
    let starts_at = time(r.u64()?);
    let ends_at = time(r.u64()?);
    let left = Some(r.u32()?).filter(|n| *n != NONE_U32);
    Ok(ResetPanelAct { profile, starts_at, ends_at, left, provider: r.text()?, kind: r.text()?, grant_ref: r.text()?, request_id: r.text()? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reset_panel_act_round_trips_and_a_short_one_is_refused() {
        let m = ResetPanelAct {
            provider: "codex".into(),
            profile: 2,
            kind: "codex_full".into(),
            grant_ref: "codex:2:g1".into(),
            starts_at: Some(1_793_478_363),
            ends_at: None,
            left: Some(1),
            request_id: "0b6a7c1e-3f1d-4a8e-9c55-2d7f0e9b1a42".into(),
        };
        let bytes = encode_reset_panel_act(&m).unwrap();
        assert_eq!(decode_reset_panel_act(&bytes).unwrap(), m);
        assert!(decode_reset_panel_act(&bytes[..bytes.len() - 1]).is_err());
        let none = ResetPanelAct { starts_at: None, left: None, ..m };
        assert_eq!(decode_reset_panel_act(&encode_reset_panel_act(&none).unwrap()).unwrap(), none);
    }
}
