//! A connection with nothing to say must put something on the wire anyway.
//!
//! This is the regression for a host that dropped and reconnected every two
//! minutes for over an hour without anyone noticing, because the reconnect was
//! fast enough to keep it in the discovery listing. The service sits behind an
//! edge that closes an idle websocket after about a hundred seconds, so silence
//! is not a stable state however correctly the connection is otherwise handled.
//!
//! **Answering the edge's pings is not enough**, and an earlier version of this
//! file tested exactly that and passed against the defect. What matters is that
//! traffic leaves *this* side on a schedule.

use std::time::Duration;

use futures_util::StreamExt;
use lowlat_kessel::{Client, Connect, Role};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as Frame;

/// Stands in for the service: accepts one connection and reports whether the
/// peer sent anything at all, unprompted, before the deadline.
async fn hears_from_an_idle_peer(listener: TcpListener) -> bool {
    let Ok((stream, _)) = listener.accept().await else {
        return false;
    };
    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
        return false;
    };
    // Deliberately says nothing. The edge that closes idle connections is not
    // waiting to be spoken to first, so neither is this.
    while let Some(Ok(frame)) = socket.next().await {
        if matches!(frame, Frame::Ping(_) | Frame::Text(_) | Frame::Binary(_)) {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn an_idle_connection_keeps_itself_alive() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = tokio::spawn(hears_from_an_idle_peer(listener));

    // Deliberately never sends anything. A host with nothing to advertise and
    // no guest is exactly this, and it is the case that failed.
    let _client = Client::connect(&Connect {
        server: format!("ws://127.0.0.1:{port}"),
        session_id: "session".to_string(),
        role: Role::Host,
        build: "test".to_string(),
        sdk_version: 1,
        // Shortened so the test does not wait out the real interval. The
        // schedule is what is under test, not its length.
        keepalive: Duration::from_millis(300),
    })
    .await
    .expect("connect");

    let spoke = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the server heard nothing at all")
        .expect("server task");

    assert!(
        spoke,
        "an idle connection sent nothing, so an edge would close it as idle"
    );
}

/// A peer that stops answering is noticed, even though the socket stays open.
/// *Named regression test.*
///
/// This is the state a running host was found in after ten hours: the
/// connection `ESTAB` with bytes stuck in its send queue, keepalives going out,
/// nothing coming back, and not one drop logged. The kernel keeps retrying a
/// dead peer for a long time and reports nothing, so **sending a keepalive
/// without expecting an answer detects nothing at all** -- it only makes the
/// silence look like traffic.
///
/// The connection must surface as closed so the layer above reconnects.
#[tokio::test]
async fn a_peer_that_stops_answering_is_noticed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    // Accepts, completes the handshake, then never says anything again and
    // never closes. Holding the socket is the whole point.
    let server = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(socket);
    });

    let mut client = Client::connect(&Connect {
        server: format!("ws://127.0.0.1:{port}"),
        session_id: "session".to_string(),
        role: Role::Host,
        build: "test".to_string(),
        sdk_version: 1,
        keepalive: Duration::from_millis(200),
    })
    .await
    .expect("connect");

    // Two missed replies past a 200 ms interval is under a second; the budget
    // is generous so the test is not measuring the scheduler.
    let closed = tokio::time::timeout(Duration::from_secs(10), client.recv())
        .await
        .expect("the connection never surfaced as closed, so nothing would reconnect");

    assert!(
        closed.is_none(),
        "a silent peer produced a message instead of a closed connection"
    );
    server.abort();
}
