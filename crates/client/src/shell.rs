//! The session's thread: the shell around the driver.
//!
//! Builds the connectivity engine and the session on the socket the seam
//! opened, lends them their rings for the life of the thread, and runs the
//! shell's loop with the driver as its application.

use std::net::SocketAddr;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};

use lowlat_common::clock::Time;
use lowlat_common::events;
use lowlat_common::spsc::Ring;
use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::conn::{Conn, Credentials};
use lowlat_core::control::CONTROL_CHANNEL;
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::{Cipher, Envelope};
use lowlat_core::init::Init;
use lowlat_core::relay::{Relay, State as RelayState};
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_net::{Running, Shell, Socket, Wake};
use std::sync::atomic::Ordering;

use crate::config;
use crate::driver::{Driver, Telemetry, Units, pack_relayed};
use crate::event::{Arrival, Ask, Event, Outcome};
use crate::input::{RING_DEPTH, Request};
use crate::sound::Packets;
use crate::{
    AUDIO_CHANNEL, AUDIO_RECV_SLOTS, BODY, CONTROL_RECV_SLOTS, CONTROL_SEND_SLOTS, VIDEO_CHANNEL,
    VIDEO_RECV_SLOTS,
};

/// How often the loop trickles reflexive candidates, in passes.
const REPORT_EVERY: u32 = 16;

/// Everything the thread is handed at spawn.
pub(crate) struct Attached {
    pub socket: Socket,
    pub servers: Vec<SocketAddr>,
    /// The relay a relay attempt goes through, and the seed its transaction
    /// identifiers are derived from.
    pub relay: Option<config::Relay>,
    pub relay_seed: [u8; 16],
    pub ours: (String, String),
    pub theirs: (String, String),
    pub material: [u8; 36],
    pub cipher: Cipher,
    pub seed: [u8; 16],
    pub init: Init,
    pub arrivals: mpsc::Receiver<Arrival>,
    pub asked: mpsc::Receiver<Ask>,
    /// The consumer end of the input ring; the seam holds the producer.
    pub requests: Arc<Ring<Request, RING_DEPTH>>,
    pub emit: events::Sender<Event>,
    pub telemetry: Arc<Telemetry>,
    pub units: Units,
    pub packets: Packets,
    /// Set to the loop's epoch once it has one, so the application's
    /// thread can read the arrival stamps sound packets carry.
    pub epoch: Arc<OnceLock<Time>>,
}

fn attach_recv<'a>(
    session: &mut Session<'a>,
    channel: u8,
    bodies: &'a mut [u8],
    meta: &'a mut [SlotMeta],
) -> bool {
    match RecvRing::new(bodies, meta, BODY) {
        Ok(ring) => session.attach_recv(channel, ring).is_ok(),
        Err(_) => false,
    }
}

fn attach_send<'a>(
    session: &mut Session<'a>,
    channel: u8,
    bodies: &'a mut [u8],
    meta: &'a mut [SendSlot],
) -> bool {
    match SendRing::new(bodies, meta, BODY, channel) {
        Ok(ring) => session.attach_send(channel, ring).is_ok(),
        Err(_) => false,
    }
}

pub(crate) fn run(args: Attached, wake: Wake, running: &Running) {
    let Attached {
        socket,
        servers,
        relay,
        relay_seed,
        ours,
        theirs,
        material,
        cipher,
        seed,
        init,
        arrivals,
        asked,
        requests,
        emit,
        telemetry,
        units,
        packets,
        epoch,
    } = args;
    let mut conn = Conn::new(
        Credentials {
            local_ufrag: &ours.0,
            local_pwd: &ours.1,
            remote_ufrag: &theirs.0,
            remote_pwd: &theirs.1,
        },
        seed,
        0.0,
    );
    // A relay attempt asks no reflexive server: its socket talks to the
    // relay alone.
    if relay.is_none() {
        for server in &servers {
            let _ = conn.add_server(*server);
        }
    }

    // Allocated once, here, and lent to the rings for the life of the
    // thread. **Video and sound are received and never sent**, so those
    // channels have no send ring; control goes both ways.
    let mut control_recv_bodies = vec![0u8; BODY * CONTROL_RECV_SLOTS];
    let mut control_recv_meta = vec![SlotMeta::default(); CONTROL_RECV_SLOTS];
    let mut control_send_bodies = vec![0u8; BODY * CONTROL_SEND_SLOTS];
    let mut control_send_meta = vec![SendSlot::default(); CONTROL_SEND_SLOTS];
    let mut video_recv_bodies = vec![0u8; BODY * VIDEO_RECV_SLOTS];
    let mut video_recv_meta = vec![SlotMeta::default(); VIDEO_RECV_SLOTS];
    let mut audio_recv_bodies = vec![0u8; BODY * AUDIO_RECV_SLOTS];
    let mut audio_recv_meta = vec![SlotMeta::default(); AUDIO_RECV_SLOTS];

    // The cipher, kept here for the thread's life and lent to the envelope.
    let Ok(record) = lowlat_crypto::Record::new(&material, cipher) else {
        emit.send(Event::Ended {
            outcome: Outcome::TransportFailed,
        });
        return;
    };
    let Ok(envelope) = Envelope::lent(&record, &material, cipher) else {
        emit.send(Event::Ended {
            outcome: Outcome::TransportFailed,
        });
        return;
    };
    let mut session = Session::new(envelope, 1, 0.0);
    if !attach_recv(
        &mut session,
        CONTROL_CHANNEL,
        &mut control_recv_bodies,
        &mut control_recv_meta,
    ) || !attach_send(
        &mut session,
        CONTROL_CHANNEL,
        &mut control_send_bodies,
        &mut control_send_meta,
    ) || !attach_recv(
        &mut session,
        VIDEO_CHANNEL,
        &mut video_recv_bodies,
        &mut video_recv_meta,
    ) || !attach_recv(
        &mut session,
        AUDIO_CHANNEL,
        &mut audio_recv_bodies,
        &mut audio_recv_meta,
    ) {
        emit.send(Event::Ended {
            outcome: Outcome::TransportFailed,
        });
        return;
    }
    let endpoint = match &relay {
        Some(relay) => Endpoint::relayed(
            conn,
            session,
            Relay::new(
                relay.server,
                &relay.username,
                &relay.password,
                relay_seed,
                0.0,
            ),
        ),
        None => Endpoint::new(conn, session),
    };
    let mut shell = Shell::new(socket, wake, endpoint);
    let _ = epoch.set(shell.base());
    let mut driver = Driver::new(init, units, packets, emit.clone(), Arc::clone(&telemetry));
    let mut reported: Vec<SocketAddr> = Vec::new();
    let mut offered = false;
    let mut pass: u32 = 0;

    while !running.stopping() {
        let arrivals = &arrivals;
        let asked = &asked;
        let requests = &requests;
        let driving = &mut driver;
        let mut leaving = false;
        let mut ended: Option<Outcome> = None;
        let turn = match shell.turn(|endpoint| {
            while let Ok(arrival) = arrivals.try_recv() {
                match arrival {
                    Arrival::Candidate(addr, kind) => {
                        let _ = endpoint.add_candidate(addr, kind);
                    }
                    Arrival::PeerReady => endpoint.conn().set_peer_ready(),
                }
            }
            // Asks need the session, which is only here.
            while let Ok(ask) = asked.try_recv() {
                match ask {
                    Ask::UserData(id, text) => {
                        driving.send_user_data(endpoint.session(), id, &text);
                    }
                    Ask::Leave => leaving = true,
                }
            }
            // Input in the order the application gave it, the viewport
            // taking effect for what follows it.
            while let Some(request) = requests.pop() {
                match request {
                    Request::Input(input) => driving.send_input(endpoint.session(), &input),
                    Request::Viewport(viewport) => driving.set_viewport(viewport),
                    Request::Video(flags) => driving.set_flags(endpoint.session(), flags),
                    Request::Decoder(flags) => {
                        driving.switch_decoder(endpoint.session(), flags);
                    }
                }
            }
        }) {
            Ok(turn) => turn,
            Err(error) => {
                lowlat_common::log_warn!("client: the transport stopped, err={error}");
                telemetry.state.store(2, Ordering::Relaxed);
                emit.send(Event::Ended {
                    outcome: Outcome::TransportFailed,
                });
                return;
            }
        };
        let now = turn.now;
        if let Some(outcome) = driver.turn(shell.endpoint(), now) {
            ended = Some(outcome);
        }
        if leaving && driver.established() {
            driver.leave(shell.endpoint().session(), now);
        }
        if driver.left(now) || (leaving && !driver.established()) {
            release(&mut shell);
            // The application caused this; it needs no event back.
            telemetry.state.store(2, Ordering::Relaxed);
            return;
        }
        if let Some(outcome) = ended {
            release(&mut shell);
            telemetry.state.store(2, Ordering::Relaxed);
            emit.send(Event::Ended { outcome });
            return;
        }

        // A relay attempt offers the relayed address once the relay's own
        // machine is permitted, then says it has offered everything. Looked
        // at every pass: the pass that readies the relay may be the last for
        // a while.
        if !offered
            && let Some(std::net::SocketAddr::V4(relayed)) =
                shell.endpoint().relay().and_then(Relay::relayed)
        {
            offered = true;
            telemetry
                .relayed
                .store(pack_relayed(relayed), Ordering::Relaxed);
            lowlat_common::log_info!("client: the relay is ready, relayed={relayed}");
            // Marked as a reflexive server's report: what a peer checks
            // after the readiness marker and draws its one probe toward.
            emit.send(Event::Candidate {
                addr: std::net::SocketAddr::V4(relayed),
                from_stun: true,
                lan: false,
            });
            emit.send(Event::Ready);
        }

        pass = pass.wrapping_add(1);
        if pass % REPORT_EVERY != 0 {
            continue;
        }
        // Trickled as they are found. The engine retains them, so this
        // reports what is new rather than what was returned by one call.
        let fresh: Vec<SocketAddr> = shell
            .endpoint()
            .conn()
            .reflexive()
            .filter(|addr| !reported.contains(addr))
            .collect();
        for addr in fresh {
            reported.push(addr);
            emit.send(Event::Candidate {
                addr,
                from_stun: addr.is_ipv4(),
                lan: addr.is_ipv6(),
            });
        }
    }
    telemetry.state.store(2, Ordering::Relaxed);
}

/// Release the relay's allocation on the way out, rather than hold a relay
/// port until it expires. After everything else, so a departure goes out
/// through the relay before the relay is let go.
fn release(shell: &mut Shell<'_, Session<'_>>) {
    let Some(relay) = shell.endpoint().relay_mut() else {
        return;
    };
    if matches!(relay.state(), RelayState::Setup | RelayState::Ready(_)) {
        relay.release();
        let _ = shell.turn(|_| {});
    }
}
