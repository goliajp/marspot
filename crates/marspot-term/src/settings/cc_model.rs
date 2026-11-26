//! Who decides Claude Code's default model.

/// Whether marspot guards the `model` key of the Claude Code settings
/// file every account shares.
///
/// One account's `/model` writes `model` into that file, and every
/// account then starts on it.  `managed` (the default) asks twice before
/// a `/model` typed in a pane is sent, refuses one sent by a program,
/// and puts the file back when the key changes without that
/// confirmation.  `manual` only says that it changed.  Every word that
/// is not `manual` reads as managed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CcModel {
    #[default]
    Managed,
    Manual,
}

impl CcModel {
    pub(super) fn parse(v: &str) -> Self {
        match v.to_ascii_lowercase().as_str() {
            "manual" => Self::Manual,
            _ => Self::Managed,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::Manual => "manual",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn the_shared_model_is_managed_unless_written_manual() {
        assert_eq!(Settings::default().cc_model, CcModel::Managed);
        for (word, want) in [
            ("managed", CcModel::Managed),
            ("manual", CcModel::Manual),
            ("Manual", CcModel::Manual),
            ("off", CcModel::Managed),
            ("maybe", CcModel::Managed),
        ] {
            let body = format!("claude.model = {word}\n");
            let s = parse(&body);
            assert_eq!(s.cc_model, want, "{word}");
            assert_eq!(parse(&render(&s, &body)).cc_model, want, "{word} after a rewrite");
        }
        assert!(render(&Settings::default(), "").contains("claude.model = managed\n"));
    }
}
