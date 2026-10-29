//! The relay's stream against a TCP peer on loopback (docs/02-io-shell.md 5-6).
//!
//! The peer is a plain socket that writes and reads bytes shaped as the relay's
//! messages; the stream is driven as the loop drives it: wait, drain and hand
//! out the messages, stage, flush. What each test shows is named on it. The
//! allocation counter is the process's, so the zero-allocation run counts on
//! its own thread after a warm-up.

#![cfg(any(target_os = "linux", target_os = "macos", windows))]
// Fixtures build lengths and fill bytes from small counters; the truncating
// casts are the obvious ones.
#![allow(clippy::cast_possible_truncation)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use lowlat_common::alloc_counter::{self, Counting};
use lowlat_net::{Stream, Wake};

#[global_allocator]
static ALLOC: Counting = Counting;

/// A message shaped as the relay's: `body` bytes, a whole number of words.
fn message(body: usize, fill: u8) -> Vec<u8> {
    assert_eq!(body % 4, 0);
    let mut out = vec![0x01, 0x17];
    out.extend_from_slice(&(body as u16).to_be_bytes());
    out.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
    out.extend_from_slice(&[fill; 12]);
    out.resize(20 + body, fill);
    out
}

/// Channel data of `len` bytes, padded as a stream carries it.
fn channel(len: usize, fill: u8) -> Vec<u8> {
    let mut out = vec![0x40, 0x00];
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.resize(4 + len, fill);
    out.resize(out.len().next_multiple_of(4), 0);
    out
}

/// Channel data as it is handed out: the padding gone.
fn unpadded(framed: &[u8]) -> &[u8] {
    if framed[0] >> 6 == 0b01 {
        let len = usize::from(u16::from_be_bytes([framed[2], framed[3]]));
        &framed[..4 + len]
    } else {
        framed
    }
}

fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

fn connect(addr: SocketAddr) -> Stream {
    Stream::connect(addr, Wake::new().unwrap()).unwrap()
}

/// Drive the stream until `done` says so, collecting every message handed
/// out; panics past `limit`.
fn pump(
    stream: &mut Stream,
    limit: Duration,
    mut done: impl FnMut(&[Vec<u8>], &Stream) -> bool,
) -> Vec<Vec<u8>> {
    let started = Instant::now();
    let mut got = Vec::new();
    while !done(&got, stream) {
        assert!(
            started.elapsed() < limit,
            "the stream stopped making progress"
        );
        let ready = stream.wait(10.0).unwrap();
        if ready.socket {
            loop {
                let found = stream.drain().unwrap();
                got.extend(stream.frames().map(|(_, _, bytes)| bytes.to_vec()));
                if found == 0 || !stream.saturated() {
                    break;
                }
            }
        }
        stream.flush().unwrap();
    }
    got
}

/// A connect nobody answers ends the stream, as the error the system gave.
#[test]
fn a_refused_connect_ends_the_stream() {
    let (listener, addr) = listener();
    drop(listener);
    let mut stream = connect(addr);
    let started = Instant::now();
    let error = loop {
        // A refusal takes the system a few seconds on one platform.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the connect never ended"
        );
        if let Err(error) = stream.wait(50.0) {
            break error;
        }
    };
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "{error}"
    );
    assert!(!stream.connected());
}

/// Nothing is staged before the connect completes, and then it is.
#[test]
fn nothing_is_staged_before_the_connect() {
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    if !stream.connected() {
        assert!(stream.room().is_none(), "room handed out while connecting");
    }
    let _peer = listener.accept().unwrap();
    pump(&mut stream, Duration::from_secs(5), |_, stream| {
        stream.connected()
    });
    assert!(stream.room().is_some());
}

/// Messages written one byte at a time, then in reads that end inside
/// headers, bodies and padding, all come out whole and in order, padding gone.
#[test]
fn messages_split_anywhere_come_out_whole() {
    let mut sent: Vec<Vec<u8>> = Vec::new();
    for (index, len) in [0_usize, 1, 2, 3, 4, 5, 1229, 2000].iter().enumerate() {
        sent.push(channel(*len, index as u8 + 1));
        sent.push(message(4 * (*len / 4), index as u8 + 0x40));
    }
    let wire: Vec<u8> = sent.concat();
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_nodelay(true).unwrap();
        let mut at = 0;
        let mut step = 1;
        while at < wire.len() {
            let end = (at + step).min(wire.len());
            socket.write_all(&wire[at..end]).unwrap();
            at = end;
            step = step % 7 + 1;
            thread::sleep(Duration::from_micros(200));
        }
        socket
    });
    let got = pump(&mut stream, Duration::from_secs(10), |got, _| {
        got.len() == sent.len()
    });
    let expected: Vec<&[u8]> = sent.iter().map(|framed| unpadded(framed)).collect();
    assert_eq!(got.iter().map(Vec::as_slice).collect::<Vec<_>>(), expected);
    assert_eq!(stream.skipped(), 0);
    drop(peer.join().unwrap());
}

/// A reserved channel, and a message longer than anything the endpoint takes,
/// are skipped by their length and counted; the stream stays in step.
#[test]
fn reserved_and_overlong_messages_are_skipped_in_step() {
    let mut reserved = channel(9, 0x55);
    reserved[0] = 0x50;
    let overlong = message(4 * 1024, 0x66);
    let after = channel(3, 0x77);
    let wire = [reserved, overlong, after.clone()].concat();
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.write_all(&wire).unwrap();
        socket
    });
    let got = pump(&mut stream, Duration::from_secs(5), |got, _| {
        !got.is_empty()
    });
    assert_eq!(got, [unpadded(&after).to_vec()]);
    assert_eq!(stream.skipped(), 2);
    drop(peer.join().unwrap());
}

/// A head that is neither a message nor channel data ends the stream: nothing
/// scans on for a plausible header.
#[test]
fn a_head_that_is_neither_ends_the_stream() {
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.write_all(&[0x80, 0, 0, 4, 1, 2, 3, 4]).unwrap();
        socket
    });
    let started = Instant::now();
    let error = loop {
        assert!(started.elapsed() < Duration::from_secs(5), "never ended");
        let ready = stream.wait(10.0).unwrap();
        if ready.socket {
            if let Err(error) = stream.drain() {
                break error;
            }
        }
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    drop(peer.join().unwrap());
}

/// The stream's end right behind messages still hands every one of them out;
/// the end is the drain after the last. More messages than two drains hand
/// out, so the end is read while more than a drain's worth are still held.
#[test]
fn the_end_after_messages_still_hands_them_all_out() {
    let sent: Vec<Vec<u8>> = (0..200_u8).map(|index| channel(5, index)).collect();
    let wire = sent.concat();
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.write_all(&wire).unwrap();
        // Gone, at once, behind the messages.
    });
    peer.join().unwrap();
    // Everything there before the first drain, the end included.
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    let mut got = Vec::new();
    let error = loop {
        assert!(started.elapsed() < Duration::from_secs(5), "never ended");
        let ready = stream.wait(10.0).unwrap();
        if !ready.socket {
            continue;
        }
        match stream.drain() {
            Ok(_) => got.extend(stream.frames().map(|(_, _, bytes)| bytes.to_vec())),
            Err(error) => break error,
        }
    };
    let expected: Vec<Vec<u8>> = sent
        .iter()
        .map(|framed| unpadded(framed).to_vec())
        .collect();
    assert_eq!(got.len(), expected.len(), "messages lost to the end");
    assert_eq!(got, expected);
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

/// Once what arrived has been read, the wait waits for more: nothing is
/// reported again until it comes, so an arrival costs the loop one pass.
#[test]
fn a_drained_stream_waits_for_more() {
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let peer = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.write_all(&channel(31, 0x22)).unwrap();
        socket
    });
    let got = pump(&mut stream, Duration::from_secs(5), |got, _| {
        !got.is_empty()
    });
    assert_eq!(got.len(), 1);
    let ready = stream.wait(50.0).unwrap();
    assert!(!ready.socket, "more said to be there with nothing sent");
    drop(peer.join().unwrap());
}

/// A peer that resets the connection ends the stream with the reset.
#[test]
fn a_reset_ends_the_stream() {
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let peer = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        reset(socket);
    });
    peer.join().unwrap();
    let started = Instant::now();
    let error = loop {
        assert!(started.elapsed() < Duration::from_secs(5), "never ended");
        match stream.wait(10.0) {
            Err(error) => break error,
            Ok(ready) if ready.socket => {
                if let Err(error) = stream.drain() {
                    break error;
                }
            }
            Ok(_) => {}
        }
    };
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
        ),
        "{error}"
    );
}

/// Close with no lingering, which the system sends as a reset.
fn reset(socket: TcpStream) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::fd::AsRawFd;
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: a linger option and its exact size, on a socket we own.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&raw const linger).cast(),
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock as ws;
        let linger = ws::LINGER {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: a linger option and its exact size, on a socket we own.
        let rc = unsafe {
            ws::setsockopt(
                socket.as_raw_socket() as ws::SOCKET,
                ws::SOL_SOCKET,
                ws::SO_LINGER,
                (&raw const linger).cast(),
                std::mem::size_of::<ws::LINGER>() as i32,
            )
        };
        assert_eq!(rc, 0);
    }
    drop(socket);
}

/// Stage one message a pass, as a loop with a little to send does, until the
/// socket stops taking them; returns how many bytes went into the staging.
/// The staging is left nearly empty, so only the unfinished write can be what
/// refuses more.
fn fill_until_blocked(stream: &mut Stream) -> usize {
    let started = Instant::now();
    let mut staged = 0;
    let filler = channel(1200, 0x5A);
    while !stream.writing() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the socket never blocked"
        );
        if let Some(room) = stream.room() {
            room[..filler.len()].copy_from_slice(&filler);
            stream.commit(filler.len());
            staged += filler.len();
        }
        stream.flush().unwrap();
    }
    staged
}

/// What the socket could not take is written once it can, in order and whole,
/// and nothing more is staged meanwhile.
#[test]
fn a_write_the_socket_cannot_take_finishes_later() {
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let (socket, _) = listener.accept().unwrap();
    pump(&mut stream, Duration::from_secs(5), |_, stream| {
        stream.connected()
    });
    let staged = fill_until_blocked(&mut stream);
    assert!(
        stream.room().is_none(),
        "room handed out while a write is unfinished"
    );

    // The socket comes back with the bytes, held open past the last drain:
    // a peer gone at once would end the stream before the write's completion
    // is collected.
    let reader = thread::spawn(move || {
        let mut socket = socket;
        let mut wire = vec![0u8; staged];
        socket.read_exact(&mut wire).unwrap();
        (wire, socket)
    });
    pump(&mut stream, Duration::from_secs(20), |_, stream| {
        !stream.writing()
    });
    let (wire, _socket) = reader.join().unwrap();
    let filler = channel(1200, 0x5A);
    assert_eq!(wire.len() % filler.len(), 0);
    assert!(
        wire.chunks(filler.len()).all(|chunk| chunk == filler),
        "the bytes arrived altered"
    );
    assert!(stream.room().is_some());
}

/// Offer up to `left` copies of `filler`, as the loop offers the core's
/// output; returns how many were staged.
fn offer(stream: &mut Stream, left: &mut usize, filler: &[u8]) -> usize {
    stream
        .send_from(|room| {
            if *left == 0 {
                return None;
            }
            *left -= 1;
            room[..filler.len()].copy_from_slice(filler);
            Some(filler.len())
        })
        .unwrap()
}

/// A burst several stagings long goes in one call while the socket takes it:
/// a full staging is written and staging goes on, so nothing waits a pass for
/// room. Only a write the socket would not take ends the call early.
#[test]
fn a_burst_past_the_staging_goes_in_one_call() {
    let filler = channel(1200, 0x5A);
    let offered = 100;
    let total = offered * filler.len();
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let reader = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut wire = vec![0u8; total];
        socket.read_exact(&mut wire).unwrap();
        (wire, socket)
    });
    pump(&mut stream, Duration::from_secs(5), |_, stream| {
        stream.connected()
    });
    let mut left = offered;
    let staged = offer(&mut stream, &mut left, &filler);
    assert!(
        staged == offered || stream.writing(),
        "stopped after {staged} of {offered} with the socket still taking"
    );
    let started = Instant::now();
    while left > 0 || stream.writing() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the burst never went"
        );
        stream.wait(10.0).unwrap();
        offer(&mut stream, &mut left, &filler);
    }
    let (wire, _socket) = reader.join().unwrap();
    assert!(
        wire.chunks(filler.len()).all(|chunk| chunk == filler),
        "the bytes arrived altered"
    );
}

/// A write that makes no progress past its bound ends the stream. The writer
/// keeps offering all the staging takes, as a busy loop would, so the peer's
/// window and the socket fill and a write finally stops moving.
#[test]
fn a_stalled_write_ends_the_stream() {
    let (listener, addr) = listener();
    let mut stream =
        Stream::with_stall(addr, Wake::new().unwrap(), Duration::from_millis(300)).unwrap();
    let (_socket, _) = listener.accept().unwrap();
    pump(&mut stream, Duration::from_secs(5), |_, stream| {
        stream.connected()
    });
    let filler = channel(1200, 0x5A);
    let started = Instant::now();
    let mut last_progress = Instant::now();
    let error = loop {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the stall never ended it"
        );
        match stream.wait(1.0) {
            Err(error) => break error,
            Ok(_) => {
                while let Some(room) = stream.room() {
                    room[..filler.len()].copy_from_slice(&filler);
                    stream.commit(filler.len());
                }
                if stream.flush().unwrap() > 0 {
                    last_progress = Instant::now();
                }
            }
        }
    };
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(
        last_progress.elapsed() >= Duration::from_millis(300),
        "ended {} ms after a write moved",
        last_progress.elapsed().as_millis()
    );
}

/// Dropped with operations outstanding -- a write the socket could not take,
/// the receive that waits for bytes -- the stream takes them back at once.
#[test]
fn a_stream_dropped_mid_write_lets_go_at_once() {
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let (_socket, _) = listener.accept().unwrap();
    pump(&mut stream, Duration::from_secs(5), |_, stream| {
        stream.connected()
    });
    fill_until_blocked(&mut stream);
    let started = Instant::now();
    drop(stream);
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "teardown waited out its bound"
    );
}

/// Steady state allocates nothing, either way: messages staged, written,
/// echoed back, read and handed out, counted on this thread after a warm-up,
/// with the messages that crossed counted too.
#[test]
fn messages_cross_both_ways_without_allocating() {
    let (listener, addr) = listener();
    let mut stream = connect(addr);
    let echo = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_nodelay(true).unwrap();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match socket.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if socket.write_all(&buffer[..read]).is_err() {
                        return;
                    }
                }
            }
        }
    });
    pump(&mut stream, Duration::from_secs(5), |_, stream| {
        stream.connected()
    });
    let messages = [channel(1229, 0x11), channel(31, 0x22), message(96, 0x33)];

    let round = |stream: &mut Stream| -> usize {
        for framed in &messages {
            let room = stream.room().expect("room");
            room[..framed.len()].copy_from_slice(framed);
            stream.commit(framed.len());
        }
        stream.flush().unwrap();
        let mut back = 0;
        let started = Instant::now();
        while back < messages.len() {
            assert!(started.elapsed() < Duration::from_secs(5), "no echo");
            let ready = stream.wait(10.0).unwrap();
            if ready.socket {
                loop {
                    let found = stream.drain().unwrap();
                    back += stream
                        .frames()
                        .filter(|(_, _, bytes)| !bytes.is_empty())
                        .count();
                    if found == 0 || !stream.saturated() {
                        break;
                    }
                }
            }
            stream.flush().unwrap();
        }
        back
    };

    for _ in 0..50 {
        round(&mut stream);
    }
    let before = alloc_counter::count();
    let mut crossed = 0;
    for _ in 0..500 {
        crossed += round(&mut stream);
    }
    let allocations = alloc_counter::count() - before;
    assert_eq!(crossed, 500 * messages.len(), "the messages that crossed");
    assert_eq!(
        allocations, 0,
        "allocations over {crossed} messages each way"
    );
    drop(stream);
    echo.join().unwrap();
}
