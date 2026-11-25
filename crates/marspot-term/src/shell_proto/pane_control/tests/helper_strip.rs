//! The helper strip's record: what survives the trip, what is cut, and
//! builds that know more or less of it.

use super::super::*;
use crate::shell_proto::{Frame, MsgType, PROTO_VERSION, decode_ping, encode_ping};

fn row(sid: u64) -> StripRow {
    StripRow {
        sid,
        number: Some(3381),
        state: StripState::Working,
        agent: StripAgent::Claude,
        gone: false,
        had_tool: true,
        started_ms: 1_800_000_000_000,
        worked_ms: 90_000,
        running_since_ms: Some(1_800_000_100_000),
        ended_ms: None,
        tokens: None,
        name: "claude opus".into(),
        model: "opus".into(),
        task: "看 parser".into(),
    }
}

pub(super) fn strip(n: u64) -> HelperStrip {
    HelperStrip {
        root_sid: 0x1122_3344_5566_7788,
        batch: 1_800_000_000_000,
        parent_agent: StripAgent::Codex,
        flags: 0,
        rows: (1..=n).map(row).collect(),
    }
}

fn back(m: &HelperStrip) -> HelperStrip {
    decode_helper_strip(&encode_helper_strip(m).unwrap()).unwrap()
}

#[test]
fn helper_strip_round_trips_each_state_and_what_is_not_known() {
    for m in [strip(0), strip(1), strip(24), HelperStrip::empty(9)] {
        assert_eq!(back(&m), m);
    }
    for state in (0..=5).map(StripState::from_u8) {
        assert!(!matches!(state, StripState::Unknown(_)));
        let r = StripRow { state, running_since_ms: None, ended_ms: Some(7), tokens: Some(0), number: None, ..row(1) };
        let m = HelperStrip { rows: vec![r], ..strip(0) };
        assert_eq!(back(&m), m, "no tokens counted is not tokens unknown");
    }
    for bits in 0..4 {
        let r = StripRow { gone: bits & 1 != 0, had_tool: bits & 2 != 0, ..row(1) };
        let m = HelperStrip { rows: vec![r], ..strip(0) };
        assert_eq!(encode_helper_strip(&m).unwrap()[33], bits);
        assert_eq!(back(&m), m);
    }
    let mut older = encode_helper_strip(&strip(1)).unwrap();
    older[33] |= 4;
    assert_eq!(decode_helper_strip(&older).unwrap(), strip(1), "the bit an older shell sets is read by no one");
    let p = encode_helper_strip(&strip(1)).unwrap();
    // the row starts at 21: ended_ms at 37 of it, tokens at 45
    assert_eq!(&p[58..66], &[0; 8], "still at work is 0 on the wire");
    assert_eq!(&p[66..74], &[0xFF; 8], "tokens unknown is all ones on the wire");
    assert!(HelperStrip::empty(9).is_empty() && !strip(1).is_empty());
}

#[test]
fn helper_strip_end_marker_is_a_record_of_no_pane() {
    let m = HelperStrip::all_told();
    assert!(m.is_all_told() && m.is_empty() && m.root_sid == 0);
    assert!(back(&m).is_all_told());
    assert!(!strip(1).is_all_told() && !HelperStrip::empty(9).is_all_told());
}

#[test]
fn helper_strip_elapsed_leaves_out_the_time_it_was_idle() {
    let running = StripRow { worked_ms: 5_000, running_since_ms: Some(100_000), ..row(1) };
    assert_eq!(running.elapsed_ms(102_500), 7_500);
    assert_eq!(running.elapsed_ms(99_000), 5_000, "a clock that went back");
    assert_eq!(StripRow { worked_ms: u64::MAX, ..running.clone() }.elapsed_ms(102_500), u64::MAX);
    let idle = StripRow { running_since_ms: None, ..running };
    assert_eq!(idle.elapsed_ms(200_000), 5_000);
}

#[test]
fn helper_strip_rows_past_the_most_are_left_out() {
    assert_eq!(back(&strip(30)), strip(24));
    // a later build carries more rows than this one reads
    let mut p = encode_helper_strip(&strip(24)).unwrap();
    let one = p[19..].len() / 24;
    let extra = p[19..19 + one].to_vec();
    p[18] = 25;
    p.extend_from_slice(&extra);
    assert_eq!(decode_helper_strip(&p).unwrap(), strip(24));
}

#[test]
fn helper_strip_largest_record_fits_a_payload() {
    let widest = |sid| StripRow { name: "x".repeat(64), model: "x".repeat(64), task: "x".repeat(480), ..row(sid) };
    let m = HelperStrip { rows: (1..=24).map(widest).collect(), ..strip(0) };
    let p = encode_helper_strip(&m).unwrap();
    assert!(p.len() < 64 * 1024, "{}", p.len());
    assert_eq!(decode_helper_strip(&p).unwrap(), m);
}

#[test]
fn helper_strip_text_is_cut_between_characters() {
    let r = StripRow { name: "模".repeat(40), model: "x".repeat(65), task: "任".repeat(161), ..row(1) };
    let m = back(&HelperStrip { rows: vec![r], ..strip(0) });
    let r = &m.rows[0];
    assert_eq!(r.name, "模".repeat(21), "64 bytes end inside the 22nd");
    assert_eq!(r.model.len(), HELPER_STRIP_NAME_MAX);
    assert_eq!(r.task, "任".repeat(160));
    // 120 of the widest characters a task line has
    let r = StripRow { task: "🦀".repeat(120), ..row(1) };
    let m = HelperStrip { rows: vec![r], ..strip(0) };
    assert_eq!(back(&m), m);
}

#[test]
fn helper_strip_takes_a_later_builds_values_and_longer_rows() {
    let mut p = encode_helper_strip(&strip(1)).unwrap();
    (p[16], p[17], p[31], p[32]) = (77, 0x80, 88, 99);
    let seen = decode_helper_strip(&p).unwrap();
    assert_eq!((seen.parent_agent, seen.flags), (StripAgent::Unknown(77), 0x80));
    assert_eq!((seen.rows[0].state, seen.rows[0].agent), (StripState::Unknown(88), StripAgent::Unknown(99)));
    assert_eq!(encode_helper_strip(&seen).unwrap(), p);
    // a row with a field after the last this build knows, then another row
    let two = encode_helper_strip(&strip(2)).unwrap();
    let len = u16::from_le_bytes([two[19], two[20]]) as usize;
    let mut longer = two[..19].to_vec();
    longer.extend_from_slice(&(len as u16 + 3).to_le_bytes());
    longer.extend_from_slice(&two[21..21 + len]);
    longer.extend_from_slice(&[1, 2, 3]);
    longer.extend_from_slice(&two[21 + len..]);
    assert_eq!(decode_helper_strip(&longer).unwrap(), strip(2));
}

#[test]
fn helper_strip_refuses_a_row_shorter_than_its_layout_and_bad_text() {
    let p = encode_helper_strip(&strip(2)).unwrap();
    let mut short = p.clone();
    short[19] -= 1;
    assert!(decode_helper_strip(&short).is_err(), "the row ends inside its task");
    let mut bad = p;
    let last = bad.len() - 1;
    bad[last] = 0xFF;
    assert!(decode_helper_strip(&bad).is_err(), "not utf-8");
}

/// A core from before this frame skips it whole and reads what follows;
/// nothing it announces makes a shell send one.
#[test]
fn helper_strip_is_skipped_by_a_build_without_it() {
    assert_eq!(PROTO_VERSION, 2);
    let mut buf = Vec::new();
    Frame::new(MsgType::HelperStrip, encode_helper_strip(&strip(5)).unwrap()).write_to(&mut buf).unwrap();
    let at = buf.windows(4).position(|w| w == 144u32.to_le_bytes()).unwrap();
    // as a build sees it that has no name for the number
    buf[at..at + 4].copy_from_slice(&199u32.to_le_bytes());
    Frame::new(MsgType::Ping, encode_ping(7)).write_to(&mut buf).unwrap();
    let f = Frame::read_from(&mut std::io::Cursor::new(buf)).unwrap().unwrap();
    assert_eq!((f.msg_type, decode_ping(&f.payload).unwrap()), (MsgType::Ping, 7));
    let before = CAP_TYPE_AND_WATCH | CAP_SHADOWS | CAP_PEEK | CAP_HELPER_PANEL;
    assert_eq!(before & CAP_HELPER_STRIP, 0);
}

#[test]
fn helper_strip_keys_frame_round_trips_and_is_skipped_by_a_build_without_it() {
    for m in [StripKeys { window_id: 7, on: true }, StripKeys { window_id: u32::MAX, on: false }] {
        assert_eq!(decode_strip_keys(&encode_strip_keys(&m)).unwrap(), m);
    }
    assert!(decode_strip_keys(&[1, 0, 0]).is_err());
    assert_eq!(MsgType::from_u32(145), Some(MsgType::StripKeys));
    let mut buf = Vec::new();
    Frame::new(MsgType::StripKeys, encode_strip_keys(&StripKeys { window_id: 1, on: true })).write_to(&mut buf).unwrap();
    let at = buf.windows(4).position(|w| w == 145u32.to_le_bytes()).unwrap();
    buf[at..at + 4].copy_from_slice(&199u32.to_le_bytes());
    Frame::new(MsgType::Ping, encode_ping(8)).write_to(&mut buf).unwrap();
    let f = Frame::read_from(&mut std::io::Cursor::new(buf)).unwrap().unwrap();
    assert_eq!((f.msg_type, decode_ping(&f.payload).unwrap()), (MsgType::Ping, 8));
}
