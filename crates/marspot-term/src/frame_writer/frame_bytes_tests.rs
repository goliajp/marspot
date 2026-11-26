use super::*;
use crate::shell_proto::FrameReader;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Every frame `peer` gets until the writer's end closes.
fn read_all(mut peer: UnixStream) -> Vec<(MsgType, Vec<u8>)> {
    peer.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut reader = FrameReader::new();
    let mut got = Vec::new();
    while let Some(f) = reader.read_known(&mut peer, |t, p| (t, p.to_vec())).unwrap() {
        got.push(f);
    }
    got
}

fn wait_drained(w: &FrameWriter) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while w.backlog() > 0 {
        assert!(Instant::now() < deadline, "the writer never drained");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn queue_capacity(w: &FrameWriter) -> usize {
    w.shared.lock().bytes.capacity()
}

/// A payload naming its sender and its place in that sender's order,
/// padded to a length that varies so frames do not line up.
fn payload(out: &mut Vec<u8>, sender: u8, seq: u32) {
    out.push(sender);
    out.extend_from_slice(&seq.to_le_bytes());
    out.resize(out.len() + (seq as usize * 7 + sender as usize) % 300, sender);
}

#[test]
fn frames_from_many_threads_arrive_whole_and_each_senders_in_order() {
    const SENDERS: u8 = 6;
    const EACH: u32 = 2000;
    let (a, b) = UnixStream::pair().unwrap();
    let reading = std::thread::spawn(move || read_all(b));
    let w = Arc::new(FrameWriter::new("test-writer", a, 4 * 1024 * 1024));
    let threads: Vec<_> = (0..SENDERS)
        .map(|sender| {
            let w = Arc::clone(&w);
            std::thread::spawn(move || {
                for seq in 0..EACH {
                    let msg = if seq % 2 == 0 { MsgType::KeyEvent } else { MsgType::InjectInput };
                    assert!(w.send_with(msg, |out| payload(out, sender, seq)));
                }
            })
        })
        .collect();
    threads.into_iter().for_each(|t| t.join().unwrap());
    drop(Arc::into_inner(w).unwrap());
    let got = reading.join().unwrap();
    assert_eq!(got.len(), SENDERS as usize * EACH as usize);
    let mut next = [0u32; SENDERS as usize];
    for (msg, p) in got {
        let sender = p[0];
        let seq = u32::from_le_bytes(p[1..5].try_into().unwrap());
        assert_eq!(seq, next[sender as usize], "sender {sender} out of order");
        next[sender as usize] += 1;
        let want_msg = if seq % 2 == 0 { MsgType::KeyEvent } else { MsgType::InjectInput };
        assert_eq!(msg, want_msg);
        let mut want = Vec::new();
        payload(&mut want, sender, seq);
        assert_eq!(p, want, "sender {sender} frame {seq} not whole");
    }
}

#[test]
fn the_buffers_are_given_back_after_a_burst() {
    let (a, mut b) = UnixStream::pair().unwrap();
    // read raw: these frames are past what a frame reader accepts
    let reading = std::thread::spawn(move || {
        let mut all = Vec::new();
        std::io::Read::read_to_end(&mut b, &mut all).unwrap();
        all.len()
    });
    let w = FrameWriter::new("test-writer", a, 4 * 1024 * 1024);
    let big = vec![b'x'; 256 * 1024];
    for _ in 0..4 {
        assert!(w.send_with(MsgType::SelectionText, |out| out.extend_from_slice(&big)));
    }
    wait_drained(&w);
    assert!(queue_capacity(&w) <= KEEP_CAPACITY, "the queue kept {} B", queue_capacity(&w));
    // the next frame is queued in the buffer the writer thread had
    assert!(w.send_with(MsgType::KeyEvent, |out| out.push(1)));
    wait_drained(&w);
    assert!(queue_capacity(&w) <= KEEP_CAPACITY, "the writer kept {} B", queue_capacity(&w));
    drop(w);
    assert_eq!(reading.join().unwrap(), 4 * (HEADER_LEN + big.len()) + HEADER_LEN + 1);
}

#[test]
fn a_refused_frame_does_not_leave_the_queue_large() {
    let (a, _b) = UnixStream::pair().unwrap();
    let w = FrameWriter::new("test-writer", a, 128 * 1024);
    assert!(!w.send_with(MsgType::SelectionText, |out| out.resize(out.len() + 1024 * 1024, b'x')));
    assert!(queue_capacity(&w) <= KEEP_CAPACITY, "the queue kept {} B", queue_capacity(&w));
    assert_eq!(w.dropped_bytes(), HEADER_LEN + 1024 * 1024);
    assert_eq!(w.backlog(), 0);
}

#[test]
fn a_panic_while_encoding_leaves_no_part_of_a_frame() {
    let (a, b) = UnixStream::pair().unwrap();
    let reading = std::thread::spawn(move || read_all(b));
    let w = FrameWriter::new("test-writer", a, 1024 * 1024);
    assert!(w.send_with(MsgType::KeyEvent, |out| out.push(1)));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        w.send_with(MsgType::KeyEvent, |out| {
            out.extend_from_slice(&[9; 40]);
            panic!("encode failed");
        })
    }));
    assert!(panicked.is_err());
    assert!(w.send_with(MsgType::KeyEvent, |out| out.push(2)));
    drop(w);
    assert_eq!(
        reading.join().unwrap(),
        vec![(MsgType::KeyEvent, vec![1]), (MsgType::KeyEvent, vec![2])]
    );
}

#[test]
fn close_writes_everything_queued_before_the_end() {
    let (a, b) = UnixStream::pair().unwrap();
    let mut w = FrameWriter::new("test-writer", a, 4 * 1024 * 1024);
    // far more than the socket holds, with nobody reading yet
    for seq in 0..400u32 {
        assert!(w.send_with(MsgType::InjectInput, |out| payload(out, 0, seq)));
    }
    w.close();
    assert!(!w.send_with(MsgType::KeyEvent, |out| out.push(0)), "a closed writer took a frame");
    let got = read_all(b);
    assert_eq!(got.len(), 400);
    for (seq, (_, p)) in got.into_iter().enumerate() {
        let mut want = Vec::new();
        payload(&mut want, 0, seq as u32);
        assert_eq!(p, want);
    }
}
