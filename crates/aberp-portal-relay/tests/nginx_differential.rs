//! Differential test: our front versus a **live nginx**, byte for byte.
//!
//! `nginx_indistinguishable.rs` pins the front against goldens that
//! were transcribed by hand into `tests/fixtures/nginx-goldens.txt`.
//! Transcription is exactly where a disguise rots — the earlier
//! hand-written 404 body in this crate was 4 bytes short of the real
//! one for precisely that reason. So this test removes the human step:
//! it starts a real nginx with `server_tokens off`, sends the same raw
//! bytes to both servers, and diffs the responses.
//!
//! # It skips when nginx is absent
//!
//! CI has no nginx and is not going to grow one for this. The test is
//! therefore a **local, opportunistic** check: present on the
//! developer's machine, it is the highest-value test in the crate;
//! absent, it prints why it skipped and passes. That is a deliberate
//! trade — the goldens file is the CI-side guard, and this is what
//! keeps the goldens file honest.
//!
//! Run it explicitly after any change to `nginx.rs` or `http1.rs`:
//!
//! ```text
//! brew install nginx     # or apt-get install nginx-light
//! cargo test -p aberp-portal-relay --test nginx_differential -- --nocapture
//! ```

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aberp_portal_relay::{http1, Broker, Canary, Front};

/// Every request class, as raw bytes. Deliberately includes several no
/// HTTP client would ever send — those are the ones that used to be
/// answered by hyper rather than by us.
const CASES: &[(&str, &[u8])] = &[
    ("404 close", b"GET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("404 keep-alive", b"GET /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("404 deep path", b"GET /a/b/c/d.php?q=1 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("404 HTTP/1.0", b"GET /nope HTTP/1.0\r\nHost: x\r\n\r\n"),
    ("404 HTTP/1.0 keep-alive", b"GET /nope HTTP/1.0\r\nHost: x\r\nConnection: keep-alive\r\n\r\n"),
    ("404 HTTP/1.0 no Host", b"GET /nope HTTP/1.0\r\n\r\n"),
    ("404 absolute form", b"GET http://x/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("404 leading CRLF", b"\r\n\r\nGET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("404 bare LF", b"GET /nope HTTP/1.1\nHost: x\nConnection: close\n\n"),
    ("404 uppercase Connection", b"GET /nope HTTP/1.1\r\nHost: x\r\nConnection: KEEP-ALIVE\r\n\r\n"),
    ("404 POST", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabc"),
    ("404 chunked", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"),
    ("404 HEAD", b"HEAD /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("400 bad request line", b"NOT A VALID REQUEST LINE\r\n\r\n"),
    ("400 no Host on 1.1", b"GET /nope HTTP/1.1\r\n\r\n"),
    ("400 duplicate Host", b"GET /nope HTTP/1.1\r\nHost: x\r\nHost: y\r\n\r\n"),
    ("400 space in target", b"GET /no pe HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 bad header name", b"GET /nope HTTP/1.1\r\nHost: x\r\nBad Header Name: v\r\n\r\n"),
    ("400 asterisk form", b"OPTIONS * HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 TLS ClientHello", b"\x16\x03\x01\x00\x50\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
    ("405 unknown method", b"FROBNICATE /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("505 HTTP/9.9", b"GET /nope HTTP/9.9\r\nHost: x\r\n\r\n"),
    ("505 HTTP/2.0", b"GET /nope HTTP/2.0\r\nHost: x\r\n\r\n"),
    ("0.9 no version", b"GET /nope\r\n\r\n"),
];

fn nginx_available() -> bool {
    std::process::Command::new("nginx")
        .arg("-v")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Blocking raw exchange — the whole point is to bypass every client
/// abstraction, so this is a socket and two syscalls.
fn exchange(port: u16, raw: &[u8]) -> Vec<u8> {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return Vec::new();
    };
    s.set_read_timeout(Some(Duration::from_millis(700))).ok();
    if s.write_all(raw).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(n) = s.read(&mut chunk) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    out
}

/// Normalise the one field that legitimately differs.
fn normalise(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .split("\r\n")
        .map(|l| {
            if l.starts_with("Date: ") {
                "Date: {DATE}".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

struct Nginx {
    port: u16,
    dir: PathBuf,
    child: std::process::Child,
}

impl Drop for Nginx {
    fn drop(&mut self) {
        let _ = std::process::Command::new("nginx")
            .args([
                "-p",
                &self.dir.display().to_string(),
                "-c",
                "nginx.conf",
                "-s",
                "stop",
            ])
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_nginx() -> Option<Nginx> {
    // A per-process port so two concurrent runs cannot collide.
    let port = 20_000 + (std::process::id() % 20_000) as u16;
    let dir = std::env::temp_dir().join(format!("portal-nginx-diff-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("logs")).ok()?;
    std::fs::create_dir_all(dir.join("html")).ok()?;
    let conf = format!(
        "worker_processes 1;\n\
         daemon off;\n\
         error_log logs/error.log;\n\
         pid logs/nginx.pid;\n\
         events {{ worker_connections 64; }}\n\
         http {{\n\
           access_log off;\n\
           server_tokens off;\n\
           default_type application/octet-stream;\n\
           server {{ listen {port}; server_name _; root html; }}\n\
         }}\n"
    );
    std::fs::write(dir.join("nginx.conf"), conf).ok()?;
    let child = std::process::Command::new("nginx")
        .args(["-p", &dir.display().to_string(), "-c", "nginx.conf"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut n = Nginx { port, dir, child };
    // Wait for the listener rather than napping a fixed amount.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", n.port)).is_ok() {
            return Some(n);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = n.child.kill();
    None
}

#[test]
fn our_front_is_byte_identical_to_a_live_nginx_across_every_request_class() {
    if !nginx_available() {
        eprintln!(
            "SKIPPED: no `nginx` on PATH. This is the differential guard for the §3.2 \n\
             disguise; install nginx and re-run to exercise it. The transcribed goldens \n\
             in tests/fixtures/nginx-goldens.txt still gate CI."
        );
        return;
    }
    let Some(nginx) = start_nginx() else {
        eprintln!("SKIPPED: nginx is installed but would not start.");
        return;
    };

    // Our front, on its own runtime thread, with no agent lease — the
    // state every un-authenticated caller sees.
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let ours = rt.block_on(async {
        let (canary, rx) = Canary::new();
        // Held so the queue never closes under the front.
        std::mem::forget(rx);
        let front = Arc::new(Front {
            broker: Arc::new(Broker::new()),
            canary,
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = l.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, peer)) = l.accept().await else {
                    return;
                };
                let front = Arc::clone(&front);
                tokio::spawn(async move { http1::serve(tcp, Some(peer), front).await });
            }
        });
        port
    });

    let mut mismatches = Vec::new();
    for (name, raw) in CASES {
        let theirs = normalise(&exchange(nginx.port, raw));
        let mine = normalise(&exchange(ours, raw));
        if theirs == mine {
            println!("  ok   {name} ({} bytes)", theirs.len());
        } else {
            println!("  DIFF {name}\n    nginx: {theirs:?}\n    ours : {mine:?}");
            mismatches.push(*name);
        }
    }
    assert!(
        mismatches.is_empty(),
        "the disguise differs from a live nginx in {} of {} classes: {:?}",
        mismatches.len(),
        CASES.len(),
        mismatches
    );
    println!(
        "\nall {} request classes are byte-identical to nginx",
        CASES.len()
    );
}
