//! Another process's argv and environment, via `sysctl(KERN_PROCARGS2)`.

use std::cell::RefCell;
use std::sync::OnceLock;

/// `kern.argmax`, read once: the largest argv + envp block the kernel
/// will hand out, so one buffer of this size fits every process.
fn argmax() -> Option<usize> {
    static ARGMAX: OnceLock<Option<usize>> = OnceLock::new();
    *ARGMAX.get_or_init(|| {
        let mut v: libc::c_int = 0;
        let mut sz: libc::size_t = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                &mut v as *mut _ as *mut libc::c_void,
                &mut sz,
                std::ptr::null_mut(),
                0,
            )
        };
        (r == 0 && v > 0).then_some(v as usize)
    })
}

thread_local! {
    static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` over `pid`'s raw `KERN_PROCARGS2` block, in a per-thread
/// buffer that is allocated once and reused.
fn with_procargs<T>(pid: i32, f: impl FnOnce(&[u8]) -> Option<T>) -> Option<T> {
    const KERN_PROCARGS2: libc::c_int = 49;
    let cap = argmax()?;
    BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() != cap {
            buf.resize(cap, 0);
        }
        let mut mib = [libc::CTL_KERN, KERN_PROCARGS2, pid];
        let mut got: libc::size_t = buf.len();
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut got,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return None;
        }
        f(&buf[..got.min(buf.len())])
    })
}

/// Split a `KERN_PROCARGS2` block into argv and the bytes after it.
///
/// Layout: `[argc: i32][exec path]\0[padding \0s][argv[0]]\0...[argv[argc-1]]\0[envp...]`.
/// An empty argv element is kept.  An empty argv[0] cannot be told
/// from the padding and is lost; nothing we look for runs with one.
fn split_argv(block: &[u8]) -> Option<(Vec<&[u8]>, &[u8])> {
    let argc = i32::from_ne_bytes(block.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    let mut rest = &block[4..];
    let path_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    rest = &rest[path_end..];
    let first = rest.iter().position(|&b| b != 0).unwrap_or(rest.len());
    rest = &rest[first..];
    let mut argv = Vec::with_capacity(argc.min(256));
    for _ in 0..argc {
        if rest.is_empty() {
            break;
        }
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        argv.push(&rest[..end]);
        rest = rest.get(end + 1..).unwrap_or(&[]);
    }
    Some((argv, rest))
}

/// The value of `key` in an envp area: NUL-separated `KEY=VALUE`
/// entries ending at an empty entry.
fn env_lookup<'a>(mut env: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let key = key.as_bytes();
    while !env.is_empty() {
        let end = env.iter().position(|&b| b == 0).unwrap_or(env.len());
        let entry = &env[..end];
        if entry.is_empty() {
            return None;
        }
        if entry.len() > key.len() && entry.starts_with(key) && entry[key.len()] == b'=' {
            return Some(&entry[key.len() + 1..]);
        }
        env = env.get(end + 1..).unwrap_or(&[]);
    }
    None
}

/// `pid`'s argv, one string per element, empty elements included.
pub fn proc_argv(pid: i32) -> Option<Vec<String>> {
    with_procargs(pid, |block| {
        let (argv, _) = split_argv(block)?;
        Some(
            argv.iter()
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect(),
        )
    })
}

/// Full command line of `pid`: argv joined by spaces.  Empty argv gives
/// an empty string.
pub fn proc_cmdline(pid: i32) -> Option<String> {
    with_procargs(pid, |block| {
        let (argv, _) = split_argv(block)?;
        let mut out = String::new();
        for (i, a) in argv.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(&String::from_utf8_lossy(a));
        }
        Some(out)
    })
}

/// Look up one environment variable on a running pid.  None when the
/// variable is not set or the call is denied (sandbox / SIP).
pub fn proc_env_value(pid: i32, key: &str) -> Option<String> {
    with_procargs(pid, |block| {
        let (_, env) = split_argv(block)?;
        env_lookup(env, key).map(|v| String::from_utf8_lossy(v).into_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn block(argc: i32, body: &[u8]) -> Vec<u8> {
        let mut v = argc.to_ne_bytes().to_vec();
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn split_keeps_empty_elements_and_finds_the_environment() {
        let b = block(4, b"/bin/sh\0\0\0\0sh\0-c\0\0x\0A=1\0KEY=v\0\0junk");
        let (argv, env) = split_argv(&b).expect("parse");
        assert_eq!(argv, vec![&b"sh"[..], b"-c", b"", b"x"]);
        assert_eq!(env_lookup(env, "KEY"), Some(&b"v"[..]));
        assert_eq!(env_lookup(env, "KE"), None);
        assert_eq!(
            env_lookup(env, "junk"),
            None,
            "lookup stops at the empty entry"
        );
    }

    #[test]
    fn split_stays_in_bounds_on_truncated_blocks() {
        assert!(split_argv(&[1, 0]).is_none());
        let b = block(5, b"/bin/sh\0\0sh\0-c");
        let (argv, env) = split_argv(&b).expect("parse");
        assert_eq!(argv, vec![&b"sh"[..], b"-c"]);
        assert!(env.is_empty());
        assert!(split_argv(&block(-1, b"x\0")).is_none());
        let empty = block(3, b"");
        assert!(split_argv(&empty).expect("parse").0.is_empty());
    }

    /// Read back the argv of a real process that was started with an
    /// empty element in the middle.
    #[test]
    fn a_real_argv_with_an_empty_element_round_trips() {
        use std::os::unix::process::CommandExt;
        // `; :` keeps sh from exec'ing sleep in its own place, so the
        // sleep is a child of the shell and only the group takes it
        let child = crate::test_env::ChildGuard::group(
            std::process::Command::new("/bin/sh")
                .args(["-c", "sleep 30; :", "", "x"])
                .process_group(0)
                .spawn()
                .expect("spawn sh"),
        );
        let pid = child.id() as i32;
        let want = vec!["/bin/sh", "-c", "sleep 30; :", "", "x"];
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = None;
        while Instant::now() < deadline {
            got = proc_argv(pid);
            if got.as_deref().is_some_and(|a| a.len() == want.len()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let cmdline = proc_cmdline(pid);
        // Wait for the sleep, so the guard always has a grandchild to
        // take: dropped before the shell forks, the test passes
        // whether or not the guard kills the group.
        let mut sleeps = Vec::new();
        while sleeps.is_empty() && Instant::now() < deadline {
            sleeps = crate::pidtree::child_pids(pid);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(child);
        assert_eq!(sleeps.len(), 1, "the shell started its sleep");
        let gone = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(sleeps[0], 0) } == 0 && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(unsafe { libc::kill(sleeps[0], 0) }, 0, "the shell's sleep outlived the test");
        assert_eq!(got.expect("argv of the child"), want);
        assert_eq!(cmdline.as_deref(), Some("/bin/sh -c sleep 30; :  x"));
    }

    #[test]
    fn one_buffer_serves_repeated_reads() {
        let me = std::process::id() as i32;
        let a = proc_cmdline(me).expect("own cmdline");
        let b = proc_cmdline(me).expect("own cmdline");
        assert_eq!(a, b);
        let cap = BUF.with(|c| c.borrow().len());
        assert_eq!(Some(cap), argmax(), "the buffer is sized from kern.argmax");
        assert!(proc_argv(i32::MAX).is_none());
    }
}
