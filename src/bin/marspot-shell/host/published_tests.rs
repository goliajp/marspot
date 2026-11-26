use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use super::Published;

#[test]
fn nothing_is_read_before_the_first_publish() {
    let p: Published<Vec<u32>> = Published::new();
    assert!(p.get().is_none());
    assert!(p.publish(vec![1]));
    assert_eq!(*p.get().unwrap(), vec![1]);
}

#[test]
fn equal_content_is_not_published_again() {
    let p = Published::with(vec![1, 2]);
    let first = p.get().unwrap();
    assert!(!p.publish(vec![1, 2]));
    assert!(Arc::ptr_eq(&first, &p.get().unwrap()), "readers keep the same snapshot");
    assert!(p.publish(vec![3]));
    assert_eq!(*first, vec![1, 2], "a snapshot taken before does not change");
    let third = p.get().unwrap();
    p.update(|_| {});
    assert!(Arc::ptr_eq(&third, &p.get().unwrap()), "an update that changes nothing publishes nothing");
}

/// Panics when compared, the way a writer may panic while it holds the
/// writer lock.
#[derive(Clone, Default)]
struct Touchy(u32);

impl PartialEq for Touchy {
    fn eq(&self, other: &Self) -> bool {
        if other.0 == 99 {
            panic!("compare");
        }
        self.0 == other.0
    }
}

#[test]
fn a_writer_that_panics_mid_swap_leaves_readers_a_snapshot() {
    let p = Arc::new(Published::with(Touchy(1)));
    // a panic inside update: its copy is lost, the writer lock poisoned
    let w = Arc::clone(&p);
    let r = std::thread::spawn(move || {
        w.update(|v| {
            v.0 = 5;
            panic!("writer");
        })
    })
    .join();
    assert!(r.is_err());
    assert_eq!(p.get().unwrap().0, 1, "the value from before stays");
    // a panic in the comparison of a publish
    let w = Arc::clone(&p);
    assert!(std::thread::spawn(move || w.publish(Touchy(99))).join().is_err());
    assert_eq!(p.get().unwrap().0, 1);
    // readers on other threads still get it, and writers still write
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let r = Arc::clone(&p);
            std::thread::spawn(move || r.get().map(|v| v.0))
        })
        .collect();
    for h in readers {
        assert_eq!(h.join().unwrap(), Some(1));
    }
    assert!(p.publish(Touchy(2)));
    assert_eq!(p.update(|v| v.0), 2);
}

#[test]
fn a_poisoned_snapshot_lock_is_taken_back() {
    let p = Published::with(7u32);
    let r = catch_unwind(AssertUnwindSafe(|| {
        let _held = p.current.lock().unwrap();
        panic!("holder");
    }));
    assert!(r.is_err());
    assert!(p.current.is_poisoned());
    assert_eq!(*p.get().unwrap(), 7);
    assert!(p.publish(8));
    assert_eq!(*p.get().unwrap(), 8);
}

#[test]
fn updates_from_several_threads_are_not_lost() {
    let p = Arc::new(Published::<u64>::new());
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let p = Arc::clone(&p);
            std::thread::spawn(move || {
                for _ in 0..250 {
                    p.update(|n| *n += 1);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(*p.get().unwrap(), 1000);
}
