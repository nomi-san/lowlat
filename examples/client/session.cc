// The session. See session.h.

#include "session.h"

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <atomic>

#include "matoya.h"

#include "app.h"
#include "clocks.h"
#include "config.h"
#include "input.h"
#include "logs.h"
#include "pads.h"
#include "present.h"
#include "report.h"
#include "signaling.h"
#include "view.h"

namespace session {

static lowlat_client *_client;
// The attempt in flight, the peer the list is at, whether the host's answer
// has begun the attempt, and when the stay there is over.
static char _attempt[LOWLAT_ATTEMPT_MAX];
static unsigned _peer_at;
static bool _begun;
static double _leave_at_ms;
// The decoders this machine can open, as listed at start, and which of
// them the session decodes on; the chord moves to the next.
static lowlat_decoder_info _rows[32];
static unsigned _row_count;
static unsigned _row;
// The picture preferences, cycled by the chord; whether pictures are asked
// for as device handles, at creation and switched by the chord since.
static lowlat_client_video_config _video;
static bool _handles;
// Switching the streamed output: the host's outputs and its current
// configuration, both asked for on the chord and acted on together; the
// host also sends both whenever its capture changes, which is no chord.
static MTY_JSON *_outputs;
static MTY_JSON *_host_config;
static bool _switching;
// Whether the output named by LOWLAT_OUTPUT was asked for yet, and whether
// the host listed it and was asked for it.
static bool _named;
static bool _named_asked;
// How many of LOWLAT_MODES were asked for, and the second the next is due,
// zero until the session is up.
static unsigned _moded;
static uint64_t _mode_due;
// Whether the session's rate was asked for: set by the present thread, which
// asks, and cleared by the reconnect.
static std::atomic_bool _asked;
static Roster _roster;

// Six random groups: the attempt identifier's shape.
static void attempt_id(char *out, size_t size)
{
	uint32_t words[6];
	MTY_GetRandomBytes(words, sizeof words);
	snprintf(out, size, "%08" PRIx32 "-%08" PRIx32 "-%08" PRIx32 "-%08" PRIx32 "-%08" PRIx32
		"-%08" PRIx32, words[0], words[1], words[2], words[3], words[4], words[5]);
}

// The reflexive servers `LOWLAT_STUN` names, into `servers`; how many.
static uint32_t stun_servers(char (*servers)[LOWLAT_SERVER_MAX])
{
	uint32_t count = 0;
	const char *stun = config::get().stun;
	for (const char *at = stun; at != NULL && *at != '\0' && count < LOWLAT_SERVERS_MAX;) {
		size_t len = strcspn(at, ",");
		if (len > 0 && len < LOWLAT_SERVER_MAX)
			snprintf(servers[count++], LOWLAT_SERVER_MAX, "%.*s", (int) len, at);
		at += len + (at[len] == ',');
	}
	return count;
}

const char *video_words()
{
	const lowlat_client_video_config *v = &_video;
	return !v->hevc && !v->ten_bit && !v->chroma_444 ? "h264"
		: v->ten_bit && v->chroma_444 ? "hevc 10bit 444"
		: v->ten_bit ? "hevc 10bit"
		: v->chroma_444 ? "hevc 444" : "hevc";
}

// The chord walks the preferences up the host's own order and back to the
// start: none, the second codec, ten-bit, full chroma, both.
void cycle_video()
{
	lowlat_client_video_config *v = &_video;
	if (!v->hevc) {
		v->hevc = true;
	} else if (!v->ten_bit && !v->chroma_444) {
		v->ten_bit = true;
	} else if (v->ten_bit && !v->chroma_444) {
		v->ten_bit = false;
		v->chroma_444 = true;
	} else if (!v->ten_bit) {
		v->ten_bit = true;
	} else {
		v->hevc = v->ten_bit = v->chroma_444 = false;
	}
	lowlat_status s = lowlat_client_set_video_config(_client, v);
	printf("demo: asked %s: %s\n", video_words(), lowlat_status_string(s));
}

// The chord moves the session to the next decoder this machine listed at
// start: one call, one keyframe; a row that does not open leaves the
// session where it was and says so.
void cycle_decoder()
{
	if (_row_count < 2) {
		printf("demo: one decoder here, nothing to move to\n");
		return;
	}
	unsigned next = (_row + 1) % _row_count;
	const lowlat_decoder_info *row = &_rows[next];
	lowlat_status s = lowlat_client_set_decoder(_client, row->decoder, row->device);
	printf("demo: decoder [%u] %s (%s) on %s: %s\n", row->index, row->name, row->driver,
		row->device[0] ? row->device : "any device", lowlat_status_string(s));
	if (s == LOWLAT_OK)
		_row = next;
}

// The chord switches the pictures between planes and device handles at the
// next one, with no keyframe; a decoder that hands out no handles refuses,
// and nothing changes.
void toggle_kind()
{
	uint32_t kind = _handles ? LOWLAT_FRAME_PLANES : LOWLAT_FRAME_HANDLE;
	lowlat_status s = lowlat_client_set_frame_kind(_client, kind);
	printf("demo: pictures as %s: %s\n", kind == LOWLAT_FRAME_HANDLE ? "handles" : "planes",
		lowlat_status_string(s));
	if (s == LOWLAT_OK)
		_handles = !_handles;
}

// The chord: ask the host what it can stream and what it streams now.
void ask_outputs()
{
	MTY_JSONDestroy(&_outputs);
	MTY_JSONDestroy(&_host_config);
	_switching = true;
	lowlat_client_send_user_data(_client, 10, "", 0);
	lowlat_client_send_user_data(_client, 9, "", 0);
}

// Both answers in: the output after the current one, in the host's own
// configuration sent back whole, because a host reads the element whole.
static void cycle_output()
{
	if (!_switching || _outputs == NULL || _host_config == NULL)
		return;
	_switching = false;
	const MTY_JSON *video = MTY_JSONObjGetItem(_host_config, "video");
	const MTY_JSON *first = video != NULL ? MTY_JSONArrayGetItem(video, 0) : NULL;
	const char *current = first != NULL ? MTY_JSONStringPtr(MTY_JSONObjGetItem(first, "output")) : NULL;
	uint32_t n = MTY_JSONArrayGetLength(_outputs);
	if (first == NULL || n == 0)
		return;
	// The output LOWLAT_OUTPUT names, where it does, in place of the next.
	const char *wanted = config::get().output;
	uint32_t at = wanted != NULL ? n : 0;
	for (uint32_t i = 0; i < n; i++) {
		const char *id = MTY_JSONStringPtr(MTY_JSONObjGetItem(MTY_JSONArrayGetItem(_outputs, i), "id"));
		if (id == NULL)
			continue;
		if (wanted != NULL && strcmp(id, wanted) == 0)
			at = i;
		else if (wanted == NULL && current != NULL && strcmp(id, current) == 0)
			at = (i + 1) % n;
	}
	if (at == n) {
		// What it does list, so the name it goes by can be given instead.
		printf("demo: the host lists no output %s; it lists", wanted);
		for (uint32_t i = 0; i < n; i++) {
			const MTY_JSON *item = MTY_JSONArrayGetItem(_outputs, i);
			const char *id = MTY_JSONStringPtr(MTY_JSONObjGetItem(item, "id"));
			const char *name = MTY_JSONStringPtr(MTY_JSONObjGetItem(item, "name"));
			printf(" \"%s\" (%s)", id != NULL ? id : "?", name != NULL ? name : "?");
		}
		printf("\n");
		MTY_JSONDestroy(&_outputs);
		MTY_JSONDestroy(&_host_config);
		return;
	}
	const char *next = MTY_JSONStringPtr(MTY_JSONObjGetItem(MTY_JSONArrayGetItem(_outputs, at), "id"));
	if (next == NULL)
		return;
	// The lookup above borrows from the object the set replaces, so the
	// name is copied first.
	char chosen[256];
	snprintf(chosen, sizeof chosen, "%s", next);
	MTY_JSONObjSetItem((MTY_JSON *) first, "output", MTY_JSONStringCreate(chosen));
	char *body = MTY_JSONSerialize(_host_config);
	lowlat_status s = lowlat_client_send_user_data(_client, 11, body, (uint32_t) strlen(body));
	printf("demo: asked for output %s%s\n", chosen, s == LOWLAT_OK ? "" : ", refused");
	if (wanted != NULL && s == LOWLAT_OK)
		_named_asked = true;
	MTY_Free(body);
	MTY_JSONDestroy(&_outputs);
	MTY_JSONDestroy(&_host_config);
}

// The room: find this client's own entry by its number and keep what the
// host says about it -- owner, permissions, and its figures for this guest.
// A body that does not parse is dropped, as an established client drops it.
static void on_guest_list(uint32_t number, const char *body)
{
	MTY_JSON *list = MTY_JSONParse(body);
	if (list == NULL) {
		printf("demo: guest list did not parse (%zu bytes)\n", strlen(body));
		return;
	}
	Roster &r = _roster;
	r.rosters++;
	r.number = number;
	uint32_t n = MTY_JSONArrayGetLength(list);
	for (uint32_t i = 0; i < n; i++) {
		const MTY_JSON *guest = MTY_JSONArrayGetItem(list, i);
		int32_t id = -1;
		if (!MTY_JSONInt32(MTY_JSONObjGetItem(guest, "id"), &id) || (uint32_t) id != number)
			continue;
		MTY_JSONObjGetBool(guest, "owner", &r.owner);
		const MTY_JSON *perms = MTY_JSONObjGetItem(guest, "perms");
		if (perms != NULL) {
			MTY_JSONObjGetBool(perms, "mouse", &r.perm_mouse);
			MTY_JSONObjGetBool(perms, "keyboard", &r.perm_keyboard);
			MTY_JSONObjGetBool(perms, "gamepad", &r.perm_gamepad);
		}
		const MTY_JSON *metrics = MTY_JSONObjGetItem(guest, "metrics");
		const MTY_JSON *video = metrics != NULL && MTY_JSONGetType(metrics) == MTY_JSON_ARRAY
			? MTY_JSONArrayGetItem(metrics, 0) : metrics;
		if (video != NULL) {
			MTY_JSONNumber(MTY_JSONObjGetItem(video, "networkLatency"), &r.host_rtt_ms);
			MTY_JSONNumber(MTY_JSONObjGetItem(video, "bitrate"), &r.host_mbps);
			MTY_JSONNumber(MTY_JSONObjGetItem(video, "encodeLatency"), &r.host_encode_ms);
			MTY_JSONNumber(MTY_JSONObjGetItem(video, "decodeLatency"), &r.host_decode_ms);
			MTY_JSONInt32(MTY_JSONObjGetItem(video, "packetsSent"), &r.host_packets);
			MTY_JSONInt32(MTY_JSONObjGetItem(video, "fastRTs"), &r.host_fast_rts);
			MTY_JSONInt32(MTY_JSONObjGetItem(video, "slowRTs"), &r.host_slow_rts);
			MTY_JSONInt32(MTY_JSONObjGetItem(video, "cgEvents"), &r.host_cg_events);
		}
		break;
	}
	if (r.rosters == 1)
		printf("demo: guest list: %u guests, this client is %u%s, mouse=%d keyboard=%d gamepad=%d\n",
			n, number, r.owner ? " (owner)" : "", r.perm_mouse, r.perm_keyboard,
			r.perm_gamepad);
	MTY_JSONDestroy(&list);
}

// The rate is a change to a running stream, so it is asked for once the
// first picture proves there is one. The same message a settings panel
// sends; the library adds the terminator.
static void ask_rate()
{
	uint32_t fps = config::get().ask_fps;
	char body[64];
	int n = snprintf(body, sizeof body, "{\"video\":[{\"encoderFPS\":%u}]}", fps);
	lowlat_status s = lowlat_client_send_user_data(_client, 11, body, (uint32_t) n);
	printf("demo: asked the host for %u fps%s\n", fps, s == LOWLAT_OK ? "" : ", refused");
}

// The next of LOWLAT_MODES: a size, a turn or a bitrate, alone in the element
// as a settings panel sends each, so nothing else is changed.
static void ask_mode()
{
	const char *at = config::get().modes;
	for (unsigned i = 0; i < _moded && at != NULL; i++) {
		at = strchr(at, ',');
		at = at != NULL ? at + 1 : NULL;
	}
	if (at == NULL || *at == '\0')
		return;
	_moded++;
	char step[32];
	snprintf(step, sizeof step, "%.*s", (int) strcspn(at, ","), at);
	char body[96];
	unsigned width = 0, height = 0, mbps = 0;
	int end = 0;
	if (strcmp(step, "turn") == 0 || strcmp(step, "upright") == 0) {
		snprintf(body, sizeof body, "{\"video\":[{\"rotated\":%s}]}",
			step[0] == 't' ? "true" : "false");
	} else if (sscanf(step, "%ux%u", &width, &height) == 2) {
		snprintf(body, sizeof body, "{\"video\":[{\"resolutionX\":%u,\"resolutionY\":%u}]}",
			width, height);
	} else if (sscanf(step, "%u%n", &mbps, &end) == 1 && strcmp(step + end, "mbps") == 0) {
		snprintf(body, sizeof body, "{\"video\":[{\"encoderMaxBitrate\":%u}]}", mbps);
	} else {
		printf("demo: %s is neither a size, a turn nor a bitrate\n", step);
		return;
	}
	lowlat_status s = lowlat_client_send_user_data(_client, 11, body, (uint32_t) strlen(body));
	printf("demo: asked the host for %s%s\n", step, s == LOWLAT_OK ? "" : ", refused");
}

void first_picture()
{
	if (config::get().ask_fps != 0 && !_asked.exchange(true))
		ask_rate();
}

// Forward what the library found to the host, and act on what ended.
static void pump_library()
{
	static char body[65536];
	for (;;) {
		lowlat_event e;
		uint32_t body_len = sizeof body - 1;
		lowlat_status s = lowlat_client_poll_events(_client, 0, &e, body, &body_len);
		if (s == LOWLAT_ERR_TOO_SMALL) {
			// Too long to be one of the two answers the demo reads; taken
			// off the queue and dropped.
			uint32_t none = 0;
			lowlat_client_poll_events(_client, 0, &e, NULL, &none);
			continue;
		}
		if (s != LOWLAT_OK)
			break;
		switch (e.kind) {
			case LOWLAT_EVENT_USER_DATA:
				body[body_len] = '\0';
				if (e.body.user_data.id == 12) {
					MTY_JSONDestroy(&_outputs);
					_outputs = MTY_JSONParse(body);
					cycle_output();
				} else if (e.body.user_data.id == 11) {
					MTY_JSONDestroy(&_host_config);
					_host_config = MTY_JSONParse(body);
					cycle_output();
				}
				break;
			case LOWLAT_EVENT_RELATIVE:
				input::host_relative(&e.body.relative);
				break;
			case LOWLAT_EVENT_CANDIDATE:
				logs::candidate("our", e.body.candidate.address, e.body.candidate.port,
					e.body.candidate.lan, e.body.candidate.from_stun);
				signaling::send_candidate(e.body.candidate.address, e.body.candidate.port,
					e.body.candidate.lan, e.body.candidate.from_stun, false);
				break;
			case LOWLAT_EVENT_READY:
				logs::line("our readiness marker");
				signaling::send_candidate("0.0.0.0", 0, false, false, true);
				break;
			case LOWLAT_EVENT_ESTABLISHED: {
				lowlat_client_status st{.size = sizeof(lowlat_client_status)};
				lowlat_client_get_status(_client, &st);
				if (st.relayed)
					printf("demo: established with %s:%u through the relay, relayed at %s:%u\n",
						e.body.established.address, (unsigned) e.body.established.port,
						st.relay_address, (unsigned) st.relay_port);
				else
					printf("demo: established with %s:%u\n", e.body.established.address,
						(unsigned) e.body.established.port);
				// The service has done its part: the socket goes, and the
				// next attempt opens its own.
				signaling::close(false);
				break;
			}
			case LOWLAT_EVENT_ENDED:
				printf("demo: ended, outcome %d reason %d\n", (int) e.body.ended.outcome,
					(int) e.body.ended.reason);
				app::quit();
				break;
			case LOWLAT_EVENT_BLOCKED:
				printf("demo: input %s\n", e.body.blocked.blocked ? "blocked" : "unblocked");
				break;
			case LOWLAT_EVENT_MICROPHONE:
				// A capture device would be opened or closed here; the tone
				// keeps running and the library refuses it while closed.
				printf("demo: the host %s the microphone\n",
					e.body.microphone.enabled ? "takes" : "takes no");
				break;
			case LOWLAT_EVENT_STREAM_ENDED:
				printf("demo: stream %u ended, status %d\n", (unsigned) e.body.stream_ended.stream,
					(int) e.body.stream_ended.status);
				break;
			case LOWLAT_EVENT_HOST_MODE:
				printf("demo: host mode %u\n", (unsigned) e.body.host_mode.mode);
				break;
			case LOWLAT_EVENT_CURSOR:
				view::cursor(&e.body.cursor);
				break;
			case LOWLAT_EVENT_RUMBLE:
				pads::rumble(&e.body.rumble);
				break;
			case LOWLAT_EVENT_PAD_REPORT:
				pads::written(&e.body.pad_report);
				break;
			case LOWLAT_EVENT_GUEST_LIST:
				body[body_len] = '\0';
				on_guest_list(e.body.guest_list.number, body);
				break;
			default:
				break;
		}
	}
}

// What the host said through the service.
static void pump_signaling()
{
	for (;;) {
		lowlat_credentials theirs;
		lowlat_candidate candidate;
		signaling::Event e = signaling::poll(0, &theirs, &candidate);
		if (e == signaling::Event::nothing)
			break;
		if (e == signaling::Event::closed) {
			if (!_begun)
				app::quit();
			break;
		}
		if (e == signaling::Event::answer && !_begun) {
			lowlat_status s = lowlat_client_begin_p2p(_client, _attempt, &theirs);
			if (s != LOWLAT_OK) {
				fprintf(stderr, "demo: begin refused: %s\n", lowlat_status_string(s));
				app::quit();
				break;
			}
			_begun = true;
		}
		if (e == signaling::Event::candidate) {
			if (candidate.sync)
				logs::line("their readiness marker");
			else
				logs::candidate("their", candidate.address, candidate.port, candidate.lan,
					candidate.reflexive);
			lowlat_client_add_candidate(_client, _attempt, &candidate);
		}
	}
}

// An attempt to the peer the list is at: an identifier, the credentials the
// library mints for the offer, the service's socket and the offer itself.
// The preferences are the ones in force, the chord's moves included; the
// relay's password is read here each time and kept nowhere.
static bool begin_attempt()
{
	const config::Settings &knobs = config::get();
	const char *peer = knobs.peers[_peer_at];
	attempt_id(_attempt, sizeof _attempt);
	lowlat_credentials ours{.size = sizeof(lowlat_credentials)};
	// The attempt's configuration: the defaults, but for sound asked
	// uncompressed when the knob says so.
	lowlat_client_config cfg{.size = sizeof(lowlat_client_config)};
	cfg.raw_audio = knobs.raw_audio;
	// No media key offered, so the session takes the legacy 128-bit cipher.
	cfg.legacy_cipher = knobs.legacy_cipher;
	// IPv4 addresses only, offered and checked, so the attempt crosses the
	// translation on the IPv4 path where IPv6 would connect directly.
	cfg.ipv4_only = knobs.ipv4_only;
	cfg.video = _video;
	cfg.server_count = stun_servers(cfg.servers);
	if (knobs.relay[0] != '\0') {
		const char *password = getenv("LOWLAT_RELAY_PASS");
		snprintf(cfg.relay, sizeof cfg.relay, "%s", knobs.relay);
		snprintf(cfg.relay_username, sizeof cfg.relay_username, "%s", knobs.relay_user);
		snprintf(cfg.relay_password, sizeof cfg.relay_password, "%s",
			password != NULL ? password : "");
		cfg.relay_carrier = knobs.relay_carrier;
		printf("demo: a relay attempt through %s over %s\n", knobs.relay,
			knobs.relay_carrier == LOWLAT_RELAY_CARRIER_TCP ? "TCP" : "UDP");
	}
	printf("demo: asking %s\n", video_words());
	lowlat_status s = lowlat_client_new_attempt(_client, &cfg, _attempt, LOWLAT_TRANSPORT_BUD,
		&ours);
	// The library has its own copy of the credential now.
	memset(cfg.relay_password, 0, sizeof cfg.relay_password);
	if (s != LOWLAT_OK) {
		fprintf(stderr, "demo: no attempt: %s\n", lowlat_status_string(s));
		return false;
	}
	if (!signaling::connect(knobs.server, knobs.session, peer, _attempt))
		return false;
	if (!signaling::offer(&ours))
		return false;
	printf("demo: offered attempt %s to %s\n", _attempt, peer);
	return true;
}

// The reconnect: the session left, the service's socket closed, what the
// session told this side forgotten, and an attempt to the next peer on the
// same handle. The picture on the screen stays until the next one comes.
static bool next_peer()
{
	double began = clocks::now_ms();
	lowlat_client_end_connection(_client);
	// Still open only for an attempt that never came up, whose offer is
	// withdrawn.
	signaling::close(true);
	printf("demo: left %s in %.0f ms\n", config::get().peers[_peer_at], clocks::now_ms() - began);
	_begun = false;
	_asked.store(false);
	report::forget_session();
	// Told again once the next session's first picture is in, whatever its
	// size.
	present::forget_picture();
	view::forget_session();
	input::forget_session();
	_roster = Roster{};
	_peer_at++;
	return begin_attempt();
}

bool create()
{
	const config::Settings &knobs = config::get();
	_handles = knobs.handles;
	_video = knobs.video;
	lowlat_client_create_info info{.size = sizeof(lowlat_client_create_info)};
	info.decoder = knobs.decoder;
	info.frame_kind = knobs.handles ? LOWLAT_FRAME_HANDLE : LOWLAT_FRAME_PLANES;
	snprintf(info.device, sizeof info.device, "%s", knobs.device);
	info.port = knobs.port;
	snprintf(info.port_seed, sizeof info.port_seed, "%s", knobs.port_seed);
	info.port_mapping = knobs.port_mapping;

	// What this machine can open, one row each; a row picked by number
	// names the decoder and the device for creation.
	const char *pick = knobs.decoder_index;
	lowlat_decoder_info row{.size = sizeof(lowlat_decoder_info)};
	// The table is fixed and every slot answers; the rows kept are the
	// available ones, the rest printed with their reason and passed over.
	for (uint32_t i = 0; lowlat_enum_decoders(i, &row); i++) {
		if (!row.available) {
			printf("demo: decoder [%u] %s unavailable%s%s: %s\n", row.index, row.name,
				row.device[0] ? " on " : "", row.device, row.driver);
			continue;
		}
		printf("demo: decoder [%u] %s (%s) on %s: h264 %ux%u, hevc %ux%u%s%s%s, %s\n",
			row.index, row.name, row.driver, row.device[0] ? row.device : "any device",
			row.max_width_h264, row.max_height_h264,
			row.max_width_hevc, row.max_height_hevc,
			row.hevc_10 ? ", 10-bit" : "",
			row.hevc_444 ? ", 4:4:4" : "",
			row.hevc_444_10 ? ", 4:4:4 10-bit" : "",
			row.handle ? "handles" : "planes only");
		if (pick != NULL && strtoul(pick, NULL, 10) == row.index) {
			info.decoder = row.decoder;
			snprintf(info.device, sizeof info.device, "%s", row.device);
			_row = _row_count;
		}
		if (_row_count < sizeof _rows / sizeof _rows[0])
			_rows[_row_count++] = row;
	}
	lowlat_status s = lowlat_client_create(&info, &_client);
	if (s != LOWLAT_OK) {
		fprintf(stderr, "demo: no client: %s\n", lowlat_status_string(s));
		return false;
	}
	if (pick == NULL) {
		// Which row creation settled on, so the chord moves from it: the
		// backend as status reports it, on the device named if one was.
		lowlat_client_status st{.size = sizeof(lowlat_client_status)};
		lowlat_client_get_status(_client, &st);
		for (unsigned i = 0; i < _row_count; i++) {
			if (_rows[i].decoder == st.backend
				&& (knobs.device[0] == 0 || strcmp(_rows[i].device, knobs.device) == 0)) {
				_row = i;
				break;
			}
		}
	}
	return true;
}

// The translation in front of the port, asked of the reflexive servers and
// waited for, then printed: the number, how the translation maps, where the
// first server saw the port, and what the gateway says beside it.
void probe_nat()
{
	lowlat_nat_probe probe{.size = sizeof(lowlat_nat_probe)};
	probe.server_count = stun_servers(probe.servers);
	lowlat_status s = lowlat_client_probe_nat(_client, &probe);
	if (s != LOWLAT_OK) {
		printf("demo: no NAT probe: %s\n", lowlat_status_string(s));
		return;
	}
	lowlat_nat_info nat{.size = sizeof(lowlat_nat_info)};
	do {
		MTY_Sleep(50);
		lowlat_client_get_nat(_client, &nat);
	} while (nat.state == LOWLAT_NAT_STATE_PROBING);
	static const char *const mappings[] = {"unknown", "no answer", "none", "independent",
		"dependent", "address dependent", "address and port dependent"};
	static const char *const by[] = {"none", "PCP", "NAT-PMP", "UPnP"};
	char number[16];
	if (nat.nat_type == LOWLAT_NAT_TYPE_UNKNOWN)
		snprintf(number, sizeof number, "unknown");
	else
		snprintf(number, sizeof number, "%u", (unsigned) nat.nat_type);
	printf("demo: NAT type %s, mapping %s, public %s:%u%s, %u of %u servers answered\n",
		number, nat.mapping < 7 ? mappings[nat.mapping] : "?", nat.public_address,
		(unsigned) nat.public_port, nat.port_preserved ? " (port kept)" : "",
		(unsigned) nat.servers_answered, (unsigned) nat.servers_asked);
	printf("demo: gateway mapping by %s%s%s%s\n", nat.gateway < 4 ? by[nat.gateway] : "?",
		nat.gateway_confirmed ? ", confirmed" : "",
		nat.double_translation ? ", a second translator beyond it" : "",
		nat.carrier_range ? ", in a carrier's shared range" : "");
}

bool begin()
{
	return begin_attempt();
}

void schedule_leave(double now_ms)
{
	const config::Settings &knobs = config::get();
	_leave_at_ms = knobs.seconds > 0 ? now_ms + knobs.stay_ms : 0.0;
}

void leave_when_due(double now_ms)
{
	if (_leave_at_ms <= 0.0 || now_ms < _leave_at_ms)
		return;
	const config::Settings &knobs = config::get();
	if (_peer_at + 1 < knobs.peer_count) {
		printf("demo: leaving %s after %" PRIu64 " s, for %s\n", knobs.peers[_peer_at],
			report::seconds(), knobs.peers[_peer_at + 1]);
		if (!next_peer())
			app::quit();
		_leave_at_ms = clocks::now_ms() + knobs.stay_ms;
	} else {
		printf("demo: leaving after %" PRIu64 " s\n", report::seconds());
		app::quit();
	}
}

void pump()
{
	pump_signaling();
	pump_library();
}

void walk(uint64_t seconds, bool established)
{
	const config::Settings &knobs = config::get();
	// The timed walk through the preferences, the chord's without a
	// hand on the keyboard: once the session is up, every so many
	// seconds.
	if (knobs.switch_every > 0 && established && seconds % knobs.switch_every == 0)
		cycle_video();
	// And the same walk through the decoders this machine listed.
	if (knobs.decoder_every > 0 && established && seconds % knobs.decoder_every == 0)
		cycle_decoder();
	// And between planes and handles.
	if (knobs.kind_every > 0 && established && seconds % knobs.kind_every == 0)
		toggle_kind();
	// And to the output named, once, as the chord would.
	if (knobs.output != NULL && established && !_named) {
		_named = true;
		ask_outputs();
	}
	// And the sizes and turns named, one a period, the first a period after
	// the session is up -- or after an output named above was asked for, so
	// it is already streaming; never when the host did not list it, or they
	// would land on whatever display it streams instead.
	if (knobs.modes != NULL && knobs.mode_every > 0 && established
		&& (knobs.output == NULL || _named_asked)) {
		if (_mode_due == 0)
			_mode_due = seconds + knobs.mode_every;
		if (seconds >= _mode_due) {
			_mode_due += knobs.mode_every;
			ask_mode();
		}
	}
}

lowlat_client *client()
{
	return _client;
}

const Roster &roster()
{
	return _roster;
}

const lowlat_decoder_info *decoder_row()
{
	return _row < _row_count ? &_rows[_row] : NULL;
}

void end()
{
	lowlat_client_end_connection(_client);
	signaling::close(true);
}

void destroy()
{
	lowlat_client_destroy(_client);
	MTY_JSONDestroy(&_outputs);
	MTY_JSONDestroy(&_host_config);
}

}
