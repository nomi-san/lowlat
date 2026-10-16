//! Real TCP framing and hot-path allocation checks. These do not claim that
//! TURN allocation, TLS or the client event loop is integrated yet.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use lowlat_core::turn::stream::Decoder;
use lowlat_net::turn_stream::{Output, Progress};

#[global_allocator]
static ALLOC: lowlat_common::alloc_counter::Counting = lowlat_common::alloc_counter::Counting;

const MESSAGE: &[u8] = &[0x40, 0, 0, 3, 1, 2, 3];
const WIRE: &[u8] = &[0x40, 0, 0, 3, 1, 2, 3, 0];

#[test]
fn turn_stream_reuses_storage_without_hot_path_allocations() {
    let mut output = Output::new(64).unwrap();
    let mut storage = [0; 64];
    let mut decoder = Decoder::new(&mut storage).unwrap();
    let mut writer = std::io::sink();
    lowlat_common::alloc_counter::assert_no_alloc(|| {
        for _ in 0..1_000 {
            output.queue(MESSAGE).unwrap();
            assert_eq!(output.write_ready(&mut writer).unwrap(), Progress::Complete);
            assert_eq!(decoder.push(WIRE).unwrap(), (WIRE.len(), Some(MESSAGE)));
        }
    });
}

#[test]
fn turn_stream_frames_roundtrip_over_a_real_tcp_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let peer = std::thread::spawn(move || {
        let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket.set_nodelay(true).unwrap();
        // Actual read boundaries are the OS's choice; every logical split is
        // independently covered by the sans-IO decoder's exhaustive tests.
        for byte in WIRE {
            socket.write_all(&[*byte]).unwrap();
        }
        let mut reply = [0; 8];
        socket.read_exact(&mut reply).unwrap();
        assert_eq!(reply, WIRE);
    });
    let (mut socket, _) = listener.accept().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket.set_nodelay(true).unwrap();
    let mut storage = [0; 64];
    let mut decoder = Decoder::new(&mut storage).unwrap();
    let mut buffer = [0; 3];
    loop {
        let count = socket.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "peer closed before the frame completed");
        let (used, message) = decoder.push(&buffer[..count]).unwrap();
        assert_eq!(used, count);
        if let Some(message) = message {
            assert_eq!(message, MESSAGE);
            break;
        }
    }
    let mut output = Output::new(64).unwrap();
    output.queue(MESSAGE).unwrap();
    assert_eq!(output.write_ready(&mut socket).unwrap(), Progress::Complete);
    peer.join().unwrap();
}
