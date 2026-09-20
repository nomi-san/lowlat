// The Sony pads read raw from their own nodes, beside the toolkit's
// controllers (docs/10-client.md section 8).
//
// The toolkit has no HID path on Linux, so a DualShock 4 or a DualSense that
// is to travel as its own report is read here: its raw node found by
// identity, polled in the demo's loop, its feature reports read once at open
// and sent ahead of the first input report, and what the host's device is
// written put back on the node. The toolkit's own events for these pads are
// dropped while this is on, so nothing is sent twice.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "lowlat.h"

#define RAW_PADS 4

struct raw_pad {
	int fd;                    // -1 when the slot is empty
	unsigned node;             // the hidraw number
	uint32_t id;               // as named to the library
	uint32_t type;             // one of lowlat_pad_type
	bool wireless;             // the wireless framing on the node
	uint8_t seq;               // a wireless DualSense's output sequence
};

struct raw_pads {
	struct raw_pad pads[RAW_PADS];
	double scanned_ms;         // when the nodes were last looked for
	bool trace;
	// The second's counts: input reports sent, and the host's writes put
	// on the nodes.
	uint32_t reports;
	uint32_t outputs;
};

void raw_pads_init(struct raw_pads *r, bool trace);
// Look for pads that appeared, every so often, once the session is up: a
// report before that has no session to travel on, and the feature reports go
// first. `now_ms` is the caller's clock.
void raw_pads_scan(struct raw_pads *r, lowlat_client *client, double now_ms, bool established);
// Every report the nodes have queued, to the library.
void raw_pads_pump(struct raw_pads *r, lowlat_client *client);
// Whether a pad the toolkit reports is one this side reads raw instead: the
// vendor's, from the start, so the host never gets a state for it under the
// toolkit's name before its node is open here. A node this side cannot open
// is said once, and that pad is then dead rather than sent both ways.
bool raw_pads_owns_vendor(const struct raw_pads *r, uint16_t vid);
// The host's write, back to the pad it names. False when the pad is not one
// read here.
bool raw_pads_write(struct raw_pads *r, const lowlat_pad_report_event *e);
// The host's rumble message for a pad read here: a motor-only report,
// the lights untouched. False when the pad is not one read here.
bool raw_pads_rumble(struct raw_pads *r, uint32_t pad, uint8_t large, uint8_t small);
void raw_pads_close(struct raw_pads *r, lowlat_client *client);
