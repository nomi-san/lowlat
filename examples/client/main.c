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
// (and takes it again), Ctrl+Alt+O asks the host to stream its next output,
// Ctrl+Alt+C cycles the colour preferences mid-session.
// A bare Windows key is not sent, because the desktop here takes it and the
// host would be left with the modifier held; it reaches the host on chords.
//
// `LOWLAT_SERVER` names the signaling service (kessel-ws.parsec.app by
// default), `LOWLAT_DEVICE` a render node for the decoder (the first that
// decodes by default), `LOWLAT_DECODER` one of `auto`, `open`, `vendor`,
// `none`. The decoders this machine can open are printed at start, one
// row each, and `LOWLAT_DECODER_INDEX` picks a row by its number instead.
// `LOWLAT_HANDLE` asks for pictures as device handles, which the renderer
// imports and draws with no copy through this process; only a decoder that
// exports them (a row saying "handles") can be opened for that.
// `LOWLAT_HEVC`, `LOWLAT_10BIT` and `LOWLAT_444` are the preferences the
// attempt starts with: each is "prefer this if the host has it", masked by
// what the decoder takes before anything is declared; `LOWLAT_SWITCH_EVERY`
// walks them every that many seconds, as the chord does by hand.
// `LOWLAT_FPS` asks the host for that rate through the application
// protocol once the first picture is in; `LOWLAT_PRESENT_HZ` caps how often
// a new picture is taken (the cached one is still drawn every refresh), so
// a stream faster than the presentation can be measured on one display;
// `LOWLAT_SECONDS` leaves cleanly after that long; `LOWLAT_DUMP_FRAME` names
// a file the tenth picture's planes are written to, so what the renderer
// was handed can be looked at with another tool. `LOWLAT_RAW_AUDIO` asks
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
// reader's lag, and the process's resident set; then this side's figures
// for the video channel (fragments, late arrivals, negatives sent, the
// recent loss) beside the host's figures for this guest as its last guest
// list carried them (`h_*`), which are the two ends of one path.
//
// The host's pointer is drawn by the toolkit from the picture the library
// decodes, resampled with its hotspot by the ratio the picture is drawn at,
// so a pointer from a host at twice the scale shows at the size it has in
// the picture and shrinks with a letterboxed window; a pointer the host
// withholds for touch is hidden here too. A rumble goes to the pad the host
// named. The guest list is parsed here, with the toolkit's own reader.

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
#include "pads.h"
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
	// Whether pictures arrive as device handles, asked at creation.
	bool handles;
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
	// Cycle the preferences every so many seconds; zero for never.
	uint64_t switch_every;

	// Where the picture is drawn: stretched to the window, or at its own
	// size when it fits. The rectangle last told to the library.
	atomic_bool stretch;
	int32_t viewport[4];

	// The host's pointer mode, and whether the chord let go of it.
	bool relative;
	bool released;

	// The host's pointer picture, kept at its native size; the toolkit is
	// given it at the size it has in the drawn picture (the toolkit's own
	// cursor-size call is a no-op on this platform and on Windows, so the
	// resample is done here, as an established client does it), so a
	// pointer from a host at twice the scale shows at the size it has in
	// the picture and shrinks with a letterboxed window. The target last
	// applied says when to redo it.
	uint8_t *cursor_rgba;
	uint32_t cursor_width;
	uint32_t cursor_height;
	uint32_t cursor_hot_x;
	uint32_t cursor_hot_y;
	uint32_t cursor_checksum;
	uint32_t cursor_target_w;
	uint32_t cursor_target_h;
	bool cursor_suppressed;

	// The room as the host describes it: this client's number, its own
	// entry's owner flag and permissions, and the host's figures for it,
	// drawn on the line beside this side's own.
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
	uint32_t rumbles;

	// Switching the streamed output: the host's outputs and its current
	// configuration, both asked for on the chord and acted on together.
	MTY_JSON *outputs;
	MTY_JSON *config;

	// The picture preferences, cycled by the chord.
	lowlat_client_video_config video;

	// The reader's lag, sampled once a second: thirty or more messages
	// behind for sixty consecutive seconds is the warning every client
	// shows, and it clears the moment the lag drops under.
	unsigned behind_seconds;
	bool behind_warned;

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
	// The Sony pads read raw from their nodes when the knob says so; the
	// toolkit's events for them are dropped then, and with the knob at
	// "only", every controller the toolkit reports: on the host's own
	// machine, the virtual pads the host makes from this side's reports are
	// controllers to the toolkit, and would go back as states.
	bool raw_on;
	bool raw_only;
	struct raw_pads raw;
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

// The toolkit's own lines, which say when a draw could not be made: a
// picture that failed to import would otherwise leave the last one on the
// screen with every counter looking healthy.
static void toolkit_line(const char *message, void *opaque)
{
	(void) opaque;
	fprintf(stderr, "! toolkit: %s\n", message);
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

// The drawn size of the picture as shown (a quarter turn swaps its sides),
// or zero before there is one.
static void drawn_size(const struct demo *d, uint32_t *across, uint32_t *down)
{
	uint32_t width = atomic_load(&d->picture_width);
	uint32_t height = atomic_load(&d->picture_height);
	uint32_t rotation = atomic_load(&d->picture_rotation);
	bool turned = rotation == LOWLAT_ROTATION_90 || rotation == LOWLAT_ROTATION_270;
	*across = turned ? height : width;
	*down = turned ? width : height;
}

// Halve a picture: each output pixel the mean of its two-by-two block.
static void halve_rgba(const uint8_t *src, uint32_t sw, uint32_t sh, uint8_t *dst,
	uint32_t dw, uint32_t dh)
{
	for (uint32_t y = 0; y < dh; y++) {
		uint32_t y0 = y * 2, y1 = y0 + 1 < sh ? y0 + 1 : y0;
		for (uint32_t x = 0; x < dw; x++) {
			uint32_t x0 = x * 2, x1 = x0 + 1 < sw ? x0 + 1 : x0;
			const uint8_t *a = src + ((size_t) y0 * sw + x0) * 4;
			const uint8_t *b = src + ((size_t) y0 * sw + x1) * 4;
			const uint8_t *c = src + ((size_t) y1 * sw + x0) * 4;
			const uint8_t *e = src + ((size_t) y1 * sw + x1) * 4;
			uint8_t *q = dst + ((size_t) y * dw + x) * 4;
			for (int k = 0; k < 4; k++)
				q[k] = (uint8_t) ((a[k] + b[k] + c[k] + e[k] + 2) / 4);
		}
	}
}

// Resample bilinearly, in 16.16 fixed point, from a picture no more than
// twice the target.
static void bilinear_rgba(const uint8_t *src, uint32_t sw, uint32_t sh, uint8_t *dst,
	uint32_t dw, uint32_t dh)
{
	uint32_t step_x = dw > 1 ? ((sw - 1) << 16) / (dw - 1) : 0;
	uint32_t step_y = dh > 1 ? ((sh - 1) << 16) / (dh - 1) : 0;
	uint32_t fy = 0;
	for (uint32_t y = 0; y < dh; y++, fy += step_y) {
		uint32_t y0 = fy >> 16, y1 = y0 + 1 < sh ? y0 + 1 : sh - 1;
		uint32_t wy = (fy >> 8) & 0xFF;
		uint32_t fx = 0;
		for (uint32_t x = 0; x < dw; x++, fx += step_x) {
			uint32_t x0 = fx >> 16, x1 = x0 + 1 < sw ? x0 + 1 : sw - 1;
			uint32_t wx = (fx >> 8) & 0xFF;
			const uint8_t *p00 = src + ((size_t) y0 * sw + x0) * 4;
			const uint8_t *p01 = src + ((size_t) y0 * sw + x1) * 4;
			const uint8_t *p10 = src + ((size_t) y1 * sw + x0) * 4;
			const uint8_t *p11 = src + ((size_t) y1 * sw + x1) * 4;
			uint32_t w00 = (256 - wx) * (256 - wy), w01 = wx * (256 - wy);
			uint32_t w10 = (256 - wx) * wy, w11 = wx * wy;
			uint8_t *q = dst + ((size_t) y * dw + x) * 4;
			for (int k = 0; k < 4; k++)
				q[k] = (uint8_t) ((w00 * p00[k] + w01 * p01[k] + w10 * p10[k] + w11 * p11[k]
					+ 0x8000) >> 16);
		}
	}
}

// Give the toolkit the host's pointer at the size it has in the drawn
// picture, the rule an established client applies: the target is the
// picture's size times the drawn-to-stream ratio, per axis; a target within
// two pixels of the native size, or of exactly half of it, snaps there, so a
// picture a few percent smaller than the window keeps the native pointer
// untouched; anything else is halved by box averaging while it is still at
// least twice the target and then resampled bilinearly, which keeps a
// shrunk pointer crisp without aliasing. The hotspot scales with it. Skipped
// when neither the picture nor the target changed.
static void apply_cursor(struct demo *d)
{
	if (d->cursor_rgba == NULL)
		return;
	uint32_t across, down;
	drawn_size(d, &across, &down);
	uint32_t cw = d->cursor_width, ch = d->cursor_height;
	uint32_t tw = cw, th = ch;
	if (across != 0 && down != 0 && d->viewport[2] > 0 && d->viewport[3] > 0) {
		tw = (uint32_t) ((uint64_t) cw * (uint64_t) d->viewport[2] + across / 2) / across;
		th = (uint32_t) ((uint64_t) ch * (uint64_t) d->viewport[3] + down / 2) / down;
	}
	if (tw == 0) tw = 1;
	if (th == 0) th = 1;
	if (cw <= 127 && ch <= 127) {
		for (uint32_t div = 1; div <= 2; div++) {
			uint32_t sw = cw / div, sh = ch / div;
			if (sw == 0 || sh == 0)
				break;
			uint32_t dx = sw > tw ? sw - tw : tw - sw;
			uint32_t dy = sh > th ? sh - th : th - sh;
			if (dx <= 2 && dy <= 2) {
				tw = sw;
				th = sh;
				break;
			}
		}
	}
	if (tw == d->cursor_target_w && th == d->cursor_target_h)
		return;
	d->cursor_target_w = tw;
	d->cursor_target_h = th;
	uint32_t hot_x = (uint32_t) (((uint64_t) tw * d->cursor_hot_x + cw / 2) / cw);
	uint32_t hot_y = (uint32_t) (((uint64_t) th * d->cursor_hot_y + ch / 2) / ch);
	if (tw == cw && th == ch) {
		MTY_AppSetRGBACursor(d->app, d->cursor_rgba, cw, ch, d->cursor_hot_x, d->cursor_hot_y);
		return;
	}
	// The chain of halvings, then the bilinear step.
	uint8_t *cur = d->cursor_rgba;
	uint32_t sw = cw, sh = ch;
	while ((sw > sh ? sw : sh) >= 2 * tw && sw >= 2 && sh >= 2) {
		uint32_t hw = sw / 2, hh = sh / 2;
		uint8_t *next = malloc((size_t) hw * hh * 4);
		if (next == NULL)
			break;
		halve_rgba(cur, sw, sh, next, hw, hh);
		if (cur != d->cursor_rgba)
			free(cur);
		cur = next;
		sw = hw;
		sh = hh;
	}
	uint8_t *scaled = malloc((size_t) tw * th * 4);
	if (scaled != NULL) {
		if (sw == tw && sh == th)
			memcpy(scaled, cur, (size_t) tw * th * 4);
		else
			bilinear_rgba(cur, sw, sh, scaled, tw, th);
		MTY_AppSetRGBACursor(d->app, scaled, tw, th, hot_x, hot_y);
		free(scaled);
	}
	if (cur != d->cursor_rgba)
		free(cur);
}

// One cursor event: keep the picture if one came, then apply.
static void on_cursor(struct demo *d, const lowlat_cursor_event *c)
{
	if (c->image_update && c->image != NULL && c->image_len == (uint32_t) c->width * c->height * 4) {
		uint8_t *copy = malloc(c->image_len);
		if (copy != NULL) {
			memcpy(copy, c->image, c->image_len);
			free(d->cursor_rgba);
			d->cursor_rgba = copy;
			d->cursor_width = c->width;
			d->cursor_height = c->height;
			d->cursor_hot_x = c->hot_x;
			d->cursor_hot_y = c->hot_y;
			d->cursor_checksum = c->checksum;
			d->cursor_target_w = 0;
			d->cursor_target_h = 0;
		}
	}
	apply_cursor(d);
	// A pointer the host withholds for touch is not drawn here either; it
	// is not relative mode, which the relative event handles.
	if (c->suppressed != d->cursor_suppressed) {
		d->cursor_suppressed = c->suppressed;
		MTY_AppShowCursor(d->app, !c->suppressed);
	}
}

// The room: find this client's own entry by its number and keep what the
// host says about it -- owner, permissions, and its figures for this guest.
// A body that does not parse is dropped, as an established client drops it.
static void on_guest_list(struct demo *d, uint32_t number, const char *body)
{
	MTY_JSON *list = MTY_JSONParse(body);
	if (list == NULL) {
		printf("demo: guest list did not parse (%zu bytes)\n", strlen(body));
		return;
	}
	d->rosters++;
	d->number = number;
	uint32_t n = MTY_JSONArrayGetLength(list);
	for (uint32_t i = 0; i < n; i++) {
		const MTY_JSON *guest = MTY_JSONArrayGetItem(list, i);
		int32_t id = -1;
		if (!MTY_JSONInt32(MTY_JSONObjGetItem(guest, "id"), &id) || (uint32_t) id != number)
			continue;
		MTY_JSONObjGetBool(guest, "owner", &d->owner);
		const MTY_JSON *perms = MTY_JSONObjGetItem(guest, "perms");
		if (perms != NULL) {
			MTY_JSONObjGetBool(perms, "mouse", &d->perm_mouse);
			MTY_JSONObjGetBool(perms, "keyboard", &d->perm_keyboard);
			MTY_JSONObjGetBool(perms, "gamepad", &d->perm_gamepad);
		}
		const MTY_JSON *metrics = MTY_JSONObjGetItem(guest, "metrics");
		const MTY_JSON *video = metrics != NULL && MTY_JSONGetType(metrics) == MTY_JSON_ARRAY
			? MTY_JSONArrayGetItem(metrics, 0) : metrics;
		if (video != NULL) {
			MTY_JSONNumber(MTY_JSONObjGetItem(video, "networkLatency"), &d->host_rtt_ms);
			MTY_JSONNumber(MTY_JSONObjGetItem(video, "bitrate"), &d->host_mbps);
			MTY_JSONNumber(MTY_JSONObjGetItem(video, "encodeLatency"), &d->host_encode_ms);
			MTY_JSONNumber(MTY_JSONObjGetItem(video, "decodeLatency"), &d->host_decode_ms);
			MTY_JSONInt32(MTY_JSONObjGetItem(video, "packetsSent"), &d->host_packets);
			MTY_JSONInt32(MTY_JSONObjGetItem(video, "fastRTs"), &d->host_fast_rts);
			MTY_JSONInt32(MTY_JSONObjGetItem(video, "slowRTs"), &d->host_slow_rts);
			MTY_JSONInt32(MTY_JSONObjGetItem(video, "cgEvents"), &d->host_cg_events);
		}
		break;
	}
	if (d->rosters == 1)
		printf("demo: guest list: %u guests, this client is %u%s, mouse=%d keyboard=%d gamepad=%d\n",
			n, number, d->owner ? " (owner)" : "", d->perm_mouse, d->perm_keyboard,
			d->perm_gamepad);
	MTY_JSONDestroy(&list);
}

static const char *video_words(const lowlat_client_video_config *v)
{
	return !v->hevc && !v->ten_bit && !v->chroma_444 ? "h264"
		: v->ten_bit && v->chroma_444 ? "hevc 10bit 444"
		: v->ten_bit ? "hevc 10bit"
		: v->chroma_444 ? "hevc 444" : "hevc";
}

// The chord walks the preferences up the host's own order and back to the
// start: none, the second codec, ten-bit, full chroma, both.
static void cycle_video(struct demo *d)
{
	lowlat_client_video_config *v = &d->video;
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
	lowlat_status s = lowlat_client_set_video_config(d->client, v);
	printf("demo: asked %s: %s\n", video_words(v), lowlat_status_string(s));
}

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
			case MTY_KEY_C:
				cycle_video(d);
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
// Ry on that page. The toolkit hands every stick over as a signed sixteen-bit
// value with a stick pushed away from the player positive, which is the
// wire's own convention, so the values pass through as they are. (Until
// 2026-09-19 the vertical axes were negated here, on a misread trace, and a
// game on an established host looked the wrong way up.)
static void on_controller(struct demo *d, const MTY_ControllerEvent *c)
{
	if (d->raw_only || (d->raw_on && raw_pads_owns_vendor(&d->raw, c->vid)))
		return;
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
			case 0x31: p->ly = (int16_t) scaled(a, -32768, 32767); break;
			case 0x32: p->rx = (int16_t) scaled(a, -32768, 32767); break;
			case 0x35: p->ry = (int16_t) scaled(a, -32768, 32767); break;
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
		printf("pad %u (%04x:%04x):", c->id, c->vid, c->pid);
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
			if (d->raw_only || (d->raw_on && raw_pads_owns_vendor(&d->raw, evt->controller.vid)))
				break;
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
		// The pointer is drawn at the picture's ratio, so it follows.
		apply_cursor(d);
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
			case LOWLAT_EVENT_HOST_MODE:
				printf("demo: host mode %u\n", (unsigned) e.body.host_mode.mode);
				break;
			case LOWLAT_EVENT_CURSOR:
				on_cursor(d, &e.body.cursor);
				break;
			case LOWLAT_EVENT_RUMBLE: {
				// Eight bits each on the wire; the toolkit takes sixteen, and
				// a byte broadcast into both halves maps the ends exactly. A
				// pad read raw gets a motor-only report of its own instead.
				const lowlat_rumble_event *r = &e.body.rumble;
				d->rumbles++;
				if (d->raw_on && raw_pads_rumble(&d->raw, r->pad, r->large, r->small))
					break;
				MTY_AppRumbleController(d->app, r->pad,
					(uint16_t) (r->large | (r->large << 8)),
					(uint16_t) (r->small | (r->small << 8)));
				break;
			}
			case LOWLAT_EVENT_PAD_REPORT:
				if (!raw_pads_write(&d->raw, &e.body.pad_report))
					printf("demo: a write for pad %u, which is not read here\n",
						(unsigned) e.body.pad_report.pad);
				break;
			case LOWLAT_EVENT_GUEST_LIST:
				body[body_len] = '\0';
				on_guest_list(d, e.body.guest_list.number, body);
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
	d->established = st.state == LOWLAT_CLIENT_ESTABLISHED;
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
	// This side's figures for the video channel, and beside them the host's
	// for this guest as its last guest list carried them: the two ends of
	// one path, each measured where it can be.
	lowlat_client_metrics m;
	memset(&m, 0, sizeof m);
	m.size = (uint32_t) sizeof m;
	lowlat_client_get_metrics(d->client, &m);
	printf("demo: t=%" PRIu64 " presents=%u polls=%u pictures=%u repeats=%u skips=%u "
		"codec=%s decode_us=%u readback_us=%u encode_us=%u queue=%u behind=%u behind_ms=%u "
		"rtt_ms=%u mbit=%.1f decoded=%" PRIu64 " rss_mb=%" PRIu64 " keys=%u btn=%u wheel=%u "
		"motion=%u pad=%u pad_events=%u pad_raw=%u pad_out=%u pad_in=%u pad_in_dropped=%u "
		"input_dropped=%u "
		"snd=%u snd_frames=%u snd_q_ms=%u snd_q_min=%u snd_q_max=%u snd_age_ms=%u "
		"snd_queued=%u snd_dropped=%u snd_refused=%u snd_resync=%u snd_codec=%s "
		"reported_us=%u snd_reported_us=%u asked=%#x declared=%#x stream_format=%u "
		"frag=%" PRIu64 " late=%" PRIu64 " dup=%" PRIu64 " oow=%" PRIu64 " nacks=%" PRIu64
		" loss30=%.4f cursor=%u misses=%u refused=%u rumble=%u "
		"guest=%u owner=%d rosters=%u h_rtt=%.1f h_mbps=%.2f h_enc=%.2f h_dec=%.2f "
		"h_packets=%d h_fast=%d h_slow=%d h_cg=%d\n",
		d->seconds, presents, polls, pictures, repeats, skips, codec,
		st.decode_us, st.readback_us, st.encode_us, st.queue_depth, st.behind, st.behind_ms,
		st.rtt_ms, mbit, st.decoded, rss, d->keys_sent, d->buttons_sent, d->wheels_sent,
		d->motions_sent, d->pad_sent, d->pad_events, d->raw.reports, d->raw.outputs,
		st.pad_reports_received, st.pad_reports_dropped, st.input_dropped,
		snd, snd_frames, atomic_load(&d->snd_q_ms), snd_q_min, snd_q_max, snd_age_max,
		st.audio_queued, st.audio_dropped, st.audio_refused, atomic_load(&d->snd_resyncs),
		st.audio_codec == LOWLAT_AUDIO_OPUS ? "opus"
			: st.audio_codec == LOWLAT_AUDIO_PCM ? "pcm" : "-",
		st.decode_reported_us, st.audio_reported_us, st.asked_flags, st.declared_flags,
		st.stream_format,
		m.video.fragments, m.video.late, m.video.duplicates, m.video.out_of_window,
		m.video.nacks_sent, (double) m.video.loss_30s, st.cursor_images, st.cursor_misses,
		st.cursor_refused, d->rumbles, st.number, d->owner, d->rosters, d->host_rtt_ms,
		d->host_mbps, d->host_encode_ms, d->host_decode_ms, d->host_packets, d->host_fast_rts,
		d->host_slow_rts, d->host_cg_events);
	// The warning every client shows for hardware that cannot keep up,
	// gated on the reader's lag rather than on any figure of the decoder's:
	// thirty or more messages behind for sixty consecutive seconds, cleared
	// the moment it drops under.
	if (st.behind >= 30) {
		d->behind_seconds++;
		if (d->behind_seconds >= 60 && !d->behind_warned) {
			d->behind_warned = true;
			printf("demo: WARNING the host's resolution or rate is too high for this "
				"hardware to keep up (behind=%u for %u s)\n", st.behind, d->behind_seconds);
		}
	} else {
		if (d->behind_warned)
			printf("demo: the reader caught up\n");
		d->behind_seconds = 0;
		d->behind_warned = false;
	}
	d->keys_sent = 0;
	d->buttons_sent = 0;
	d->wheels_sent = 0;
	d->motions_sent = 0;
	d->pad_events = 0;
	d->pad_sent = 0;
	d->raw.reports = 0;
	d->raw.outputs = 0;
	fflush(stdout);

	char title[320];
	uint32_t width = atomic_load(&d->picture_width);
	if (width != 0) {
		uint32_t rotation = atomic_load(&d->picture_rotation);
		uint32_t format = atomic_load(&d->picture_format);
		const char *colour = format == LOWLAT_FORMAT_P010 ? "10bit"
			: format == LOWLAT_FORMAT_YUV444 ? "444"
			: format == LOWLAT_FORMAT_YUV444_16 ? "444 10bit" : "8bit";
		snprintf(title, sizeof title,
			"lowlat | %ux%u %s %s%s | asked %s | %s | %u fps | rtt %u/%.0f ms | enc %.1f ms | "
			"dec %.1f ms | rb %.1f ms | q %u behind %u | skips %u | %.1f/%.1f Mbit/s | "
			"loss %.2f%% | snd %u ms | rss %" PRIu64 " MB | guest %u%s%s%s%s%s",
			width, atomic_load(&d->picture_height), codec, colour,
			rotation == LOWLAT_ROTATION_90 ? " 90deg"
				: rotation == LOWLAT_ROTATION_180 ? " 180deg"
				: rotation == LOWLAT_ROTATION_270 ? " 270deg" : "",
			video_words(&d->video),
			st.backend == LOWLAT_DECODER_OPEN ? "open planes"
				: st.backend == LOWLAT_DECODER_VENDOR ? (d->handles ? "vendor handles" : "vendor planes")
				: "no decoder",
			pictures, st.rtt_ms, d->host_rtt_ms, (double) st.encode_us / 1000.0,
			(double) st.decode_us / 1000.0, (double) st.readback_us / 1000.0, st.queue_depth,
			st.behind, skips, mbit, d->host_mbps, (double) m.video.loss_30s * 100.0,
			atomic_load(&d->snd_q_ms), rss, st.number, d->owner ? " owner" : "",
			d->perm_mouse ? " m" : "", d->perm_keyboard ? " k" : "", d->perm_gamepad ? " g" : "",
			d->behind_warned ? " | CANNOT KEEP UP" : "");
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
// `LOWLAT_DUMP_FRAME=<path>` writes the tenth picture's planes, each row at
// its own width with the pitch's padding left out, so the bytes a renderer
// is handed can be looked at with another tool.
static void dump_once(struct demo *d, const lowlat_frame *f)
{
	static unsigned seen;
	const char *path = getenv("LOWLAT_DUMP_FRAME");
	if (path == NULL || ++seen != 10)
		return;
	(void) d;
	bool deep = f->format == LOWLAT_FORMAT_P010 || f->format == LOWLAT_FORMAT_YUV444_16;
	bool full = f->format == LOWLAT_FORMAT_YUV444 || f->format == LOWLAT_FORMAT_YUV444_16;
	uint32_t row = f->width * (deep ? 2 : 1);
	FILE *out = fopen(path, "wb");
	if (out == NULL)
		return;
	for (uint32_t p = 0; p < 3; p++) {
		if (f->planes[p].data == NULL)
			continue;
		uint32_t rows = p == 0 || full ? f->height : f->height / 2;
		for (uint32_t r = 0; r < rows; r++)
			fwrite(f->planes[p].data + (size_t) r * f->planes[p].pitch, 1, row, out);
	}
	fclose(out);
	printf("demo: dumped picture %ux%u format=%u to %s\n", f->width, f->height, f->format, path);
}

static void *present_loop(void *opaque)
{
	struct demo *d = opaque;
	if (!MTY_WindowSetGFX(d->app, d->window, MTY_GFX_GL, true)) {
		fprintf(stderr, "demo: no graphics context\n");
		atomic_store(&d->quit, true);
		return NULL;
	}
	// A handle is only drawable on a context that imports one: asked once,
	// before any picture, rather than found out per frame.
	if (d->handles && !MTY_WindowIsValidHardwareFrame(d->app, d->window, NULL, NULL)) {
		fprintf(stderr, "demo: this graphics context does not import device handles\n");
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
				dump_once(d, &fresh);
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
			bool deep = f->format == LOWLAT_FORMAT_P010 || f->format == LOWLAT_FORMAT_YUV444_16;
			bool full = f->format == LOWLAT_FORMAT_YUV444 || f->format == LOWLAT_FORMAT_YUV444_16;
			uint32_t sample = deep ? 2 : 1;
			MTY_RenderDesc desc;
			memset(&desc, 0, sizeof desc);
			desc.format = full ? (deep ? MTY_COLOR_FORMAT_3PLANES_16 : MTY_COLOR_FORMAT_3PLANES)
				: (deep ? MTY_COLOR_FORMAT_2PLANES_16 : MTY_COLOR_FORMAT_2PLANES);
			desc.chroma = full ? MTY_CHROMA_444 : MTY_CHROMA_420;
			desc.filter = MTY_FILTER_LINEAR;
			// A handle is drawn from the toolkit's import of it, at each
			// plane's offset and pitch; planes are one image with the
			// planes in sequence and the row length as a width, each
			// plane's offset being the rows before it times that width,
			// which is how the slot is laid out.
			MTY_HardwareFrame hw;
			memset(&hw, 0, sizeof hw);
			if (f->kind == LOWLAT_FRAME_HANDLE) {
				hw.fd = f->fd;
				hw.id = f->allocation;
				hw.size = f->handle_size;
				for (uint32_t p = 0; p < 3; p++) {
					hw.offset[p] = f->planes[p].offset;
					hw.pitch[p] = f->planes[p].pitch;
				}
				desc.hardware = true;
				desc.imageWidth = f->width;
				desc.imageHeight = f->height;
			} else {
				desc.imageWidth = f->planes[0].pitch / sample;
				desc.imageHeight = (uint32_t) ((f->planes[1].data - f->planes[0].data)
					/ f->planes[0].pitch);
			}
			desc.cropWidth = f->width;
			desc.cropHeight = f->height;
			desc.rotation = f->rotation == LOWLAT_ROTATION_90 ? MTY_ROTATION_90
				: f->rotation == LOWLAT_ROTATION_180 ? MTY_ROTATION_180
				: f->rotation == LOWLAT_ROTATION_270 ? MTY_ROTATION_270 : MTY_ROTATION_NONE;
			desc.aspectRatio = (float) f->width / (float) f->height;
			// Fitted to the window, or at its own size when it fits.
			desc.scale = atomic_load(&d->stretch) ? 0.0f : 1.0f;
			// Ten-bit samples arrive in the high bits of sixteen, which is
			// already the scale a sixteen-bit texture normalises; the
			// toolkit's multiply is for samples in the low bits, and applied
			// here it saturates the chroma into a uniform magenta.
			desc.multiplyYUV = false;
			MTY_WindowDrawQuad(d->app, d->window,
				f->kind == LOWLAT_FRAME_HANDLE ? (const void *) &hw : (const void *) f->planes[0].data,
				&desc);
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
	if (d->raw_on) {
		raw_pads_scan(&d->raw, d->client, t, d->established);
		raw_pads_pump(&d->raw, d->client);
	}
	flush_pads(d);
	place_picture(d);
	if (t - d->second_began >= 1000.0) {
		d->second_began = t;
		report(d);
		// The timed walk through the preferences, the chord's without a
		// hand on the keyboard: once the session is up, every so many
		// seconds.
		if (d->switch_every > 0 && d->established && d->seconds % d->switch_every == 0)
			cycle_video(d);
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
	unsigned long switch_every = strtoul(env_or("LOWLAT_SWITCH_EVERY", "0"), NULL, 10);

	if ((lowlat_features() & LOWLAT_FEATURE_CLIENT) == 0) {
		fprintf(stderr, "demo: this library carries no client half\n");
		return 2;
	}
	lowlat_set_log_callback(log_line, NULL);
	MTY_SetLogFunc(toolkit_line, NULL);
	lowlat_set_log_level(LOWLAT_LOG_INFO);

	struct demo d;
	memset(&d, 0, sizeof d);
	d.ask_fps = (uint32_t) ask_fps;
	d.switch_every = switch_every;
	d.poll_period_ms = present_hz > 0 ? 1000.0 / (double) present_hz : 0.0;
	atomic_store(&d.stretch, true);
	d.trace_pads = getenv("LOWLAT_PAD_TRACE") != NULL;
	d.trace_audio = getenv("LOWLAT_AUDIO_TRACE") != NULL;
	// A DualShock 4 or a DualSense sent as its own report, which an
	// established host takes only in a mode its owner set; this library's
	// hosts take it as it is. The peer's kind is the application's to know,
	// and this one is told.
	d.raw_on = getenv("LOWLAT_PAD_RAW") != NULL;
	d.raw_only = d.raw_on && strcmp(getenv("LOWLAT_PAD_RAW"), "only") == 0;
	raw_pads_init(&d.raw, d.trace_pads);
	atomic_store(&d.snd_q_min, UINT32_MAX);

	lowlat_client_create_info info;
	memset(&info, 0, sizeof info);
	info.size = (uint32_t) sizeof info;
	info.decoder = strcmp(decoder, "none") == 0 ? LOWLAT_DECODER_NONE
		: strcmp(decoder, "open") == 0 ? LOWLAT_DECODER_OPEN
		: strcmp(decoder, "vendor") == 0 ? LOWLAT_DECODER_VENDOR : LOWLAT_DECODER_AUTO;
	// Pictures as device handles the renderer imports, on a decoder that
	// exports them; the decoder is then the vendor's whatever was asked.
	d.handles = getenv("LOWLAT_HANDLE") != NULL;
	info.frame_kind = d.handles ? LOWLAT_FRAME_HANDLE : LOWLAT_FRAME_PLANES;
	snprintf(info.device, sizeof info.device, "%s", device);

	// What this machine can open, one row each; a row picked by number
	// names the decoder and the device for creation.
	const char *pick = getenv("LOWLAT_DECODER_INDEX");
	lowlat_decoder_info row;
	memset(&row, 0, sizeof row);
	row.size = (uint32_t) sizeof row;
	for (uint32_t i = 0; lowlat_enum_decoders(i, &row); i++) {
		printf("demo: decoder [%u] %s on %s: h264 %ux%u, hevc %ux%u%s%s%s, %s\n",
			row.index, row.name, row.device[0] ? row.device : "any device",
			row.max_width_h264, row.max_height_h264,
			row.max_width_hevc, row.max_height_hevc,
			row.hevc_10 ? ", 10-bit" : "",
			row.hevc_444 ? ", 4:4:4" : "",
			row.hevc_444_10 ? ", 4:4:4 10-bit" : "",
			row.handle ? "handles" : "planes only");
		if (pick != NULL && strtoul(pick, NULL, 10) == row.index) {
			info.decoder = row.decoder;
			snprintf(info.device, sizeof info.device, "%s", row.device);
		}
	}
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
	d.video.hevc = getenv("LOWLAT_HEVC") != NULL;
	d.video.ten_bit = getenv("LOWLAT_10BIT") != NULL;
	d.video.chroma_444 = getenv("LOWLAT_444") != NULL;
	cfg.video = d.video;
	printf("demo: asking %s\n", video_words(&d.video));
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
	raw_pads_close(&d.raw, d.client);
	lowlat_client_end_connection(d.client);
	signaling_close(&d.sig);
	lowlat_client_destroy(d.client);
	MTY_JSONDestroy(&d.outputs);
	MTY_JSONDestroy(&d.config);
	MTY_AppDestroy(&d.app);
	return 0;
}
