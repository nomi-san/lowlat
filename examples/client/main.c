// The client demo: a window that shows a host's desktop and drives it.
//
// Pure C on the application toolkit. The library decodes and encodes input;
// this presents and reports what happened in its window. One file for the
// session and the window, one for signaling. Sound goes to the toolkit's
// device from a thread of its own; the cursor comes with its phase.
//
//   LOWLAT_PEER=<the host's peer id> LOWLAT_SESSION=<a session token> ./client
//
// Keyboard, mouse and pads go to the host as the toolkit reports them; the
// rectangle the picture is drawn into is told to the library, which maps
// positions into the picture. Chords the demo keeps for itself, never sent:
// Ctrl+Alt+F switches between the picture stretched to the window and shown
// at its own size, Ctrl+Alt+R lets go of a pointer the host has captured
// (and takes it again), Ctrl+Alt+O asks the host to stream its next output.
// A bare Windows key is not sent, because the desktop here takes it and the
// host would be left with the modifier held; it reaches the host on chords.
//
// `LOWLAT_SERVER` names the signaling service (kessel-ws.parsec.app by
// default), `LOWLAT_DEVICE` a render node for the decoder (the first that
// decodes by default), `LOWLAT_DECODER` one of `auto`, `open`, `none`.
// `LOWLAT_FPS` asks the host for that rate through the application
// protocol once the first picture is in; `LOWLAT_PRESENT_HZ` caps how often
// a new picture is taken (the cached one is still drawn every refresh), so
// a stream faster than the presentation can be measured on one display;
// `LOWLAT_SECONDS` leaves cleanly after that long. `LOWLAT_RAW_AUDIO` asks
// the host for uncompressed sound; `LOWLAT_AUDIO_TRACE` prints a line per
// sound packet with its age and what the device held. Against a host on
// this same machine the sound must go to an output the host does not
// capture, or it is captured again and echoes: a null sink named through
// the sound server's own environment (`PIPEWIRE_NODE`) is one.
//
// Once a second a line goes to stdout with the presentation cadence as
// numbers rather than a judgement: presents and pictures in the second,
// repeats (a present with no new picture) and skips (pictures published and
// never shown, because a newer one had arrived), the decoder's figures, the
// reader's lag, and the process's resident set.

#include <inttypes.h>
#include <math.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "lowlat.h"
#include "matoya.h"
#include "keys.h"
#include "signaling.h"

struct demo {
	lowlat_client *client;
	struct signaling sig;
	char attempt[LOWLAT_ATTEMPT_MAX];
	MTY_App *app;
	MTY_Window window;
	bool begun;
	atomic_bool quit;

	// The picture on the screen, held until the next replaces it. The
	// presenting thread's; what the rest reads of it is published beside.
	lowlat_frame shown;
	bool showing;
	uint64_t last_sequence;
	atomic_uint picture_width;
	atomic_uint picture_height;
	atomic_uint picture_rotation;
	atomic_uint picture_format;
	pthread_t presenter;

	// Sound: the device, on a thread of its own that acquires and queues.
	// The device's own buffer paces playback and absorbs the drift; a
	// resync is the device flushing, seen here as its queue at zero after
	// it had been fed, or past the ceiling before a queue.
	MTY_Audio *audio;
	pthread_t listener;
	bool trace_audio;
	bool audio_playing;
	bool audio_just_started;
	atomic_uint snd_packets;
	atomic_uint snd_frames;
	atomic_uint snd_q_ms;
	atomic_uint snd_q_min;
	atomic_uint snd_q_max;
	atomic_uint snd_age_max;
	atomic_uint snd_resyncs;

	// The knobs: the rate asked of the host, the presentation cap, the leave.
	uint32_t ask_fps;
	bool asked;
	double poll_period_ms;
	double last_poll_ms;
	double leave_at_ms;

	// Where the picture is drawn: stretched to the window, or at its own
	// size when it fits. The rectangle last told to the library.
	atomic_bool stretch;
	int32_t viewport[4];

	// The host's pointer mode, and whether the chord let go of it.
	bool relative;
	bool released;

	// Switching the streamed output: the host's outputs and its current
	// configuration, both asked for on the chord and acted on together.
	MTY_JSON *outputs;
	MTY_JSON *config;

	// Pads are sent once per iteration, the latest state of each: the
	// toolkit reports on every axis event, which is several hundred a
	// second from a moving stick.
	struct {
		uint32_t id;
		lowlat_pad_state state;
		bool pending;
	} pads[8];
	uint32_t pad_events;
	uint32_t pad_sent;
	bool trace_pads;
	// What went to the library in the second, by kind, so the host's own
	// count of what it received can be read against this.
	uint32_t keys_sent;
	uint32_t buttons_sent;
	uint32_t wheels_sent;
	uint32_t motions_sent;

	// The second's figures; the presenting thread counts, the main thread
	// reads and clears.
	double second_began;
	// When the demo started, for the sound lines' timestamps.
	double started_ms;
	atomic_uint presents;
	atomic_uint polls;
	atomic_uint pictures;
	atomic_uint repeats;
	atomic_uint skips;
	uint64_t seconds;
	uint64_t last_video_bytes;
	bool established;
};

static double now_ms(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (double) ts.tv_sec * 1000.0 + (double) ts.tv_nsec / 1.0e6;
}

static uint64_t resident_mb(void)
{
	FILE *f = fopen("/proc/self/statm", "r");
	if (f == NULL)
		return 0;
	unsigned long size = 0, resident = 0;
	int n = fscanf(f, "%lu %lu", &size, &resident);
	fclose(f);
	if (n != 2)
		return 0;
	return (uint64_t) resident * (uint64_t) sysconf(_SC_PAGESIZE) / (1024 * 1024);
}

static void log_line(uint32_t level, const char *message, void *opaque)
{
	(void) opaque;
	fprintf(stderr, "%s%s\n", level <= LOWLAT_LOG_WARN ? "! " : "  ", message);
}

// Six random groups: the attempt identifier's shape.
static void attempt_id(char *out, size_t size)
{
	uint32_t words[6];
	MTY_GetRandomBytes(words, sizeof words);
	snprintf(out, size, "%08" PRIx32 "-%08" PRIx32 "-%08" PRIx32 "-%08" PRIx32 "-%08" PRIx32
		"-%08" PRIx32, words[0], words[1], words[2], words[3], words[4], words[5]);
}

// The toolkit's modifier bits, in the wire's numbering.
static uint32_t mods_of(MTY_Mod m)
{
	uint32_t out = 0;
	if (m & MTY_MOD_LSHIFT) out |= LOWLAT_MOD_LSHIFT;
	if (m & MTY_MOD_RSHIFT) out |= LOWLAT_MOD_RSHIFT;
	if (m & MTY_MOD_LCTRL)  out |= LOWLAT_MOD_LCTRL;
	if (m & MTY_MOD_RCTRL)  out |= LOWLAT_MOD_RCTRL;
	if (m & MTY_MOD_LALT)   out |= LOWLAT_MOD_LALT;
	if (m & MTY_MOD_RALT)   out |= LOWLAT_MOD_RALT;
	if (m & MTY_MOD_LWIN)   out |= LOWLAT_MOD_LGUI;
	if (m & MTY_MOD_RWIN)   out |= LOWLAT_MOD_RGUI;
	if (m & MTY_MOD_CAPS)   out |= LOWLAT_MOD_CAPS;
	if (m & MTY_MOD_NUM)    out |= LOWLAT_MOD_NUM;
	return out;
}

// What went to the library, by kind, so the host's own count of what it
// received can be read against this.
static void sent(struct demo *d, lowlat_status s, uint32_t *counter)
{
	if (s == LOWLAT_OK)
		(*counter)++;
}

// The host's pointer mode, as the toolkit is told it. The chord can let go
// of a captured pointer; the host's next transition takes it back.
static void apply_relative(struct demo *d)
{
	bool want = d->relative && !d->released;
	if (MTY_AppGetRelativeMouse(d->app) != want)
		MTY_AppSetRelativeMouse(d->app, want);
}

static void ask_outputs(struct demo *d);

static void on_key(struct demo *d, const MTY_KeyEvent *k)
{
	// The demo's own chords, never sent. The release that follows one is
	// sent and names a key the host never saw down, which it drops.
	if (k->pressed && (k->mod & (MTY_MOD_LCTRL | MTY_MOD_RCTRL))
		&& (k->mod & (MTY_MOD_LALT | MTY_MOD_RALT))) {
		switch (k->key) {
			case MTY_KEY_F:
				atomic_store(&d->stretch, !atomic_load(&d->stretch));
				printf("demo: %s\n", d->stretch ? "stretched to the window" : "at its own size");
				return;
			case MTY_KEY_R:
				d->released = !d->released;
				apply_relative(d);
				printf("demo: pointer %s\n", d->released ? "let go" : "taken");
				return;
			case MTY_KEY_O:
				ask_outputs(d);
				return;
			default:
				break;
		}
	}
	if (k->key == MTY_KEY_LWIN || k->key == MTY_KEY_RWIN)
		return;
	if (k->key >= MTY_KEY_MAX || KEY_USAGE[k->key] == 0)
		return;
	sent(d, lowlat_client_send_key(d->client, KEY_USAGE[k->key], mods_of(k->mod), k->pressed),
		&d->keys_sent);
}

static uint32_t button_of(MTY_Button b)
{
	switch (b) {
		case MTY_BUTTON_LEFT: return LOWLAT_MOUSE_LEFT;
		case MTY_BUTTON_MIDDLE: return LOWLAT_MOUSE_MIDDLE;
		case MTY_BUTTON_RIGHT: return LOWLAT_MOUSE_RIGHT;
		case MTY_BUTTON_X1: return LOWLAT_MOUSE_X1;
		case MTY_BUTTON_X2: return LOWLAT_MOUSE_X2;
		default: return 0;
	}
}

// An axis scaled from the range the device reports to the wire's: sticks
// signed sixteen bit, triggers a byte.
static int32_t scaled(const MTY_Axis *a, int32_t lo, int32_t hi)
{
	int32_t span = (int32_t) a->max - (int32_t) a->min;
	if (span <= 0)
		return 0;
	int64_t v = ((int64_t) a->value - a->min) * (hi - lo);
	return (int32_t) (lo + (v + span / 2) / span);
}

// A whole pad from the toolkit's report, kept as the latest state for the
// pad and sent on the next iteration. Axes are found by their usage rather
// than their slot, because the toolkit numbers slots in the order the device
// lists its axes; a pad's sticks are X, Y, Z and Rz and its triggers Rx and
// Ry on that page. The vertical axes are inverted: a pad's own protocol
// reports a stick pushed away as positive, a device reports it as negative.
static void on_controller(struct demo *d, const MTY_ControllerEvent *c)
{
	d->pad_events++;
	size_t slot = sizeof d->pads / sizeof d->pads[0];
	for (size_t i = 0; i < sizeof d->pads / sizeof d->pads[0]; i++) {
		if (d->pads[i].id == c->id)
			slot = i;
		else if (slot == sizeof d->pads / sizeof d->pads[0] && d->pads[i].id == 0)
			slot = i;
	}
	if (slot == sizeof d->pads / sizeof d->pads[0])
		return;
	lowlat_pad_state fresh;
	memset(&fresh, 0, sizeof fresh);
	fresh.size = (uint32_t) sizeof fresh;
	lowlat_pad_state *p = &fresh;
	static const struct { MTY_CButton from; uint16_t to; } BITS[] = {
		{MTY_CBUTTON_A, LOWLAT_PAD_STATE_A}, {MTY_CBUTTON_B, LOWLAT_PAD_STATE_B},
		{MTY_CBUTTON_X, LOWLAT_PAD_STATE_X}, {MTY_CBUTTON_Y, LOWLAT_PAD_STATE_Y},
		{MTY_CBUTTON_BACK, LOWLAT_PAD_STATE_BACK}, {MTY_CBUTTON_START, LOWLAT_PAD_STATE_START},
		{MTY_CBUTTON_LEFT_THUMB, LOWLAT_PAD_STATE_LSTICK},
		{MTY_CBUTTON_RIGHT_THUMB, LOWLAT_PAD_STATE_RSTICK},
		{MTY_CBUTTON_LEFT_SHOULDER, LOWLAT_PAD_STATE_LSHOULDER},
		{MTY_CBUTTON_RIGHT_SHOULDER, LOWLAT_PAD_STATE_RSHOULDER},
		{MTY_CBUTTON_GUIDE, LOWLAT_PAD_STATE_GUIDE},
		{MTY_CBUTTON_TOUCHPAD, LOWLAT_PAD_STATE_TOUCHPAD},
		{MTY_CBUTTON_DPAD_UP, LOWLAT_PAD_STATE_DPAD_UP},
		{MTY_CBUTTON_DPAD_DOWN, LOWLAT_PAD_STATE_DPAD_DOWN},
		{MTY_CBUTTON_DPAD_LEFT, LOWLAT_PAD_STATE_DPAD_LEFT},
		{MTY_CBUTTON_DPAD_RIGHT, LOWLAT_PAD_STATE_DPAD_RIGHT},
	};
	for (size_t i = 0; i < sizeof BITS / sizeof BITS[0]; i++)
		if (c->buttons[BITS[i].from])
			p->buttons |= BITS[i].to;
	for (uint8_t i = 0; i < c->numAxes && i < MTY_CAXIS_MAX; i++) {
		const MTY_Axis *a = &c->axes[i];
		switch (a->usage) {
			case 0x30: p->lx = (int16_t) scaled(a, -32768, 32767); break;
			case 0x31: p->ly = (int16_t) -scaled(a, -32767, 32767); break;
			case 0x32: p->rx = (int16_t) scaled(a, -32768, 32767); break;
			case 0x35: p->ry = (int16_t) -scaled(a, -32767, 32767); break;
			case 0x33: p->lt = (uint8_t) scaled(a, 0, 255); break;
			case 0x34: p->rt = (uint8_t) scaled(a, 0, 255); break;
			default: break;
		}
	}
	// A trigger pulled far enough is a button on some pads and only an axis
	// on others; the wire carries the axis.
	if (c->buttons[MTY_CBUTTON_LEFT_TRIGGER] && p->lt == 0)
		p->lt = 255;
	if (c->buttons[MTY_CBUTTON_RIGHT_TRIGGER] && p->rt == 0)
		p->rt = 255;
	d->pads[slot].id = c->id;
	d->pads[slot].state = fresh;
	d->pads[slot].pending = true;
	if (d->trace_pads) {
		printf("pad %u:", c->id);
		for (uint8_t i = 0; i < c->numAxes && i < MTY_CAXIS_MAX; i++)
			printf(" u%02x=%d[%d..%d]", c->axes[i].usage, c->axes[i].value, c->axes[i].min,
				c->axes[i].max);
		printf(" -> lx=%d ly=%d rx=%d ry=%d lt=%u rt=%u buttons=%04x\n", p->lx, p->ly, p->rx,
			p->ry, p->lt, p->rt, p->buttons);
	}
}

// The latest state of each pad that reported since the last iteration.
static void flush_pads(struct demo *d)
{
	for (size_t i = 0; i < sizeof d->pads / sizeof d->pads[0]; i++) {
		if (!d->pads[i].pending)
			continue;
		d->pads[i].pending = false;
		sent(d, lowlat_client_send_pad_state(d->client, d->pads[i].id, &d->pads[i].state),
			&d->pad_sent);
	}
}

static void event_func(const MTY_Event *evt, void *opaque)
{
	struct demo *d = opaque;
	switch (evt->type) {
		case MTY_EVENT_CLOSE:
		case MTY_EVENT_QUIT:
			atomic_store(&d->quit, true);
			break;
		case MTY_EVENT_KEY:
			on_key(d, &evt->key);
			break;
		case MTY_EVENT_BUTTON:
			if (button_of(evt->button.button) != 0)
				sent(d, lowlat_client_send_mouse_button(d->client, button_of(evt->button.button),
					evt->button.pressed, evt->button.x, evt->button.y), &d->buttons_sent);
			break;
		case MTY_EVENT_SCROLL:
			sent(d, lowlat_client_send_mouse_wheel(d->client, evt->scroll.x, evt->scroll.y),
				&d->wheels_sent);
			break;
		case MTY_EVENT_MOTION:
			sent(d, lowlat_client_send_mouse_motion(d->client, evt->motion.x, evt->motion.y,
				evt->motion.relative), &d->motions_sent);
			break;
		case MTY_EVENT_CONTROLLER:
			on_controller(d, &evt->controller);
			break;
		case MTY_EVENT_DISCONNECT:
			for (size_t i = 0; i < sizeof d->pads / sizeof d->pads[0]; i++)
				if (d->pads[i].id == evt->controller.id)
					memset(&d->pads[i], 0, sizeof d->pads[i]);
			lowlat_client_send_pad_unplug(d->client, evt->controller.id);
			break;
		case MTY_EVENT_FOCUS:
			// Nothing stays held on a host whose window is no longer in
			// front.
			if (!evt->focus)
				lowlat_client_send_release_all(d->client);
			apply_relative(d);
			break;
		default:
			break;
	}
}

// The rectangle the picture is drawn into, as the toolkit computes it:
// fitted to the window keeping its shape, or at its own size when it fits;
// centred either way. Told to the library when it changes.
static void place_picture(struct demo *d)
{
	int32_t rect[4] = {0, 0, 0, 0};
	uint32_t width = atomic_load(&d->picture_width);
	if (width != 0) {
		MTY_Size size = MTY_WindowGetSize(d->app, d->window);
		uint32_t height = atomic_load(&d->picture_height);
		uint32_t rotation = atomic_load(&d->picture_rotation);
		bool turned = rotation == LOWLAT_ROTATION_90 || rotation == LOWLAT_ROTATION_270;
		double w = turned ? height : width;
		double h = turned ? width : height;
		double ar = w / h;
		bool stretch = atomic_load(&d->stretch);
		double vw = stretch || w > size.w || h > size.h ? size.w : w;
		double vh = round(vw / ar);
		if (vw > size.w) {
			vw = size.w;
			vh = round(vw / ar);
		}
		if (vh > size.h) {
			vh = size.h;
			vw = round(vh * ar);
		}
		rect[0] = (int32_t) round((size.w - vw) / 2);
		rect[1] = (int32_t) round((size.h - vh) / 2);
		rect[2] = (int32_t) vw;
		rect[3] = (int32_t) vh;
	}
	if (memcmp(rect, d->viewport, sizeof rect) != 0) {
		memcpy(d->viewport, rect, sizeof rect);
		lowlat_client_set_viewport(d->client, rect[0], rect[1], rect[2], rect[3]);
	}
}

// The chord: ask the host what it can stream and what it streams now.
static void ask_outputs(struct demo *d)
{
	MTY_JSONDestroy(&d->outputs);
	MTY_JSONDestroy(&d->config);
	lowlat_client_send_user_data(d->client, 10, "", 0);
	lowlat_client_send_user_data(d->client, 9, "", 0);
}

// Both answers in: the output after the current one, in the host's own
// configuration sent back whole, because a host reads the element whole.
static void cycle_output(struct demo *d)
{
	if (d->outputs == NULL || d->config == NULL)
		return;
	const MTY_JSON *video = MTY_JSONObjGetItem(d->config, "video");
	const MTY_JSON *first = video != NULL ? MTY_JSONArrayGetItem(video, 0) : NULL;
	const char *current = first != NULL ? MTY_JSONStringPtr(MTY_JSONObjGetItem(first, "output")) : NULL;
	uint32_t n = MTY_JSONArrayGetLength(d->outputs);
	if (first == NULL || n == 0)
		return;
	uint32_t at = 0;
	for (uint32_t i = 0; i < n && current != NULL; i++) {
		const char *id = MTY_JSONStringPtr(MTY_JSONObjGetItem(MTY_JSONArrayGetItem(d->outputs, i), "id"));
		if (id != NULL && strcmp(id, current) == 0)
			at = (i + 1) % n;
	}
	const char *next = MTY_JSONStringPtr(MTY_JSONObjGetItem(MTY_JSONArrayGetItem(d->outputs, at), "id"));
	if (next == NULL)
		return;
	// The lookup above borrows from the object the set replaces, so the
	// name is copied first.
	char chosen[256];
	snprintf(chosen, sizeof chosen, "%s", next);
	MTY_JSONObjSetItem((MTY_JSON *) first, "output", MTY_JSONStringCreate(chosen));
	char *body = MTY_JSONSerialize(d->config);
	lowlat_status s = lowlat_client_send_user_data(d->client, 11, body, (uint32_t) strlen(body));
	printf("demo: asked for output %s%s\n", chosen, s == LOWLAT_OK ? "" : ", refused");
	MTY_Free(body);
	MTY_JSONDestroy(&d->outputs);
	MTY_JSONDestroy(&d->config);
}

// Forward what the library found to the host, and act on what ended.
static void pump_library(struct demo *d)
{
	static char body[65536];
	for (;;) {
		lowlat_event e;
		uint32_t body_len = sizeof body - 1;
		lowlat_status s = lowlat_client_poll_events(d->client, 0, &e, body, &body_len);
		if (s == LOWLAT_ERR_TOO_SMALL) {
			// Too long to be one of the two answers the demo reads; taken
			// off the queue and dropped.
			uint32_t none = 0;
			lowlat_client_poll_events(d->client, 0, &e, NULL, &none);
			continue;
		}
		if (s != LOWLAT_OK)
			break;
		switch (e.kind) {
			case LOWLAT_EVENT_USER_DATA:
				body[body_len] = '\0';
				if (e.body.user_data.id == 12) {
					MTY_JSONDestroy(&d->outputs);
					d->outputs = MTY_JSONParse(body);
					cycle_output(d);
				} else if (e.body.user_data.id == 11) {
					MTY_JSONDestroy(&d->config);
					d->config = MTY_JSONParse(body);
					cycle_output(d);
				}
				break;
			case LOWLAT_EVENT_RELATIVE:
				// The host took the pointer, or gave it back: on the way
				// out it reappears where the host says, once.
				d->relative = e.body.relative.relative;
				d->released = false;
				apply_relative(d);
				if (!d->relative && e.body.relative.x >= 0 && e.body.relative.y >= 0)
					MTY_WindowWarpCursor(d->app, d->window, (uint32_t) e.body.relative.x,
						(uint32_t) e.body.relative.y);
				printf("demo: pointer %s\n", d->relative ? "captured by the host" : "returned");
				break;
			case LOWLAT_EVENT_CANDIDATE:
				signaling_candidate(&d->sig, e.body.candidate.address, e.body.candidate.port,
					e.body.candidate.lan, e.body.candidate.from_stun, false);
				break;
			case LOWLAT_EVENT_READY:
				signaling_candidate(&d->sig, "0.0.0.0", 0, false, false, true);
				break;
			case LOWLAT_EVENT_ESTABLISHED:
				printf("demo: established with %s:%u\n", e.body.established.address,
					(unsigned) e.body.established.port);
				break;
			case LOWLAT_EVENT_ENDED:
				printf("demo: ended, outcome %d reason %d\n", (int) e.body.ended.outcome,
					(int) e.body.ended.reason);
				atomic_store(&d->quit, true);
				break;
			case LOWLAT_EVENT_BLOCKED:
				printf("demo: input %s\n", e.body.blocked.blocked ? "blocked" : "unblocked");
				break;
			case LOWLAT_EVENT_STREAM_ENDED:
				printf("demo: stream %u ended, status %d\n", (unsigned) e.body.stream_ended.stream,
					(int) e.body.stream_ended.status);
				break;
			default:
				break;
		}
	}
}

// What the host said through the service.
static void pump_signaling(struct demo *d)
{
	for (;;) {
		lowlat_credentials theirs;
		lowlat_candidate candidate;
		enum signaling_event e = signaling_poll(&d->sig, 0, &theirs, &candidate);
		if (e == SIGNALING_NOTHING)
			break;
		if (e == SIGNALING_CLOSED) {
			if (!d->begun)
				atomic_store(&d->quit, true);
			break;
		}
		if (e == SIGNALING_ANSWER && !d->begun) {
			lowlat_status s = lowlat_client_begin_p2p(d->client, d->attempt, &theirs);
			if (s != LOWLAT_OK) {
				fprintf(stderr, "demo: begin refused: %s\n", lowlat_status_string(s));
				atomic_store(&d->quit, true);
				break;
			}
			d->begun = true;
		}
		if (e == SIGNALING_CANDIDATE)
			lowlat_client_add_candidate(d->client, d->attempt, &candidate);
	}
}

// The rate is a change to a running stream, so it is asked for once the
// first picture proves there is one. The same message a settings panel
// sends; the library adds the terminator.
static void ask_rate(struct demo *d)
{
	char body[64];
	int n = snprintf(body, sizeof body, "{\"video\":[{\"encoderFPS\":%u}]}", d->ask_fps);
	lowlat_status s = lowlat_client_send_user_data(d->client, 11, body, (uint32_t) n);
	printf("demo: asked the host for %u fps%s\n", d->ask_fps,
		s == LOWLAT_OK ? "" : ", refused");
}

// Once a second: the figures on the log, and the same in the title bar so
// a session can be read at a glance.
static void report(struct demo *d)
{
	lowlat_client_status st;
	memset(&st, 0, sizeof st);
	st.size = (uint32_t) sizeof st;
	lowlat_client_get_status(d->client, &st);
	d->seconds++;
	double mbit = (double) (st.video_bytes - d->last_video_bytes) * 8.0 / 1.0e6;
	d->last_video_bytes = st.video_bytes;
	uint64_t rss = resident_mb();
	const char *codec = st.codec == LOWLAT_CODEC_HEVC ? "HEVC"
		: st.codec == LOWLAT_CODEC_H264 ? "H264" : "-";
	uint32_t presents = atomic_exchange(&d->presents, 0);
	uint32_t polls = atomic_exchange(&d->polls, 0);
	uint32_t pictures = atomic_exchange(&d->pictures, 0);
	uint32_t repeats = atomic_exchange(&d->repeats, 0);
	uint32_t skips = atomic_exchange(&d->skips, 0);
	uint32_t snd = atomic_exchange(&d->snd_packets, 0);
	uint32_t snd_frames = atomic_exchange(&d->snd_frames, 0);
	uint32_t snd_q_min = atomic_exchange(&d->snd_q_min, UINT32_MAX);
	uint32_t snd_q_max = atomic_exchange(&d->snd_q_max, 0);
	uint32_t snd_age_max = atomic_exchange(&d->snd_age_max, 0);
	if (snd == 0)
		snd_q_min = 0;
	printf("demo: t=%" PRIu64 " presents=%u polls=%u pictures=%u repeats=%u skips=%u "
		"codec=%s decode_us=%u readback_us=%u encode_us=%u queue=%u behind=%u behind_ms=%u "
		"rtt_ms=%u mbit=%.1f decoded=%" PRIu64 " rss_mb=%" PRIu64 " keys=%u btn=%u wheel=%u "
		"motion=%u pad=%u pad_events=%u input_dropped=%u "
		"snd=%u snd_frames=%u snd_q_ms=%u snd_q_min=%u snd_q_max=%u snd_age_ms=%u "
		"snd_queued=%u snd_dropped=%u snd_refused=%u snd_resync=%u snd_codec=%s\n",
		d->seconds, presents, polls, pictures, repeats, skips, codec,
		st.decode_us, st.readback_us, st.encode_us, st.queue_depth, st.behind, st.behind_ms,
		st.rtt_ms, mbit, st.decoded, rss, d->keys_sent, d->buttons_sent, d->wheels_sent,
		d->motions_sent, d->pad_sent, d->pad_events, st.input_dropped,
		snd, snd_frames, atomic_load(&d->snd_q_ms), snd_q_min, snd_q_max, snd_age_max,
		st.audio_queued, st.audio_dropped, st.audio_refused, atomic_load(&d->snd_resyncs),
		st.audio_codec == LOWLAT_AUDIO_OPUS ? "opus"
			: st.audio_codec == LOWLAT_AUDIO_PCM ? "pcm" : "-");
	d->keys_sent = 0;
	d->buttons_sent = 0;
	d->wheels_sent = 0;
	d->motions_sent = 0;
	d->pad_events = 0;
	d->pad_sent = 0;
	fflush(stdout);

	char title[256];
	uint32_t width = atomic_load(&d->picture_width);
	if (width != 0) {
		uint32_t rotation = atomic_load(&d->picture_rotation);
		snprintf(title, sizeof title,
			"lowlat | %ux%u %s %s%s | %s | %u fps | rtt %u ms | enc %.1f ms | dec %.1f ms | "
			"rb %.1f ms | q %u behind %u | skips %u | %.1f Mbit/s | snd %u ms | rss %" PRIu64
			" MB",
			width, atomic_load(&d->picture_height), codec,
			atomic_load(&d->picture_format) == LOWLAT_FORMAT_P010 ? "10bit" : "8bit",
			rotation == LOWLAT_ROTATION_90 ? " 90deg"
				: rotation == LOWLAT_ROTATION_180 ? " 180deg"
				: rotation == LOWLAT_ROTATION_270 ? " 270deg" : "",
			st.backend == LOWLAT_DECODER_OPEN ? "open CPU" : "no decoder",
			pictures, st.rtt_ms, (double) st.encode_us / 1000.0,
			(double) st.decode_us / 1000.0, (double) st.readback_us / 1000.0, st.queue_depth,
			st.behind, skips, mbit, atomic_load(&d->snd_q_ms), rss);
	} else {
		snprintf(title, sizeof title, "lowlat | %s",
			st.state == LOWLAT_CLIENT_ESTABLISHED ? "established, no picture yet"
			: st.state == LOWLAT_CLIENT_OVER ? "over" : "connecting...");
	}
	MTY_WindowSetTitle(d->app, d->window, title);
}

// The presenting thread: the toolkit's graphics context is created here and
// stays here, and the loop is paced by the display through vsync. The main
// thread is the toolkit's event loop and must not be: the pads are read one
// event per iteration of it, so a loop bound to the display's rate drains a
// stick slower than it moves.
static void *present_loop(void *opaque)
{
	struct demo *d = opaque;
	if (!MTY_WindowSetGFX(d->app, d->window, MTY_GFX_GL, true)) {
		fprintf(stderr, "demo: no graphics context\n");
		atomic_store(&d->quit, true);
		return NULL;
	}
	while (!atomic_load(&d->quit)) {
		double t = now_ms();
		// The poll: the newest picture, or nothing new. Under a cap it runs
		// on the first refresh at or past the cap's period (three quarters
		// of it, so a refresh a little early still counts), so the display
		// shows every refresh and the picture changes at the cap's cadence.
		if (d->poll_period_ms <= 0.0 || t - d->last_poll_ms >= d->poll_period_ms * 0.75) {
			d->last_poll_ms = t;
			atomic_fetch_add(&d->polls, 1);
			lowlat_frame fresh;
			memset(&fresh, 0, sizeof fresh);
			fresh.size = (uint32_t) sizeof fresh;
			lowlat_status s = lowlat_client_acquire_frame(d->client, 0, 0, &fresh);
			if (s == LOWLAT_OK) {
				if (d->showing)
					lowlat_client_release_frame(d->client, &d->shown, NULL);
				if (d->showing && fresh.sequence > d->last_sequence + 1)
					atomic_fetch_add(&d->skips, (uint32_t) (fresh.sequence - d->last_sequence - 1));
				d->last_sequence = fresh.sequence;
				d->shown = fresh;
				d->showing = true;
				atomic_store(&d->picture_height, fresh.height);
				atomic_store(&d->picture_rotation, fresh.rotation);
				atomic_store(&d->picture_format, fresh.format);
				atomic_store(&d->picture_width, fresh.width);
				atomic_fetch_add(&d->pictures, 1);
				if (d->ask_fps != 0 && !d->asked) {
					d->asked = true;
					ask_rate(d);
				}
			} else {
				atomic_fetch_add(&d->repeats, 1);
			}
		}

		// Drawn every iteration, new or not: a renderer that re-presents
		// the cached picture on every refresh is what keeps the window's
		// cadence the display's rather than the stream's.
		if (d->showing) {
			const lowlat_frame *f = &d->shown;
			uint32_t sample = f->format == LOWLAT_FORMAT_P010 ? 2 : 1;
			MTY_RenderDesc desc;
			memset(&desc, 0, sizeof desc);
			desc.format = f->format == LOWLAT_FORMAT_P010 ? MTY_COLOR_FORMAT_2PLANES_16
				: MTY_COLOR_FORMAT_2PLANES;
			desc.chroma = MTY_CHROMA_420;
			desc.filter = MTY_FILTER_LINEAR;
			// The toolkit takes one image with the planes in sequence and
			// the row length as a width; the second plane's offset is the
			// first's rows times that width, which is how the slot is laid
			// out.
			desc.imageWidth = f->planes[0].pitch / sample;
			desc.imageHeight = (uint32_t) ((f->planes[1].data - f->planes[0].data)
				/ f->planes[0].pitch);
			desc.cropWidth = f->width;
			desc.cropHeight = f->height;
			desc.rotation = f->rotation == LOWLAT_ROTATION_90 ? MTY_ROTATION_90
				: f->rotation == LOWLAT_ROTATION_180 ? MTY_ROTATION_180
				: f->rotation == LOWLAT_ROTATION_270 ? MTY_ROTATION_270 : MTY_ROTATION_NONE;
			desc.aspectRatio = (float) f->width / (float) f->height;
			// Fitted to the window, or at its own size when it fits.
			desc.scale = atomic_load(&d->stretch) ? 0.0f : 1.0f;
			desc.multiplyYUV = sample == 2;
			MTY_WindowDrawQuad(d->app, d->window, f->planes[0].data, &desc);
		} else {
			MTY_WindowClear(d->app, d->window, 0.0f, 0.0f, 0.0f, 1.0f);
		}
		MTY_WindowPresent(d->app, d->window);
		atomic_fetch_add(&d->presents, 1);
	}
	if (d->showing)
		lowlat_client_release_frame(d->client, &d->shown, NULL);
	MTY_WindowSetGFX(d->app, d->window, MTY_GFX_NONE, false);
	return NULL;
}

// The listening thread: one packet at a time from the library, straight
// onto the device. The wait is the library's, long, so the thread costs
// nothing between packets; without a device it still acquires, so the
// figures are there on a box with no sound.
static void *sound_loop(void *opaque)
{
	struct demo *d = opaque;
	static int16_t pcm[8000 * 2];
	while (!atomic_load(&d->quit)) {
		uint32_t count = 8000;
		lowlat_status s = lowlat_client_acquire_audio(d->client, 100, pcm, &count);
		if (s != LOWLAT_OK)
			continue;
		lowlat_client_status st;
		memset(&st, 0, sizeof st);
		st.size = (uint32_t) sizeof st;
		lowlat_client_get_status(d->client, &st);
		uint32_t queued = 0;
		if (d->audio != NULL) {
			queued = MTY_AudioGetQueued(d->audio);
			// The device's queue reads zero once more right after playback
			// (re)starts, before the device has reported anything; that
			// read is not a resync, and neither is the zero that follows a
			// flush this already counted. So a resync is counted while the
			// device is playing: the queue past the ceiling (the device
			// flushes on this call), or at zero (it ran dry), and then the
			// device is priming again until the floor is reached.
			const char *reason = NULL;
			if (d->audio_playing && !d->audio_just_started && queued == 0)
				reason = "empty";
			else if (d->audio_playing && queued > 150)
				reason = "over";
			if (reason != NULL) {
				uint32_t n = atomic_fetch_add(&d->snd_resyncs, 1) + 1;
				printf("demo: sound resync t=%.1f n=%u queued_ms=%u age_ms=%u reason=%s\n",
					(now_ms() - d->started_ms) / 1000.0, n, queued, st.audio_age_ms, reason);
				fflush(stdout);
				d->audio_playing = false;
			}
			d->audio_just_started = false;
			if (!d->audio_playing && queued + count / 48 >= 75) {
				d->audio_playing = true;
				d->audio_just_started = true;
			}
			MTY_AudioQueue(d->audio, pcm, count);
		}
		atomic_fetch_add(&d->snd_packets, 1);
		atomic_fetch_add(&d->snd_frames, count);
		atomic_store(&d->snd_q_ms, queued);
		if (queued < atomic_load(&d->snd_q_min))
			atomic_store(&d->snd_q_min, queued);
		if (queued > atomic_load(&d->snd_q_max))
			atomic_store(&d->snd_q_max, queued);
		if (st.audio_age_ms > atomic_load(&d->snd_age_max))
			atomic_store(&d->snd_age_max, st.audio_age_ms);
		if (d->trace_audio)
			printf("demo: snd t_ms=%.1f frames=%u age_ms=%u queued_ms=%u\n",
				now_ms() - d->started_ms, count, st.audio_age_ms, queued);
	}
	return NULL;
}

// The main thread: the toolkit's events, the two pumps, the pads, the
// rectangle and the figures, at the toolkit's own cadence.
static bool app_func(void *opaque)
{
	struct demo *d = opaque;
	pump_signaling(d);
	pump_library(d);
	double t = now_ms();
	if (d->leave_at_ms > 0.0 && t >= d->leave_at_ms) {
		printf("demo: leaving after %" PRIu64 " s\n", d->seconds);
		atomic_store(&d->quit, true);
	}
	if (atomic_load(&d->quit))
		return false;
	flush_pads(d);
	place_picture(d);
	if (t - d->second_began >= 1000.0) {
		d->second_began = t;
		report(d);
	}
	return true;
}

static const char *env_or(const char *name, const char *fallback)
{
	const char *v = getenv(name);
	return v != NULL && v[0] != '\0' ? v : fallback;
}

int main(void)
{
	const char *peer = getenv("LOWLAT_PEER");
	const char *session = getenv("LOWLAT_SESSION");
	if (peer == NULL || session == NULL) {
		fprintf(stderr, "demo: LOWLAT_PEER and LOWLAT_SESSION are required\n");
		return 2;
	}
	const char *server = env_or("LOWLAT_SERVER", "kessel-ws.parsec.app");
	const char *device = env_or("LOWLAT_DEVICE", "");
	const char *decoder = env_or("LOWLAT_DECODER", "auto");
	unsigned long ask_fps = strtoul(env_or("LOWLAT_FPS", "0"), NULL, 10);
	unsigned long present_hz = strtoul(env_or("LOWLAT_PRESENT_HZ", "0"), NULL, 10);
	unsigned long seconds = strtoul(env_or("LOWLAT_SECONDS", "0"), NULL, 10);

	if ((lowlat_features() & LOWLAT_FEATURE_CLIENT) == 0) {
		fprintf(stderr, "demo: this library carries no client half\n");
		return 2;
	}
	lowlat_set_log_callback(log_line, NULL);
	lowlat_set_log_level(LOWLAT_LOG_INFO);

	struct demo d;
	memset(&d, 0, sizeof d);
	d.ask_fps = (uint32_t) ask_fps;
	d.poll_period_ms = present_hz > 0 ? 1000.0 / (double) present_hz : 0.0;
	atomic_store(&d.stretch, true);
	d.trace_pads = getenv("LOWLAT_PAD_TRACE") != NULL;
	d.trace_audio = getenv("LOWLAT_AUDIO_TRACE") != NULL;
	atomic_store(&d.snd_q_min, UINT32_MAX);

	lowlat_client_create_info info;
	memset(&info, 0, sizeof info);
	info.size = (uint32_t) sizeof info;
	info.decoder = strcmp(decoder, "none") == 0 ? LOWLAT_DECODER_NONE
		: strcmp(decoder, "open") == 0 ? LOWLAT_DECODER_OPEN : LOWLAT_DECODER_AUTO;
	info.frame_kind = LOWLAT_FRAME_PLANES;
	snprintf(info.device, sizeof info.device, "%s", device);
	lowlat_status s = lowlat_client_create(&info, &d.client);
	if (s != LOWLAT_OK) {
		fprintf(stderr, "demo: no client: %s\n", lowlat_status_string(s));
		return 1;
	}
	printf("demo: rss_mb=%" PRIu64 " after creation\n", resident_mb());

	attempt_id(d.attempt, sizeof d.attempt);
	lowlat_credentials ours;
	memset(&ours, 0, sizeof ours);
	ours.size = (uint32_t) sizeof ours;
	// The attempt's configuration: the defaults, but for sound asked
	// uncompressed when the knob says so.
	lowlat_client_config cfg;
	memset(&cfg, 0, sizeof cfg);
	cfg.size = (uint32_t) sizeof cfg;
	cfg.raw_audio = getenv("LOWLAT_RAW_AUDIO") != NULL;
	s = lowlat_client_new_attempt(d.client, &cfg, d.attempt, LOWLAT_TRANSPORT_BUD, &ours);
	if (s != LOWLAT_OK) {
		fprintf(stderr, "demo: no attempt: %s\n", lowlat_status_string(s));
		return 1;
	}

	if (!signaling_connect(&d.sig, server, session, peer, d.attempt))
		return 1;
	if (!signaling_offer(&d.sig, &ours))
		return 1;
	printf("demo: offered attempt %s to %s\n", d.attempt, peer);

	d.app = MTY_AppCreate(0, app_func, event_func, &d);
	if (d.app == NULL) {
		fprintf(stderr, "demo: no app\n");
		return 1;
	}
	MTY_Frame frame = MTY_MakeDefaultFrame(0, 0, 1280, 720, 0.8f);
	d.window = MTY_WindowCreate(d.app, "lowlat", &frame, 0);
	if (d.window < 0) {
		fprintf(stderr, "demo: no window\n");
		return 1;
	}
	MTY_AppSetTimeout(d.app, 1);
	d.second_began = now_ms();
	d.started_ms = d.second_began;
	d.leave_at_ms = seconds > 0 ? d.second_began + (double) seconds * 1000.0 : 0.0;
	if (pthread_create(&d.presenter, NULL, present_loop, &d) != 0) {
		fprintf(stderr, "demo: no presenting thread\n");
		return 1;
	}
	// The device: stereo sixteen-bit at 48 kHz, 75 ms queued before it
	// plays and a flush past 150, which is the window a desktop client
	// runs. Without one the demo runs silent and still counts.
	MTY_AudioFormat format;
	memset(&format, 0, sizeof format);
	format.sampleFormat = MTY_AUDIO_SAMPLE_FORMAT_INT16;
	format.sampleRate = 48000;
	format.channels = 2;
	format.channelMask = MTY_AUDIO_CHANNEL_CFG_STEREO;
	d.audio = MTY_AudioCreate(format, 75, 150, NULL, true);
	if (d.audio == NULL)
		fprintf(stderr, "demo: no sound device, running silent\n");
	if (pthread_create(&d.listener, NULL, sound_loop, &d) != 0) {
		fprintf(stderr, "demo: no listening thread\n");
		return 1;
	}

	MTY_AppRun(d.app);

	atomic_store(&d.quit, true);
	pthread_join(d.presenter, NULL);
	pthread_join(d.listener, NULL);
	if (d.audio != NULL)
		MTY_AudioDestroy(&d.audio);
	lowlat_client_end_connection(d.client);
	signaling_close(&d.sig);
	lowlat_client_destroy(d.client);
	MTY_JSONDestroy(&d.outputs);
	MTY_JSONDestroy(&d.config);
	MTY_AppDestroy(&d.app);
	return 0;
}
