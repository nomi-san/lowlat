//! Phase 14: the per-report path allocates nothing.
//!
//! A pad reports several hundred times a second for as long as a session
//! lasts. Building the queue and the injector is free to allocate; the region
//! inside `assert_no_alloc` is what runs per report: the message read, the
//! family rule applied, the report handed to the sink, queued for the
//! application and taken by it.

// The crate under test is built on Linux so far (docs/impl-plan-windows.md).
#![cfg(target_os = "linux")]

use std::time::Duration;

use lowlat_common::alloc_counter::{self, Counting};
use lowlat_core::control::{Control, op};
use lowlat_core::pad::{self, Inbound, Product};
use lowlat_host::padsink::{self, Report, Taken};
use lowlat_inject::Forwarded;
use lowlat_inject::event::{Device, Event, Extents, Injector, Sink};

#[global_allocator]
static ALLOC: Counting = Counting;

/// The sink the guest thread has in the application's mode, in the small: a
/// report goes straight into the application's queue.
struct Forwarding(padsink::Sender);

impl Sink for Forwarding {
    fn emit(&mut self, _device: Device, _events: &[Event]) {}

    fn report(&mut self, pad: u32, inbound: &Inbound<'_>) {
        if let Inbound::Input { product, report } = *inbound {
            self.0.send(Report::of(
                1,
                Forwarded::Input {
                    pad,
                    product,
                    report,
                },
            ));
        }
    }
}

/// **A pen's messages expand without allocating**: a hover, a touch, a
/// double click, leaving range, and the release that cancels it, over and
/// over.
#[test]
fn a_pens_messages_expand_without_allocating() {
    use lowlat_core::pen::{Pen, flag};
    let (tx, _rx) = padsink::queue();
    let mut injector = Injector::new(Extents::alone(1920, 1080));
    let mut sink = Forwarding(tx);
    let message = |flags| {
        let (a0, a1, a2) = Pen {
            x: 100,
            y: 100,
            flags,
            ..Pen::default()
        }
        .arguments();
        Control {
            a0,
            a1,
            a2,
            opcode: op::PEN_TOUCH,
            body: &[],
        }
    };
    let messages = [
        message(0),
        message(flag::TOUCHING),
        message(flag::TOUCHING | flag::DOUBLE_CLICK),
        message(flag::LEAVE),
        message(flag::TOUCHING),
    ];
    alloc_counter::assert_no_alloc(|| {
        for _ in 0..64 {
            for message in &messages {
                injector.on_control(message, &mut sink);
            }
            injector.release_all(&mut sink);
        }
    });
}

/// **Touch contacts expand without allocating**: ten down, moved, an eleventh
/// dropped -- said once, before the count -- lifted, cancelled, released.
#[test]
fn touch_contacts_expand_without_allocating() {
    use lowlat_core::touch::{Touch, flag};
    let (tx, _rx) = padsink::queue();
    let mut injector = Injector::new(Extents::alone(1920, 1080));
    let mut sink = Forwarding(tx);
    let message = |id, flags| {
        let (a0, a1, a2) = Touch {
            id,
            x: 100,
            y: 100,
            flags,
            ..Touch::default()
        }
        .arguments();
        Control {
            a0,
            a1,
            a2,
            opcode: op::PEN_TOUCH,
            body: &[],
        }
    };
    // The eleventh contact's drop is said once, here, where saying it may
    // allocate.
    for id in 0..11 {
        injector.on_control(&message(id, flag::TOUCHING), &mut sink);
    }
    injector.release_all(&mut sink);
    alloc_counter::assert_no_alloc(|| {
        for _ in 0..32 {
            for id in 0..11 {
                injector.on_control(&message(id, flag::TOUCHING), &mut sink);
            }
            injector.on_control(&message(3, flag::LEAVE), &mut sink);
            injector.on_control(&message(4, flag::LEAVE | flag::CANCELED), &mut sink);
            injector.on_control(&message(5, 0), &mut sink);
            injector.release_all(&mut sink);
        }
    });
}

#[test]
fn a_pads_report_reaches_the_application_without_allocating() {
    let ds5: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
    let (tx, rx) = padsink::queue();
    let mut injector = Injector::new(Extents::alone(1920, 1080));
    let mut sink = Forwarding(tx);
    let message = Control {
        a0: 64,
        a1: 3,
        a2: u32::from(Product::DualSense.product_id()),
        opcode: op::PAD_REPORT,
        body: ds5,
    };
    // The pad's slot is taken by its first report, outside the assertion,
    // as the queue's room was.
    injector.on_control(&message, &mut sink);
    let mut out = [0u8; pad::INPUT_LEN];
    assert!(matches!(
        rx.recv_timeout_into(Duration::ZERO, &mut out),
        Taken::Took { pad: 3, .. }
    ));

    alloc_counter::assert_no_alloc(|| {
        for _ in 0..256 {
            injector.on_control(&message, &mut sink);
            let taken = rx.recv_timeout_into(Duration::ZERO, &mut out);
            assert!(matches!(
                taken,
                Taken::Took {
                    pad: 3,
                    len: 64,
                    ..
                }
            ));
        }
        // And a queue nobody drains, at its cap, drops without allocating.
        for _ in 0..200 {
            injector.on_control(&message, &mut sink);
        }
        let taken = rx.recv_timeout_into(Duration::ZERO, &mut out);
        assert!(
            matches!(taken, Taken::Took { dropped: 136, .. }),
            "{taken:?}"
        );
    });
}
