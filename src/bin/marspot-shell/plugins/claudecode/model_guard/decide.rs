//! What a change of the shared settings file calls for.  Pure: the
//! watch reads the file and does what this says.

use marspot_peer::json;

/// The `model` key of a settings file's text: `Ok(None)` without one
/// (or without a file), `Err` for text that is not a JSON object yet,
/// as a write caught half way would be.
pub(super) fn model_of(text: Option<&str>) -> Result<Option<String>, ()> {
    let Some(text) = text else { return Ok(None) };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let v = json::parse(text).map_err(|_| ())?;
    if !matches!(v, json::Value::Obj(_)) {
        return Err(());
    }
    Ok(v.get("model").map(|m| m.str().map_or_else(|| m.to_json(), str::to_string)))
}

/// What to do about one change.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Act {
    /// Nothing worth a word: no change, marspot's own write, or text
    /// that is not whole yet.
    Nothing,
    /// The model changed without confirmation: put the text before the
    /// change back.
    PutBack { to: Option<String>, back: Option<String> },
    /// The model changed and stays.
    Keep { from: Option<String>, to: Option<String> },
    /// Something other than the model changed.
    Tell,
}

/// What marspot does when the file goes from `before` to `now`.
/// `managed` is the setting; `confirmed` says a `/model` was confirmed
/// at the keyboard a short while ago; `own` that marspot wrote `now`.
pub(super) fn act(before: Option<&str>, now: Option<&str>, managed: bool, confirmed: bool, own: bool) -> Act {
    if before == now || own {
        return Act::Nothing;
    }
    let (Ok(from), Ok(to)) = (model_of(before), model_of(now)) else {
        return Act::Nothing;
    };
    if from == to {
        Act::Tell
    } else if managed && !confirmed {
        Act::PutBack { to, back: from }
    } else {
        Act::Keep { from, to }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPUS: &str = "{\n  \"model\": \"opus\",\n  \"theme\": \"dark\"\n}\n";
    const FABLE: &str = "{\n  \"model\": \"fable\",\n  \"theme\": \"dark\"\n}\n";
    const BARE: &str = "{\n  \"theme\": \"dark\"\n}\n";
    const LIGHT: &str = "{\n  \"theme\": \"light\"\n}\n";

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn the_model_key_is_read_and_half_written_text_is_not_a_reading() {
        assert_eq!(model_of(Some(OPUS)), Ok(some("opus")));
        assert_eq!(model_of(Some(BARE)), Ok(None));
        assert_eq!(model_of(None), Ok(None));
        assert_eq!(model_of(Some("")), Ok(None));
        assert_eq!(model_of(Some("{\"model\": \"op")), Err(()));
        assert_eq!(model_of(Some("[1]")), Err(()));
    }

    #[test]
    fn under_managed_an_unconfirmed_model_is_put_back_and_a_confirmed_one_kept() {
        assert_eq!(act(Some(BARE), Some(FABLE), true, false, false), Act::PutBack { to: some("fable"), back: None });
        assert_eq!(act(Some(OPUS), Some(FABLE), true, false, false), Act::PutBack { to: some("fable"), back: some("opus") });
        assert_eq!(act(Some(OPUS), Some(BARE), true, false, false), Act::PutBack { to: None, back: some("opus") });
        assert_eq!(act(None, Some(FABLE), true, false, false), Act::PutBack { to: some("fable"), back: None });
        assert_eq!(act(Some(OPUS), Some(FABLE), true, true, false), Act::Keep { from: some("opus"), to: some("fable") });
    }

    #[test]
    fn under_manual_a_model_is_kept_and_said() {
        assert_eq!(act(Some(OPUS), Some(FABLE), false, false, false), Act::Keep { from: some("opus"), to: some("fable") });
    }

    #[test]
    fn other_changes_are_only_said_and_marspots_own_writes_not_at_all() {
        assert_eq!(act(Some(BARE), Some(LIGHT), true, false, false), Act::Tell);
        assert_eq!(act(Some(BARE), Some(LIGHT), false, false, false), Act::Tell);
        assert_eq!(act(Some(BARE), Some(FABLE), true, false, true), Act::Nothing);
        assert_eq!(act(Some(OPUS), Some(OPUS), true, false, false), Act::Nothing);
        assert_eq!(act(Some(OPUS), Some("{\"model\": "), true, false, false), Act::Nothing, "a write caught half way");
    }
}
