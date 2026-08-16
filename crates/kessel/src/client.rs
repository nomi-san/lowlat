//! The transport: one connection, one reader, one writer.
//!
//! Outbound messages go through a queue that a single task drains. Producers
//! never touch the socket, so a send cannot couple whatever produced it to
//! signaling latency, and the socket has one owner rather than a lock.
//!
//! Liveness is the connection. The service treats a dropped connection as the
//! host going away, which is why nothing here sends a heartbeat.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as Frame;

use crate::message::Inbound;
use crate::url::{self, Role};

/// What went wrong. Concrete, and small enough to carry no formatting.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The upgrade was refused. Carries the status when there was one.
    Upgrade(Option<u16>),
    /// The socket failed after it was established.
    Transport,
    /// A message could not be serialized.
    Encode,
    /// The connection is gone and the queue has nowhere to drain.
    Closed,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Upgrade(Some(status)) => write!(f, "upgrade refused, status={status}"),
            Self::Upgrade(None) => write!(f, "upgrade refused"),
            Self::Transport => write!(f, "transport failed"),
            Self::Encode => write!(f, "message could not be encoded"),
            Self::Closed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for Error {}

/// How the caller reaches an established connection.
#[derive(Debug)]
pub struct Client {
    outbound: mpsc::UnboundedSender<Frame>,
    inbound: mpsc::UnboundedReceiver<Inbound>,
}

/// Everything the upgrade needs.
#[derive(Debug, Clone)]
pub struct Connect {
    pub server: String,
    pub session_id: String,
    pub role: Role,
    pub build: String,
    pub sdk_version: u32,
}

impl Client {
    /// Open the connection and start its reader and writer.
    ///
    /// The service closes without a reply when the query is wrong, so a bad
    /// session presents here as a refused upgrade rather than as a later error.
    pub async fn connect(params: &Connect) -> Result<Self, Error> {
        // Chosen once per process. Left to feature unification this resolves to
        // no provider at all and the first connection panics inside the TLS
        // stack, which reads as a crash rather than as a configuration gap.
        static PROVIDER: std::sync::Once = std::sync::Once::new();
        PROVIDER.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });

        let url = url::connect(
            &params.server,
            &params.session_id,
            params.role,
            &params.build,
            params.sdk_version,
        );

        let (stream, response) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|err| match err {
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    Error::Upgrade(Some(response.status().as_u16()))
                }
                _ => Error::Upgrade(None),
            })?;
        lowlat_common::log_info!(
            "kessel: connected, role={} status={}",
            params.role.as_str(),
            response.status().as_u16()
        );

        let (mut sink, mut source) = stream.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Inbound>();
        let pong = out_tx.clone();

        // The one writer. Everything outbound is serialized before it reaches
        // here, so this task neither knows nor cares what a message means.
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if sink.send(frame).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // The one reader. A frame that is not JSON we understand is dropped:
        // the service is entitled to add actions, and an unknown one is not an
        // error on our side.
        tokio::spawn(async move {
            while let Some(frame) = source.next().await {
                let text = match frame {
                    Ok(Frame::Text(text)) => text,
                    // **A ping must be answered explicitly.** The library
                    // queues a pong on the connection, but the halves are
                    // split and that queue is only flushed by a write; a
                    // connection with nothing to say never writes, so the pong
                    // never leaves and the service closes us for silence. This
                    // is what liveness-is-the-connection actually rests on.
                    Ok(Frame::Ping(payload)) => {
                        if pong.send(Frame::Pong(payload)).is_err() {
                            break;
                        }
                        continue;
                    }
                    Ok(Frame::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                match serde_json::from_str::<Inbound>(&text) {
                    Ok(message) => {
                        if in_tx.send(message).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        lowlat_common::log_warn!("kessel: undecodable frame, len={}", text.len());
                    }
                }
            }
            lowlat_common::log_info!("kessel: reader ended");
        });

        Ok(Self {
            outbound: out_tx,
            inbound: in_rx,
        })
    }

    /// Queue one message. Returns once it is queued, not once it is on the wire.
    pub fn send<T: serde::Serialize>(&self, action: &str, payload: &T) -> Result<(), Error> {
        let text = crate::message::envelope(action, payload).map_err(|_| Error::Encode)?;
        self.outbound
            .send(Frame::Text(text))
            .map_err(|_| Error::Closed)
    }

    /// Queue a frame that is not one of our messages.
    ///
    /// The service accepts a bare greeting alongside the JSON protocol, which
    /// is not an envelope and cannot go through `send`.
    pub fn send_text(&self, text: &str) -> Result<(), Error> {
        self.outbound
            .send(Frame::Text(text.to_string()))
            .map_err(|_| Error::Closed)
    }

    /// The next message from the service, or `None` once the reader has ended.
    pub async fn recv(&mut self) -> Option<Inbound> {
        self.inbound.recv().await
    }
}
