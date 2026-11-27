use super::*;

/// Every `MsgType` must decode back from its own discriminant.
///
/// `PaneAgentTui = 82` was added to the enum and never to
/// `from_u32`.  A reader is required to skip an unknown type
/// silently (that is what makes the wire forward-compatible), so
/// L1 sent the "this pane is an agent TUI" declaration every tick
/// and L2 dropped every frame without a word.  Nobody noticed for
/// weeks because the consumer falls back to "the pane has a badge"
/// — which is right often enough to hide it, and wrong exactly
/// when the badge is momentarily empty.
///
/// The table is read out of this file rather than restated here on
/// purpose: a hand-kept list of variants is the same kind of thing
/// that drifted in the first place.
#[test]
fn an_icon_mask_round_trips_and_a_short_one_is_refused() {
    assert_eq!(decode_icon_mask_wanted(&encode_icon_mask_wanted(4, 26)).unwrap(), (4, 26));
    assert!(decode_icon_mask_wanted(&[1, 0, 2]).is_err());
    let mask = vec![7u8; 9];
    assert_eq!(decode_icon_mask(&encode_icon_mask(2, 13, 3, &mask)).unwrap(), (2, 13, 3, mask.clone()));
    assert!(decode_icon_mask(&encode_icon_mask(2, 13, 4, &mask)).is_err());
    assert!(decode_icon_mask(&[0; 5]).is_err());
}

#[test]
fn every_msg_type_decodes_from_its_own_discriminant() {
    let src = include_str!("../shell_proto.rs");
    let start = src.find("pub enum MsgType").expect("the enum");
    let body = &src[start..];
    let body = &body[..body.find("\n}").expect("the enum's end")];
    let mut variants: Vec<(u32, &str)> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        let Some((name, rest)) = line.split_once(" = ") else {
            continue;
        };
        let Ok(disc) = rest.trim_end_matches(',').parse::<u32>() else {
            continue;
        };
        if name.starts_with(|c: char| c.is_ascii_uppercase()) {
            variants.push((disc, name));
        }
    }
    assert!(
        variants.len() > 50,
        "parsed only {} variants — the enum's shape changed and this \
         test is no longer reading it",
        variants.len()
    );
    for (disc, name) in variants {
        let decoded = MsgType::from_u32(disc)
            .unwrap_or_else(|| panic!("MsgType::{name} = {disc} is missing from from_u32"));
        assert_eq!(
            decoded as u32, disc,
            "MsgType::{name} = {disc} decodes to a different variant",
        );
    }
}
