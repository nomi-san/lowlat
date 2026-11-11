// The session: the library's handle, the attempts made on it to the peers in
// turn, what the service and the library raise, the actions the chords take
// on the stream, and the room as the host describes it. The guest list is
// parsed here, with the toolkit's own reader. The app thread's, but client(),
// any thread's, and first_picture(), the present thread's.

#pragma once

#include <stdint.h>

#include "lowlat.h"

namespace session {

// The decoder table printed, then the handle made; false, said on stderr,
// when it is not.
bool create();
// The translation in front of the port, asked of the reflexive servers and
// printed, before the attempt.
void probe_nat();
// The first attempt, to the first peer; false when it is not made.
bool begin();
// From when the stay counts: the window up, before the threads.
void schedule_leave(double now_ms);
// The stay over: an attempt to the next peer, the reconnect, or the leave.
void leave_when_due(double now_ms);
// What the host said through the service, then what the library found.
void pump();
// The timed walks through the preferences, the decoders and the kind.
void walk(uint64_t seconds, bool established);

// The chords' actions.
void cycle_video();
void cycle_decoder();
void toggle_kind();
// Planes from the next picture, the renderer importing no handle the decoder
// hands out; on any thread.
void planes_after_all();
void ask_outputs();

// A picture came: the rate the knob names is asked for once a session.
void first_picture();

// Set before any thread starts.
lowlat_client *client();

// This client's own entry in the room -- its number, the owner flag and its
// permissions -- and the host's figures for it, from the last guest list.
struct Roster {
	uint32_t number;
	bool owner;
	bool perm_mouse;
	bool perm_keyboard;
	bool perm_gamepad;
	uint32_t rosters;
	double host_rtt_ms;
	double host_mbps;
	double host_encode_ms;
	double host_decode_ms;
	int32_t host_packets;
	int32_t host_fast_rts;
	int32_t host_slow_rts;
	int32_t host_cg_events;
};
const Roster &roster();
// The preferences in force, in words.
const char *video_words();
// The decoder row the session is on, or none.
const lowlat_decoder_info *decoder_row();

// At the end, after the threads: the session left and the service's socket
// closed; then, after the window, the handle.
void end();
void destroy();

}
