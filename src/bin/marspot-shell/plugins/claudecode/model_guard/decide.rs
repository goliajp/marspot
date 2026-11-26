//! What a change of the shared settings file calls for.  Pure: the
//! watch reads the file and does what this says.

use marspot_peer::json::{self, Value};

/// What one read of a watched file found.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Seen {
    /// No file at that name.
    Absent,
    /// A file that could not be read, or not as UTF-8: nothing is known
    /// about its model.
    Unreadable,
    /// Text that is not a whole JSON object: empty, or a write caught
    /// half way.
    Partial(String),
    /// A whole JSON object and its top-level `model`.
    Whole { text: String, model: Option<Value> },
}

impl Seen {
    pub(super) fn of_text(text: String) -> Seen {
        match json::parse(&text) {
            Ok(v @ Value::Obj(_)) => Seen::Whole { model: v.get("model").cloned(), text },
            _ => Seen::Partial(text),
        }
    }

    pub(super) fn is_whole(&self) -> bool {
        matches!(self, Seen::Whole { .. })
    }

    /// The model this says the file had, when that is known: an absent
    /// or empty file has none, and an unreadable or half-written one is
    /// not known.
    fn model(&self) -> Option<Option<&Value>> {
        match self {
            Seen::Whole { model, .. } => Some(model.as_ref()),
            Seen::Absent => Some(None),
            Seen::Partial(t) if t.trim().is_empty() => Some(None),
            Seen::Partial(_) | Seen::Unreadable => None,
        }
    }
}

/// What to do about one change.
#[derive(Debug, PartialEq)]
pub(super) enum Act {
    /// The same as before.
    Nothing,
    /// Not a whole text, after one whose model was known: a writer is
    /// half way, or the file was taken away.  The earlier reading stays
    /// the one to go back to, and nothing is done.
    Wait,
    /// Take the new text as the one to go back to, and say nothing:
    /// nothing was known about the model before it, or it is marspot's
    /// own write and leaves the model as it was.
    Adopt,
    /// The model changed: under managed, put the earlier one back
    /// unless a `/model` was confirmed.
    PutBack { to: Option<Value>, back: Option<Value> },
    /// The model changed and stays.
    Keep { from: Option<Value>, to: Option<Value> },
    /// Something other than the model changed.
    Tell,
}

/// What marspot does when the file goes from `before` to `now`.
/// `managed` is the setting; `own` says marspot itself wrote `now`.  An
/// own write that changes the model is a change of model like any
/// other: what marspot wrote may carry a model someone else had just put
/// there.
pub(super) fn act(before: &Seen, now: &Seen, managed: bool, own: bool) -> Act {
    if before == now {
        return Act::Nothing;
    }
    let Seen::Whole { model: to, .. } = now else {
        // what is known of the model stays known until a whole text
        let known = before.model().is_some() && now.model().is_none();
        return if before.is_whole() || known { Act::Wait } else { Act::Adopt };
    };
    let Some(from) = before.model() else { return Act::Adopt };
    if from == to.as_ref() {
        return if own { Act::Adopt } else { Act::Tell };
    }
    let (from, to) = (from.cloned(), to.clone());
    if managed { Act::PutBack { to, back: from } } else { Act::Keep { from, to } }
}

#[cfg(test)]
#[path = "decide_tests.rs"]
mod tests;
