// Every knob the demo reads, read once at start, on the main thread before
// any other thread starts, and only read after.
//
// `LOWLAT_SERVER` names the signaling service (kessel-ws.parsec.app by
// default), `LOWLAT_DEVICE` the decoder's device -- a render node, or on
// Windows a GPU's identity as the table prints it (`luid:HIGH:LOW`); the
// first that decodes by default -- `LOWLAT_DECODER` one of `auto`, `open`, `vendor`,
// `software`, `system` (the system's own: Windows', in software, and macOS's), `none`. The decoder table is printed at start, one slot a
// line with the unavailable ones saying why, and `LOWLAT_DECODER_INDEX`
// picks a slot instead. The software row is the machine's own codec library, an LGPL
// build of it or none; `LOWLAT_FFMPEG_DIR` names where its pair is.
// `LOWLAT_HANDLE` asks for pictures as device handles, which the renderer
// imports and draws with no copy through this process; only a decoder that
// exports them (a row saying "handles") can be opened for that. On Linux
// only GL draws them; `LOWLAT_GFX=vk` draws planes through Vulkan instead of
// GL. On Windows planes are drawn through Direct3D 11, or 12 or Vulkan by
// name (`d3d12`, `vk`), and handles through Direct3D 11 alone. Its device
// is made on the GPU the decoder is named on, else on the system's first
// adapter, and with nothing named the decoder is created there too -- by
// handle always, by planes where a hardware decoder is listed there -- so a
// picture of textures opens where it is drawn and switching to handles moves
// nothing; pictures moved to another GPU by the chord or a row, or found
// again under a new identity after a device was lost, bring it after them.
// On macOS Metal draws both, a handle being each picture's surface, imported
// by plane; only the system's decoder hands one out.
// `LOWLAT_VSYNC=0` presents a picture the moment it is drawn rather than at
// the display's next refresh: the lowest latency, torn where a picture
// lands mid-scan.
// `LOWLAT_HEVC`, `LOWLAT_10BIT` and `LOWLAT_444` are the preferences the
// attempt starts with: each is "prefer this if the host has it", masked by
// what the decoder takes before anything is declared; `LOWLAT_SWITCH_EVERY`
// walks them every that many seconds, as the chord does by hand,
// `LOWLAT_DECODER_EVERY` walks the decoders the same way, and
// `LOWLAT_KIND_EVERY` switches between planes and handles. `LOWLAT_OUTPUT`
// asks for the host's output of that identity once the session is up, as
// Ctrl+Shift+D does for the next one. The full range is
// asked for too, because the renderer takes each picture's range;
// `LOWLAT_FULL_RANGE=0` asks for the video range instead.
// `LOWLAT_FPS` asks the host for that rate through the application
// protocol once the first picture is in; `LOWLAT_PRESENT_HZ` caps how often
// a new picture is taken (the cached one is still drawn every refresh), so
// a stream faster than the presentation can be measured on one display;
// `LOWLAT_STUN` names reflexive servers, `host:port` separated by commas, up
// to four: without one a direct attempt offers only this machine's own
// addresses, which a host behind its own translator cannot answer.
// `LOWLAT_IPV4_ONLY` offers and checks IPv4 addresses only, so an attempt
// that IPv6 would connect directly crosses the translation on IPv4 instead.
// `LOWLAT_NAT=1` asks those servers at start, before the attempt, where they
// see the port, and prints what the translation in front of it is.
// `LOWLAT_PORT` is the port every attempt binds first -- by default the same
// one every run, picked by `LOWLAT_PORT_SEED` or the machine's name -- and
// the gateway is asked to keep it open, by PCP, NAT-PMP or UPnP, unless
// `LOWLAT_PORT_MAPPING=0`; a line says what it mapped whenever that changes.
// On Windows the system's firewall asks once whether the demo may receive,
// at the first attempt: a peer reaching the mapped port from a port this side
// never sent to is dropped until it is allowed.
// `LOWLAT_RELAY=host:port` makes the attempt a relay attempt through that
// relay, with `LOWLAT_RELAY_USER` and `LOWLAT_RELAY_PASS` its credential,
// which is handed to the library and never printed: the relayed address is
// all this side offers, and every check goes through the relay. The password
// is read at each attempt and kept nowhere here. `LOWLAT_RELAY_CARRIER=tcp`
// reaches that relay over one stream instead, for a network that carries no
// datagrams; `udp`, the default, is the other.
// `LOWLAT_SECONDS` leaves cleanly after that long; `LOWLAT_DUMP_FRAME` names
// a file the tenth picture's planes are written to, so what the renderer
// was handed can be looked at with another tool. `LOWLAT_RAW_AUDIO` asks
// the host for uncompressed sound; `LOWLAT_AUDIO_TRACE` prints a line per
// sound packet with its age and what the device held. Against a host on
// this same machine the sound must go to an output the host does not
// capture, or it is captured again and echoes: a null sink named through
// the sound server's own environment (`PIPEWIRE_NODE`) is one.
// `LOWLAT_MIC_TONE=440` sends a tone at that pitch as the microphone;
// `LOWLAT_PAD_RAW` sends a Sony pad as its own report (see pads.h), and
// `LOWLAT_PAD_TRACE` prints every pad's report; `LOWLAT_LATENCY_TRACE` prints
// a line per picture (see report.h).

#pragma once

#include <stdint.h>

#include "lowlat.h"
#include "matoya.h"

namespace config {

struct Settings {
	// The hosts to visit, in order, on the one handle: each held for the
	// stay and left for the next, which is a reconnect. The service and the
	// session every attempt is offered through.
	char peers[8][64];
	unsigned peer_count;
	unsigned long seconds;
	double stay_ms;
	const char *server;
	const char *session;

	// What creation asks for: the decoder and its device, or a row of the
	// table by its number; pictures as device handles or as planes.
	const char *device;
	uint32_t decoder;
	const char *decoder_index;
	bool handles;
	uint16_t port;
	const char *port_seed;
	bool port_mapping;

	// The graphics interface drawn through: on Linux GL, the one that imports
	// device handles, or Vulkan, for planes; on Windows Direct3D 11, which
	// opens the library's shared textures, or 12 or Vulkan, for planes.
	MTY_GFX gfx;
	// The GPU the Direct3D 11 context starts on (the adapter's identity, low
	// part first): the one the decoder's device names, else the system's
	// first adapter, which the toolkit's default device is made on; zero
	// where neither is known.
	uint64_t gfx_adapter;
	// Whether a present waits for the display's refresh.
	bool vsync;
	// How often a new picture is taken, zero for every refresh.
	double poll_period_ms;

	// The rate asked of the host once the first picture is in, zero for none.
	uint32_t ask_fps;
	// Walk the preferences, the decoders, and the kind every so many
	// seconds; zero for never.
	uint64_t switch_every;
	uint64_t decoder_every;
	uint64_t kind_every;
	// An output asked for by its identity once the session is up, in place
	// of the chord's next one; null for none.
	const char *output;
	// Sizes and turns asked of the host in order once the session is up,
	// one every mode_every seconds: "1280x720", "turn" or "upright", comma
	// separated; null for none.
	const char *modes;
	uint64_t mode_every;
	// The preferences the first attempt starts with.
	lowlat_client_video_config video;

	// Every attempt's: the reflexive servers as named, sound asked
	// uncompressed, no media key offered, IPv4 alone, a relay, its user and
	// what reaches it.
	const char *stun;
	bool raw_audio;
	bool legacy_cipher;
	bool ipv4_only;
	const char *relay;
	const char *relay_user;
	lowlat_relay_carrier relay_carrier;
	// The translation asked of the reflexive servers before the attempt.
	bool nat;

	double mic_hz;
	bool raw_pads;
	bool raw_pads_only;
	bool trace_pads;
	bool trace_latency;
	bool trace_audio;
	const char *dump_frame;
};

// Read them all, saying on stderr what stops the demo: zero to go on, else
// the code to exit with.
int read();
const Settings &get();

// The graphics interface by the name the knob takes; "-" for none.
const char *gfx_name(MTY_GFX gfx);
// Whether the interface drawn through imports device handles: GL on Linux,
// Direct3D 11 on Windows; the others draw planes alone.
bool draws_handles();

}
