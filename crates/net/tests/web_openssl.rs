//! The record layer against a second implementation.
//!
//! A pair of our own sessions proves the two ends agree with each other and
//! nothing more; a handshake that completes against an implementation
//! nobody here wrote is what says the records are DTLS 1.2. The peer is the
//! OpenSSL command-line server, which is on most machines and on none of the
//! CI runners, so the test is ignored and run by hand:
//!
//! ```text
//! cargo test -p lowlat-net --test web_openssl -- --ignored
//! ```

use std::io::Write;
use std::net::UdpSocket;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lowlat_core::endpoint::Media;
use lowlat_crypto::cert::Certificate;
use lowlat_net::web::{Role, WebSession};

/// One DER object as PEM, which is what the command line reads.
fn pem(label: &str, der: &[u8]) -> String {
    let body = lowlat_crypto::base64(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An OpenSSL DTLS 1.2 server on a loopback port, presenting `identity`,
/// asking for the client's certificate, and writing what it reads to a file.
fn openssl_server(dir: &std::path::Path, identity: &Certificate) -> Server {
    let cert_path = dir.join("server.pem");
    let key_path = dir.join("server.key");
    std::fs::File::create(&cert_path)
        .unwrap()
        .write_all(pem("CERTIFICATE", identity.der()).as_bytes())
        .unwrap();
    std::fs::File::create(&key_path)
        .unwrap()
        .write_all(pem("PRIVATE KEY", identity.key_pkcs8()).as_bytes())
        .unwrap();

    // A free port, found by binding and releasing it. The server binds it a
    // moment later; on a loopback interface that race is not a real one.
    let port = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let read = std::fs::File::create(dir.join("read.bin")).unwrap();
    // Standard input is a pipe held open for the server's life. On end of
    // file there it shuts the connection down, whatever else is happening.
    let child = Command::new("openssl")
        .args([
            "s_server",
            "-dtls1_2",
            "-accept",
            &format!("127.0.0.1:{port}"),
            "-cert",
            cert_path.to_str().unwrap(),
            "-key",
            key_path.to_str().unwrap(),
            "-verify",
            "1",
            "-quiet",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(read))
        .stderr(Stdio::from(
            std::fs::File::create(dir.join("stderr.txt")).unwrap(),
        ))
        .spawn()
        .expect("openssl on the path");
    std::thread::sleep(Duration::from_millis(400));
    Server { child, port }
}

/// The handshake completes against OpenSSL, with the server's certificate
/// checked against the digest the client was given, and the association's
/// first packet arrives on the far side as application data.
#[test]
#[ignore = "needs the openssl command line; run by hand"]
fn the_handshake_completes_against_openssl() {
    let dir = std::env::temp_dir().join(format!("lowlat-web-openssl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let identity = Certificate::generate().unwrap();
    let server = openssl_server(&dir, &identity);

    let ours = Certificate::generate().unwrap();
    let mut session =
        WebSession::new(Role::Client, Some(*identity.fingerprint()), &ours, 1, 0.0).unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.connect(("127.0.0.1", server.port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();

    let started = Instant::now();
    let now = || started.elapsed().as_secs_f64() * 1000.0;
    session.path_ready(now());

    let mut wire = [0u8; lowlat_core::MAX_DATAGRAM];
    let mut scratch = [0u8; lowlat_core::MAX_DATAGRAM];
    let mut up_at: Option<Instant> = None;
    while started.elapsed() < Duration::from_secs(8) {
        session.poll(now());
        while let Some(result) = session.get_output(now(), &mut wire) {
            let len = result.unwrap();
            socket.send(&wire[..len]).unwrap();
        }
        if let Ok(len) = socket.recv(&mut wire) {
            let _ = session.process_input(&wire[..len], now(), &mut scratch);
        }
        if let Some(fault) = session.fault() {
            panic!("the session faulted: {fault:?}");
        }
        if session.is_up() {
            // A moment more, so the association's first packet is out and
            // the far side has written it down.
            let since = *up_at.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_millis(300) {
                break;
            }
        } else if up_at.is_some() {
            break;
        }
    }
    drop(server);
    let stderr = std::fs::read_to_string(dir.join("stderr.txt")).unwrap_or_default();
    assert!(
        session.is_up(),
        "no handshake against openssl: {session:?}\nserver said:\n{stderr}"
    );
    let read = std::fs::read(dir.join("read.bin")).unwrap();
    // The association's first packet: both ports 5000, a zero verification
    // tag, then the checksum and the chunk.
    let init_header = [0x13, 0x88, 0x13, 0x88, 0, 0, 0, 0];
    assert!(
        read.windows(init_header.len()).any(|w| w == init_header),
        "the far side read {} bytes and none of them were the association's first packet",
        read.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
