//! A many-to-one channel that reuses its storage.
//!
//! `std::sync::mpsc` allocates a block of slots every 31 messages and
//! frees it once read, so a loop fed by it never stops allocating.  This
//! one keeps its messages in a `VecDeque` that grows to the largest
//! backlog it has seen and is then reused: after warm-up a send and a
//! receive touch no allocator.  Same shape as `mpsc` (unbounded, `send`
//! fails once the receiver is gone, `recv` fails once every sender is
//! gone and nothing is left), and the same error types.

use std::collections::VecDeque;
pub use std::sync::mpsc::{RecvError, RecvTimeoutError, SendError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A backlog past this is given back once the queue empties, so one
/// burst does not stay resident for the life of the process.
const KEEP: usize = 4096;

struct State<T> {
    items: VecDeque<T>,
    senders: usize,
    receiver: bool,
}

struct Inner<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
}

impl<T> Inner<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}

pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner {
        state: Mutex::new(State {
            items: VecDeque::new(),
            senders: 1,
            receiver: true,
        }),
        ready: Condvar::new(),
    });
    (
        Sender {
            inner: Arc::clone(&inner),
        },
        Receiver { inner },
    )
}

impl<T> Sender<T> {
    pub fn send(&self, t: T) -> Result<(), SendError<T>> {
        let mut s = self.inner.lock();
        if !s.receiver {
            return Err(SendError(t));
        }
        s.items.push_back(t);
        drop(s);
        self.inner.ready.notify_one();
        Ok(())
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.inner.lock().senders += 1;
        Sender {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut s = self.inner.lock();
        s.senders -= 1;
        if s.senders == 0 {
            drop(s);
            self.inner.ready.notify_one();
        }
    }
}

impl<T> Receiver<T> {
    fn take(s: &mut State<T>) -> Option<T> {
        let t = s.items.pop_front();
        if s.items.is_empty() && s.items.capacity() > KEEP {
            s.items.shrink_to(KEEP / 4);
        }
        t
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut s = self.inner.lock();
        match Self::take(&mut s) {
            Some(t) => Ok(t),
            None if s.senders == 0 => Err(TryRecvError::Disconnected),
            None => Err(TryRecvError::Empty),
        }
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        let mut s = self.inner.lock();
        loop {
            if let Some(t) = Self::take(&mut s) {
                return Ok(t);
            }
            if s.senders == 0 {
                return Err(RecvError);
            }
            s = self.inner.ready.wait(s).unwrap_or_else(|p| p.into_inner());
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut s = self.inner.lock();
        loop {
            if let Some(t) = Self::take(&mut s) {
                return Ok(t);
            }
            if s.senders == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            s = self
                .inner
                .ready
                .wait_timeout(s, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Everything waiting now, without blocking.
    pub fn try_iter(&self) -> impl Iterator<Item = T> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut s = self.inner.lock();
        s.receiver = false;
        // what nobody will read goes now, not when the last sender does;
        // dropped outside the lock, since a message may hold a sender
        let unread = std::mem::take(&mut s.items);
        drop(s);
        drop(unread);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_behaves_as_mpsc_does_at_both_ends() {
        let (tx, rx) = channel();
        let tx2 = tx.clone();
        tx.send(1).unwrap();
        tx2.send(2).unwrap();
        assert_eq!(rx.recv(), Ok(1));
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(5)),
            Err(RecvTimeoutError::Timeout)
        );
        let t = std::thread::spawn(move || tx2.send(3).unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(3));
        t.join().unwrap();
        drop(tx);
        assert_eq!(rx.recv(), Err(RecvError));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        );

        let (tx, rx) = channel();
        drop(rx);
        assert_eq!(tx.send(4), Err(SendError(4)));
    }
}
