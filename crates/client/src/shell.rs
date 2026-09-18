//! The session's thread: the shell around the driver.
//!
//! Builds the connectivity engine and the session on the socket the seam
//! opened, lends them their rings for the life of the thread, and runs the
//! shell's loop with the driver as its application.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;

use lowlat_common::events;
use lowlat_common::spsc::Ring;
use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::conn::{Conn, Credentials};
use lowlat_core::control::CONTROL_CHANNEL;
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::{Cipher, Envelope};
use lowlat_core::init::Init;
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_net::{Running, Shell, Socket, Wake};
use std::sync::atomic::Ordering;

use crate::driver::{Driver, Telemetry, Units};
use crate::input::{RING_DEPTH, Request};
use crate::seam::{Arrival, Ask, Event, Outcome};
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
    for server in &servers {
        let _ = conn.add_server(*server);
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

    let Ok(envelope) = Envelope::from_credential(&material, cipher) else {
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
    let mut shell = Shell::new(socket, wake, Endpoint::new(conn, session));
    let mut driver = Driver::new(init, units, emit.clone(), Arc::clone(&telemetry));
    let mut reported: Vec<SocketAddr> = Vec::new();
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
                        let _ = endpoint.conn().add_candidate(addr, kind);
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
            // The application caused this; it needs no event back.
            telemetry.state.store(2, Ordering::Relaxed);
            return;
        }
        if let Some(outcome) = ended {
            telemetry.state.store(2, Ordering::Relaxed);
            emit.send(Event::Ended { outcome });
            return;
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
