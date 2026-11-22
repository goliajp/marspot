//! The bell a pane's program rang.  The session that parsed the BEL
//! has no window, so it says so in a frame and the sound is made here.
//! Nothing else: no flash, no badge, no dock bounce.

/// Bells rung so far; a test cannot hear one.
#[cfg(test)]
pub(super) static RUNG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Every bell goes through here.  Called from a pane's control reader
/// thread: the binding of `NSBeep` does not require the main thread.
pub(super) fn ring() {
    #[cfg(test)]
    RUNG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(test))]
    objc2_app_kit::NSBeep();
}

#[cfg(test)]
mod tests {
    use super::super::{CoreEvent, l3_reader_loop};
    use super::RUNG;
    use marspot::shell_proto::{Frame, MsgType, HEADER_LEN, MAGIC};
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    fn frame_of_type(t: u32) -> Vec<u8> {
        let mut b = Vec::with_capacity(HEADER_LEN);
        b.extend_from_slice(&MAGIC.to_le_bytes());
        b.extend_from_slice(&t.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    }

    /// A session's bells ring once each, and a frame this core does
    /// not know between them is skipped without ending the stream: what
    /// an older core does with `PaneBell` itself.
    #[test]
    fn each_bell_frame_rings_once_and_an_unknown_frame_is_skipped() {
        let (mut session, core) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let (selection_tx, _selection_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || l3_reader_loop(core, 7, tx, selection_tx));
        let before = RUNG.load(Ordering::Relaxed);
        let bell = Frame::new(MsgType::PaneBell, Vec::new());
        bell.write_to(&mut session).unwrap();
        session.write_all(&frame_of_type(250)).unwrap();
        bell.write_to(&mut session).unwrap();
        Frame::new(MsgType::GridReady, Vec::new()).write_to(&mut session).unwrap();
        drop(session);
        reader.join().unwrap();
        assert_eq!(RUNG.load(Ordering::Relaxed) - before, 2);
        let events: Vec<CoreEvent> = rx.try_iter().collect();
        assert!(matches!(events[0], CoreEvent::L3Ready), "the frame after the bells still arrives");
        assert!(matches!(events[1], CoreEvent::L3ControlEof(7)));
        assert_eq!(events.len(), 2);
    }
}
