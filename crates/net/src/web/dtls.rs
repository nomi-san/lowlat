//! The record layer: one DTLS 1.2 endpoint driven by the shell's clock.
//!
//! Every instant the engine sees is derived from the session's millisecond
//! clock against one epoch read when the session was made, so a test that
//! advances a fake clock advances the handshake timers with it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dimpl::{Config, Dtls, DtlsCertificate, Output};

use super::DTLS_MTU;

/// What the record layer handed back on one poll.
#[derive(Debug)]
pub(super) enum Out<'b> {
    /// A record to put on the wire, in the first `len` bytes of the buffer.
    Packet(usize),
    /// Decrypted application data.
    Data(&'b [u8]),
    /// The handshake completed.
    Connected,
    /// The peer's certificate, DER encoded, for the digest check.
    PeerCert(&'b [u8]),
    /// The peer closed the session cleanly.
    Closed,
    /// The buffer cannot hold the next output. It is kept for a larger one.
    TooSmall,
    /// Nothing more this poll. The next deadline has been recorded.
    Idle,
    /// An output this transport does not act on. Poll again.
    Skip,
}

/// One endpoint of the record layer.
pub(super) struct Link {
    dtls: Dtls,
    epoch: Instant,
    deadline: Option<Instant>,
}

impl core::fmt::Debug for Link {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Link")
            .field("dtls", &self.dtls)
            .field("deadline", &self.deadline.map(|d| self.ms_of(d)))
            .finish()
    }
}

impl Link {
    /// The configuration every link shares.
    pub(super) fn config() -> Result<Arc<Config>, dimpl::Error> {
        Config::builder().mtu(DTLS_MTU).build().map(Arc::new)
    }

    /// A link in the client role: it fires the first handshake flight.
    ///
    /// The flight is queued here, on the first run of the timers, so a link
    /// is made only once there is a path to send it on; made earlier, its
    /// retries would run out while the punch was still finding one.
    pub(super) fn client(
        config: Arc<Config>,
        certificate: DtlsCertificate,
        epoch: Instant,
        now_ms: f64,
    ) -> Result<Self, dimpl::Error> {
        let now = instant(epoch, now_ms);
        let mut dtls = Dtls::new_12(config, certificate, now);
        dtls.set_active(true);
        dtls.handle_timeout(now)?;
        Ok(Self {
            dtls,
            epoch,
            deadline: None,
        })
    }

    /// A link in the server role: it waits for the peer's first flight.
    pub(super) fn server(
        config: Arc<Config>,
        certificate: DtlsCertificate,
        epoch: Instant,
        now_ms: f64,
    ) -> Self {
        let dtls = Dtls::new_12(config, certificate, instant(epoch, now_ms));
        Self {
            dtls,
            epoch,
            deadline: None,
        }
    }

    /// Feed one datagram. Malformed input is discarded inside; an error is
    /// fatal to the link.
    pub(super) fn feed(&mut self, datagram: &[u8]) -> Result<(), dimpl::Error> {
        self.dtls.handle_packet(datagram)
    }

    /// Run the timers if the deadline has passed.
    pub(super) fn tick(&mut self, now_ms: f64) -> Result<(), dimpl::Error> {
        let now = instant(self.epoch, now_ms);
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.deadline = None;
            self.dtls.handle_timeout(now)?;
        }
        Ok(())
    }

    /// Milliseconds until the timers next want running.
    pub(super) fn timer_ms(&self, now_ms: f64) -> f64 {
        match self.deadline {
            Some(deadline) => (self.ms_of(deadline) - now_ms).max(0.0),
            None => f64::INFINITY,
        }
    }

    /// Queue application data for the wire. Wrapped on the next poll.
    pub(super) fn send(&mut self, data: &[u8]) -> Result<(), dimpl::Error> {
        self.dtls.send_application_data(data)
    }

    /// Say goodbye. The alert leaves on the next poll.
    pub(super) fn close(&mut self) -> Result<(), dimpl::Error> {
        self.dtls.close()
    }

    /// One output, into `buf`. Poll again on [`Out::Skip`].
    pub(super) fn poll<'b>(&mut self, buf: &'b mut [u8]) -> Out<'b> {
        match self.dtls.poll_output(buf) {
            Output::Packet(packet) => Out::Packet(packet.len()),
            Output::ApplicationData(data) => Out::Data(data),
            Output::PeerCert(der) => Out::PeerCert(der),
            Output::Connected => Out::Connected,
            Output::CloseNotify => Out::Closed,
            Output::BufferTooSmall { .. } => Out::TooSmall,
            Output::Timeout(deadline) => {
                self.deadline = Some(deadline);
                Out::Idle
            }
            // Keying material is for media encryption this transport does
            // not use; anything the engine adds later is nothing to act on.
            _ => Out::Skip,
        }
    }

    fn ms_of(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.epoch).as_secs_f64() * 1000.0
    }
}

/// The session clock as the engine wants it.
fn instant(epoch: Instant, now_ms: f64) -> Instant {
    epoch + Duration::from_secs_f64(now_ms.max(0.0) / 1000.0)
}
