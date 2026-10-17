//! The updater's whole network path against a feed served from the
//! test itself: fetch, compare versions, download, verify, extract,
//! stage.  Releases are signed with a throwaway key the sandbox trusts
//! (see `verify::trusted_pubkeys`); every way a release can be wrong
//! must leave `binaries/pending/` empty.

use super::verify::ENV_TEST_UPDATE_PUBKEY;
use super::verify::testkey::{keypair, sign};
use super::*;
use crate::test_env::{StateSandbox, sandbox_state_dir};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;

/// A one-thread HTTP server on an ephemeral loopback port, serving
/// fixed bodies by path and recording every path asked for.
struct Server {
    port: u16,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    asked: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (f, a, s) = (files.clone(), asked.clone(), stop.clone());
        let thread = thread::spawn(move || {
            for conn in listener.incoming() {
                if s.load(Ordering::Acquire) {
                    return;
                }
                if let Ok(conn) = conn {
                    serve(conn, &f, &a);
                }
            }
        });
        Self {
            port,
            files,
            asked,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}/{path}", self.port)
    }

    fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.files.lock().unwrap().insert(format!("/{path}"), body.into());
    }

    fn times_asked(&self, path: &str) -> usize {
        let want = format!("/{path}");
        self.asked.lock().unwrap().iter().filter(|p| **p == want).count()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(conn: TcpStream, files: &Mutex<HashMap<String, Vec<u8>>>, asked: &Mutex<Vec<String>>) {
    conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(&conn);
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header == "\r\n" => break,
            Ok(_) => {}
        }
    }
    let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
    asked.lock().unwrap().push(path.clone());
    let body = files.lock().unwrap().get(&path).cloned();
    let mut w = &conn;
    let _ = match body {
        Some(b) => write!(
            w,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            b.len()
        )
        .and_then(|_| w.write_all(&b)),
        None => w.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    };
}

const ASSET: &str = "marspot-aarch64-apple-darwin.tar.gz";

/// A tarball of stand-in binaries, each holding its own name, at
/// `prefix` inside the archive.
fn tarball(dir: &Path, bins: &[&str], prefix: &str) -> Vec<u8> {
    let src = dir.join("tar-src");
    let _ = std::fs::remove_dir_all(&src);
    std::fs::create_dir_all(src.join(prefix)).unwrap();
    for b in bins {
        std::fs::write(src.join(prefix).join(b), format!("#!/bin/sh\n# {b}\n")).unwrap();
    }
    let out = dir.join("out.tar.gz");
    let mut cmd = Command::new("/usr/bin/tar");
    cmd.arg("-czf").arg(&out).arg("-C").arg(&src);
    for b in bins {
        cmd.arg(Path::new(prefix).join(b));
    }
    assert!(cmd.status().unwrap().success(), "tar");
    std::fs::read(out).unwrap()
}

fn feed(server: &Server, tag: &str, with_sig: bool) -> String {
    let sig = if with_sig {
        format!(
            r#",{{"name": "{ASSET}.sig", "browser_download_url": "{}"}}"#,
            server.url(&format!("{ASSET}.sig"))
        )
    } else {
        String::new()
    };
    format!(
        r#"{{"tag_name": "{tag}", "assets": [{{"name": "{ASSET}", "browser_download_url": "{}"}}{sig}]}}"#,
        server.url(ASSET)
    )
}

/// A sandbox that trusts a fresh test key, a server, and a release of
/// all three binaries signed with that key, offered as `tag`.
struct Release {
    sandbox: StateSandbox,
    server: Server,
    sec: PathBuf,
    tarball: Vec<u8>,
}

impl Release {
    fn new(tag: &str) -> Self {
        let mut sandbox = sandbox_state_dir();
        let keys = sandbox.path().join("keys");
        std::fs::create_dir_all(&keys).unwrap();
        let (sec, public) = keypair(&keys, "test");
        sandbox.env.set(ENV_TEST_UPDATE_PUBKEY, &public);
        let server = Server::start();
        sandbox.env.set("MARSPOT_UPDATE_FEED", server.url("feed.json"));
        let tarball = tarball(&keys, STAGED_BINARIES, "");
        let r = Self {
            sandbox,
            server,
            sec,
            tarball,
        };
        r.offer(tag, r.tarball.clone(), true);
        r
    }

    /// Serve `body` as the asset, its signature by the trusted key over
    /// the release's real tarball, and a feed naming them.
    fn offer(&self, tag: &str, body: Vec<u8>, with_sig: bool) {
        let dir = self.sandbox.path().join("keys");
        let signed = dir.join("signed.tar.gz");
        std::fs::write(&signed, &self.tarball).unwrap();
        let sig = dir.join("signed.sig");
        sign(&self.sec, &signed, &sig);
        self.server.put(ASSET, body);
        self.server.put(&format!("{ASSET}.sig"), std::fs::read(sig).unwrap());
        self.server.put("feed.json", feed(&self.server, tag, with_sig));
    }

    fn pending(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.sandbox.path().join("binaries/pending"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn assert_nothing_staged(&self, what: &str) {
        assert_eq!(self.pending(), Vec::<String>::new(), "{what}: something was staged");
    }
}

/// A newer release, signed with a trusted key: every binary in it lands
/// in `pending/`, byte for byte and executable, and the download's
/// scratch files are gone.
#[test]
fn a_signed_newer_release_is_staged() {
    let r = Release::new("v1.0.1");
    assert_eq!(check_and_stage("1.0.0"), Ok(true));
    let mut want: Vec<String> = STAGED_BINARIES.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(r.pending(), want);
    for b in STAGED_BINARIES {
        let p = pending_binary_path(b);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("#!/bin/sh\n# {b}\n"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755);
    }
    let download = r.sandbox.path().join("download");
    assert!(!download.join("marspot.tar.gz").exists());
    assert!(!download.join("marspot.tar.gz.sig").exists());
    assert!(!download.join("marspot.tar.d").exists());
    assert!(pending_already_staged());
}

/// A release signed with a key nobody trusts is rejected before
/// anything is unpacked.
#[test]
fn a_release_signed_with_another_key_is_rejected() {
    let r = Release::new("v1.0.1");
    let (other, _) = keypair(&r.sandbox.path().join("keys"), "other");
    let file = r.sandbox.path().join("keys/other.tar.gz");
    std::fs::write(&file, &r.tarball).unwrap();
    let sig = r.sandbox.path().join("keys/other.sig");
    sign(&other, &file, &sig);
    r.server.put(&format!("{ASSET}.sig"), std::fs::read(sig).unwrap());
    let e = check_and_stage("1.0.0").unwrap_err();
    assert!(e.contains("signature verification failed"), "{e}");
    r.assert_nothing_staged("wrong key");
    assert!(!r.sandbox.path().join("download/marspot.tar.gz").exists());
}

/// One byte changed after signing and the signature no longer holds.
#[test]
fn a_tampered_release_is_rejected() {
    let r = Release::new("v1.0.1");
    let mut evil = r.tarball.clone();
    let last = evil.len() - 1;
    evil[last] ^= 0x01;
    r.offer("v1.0.1", evil, true);
    let e = check_and_stage("1.0.0").unwrap_err();
    assert!(e.contains("signature verification failed"), "{e}");
    r.assert_nothing_staged("tampered");
}

/// No signature asset at all: rejected without downloading the tarball.
#[test]
fn an_unsigned_release_is_rejected() {
    let r = Release::new("v1.0.1");
    r.offer("v1.0.1", r.tarball.clone(), false);
    let e = check_and_stage("1.0.0").unwrap_err();
    assert!(e.contains("unsigned releases are rejected"), "{e}");
    r.assert_nothing_staged("unsigned");
    assert_eq!(r.server.times_asked(ASSET), 0, "the tarball was downloaded anyway");
}

/// A signature asset the feed names but the server does not have is a
/// failed download, not a pass.
#[test]
fn a_signature_that_cannot_be_downloaded_is_a_failure() {
    let r = Release::new("v1.0.1");
    r.server.files.lock().unwrap().remove(&format!("/{ASSET}.sig"));
    let e = check_and_stage("1.0.0").unwrap_err();
    assert!(e.contains("curl download"), "{e}");
    r.assert_nothing_staged("missing signature");
}

/// The same or an older version is not an update, and nothing is
/// downloaded for it.
#[test]
fn the_same_or_an_older_version_is_left_alone() {
    let r = Release::new("v1.0.0");
    assert_eq!(check_and_stage("1.0.0"), Ok(false), "equal");
    r.offer("v0.9.9", r.tarball.clone(), true);
    assert_eq!(check_and_stage("1.0.0"), Ok(false), "older");
    r.assert_nothing_staged("not newer");
    assert_eq!(r.server.times_asked(ASSET), 0);
    assert_eq!(r.server.times_asked("feed.json"), 2);
}

/// Signed with the test key, but the key is not trusted in this run:
/// rejected like any other stranger's release.
#[test]
fn without_the_test_key_trusted_its_release_is_rejected() {
    let mut r = Release::new("v1.0.1");
    r.sandbox.env.remove(ENV_TEST_UPDATE_PUBKEY);
    let e = check_and_stage("1.0.0").unwrap_err();
    assert!(e.contains("signature verification failed"), "{e}");
    r.assert_nothing_staged("untrusted test key");
}

/// The archive's layout is not fixed: a binary is found wherever it
/// sits, and one the archive lacks is an error the caller can skip.
#[test]
fn extract_finds_a_binary_anywhere_in_the_archive() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path();
    let archive = dir.join("nested.tar.gz");
    std::fs::write(&archive, tarball(dir, &["marspot-core"], "release/bin")).unwrap();
    let dst = dir.join("core-out");
    extract_binary(&archive, "marspot-core", &dst).unwrap();
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), "#!/bin/sh\n# marspot-core\n");
    assert!(!dir.join("nested.tar.d").exists(), "the unpacked tree is cleaned up");
    let e = extract_binary(&archive, "marspot-shell", &dir.join("shell-out")).unwrap_err();
    assert!(e.contains("no 'marspot-shell'"), "{e}");
    assert!(!dir.join("shell-out").exists());
    let garbage = dir.join("garbage.tar.gz");
    std::fs::write(&garbage, b"not a tarball").unwrap();
    assert!(extract_binary(&garbage, "marspot-core", &dst).unwrap_err().contains("tar failed"));
}

/// A release carrying only some of the binaries stages those and skips
/// the rest; one carrying none of them is an error.
#[test]
fn a_partial_release_stages_what_it_has() {
    let r = Release::new("v1.0.1");
    let only_core = tarball(&r.sandbox.path().join("keys"), &["marspot-core"], "");
    let file = r.sandbox.path().join("keys/partial.tar.gz");
    std::fs::write(&file, &only_core).unwrap();
    let sig = r.sandbox.path().join("keys/partial.sig");
    sign(&r.sec, &file, &sig);
    r.server.put(ASSET, only_core);
    r.server.put(&format!("{ASSET}.sig"), std::fs::read(&sig).unwrap());
    assert_eq!(check_and_stage("1.0.0"), Ok(true));
    assert_eq!(r.pending(), vec!["marspot-core".to_string()]);

    std::fs::remove_dir_all(r.sandbox.path().join("binaries")).unwrap();
    let none = tarball(&r.sandbox.path().join("keys"), &["README"], "");
    std::fs::write(&file, &none).unwrap();
    sign(&r.sec, &file, &sig);
    r.server.put(ASSET, none);
    r.server.put(&format!("{ASSET}.sig"), std::fs::read(&sig).unwrap());
    let e = check_and_stage("1.0.0").unwrap_err();
    assert!(e.contains("none of the expected binaries"), "{e}");
    r.assert_nothing_staged("no known binary");
}

/// A download writes exactly what was served; a 404 is an error.
#[test]
fn download_writes_the_body_and_fails_on_404() {
    let sandbox = sandbox_state_dir();
    let server = Server::start();
    let body: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    server.put("blob", body.clone());
    let dst = sandbox.path().join("blob");
    download_to(&server.url("blob"), &dst).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), body);
    let e = download_to(&server.url("missing"), &sandbox.path().join("missing")).unwrap_err();
    assert!(e.contains("404"), "{e}");
}
