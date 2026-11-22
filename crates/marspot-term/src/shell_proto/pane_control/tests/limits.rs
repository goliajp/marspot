//! Unknown values, bad text, size limits and older readers.

use super::super::*;
use super::{samples, spawn, typed_input, watch};
use crate::shell_proto::{Frame, MAX_PAYLOAD_LEN, MsgType, Next, decode_ping, encode_ping};

#[test]
fn pane_control_unknown_enum_values_decode_and_encode_back() {
    let mut p = encode_pane_watch(&PaneWatch {
        req_id: 1,
        sid: 2,
        watch: watch(""),
    })
    .unwrap();
    p[16] = 42;
    let m = decode_pane_watch(&p).unwrap();
    assert_eq!(m.watch.kind, WatchKind::Unknown(42));
    assert_eq!(encode_pane_watch(&m).unwrap(), p);
    let mut p = encode_pane_done(&PaneDone {
        req_id: 1,
        code: DoneCode::Other,
        text: String::new(),
    })
    .unwrap();
    p[4] = 200;
    assert_eq!(decode_pane_done(&p).unwrap().code, DoneCode::Unknown(200));
    assert_eq!(TypedResult::from_u8(9), TypedResult::Unknown(9));
    assert_eq!(WatchReason::from_u8(0), WatchReason::Unknown(0));
    assert_eq!(RetireMode::from_u8(2).to_u8(), 2);
    assert_eq!(PlaceWhere::from_u8(4), PlaceWhere::Unknown(4));
    assert_eq!(PanelAct::from_u8(0), PanelAct::Unknown(0));
    assert!(
        WatchBody {
            kind: WatchKind::Cancel,
            watch_id: 0,
            ..watch("")
        }
        .is_clear_all()
    );
    assert!(
        !WatchBody {
            kind: WatchKind::Cancel,
            watch_id: 1,
            ..watch("")
        }
        .is_clear_all()
    );
}

#[test]
fn pane_control_text_that_is_not_utf8_is_refused() {
    let mut p = encode_pane_done(&PaneDone {
        req_id: 1,
        code: DoneCode::Other,
        text: "ab".into(),
    })
    .unwrap();
    p[7] = 0xFF;
    assert!(decode_pane_done(&p).is_err());
    let mut p = encode_pane_spawn(&PaneSpawn {
        cwd: "/x".into(),
        ..spawn()
    })
    .unwrap();
    p[24] = 0xFF;
    assert!(decode_pane_spawn(&p).is_err());
}

#[test]
fn pane_control_size_limits_are_enforced() {
    let at_limit = PaneType {
        bytes: vec![b'a'; MAX_TYPE_BYTES],
        ..typed_input(true)
    };
    let p = encode_pane_type(&at_limit).unwrap();
    assert_eq!(decode_pane_type(&p).unwrap(), at_limit);
    let over = PaneType {
        bytes: vec![b'a'; MAX_TYPE_BYTES + 1],
        ..typed_input(false)
    };
    assert!(encode_pane_type(&over).is_err());
    // a hand-made frame over the input limit is refused by the reader too
    let mut p = encode_pane_type(&typed_input(false)).unwrap();
    p.truncate(13);
    p.extend_from_slice(&((MAX_TYPE_BYTES + 1) as u32).to_le_bytes());
    p.resize(17 + MAX_TYPE_BYTES + 1, b'a');
    assert!(decode_pane_type(&p).is_err());
    // the watch text fits its u16 length but the frame would not
    let big = PaneType {
        watch: Some(watch(&"w".repeat(40_000))),
        ..at_limit
    };
    assert!(encode_pane_type(&big).is_err());
    let cwd = "c".repeat(u16::MAX as usize);
    assert!(
        encode_pane_spawn(&PaneSpawn { cwd, ..spawn() }).is_err(),
        "over the frame limit"
    );
    let text = "t".repeat(u16::MAX as usize + 1);
    assert!(
        encode_pane_done(&PaneDone {
            req_id: 1,
            code: DoneCode::Other,
            text
        })
        .is_err(),
        "over the u16 length"
    );
    let fits = PaneDone {
        req_id: 1,
        code: DoneCode::Other,
        text: "t".repeat(MAX_PAYLOAD_LEN - 7),
    };
    assert_eq!(encode_pane_done(&fits).unwrap().len(), MAX_PAYLOAD_LEN);
}

/// Each frame's length keeps the reader aligned with the frame after it,
/// which is also what lets a reader that does not know the type skip it.
#[test]
fn pane_control_frames_stay_aligned_with_their_neighbours() {
    let mut buf = Vec::new();
    let mut frames: Vec<(MsgType, Vec<u8>)> =
        samples().into_iter().map(|(t, p, _)| (t, p)).collect();
    frames.push((MsgType::SessionReadsPaneControl, Vec::new()));
    for (i, (t, p)) in frames.iter().enumerate() {
        Frame::new(*t, p.clone()).write_to(&mut buf).unwrap();
        Frame::new(MsgType::Ping, encode_ping(i as u32))
            .write_to(&mut buf)
            .unwrap();
    }
    let mut cur = std::io::Cursor::new(buf);
    let mut pings = Vec::new();
    while let Some(next) = Frame::read_next(&mut cur).unwrap() {
        match next {
            Next::Frame(f) if (130..=143).contains(&(f.msg_type as u32)) => {}
            Next::Frame(f) => pings.push(decode_ping(&f.payload).unwrap()),
            Next::Unknown(t) => panic!("type {t} is known to this build"),
        }
    }
    assert_eq!(pings, (0..frames.len() as u32).collect::<Vec<_>>());
}
