use super::*;

#[test]
fn pane_render_markup_round_trips() {
    for on in [true, false] {
        let (sid, got) = decode_pane_render_markup(&encode_pane_render_markup(4_242, on)).unwrap();
        assert_eq!((sid, got), (4_242, on));
    }
}

#[test]
fn a_truncated_payload_is_an_error_not_a_guess() {
    assert!(decode_pane_render_markup(&[0u8; 8]).is_err());
}
