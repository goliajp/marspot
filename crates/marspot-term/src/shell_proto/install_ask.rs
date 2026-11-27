//! `InstallAsk`: the shell is about to install an agent CLI for the
//! first time and needs a yes first; the core asks over the focused pane
//! and writes the answer into the settings.

use std::io;

/// What the question says: which CLI, where it goes, and about how
/// much room it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallAsk {
    /// A name of `settings::INSTALL_CLIS`.
    pub cli: String,
    /// Where it is put, as a person reads it (`~/.local/bin`).
    pub place: String,
    pub megabytes: u32,
}

/// The longest place carried; a longer one is refused.
pub const INSTALL_PLACE_MAX: usize = 256;

/// `cli_len u8, cli, place_len u16 LE, place, megabytes u32 LE`.  Bytes
/// after it are a later build's additions.
pub fn encode_install_ask(a: &InstallAsk) -> Vec<u8> {
    let mut v = Vec::with_capacity(7 + a.cli.len() + a.place.len());
    v.push(a.cli.len() as u8);
    v.extend_from_slice(a.cli.as_bytes());
    v.extend_from_slice(&(a.place.len() as u16).to_le_bytes());
    v.extend_from_slice(a.place.as_bytes());
    v.extend_from_slice(&a.megabytes.to_le_bytes());
    v
}

/// Refuses a CLI the settings have no answer for, so the core never
/// writes a key nobody reads.
pub fn decode_install_ask(payload: &[u8]) -> io::Result<InstallAsk> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("install_ask payload: {why}"));
    let (&cli_len, rest) = payload.split_first().ok_or_else(|| bad("empty"))?;
    let (cli, rest) = rest.split_at_checked(cli_len as usize).ok_or_else(|| bad("cli cut short"))?;
    let cli = std::str::from_utf8(cli).map_err(|_| bad("cli not utf-8"))?;
    if !crate::settings::INSTALL_CLIS.contains(&cli) {
        return Err(bad("not a cli marspot installs"));
    }
    let (len, rest) = rest.split_first_chunk::<2>().ok_or_else(|| bad("no place length"))?;
    let len = u16::from_le_bytes(*len) as usize;
    if len > INSTALL_PLACE_MAX {
        return Err(bad("place too long"));
    }
    let (place, rest) = rest.split_at_checked(len).ok_or_else(|| bad("place cut short"))?;
    let place = std::str::from_utf8(place).map_err(|_| bad("place not utf-8"))?;
    let mb = rest.first_chunk::<4>().ok_or_else(|| bad("no size"))?;
    Ok(InstallAsk { cli: cli.to_string(), place: place.to_string(), megabytes: u32::from_le_bytes(*mb) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_install_question_round_trips_and_a_bad_one_is_refused() {
        let a = InstallAsk { cli: "codex".into(), place: "~/.codex".into(), megabytes: 740 };
        let wire = encode_install_ask(&a);
        assert_eq!(decode_install_ask(&wire).unwrap(), a);
        let mut later = wire.clone();
        later.push(1);
        assert_eq!(decode_install_ask(&later).unwrap(), a, "a later build's bytes are skipped");
        assert!(decode_install_ask(&wire[..wire.len() - 1]).is_err(), "cut short");
        assert!(decode_install_ask(&[]).is_err());
        let npm = InstallAsk { cli: "npm".into(), ..a.clone() };
        assert!(decode_install_ask(&encode_install_ask(&npm)).is_err(), "a cli with no setting");
        let long = InstallAsk { place: "x".repeat(INSTALL_PLACE_MAX + 1), ..a };
        assert!(decode_install_ask(&encode_install_ask(&long)).is_err());
    }
}
