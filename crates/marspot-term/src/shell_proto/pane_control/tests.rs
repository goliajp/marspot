use super::*;
use crate::shell_proto::MsgType;

mod helper_strip;
mod limits;

pub(super) fn watch(text: &str) -> WatchBody {
    WatchBody {
        watch_id: 0x0102_0304,
        kind: WatchKind::Text,
        since_pos: 0x1122_3344_5566_7788,
        quiet_ms: 1500,
        text: text.into(),
    }
}

pub(super) fn spawn() -> PaneSpawn {
    PaneSpawn {
        req_id: 7,
        shadow: true,
        no_focus: true,
        parent_sid: 0xAB_CDEF,
        idle_s: 21600,
        cols: 120,
        rows: 40,
        cwd: "/tmp/目录".into(),
    }
}

pub(super) fn typed_input(with_watch: bool) -> PaneType {
    PaneType {
        req_id: 9,
        sid: 0xFEED,
        paste: true,
        enter: true,
        only_at_shell: false,
        bytes: b"cargo test\n".to_vec(),
        watch: with_watch.then(|| WatchBody {
            kind: WatchKind::CommandEnd,
            ..watch("")
        }),
    }
}

pub(super) type Decode = fn(&[u8]) -> std::io::Result<String>;

/// One encoded payload of every frame that has one, with a decoder
/// that renders what it read, so the frames can be checked alike.
pub(super) fn samples() -> Vec<(MsgType, Vec<u8>, Decode)> {
    fn s(t: MsgType, p: Vec<u8>, d: Decode) -> (MsgType, Vec<u8>, Decode) {
        (t, p, d)
    }
    fn shown<T: std::fmt::Debug>(r: std::io::Result<T>) -> std::io::Result<String> {
        r.map(|t| format!("{t:?}"))
    }
    vec![
        s(
            MsgType::PaneSpawn,
            encode_pane_spawn(&spawn()).unwrap(),
            |p| shown(decode_pane_spawn(p)),
        ),
        s(
            MsgType::PaneSpawned,
            encode_pane_spawned(&PaneSpawned {
                req_id: 7,
                ok: false,
                sid: 0,
                err: "no room".into(),
            })
            .unwrap(),
            |p| shown(decode_pane_spawned(p)),
        ),
        s(
            MsgType::PaneRetire,
            encode_pane_retire(&PaneRetire {
                req_id: 1,
                sid: 2,
                mode: RetireMode::Recycle,
            }),
            |p| shown(decode_pane_retire(p)),
        ),
        s(
            MsgType::PanePlace,
            encode_pane_place(&PanePlace {
                req_id: 1,
                sid: 2,
                to: PlaceWhere::Peek,
                host_sid: 3,
            }),
            |p| shown(decode_pane_place(p)),
        ),
        s(
            MsgType::PaneWatch,
            encode_pane_watch(&PaneWatch {
                req_id: 4,
                sid: 5,
                watch: watch("test result:"),
            })
            .unwrap(),
            |p| shown(decode_pane_watch(p)),
        ),
        s(
            MsgType::PaneWatchFired,
            encode_pane_watch_fired(&PaneWatchFired {
                sid: 5,
                watch_id: 6,
                reason: WatchReason::Met,
                pos: 1 << 40,
                status: Some(1),
                line: Some(12345),
            }),
            |p| shown(decode_pane_watch_fired(p)),
        ),
        s(
            MsgType::SessionShadow,
            encode_session_shadow(&SessionShadow {
                req_id: 3,
                parent_sid: 8,
                idle_s: 60,
            }),
            |p| shown(decode_session_shadow(p)),
        ),
        s(
            MsgType::PaneSize,
            encode_pane_size(&PaneSize {
                req_id: 3,
                sid: 8,
                cols: 200,
                rows: 50,
            }),
            |p| shown(decode_pane_size(p)),
        ),
        s(
            MsgType::PaneType,
            encode_pane_type(&typed_input(false)).unwrap(),
            |p| shown(decode_pane_type(p)),
        ),
        s(
            MsgType::PaneType,
            encode_pane_type(&typed_input(true)).unwrap(),
            |p| shown(decode_pane_type(p)),
        ),
        s(
            MsgType::PaneTyped,
            encode_pane_typed(&PaneTyped {
                req_id: 9,
                sid: 0xFEED,
                result: TypedResult::NotReading,
                echo_off: true,
                pos: 77,
                written: 11,
            }),
            |p| shown(decode_pane_typed(p)),
        ),
        s(
            MsgType::PaneDone,
            encode_pane_done(&PaneDone {
                req_id: 2,
                code: DoneCode::OldSession,
                text: "old".into(),
            })
            .unwrap(),
            |p| shown(decode_pane_done(p)),
        ),
        s(
            MsgType::PaneControlCaps,
            encode_pane_control_caps(CAP_TYPE_AND_WATCH | CAP_SHADOWS),
            |p| shown(decode_pane_control_caps(p)),
        ),
        s(
            MsgType::HelperPanelAct,
            encode_helper_panel_act(&HelperPanelAct {
                sid: 4,
                act: PanelAct::ToggleCwdControl,
                want: CwdControl::Allow,
            }),
            |p| shown(decode_helper_panel_act(p)),
        ),
        s(
            MsgType::HelperStrip,
            encode_helper_strip(&helper_strip::strip(2)).unwrap(),
            |p| shown(decode_helper_strip(p)),
        ),
    ]
}

#[test]
fn pane_control_type_numbers_are_130_to_144() {
    let names = [
        MsgType::PaneSpawn,
        MsgType::PaneSpawned,
        MsgType::PaneRetire,
        MsgType::PanePlace,
        MsgType::PaneWatch,
        MsgType::PaneWatchFired,
        MsgType::SessionShadow,
        MsgType::PaneSize,
        MsgType::PaneType,
        MsgType::PaneTyped,
        MsgType::PaneDone,
        MsgType::PaneControlCaps,
        MsgType::SessionReadsPaneControl,
        MsgType::HelperPanelAct,
        MsgType::HelperStrip,
    ];
    for (i, t) in names.into_iter().enumerate() {
        assert_eq!(t as u32, 130 + i as u32);
        assert_eq!(MsgType::from_u32(130 + i as u32), Some(t));
    }
}

#[test]
fn pane_control_each_frame_round_trips() {
    let s = spawn();
    assert_eq!(
        decode_pane_spawn(&encode_pane_spawn(&s).unwrap()).unwrap(),
        s
    );
    let s = PaneSpawn {
        shadow: false,
        no_focus: false,
        cwd: String::new(),
        ..s
    };
    assert_eq!(
        decode_pane_spawn(&encode_pane_spawn(&s).unwrap()).unwrap(),
        s
    );
    let m = PaneSpawned {
        req_id: 1,
        ok: true,
        sid: u64::MAX - 1,
        err: String::new(),
    };
    assert_eq!(
        decode_pane_spawned(&encode_pane_spawned(&m).unwrap()).unwrap(),
        m
    );
    let m = PaneRetire {
        req_id: 1,
        sid: 2,
        mode: RetireMode::Delete,
    };
    assert_eq!(decode_pane_retire(&encode_pane_retire(&m)).unwrap(), m);
    for to in [
        PlaceWhere::Shadow,
        PlaceWhere::Window,
        PlaceWhere::Peek,
        PlaceWhere::Unpeek,
    ] {
        let m = PanePlace {
            req_id: 1,
            sid: 2,
            to,
            host_sid: 9,
        };
        assert_eq!(decode_pane_place(&encode_pane_place(&m)).unwrap(), m);
    }
    let m = PaneWatch {
        req_id: 4,
        sid: 5,
        watch: watch("匹配 this"),
    };
    assert_eq!(
        decode_pane_watch(&encode_pane_watch(&m).unwrap()).unwrap(),
        m
    );
    let m = PaneWatchFired {
        sid: 5,
        watch_id: 6,
        reason: WatchReason::Restarting,
        pos: 3,
        status: Some(0),
        line: Some(0),
    };
    assert_eq!(
        decode_pane_watch_fired(&encode_pane_watch_fired(&m)).unwrap(),
        m
    );
    let m = PaneWatchFired {
        status: None,
        line: None,
        ..m
    };
    let p = encode_pane_watch_fired(&m);
    assert_eq!(
        &p[21..23],
        &(-1i16).to_le_bytes(),
        "no status is -1 on the wire"
    );
    assert_eq!(&p[23..31], &[0xFF; 8], "no line is all ones on the wire");
    assert_eq!(decode_pane_watch_fired(&p).unwrap(), m);
    let m = SessionShadow {
        req_id: 3,
        parent_sid: 0,
        idle_s: 0,
    };
    assert_eq!(
        decode_session_shadow(&encode_session_shadow(&m)).unwrap(),
        m
    );
    let m = PaneSize {
        req_id: 3,
        sid: 8,
        cols: u16::MAX,
        rows: 1,
    };
    assert_eq!(decode_pane_size(&encode_pane_size(&m)).unwrap(), m);
    let m = PaneTyped {
        req_id: 9,
        sid: 1,
        result: TypedResult::Written,
        echo_off: false,
        pos: 1,
        written: 2,
    };
    assert_eq!(decode_pane_typed(&encode_pane_typed(&m)).unwrap(), m);
    let m = PaneDone {
        req_id: 2,
        code: DoneCode::Ok,
        text: String::new(),
    };
    assert_eq!(decode_pane_done(&encode_pane_done(&m).unwrap()).unwrap(), m);
    assert_eq!(
        decode_pane_control_caps(&encode_pane_control_caps(0xF)).unwrap(),
        0xF
    );
    let m = HelperPanelAct {
        sid: 4,
        act: PanelAct::CloseAllOfParent,
        want: CwdControl::Deny,
    };
    let p = encode_helper_panel_act(&m);
    assert_eq!(decode_helper_panel_act(&p).unwrap(), m);
    // as first laid out, with nothing after the act
    let old = HelperPanelAct { want: CwdControl::Flip, ..m };
    assert_eq!(decode_helper_panel_act(&p[..9]).unwrap(), old);
}

#[test]
fn pane_control_layouts_have_the_documented_sizes() {
    let sizes: Vec<usize> = samples().iter().map(|(_, p, _)| p.len()).collect();
    let cwd = "/tmp/目录".len();
    let w = 4 + 1 + 8 + 4 + 2;
    let expected = [
        23 + cwd,
        15 + 7,
        13,
        21,
        12 + w + 12,
        31,
        16,
        16,
        17 + 11,
        17 + 11 + w,
        26,
        7 + 3,
        4,
        10,
        19 + 2 * (2 + 53 + 6 + 11 + 4 + "看 parser".len()),
    ];
    assert_eq!(sizes, expected);
}

#[test]
fn pane_control_type_with_and_without_a_watch() {
    for with in [false, true] {
        let m = typed_input(with);
        let p = encode_pane_type(&m).unwrap();
        assert_eq!(
            p[12] & 0b1000 != 0,
            with,
            "bit 3 says whether a watch follows"
        );
        assert_eq!(p[12] & 0b0111, 0b011);
        let back = decode_pane_type(&p).unwrap();
        assert_eq!(back, m);
        if with {
            // the embedded body is PaneWatch's from watch_id on
            let pw = encode_pane_watch(&PaneWatch {
                req_id: m.req_id,
                sid: m.sid,
                watch: m.watch.clone().unwrap(),
            })
            .unwrap();
            assert_eq!(&p[17 + m.bytes.len()..], &pw[12..]);
        }
    }
    let m = PaneType {
        only_at_shell: true,
        paste: false,
        enter: false,
        bytes: Vec::new(),
        ..typed_input(false)
    };
    assert_eq!(decode_pane_type(&encode_pane_type(&m).unwrap()).unwrap(), m);
}

#[test]
fn pane_control_truncated_payloads_are_refused_at_every_length() {
    for (t, p, decode) in samples() {
        assert!(decode(&p).is_ok(), "{t:?} decodes whole");
        // the last byte came later, and may be missing
        let least = p.len() - usize::from(t == MsgType::HelperPanelAct);
        for n in 0..least {
            assert!(
                decode(&p[..n]).is_err(),
                "{t:?} cut to {n} of {} bytes decoded",
                p.len()
            );
        }
    }
}

#[test]
fn pane_control_trailing_bytes_are_ignored() {
    for (t, p, decode) in samples() {
        let whole = decode(&p).unwrap();
        let mut longer = p.clone();
        longer.extend_from_slice(&[0xFF, 0, 7, 9]);
        assert_eq!(
            decode(&longer).unwrap(),
            whole,
            "{t:?} with a later build's tail"
        );
    }
}
