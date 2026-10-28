// The pads. See pads.h.

#include "pads.h"

#include <stdio.h>
#include <string.h>

#include <SDL3/SDL.h>

#include "app.h"
#include "config.h"
#include "session.h"

namespace pads {

#define VENDOR_SONY 0x054C

// Which product a Sony pad is; zero for one the report path does not carry.
static uint32_t type_of(unsigned product)
{
	switch (product) {
		case 0x05C4:
		case 0x09CC: return LOWLAT_PAD_TYPE_DS4;
		case 0x0CE6: return LOWLAT_PAD_TYPE_DS5;
		default: return 0;
	}
}

// Whether a device is one of the pads a host on this same machine made from
// this demo's own: sending it back would make more of them. Such a pad names
// its location as the host's. Linux's alone: no host of this library makes
// pads elsewhere.
static bool hosts_own(const char *path)
{
#if defined(__linux__)
	unsigned node;
	char file[96];
	if (sscanf(path, "/dev/hidraw%u", &node) == 1)
		snprintf(file, sizeof file, "/sys/class/hidraw/hidraw%u/device/uevent", node);
	else if (sscanf(path, "/dev/input/event%u", &node) == 1)
		snprintf(file, sizeof file, "/sys/class/input/event%u/device/phys", node);
	else
		return false;
	FILE *f = fopen(file, "r");
	if (f == NULL)
		return false;
	bool own = false;
	char line[160];
	while (fgets(line, sizeof line, f) != NULL)
		if (strncmp(line, "HID_PHYS=lowlat/", 16) == 0 || strncmp(line, "lowlat/", 7) == 0)
			own = true;
	fclose(f);
	return own;
#else
	(void) path;
	return false;
#endif
}

// The Sony pads read raw through SDL's HID layer, while the knob says so.

#define RAW_PADS 4
// The raw pads are named to the library apart from the gamepads, so a rumble
// message can be told to the right side, and **below 256**: an established
// host keys its pads on the low eight bits of the identifier for the state,
// button, axis and unplug messages and on the whole identifier for the report
// message, so a report under a wider identifier never reaches the pad the
// states made (docs/01-protocol.md 11.1), and its rumble comes back under the
// eight-bit one.
#define RAW_ID_BASE 200

struct raw_pad {
	SDL_hid_device *dev;       // NULL when the slot is empty
	char path[256];            // where it was opened, to know it again
	uint32_t id;               // as named to the library
	uint32_t type;             // one of lowlat_pad_type
	bool wireless;             // the wireless framing on the device
	bool features_due;         // its feature reports go ahead of its next input report
	uint8_t seq;               // a wireless DualSense's output sequence
};

static raw_pad _raw[RAW_PADS];
// The system's device list as it was when the pads were last looked for.
static Uint32 _raw_seen;
static bool _raw_scanned;
static bool _was_established;
// The second's counts: input reports sent, and the host's writes put on the
// pads.
static uint32_t _raw_reports;
static uint32_t _raw_outputs;

// The two feature reports the host's driver asks for, per product and
// transport: calibration and firmware, identifier and length.
struct feature {
	uint8_t id;
	uint32_t len;
};

static void features_of(uint32_t type, bool wireless, feature out[2])
{
	if (type == LOWLAT_PAD_TYPE_DS4) {
		out[0] = feature{(uint8_t) (wireless ? 0x05 : 0x02), wireless ? 41u : 37u};
		out[1] = feature{0xA3, 49};
	} else {
		out[0] = feature{0x05, 41};
		out[1] = feature{0x20, 64};
	}
}

static raw_pad *raw_by_id(uint32_t id)
{
	for (size_t i = 0; i < RAW_PADS; i++)
		if (_raw[i].dev != NULL && _raw[i].id == id)
			return &_raw[i];
	return NULL;
}

static void drop(raw_pad *p)
{
	printf("demo: pad %u gone\n", p->id);
	SDL_hid_close(p->dev);
	p->dev = NULL;
	lowlat_client *client = session::client();
	if (client != NULL)
		lowlat_client_send_pad_unplug(client, p->id);
}

// The feature reports, read from the pad and sent ahead of its first input
// report in each session: the host's driver asks for them when the device
// appears, and the library keeps none from one session to the next. One the
// pad does not answer is left to the host's default.
static void send_features(raw_pad *p)
{
	feature features[2];
	features_of(p->type, p->wireless, features);
	for (size_t i = 0; i < 2; i++) {
		uint8_t buf[64];
		memset(buf, 0, sizeof buf);
		buf[0] = features[i].id;
		int got = SDL_hid_get_feature_report(p->dev, buf, features[i].len);
		if (got <= 0) {
			printf("demo: pad %u feature 0x%02x not answered (%s)\n", p->id, features[i].id,
				SDL_GetError());
			continue;
		}
		lowlat_status s = lowlat_client_send_pad_report(session::client(), p->id, p->type,
			LOWLAT_PAD_REPORT_FEATURE, buf, (uint32_t) got);
		if (config::get().trace_pads || s != LOWLAT_OK)
			printf("demo: pad %u feature 0x%02x %d bytes -> %s\n", p->id, features[i].id, got,
				lowlat_status_string(s));
	}
}

// Look for pads that appeared, whenever the system's device list changed,
// once the session is up: a report before that has no session to travel on.
static void raw_scan(bool established)
{
	if (!established)
		return;
	Uint32 seen = SDL_hid_device_change_count();
	if (_raw_scanned && seen == _raw_seen)
		return;
	_raw_scanned = true;
	_raw_seen = seen;

	SDL_hid_device_info *list = SDL_hid_enumerate(VENDOR_SONY, 0);
	for (SDL_hid_device_info *d = list; d != NULL; d = d->next) {
		uint32_t type = type_of(d->product_id);
		bool usb = d->bus_type == SDL_HID_API_BUS_USB;
		bool bluetooth = d->bus_type == SDL_HID_API_BUS_BLUETOOTH;
		if (type == 0 || (!usb && !bluetooth) || hosts_own(d->path))
			continue;
		bool open_already = false;
		for (size_t i = 0; i < RAW_PADS; i++)
			if (_raw[i].dev != NULL && strcmp(_raw[i].path, d->path) == 0)
				open_already = true;
		if (open_already)
			continue;
		raw_pad *slot = NULL;
		for (size_t i = 0; i < RAW_PADS && slot == NULL; i++)
			if (_raw[i].dev == NULL)
				slot = &_raw[i];
		if (slot == NULL)
			break;
		SDL_hid_device *dev = SDL_hid_open_path(d->path);
		if (dev == NULL) {
			// Once, not at every change: on Linux the seat's access rule is
			// what grants the node, and a node without it stays without it.
			static bool said;
			if (!said)
				printf("demo: %s not opened (%s)\n", d->path, SDL_GetError());
			said = true;
			continue;
		}
		*slot = raw_pad{
			.dev = dev,
			.path = {},
			.id = RAW_ID_BASE + (uint32_t) (slot - _raw),
			.type = type,
			.wireless = bluetooth,
			.features_due = true,
			.seq = 0,
		};
		snprintf(slot->path, sizeof slot->path, "%s", d->path);
		printf("demo: pad %u is %s %04x over %s, read raw\n", slot->id,
			type == LOWLAT_PAD_TYPE_DS4 ? "DualShock 4" : "DualSense", d->product_id,
			bluetooth ? "Bluetooth" : "USB");
	}
	SDL_hid_free_enumeration(list);
}

// Every report the pads have queued: to the library once the session is up,
// the feature reports first; read and dropped before, so a session starts
// from the pad's present rather than its past.
static void raw_pump(bool established)
{
	for (size_t i = 0; i < RAW_PADS; i++) {
		raw_pad *p = &_raw[i];
		if (p->dev == NULL)
			continue;
		if (established && p->features_due) {
			p->features_due = false;
			send_features(p);
		}
		// Everything queued, not one report a pass: a wired pad reports 250
		// times a second.
		for (;;) {
			uint8_t buf[1024];
			int n = SDL_hid_read_timeout(p->dev, buf, sizeof buf, 0);
			if (n == 0)
				break;
			if (n < 0) {
				drop(p);
				break;
			}
			if (!established)
				continue;
			// A wireless report is its first 78 bytes, whatever length the
			// system hands it out at.
			uint32_t len = (uint32_t) n;
			if (p->wireless && len > LOWLAT_PAD_REPORT_MAX)
				len = LOWLAT_PAD_REPORT_MAX;
			lowlat_status s = lowlat_client_send_pad_report(session::client(), p->id, p->type,
				LOWLAT_PAD_REPORT_INPUT, buf, len);
			if (s == LOWLAT_OK)
				_raw_reports++;
			else if (config::get().trace_pads)
				printf("demo: pad %u report %u bytes id 0x%02x -> %s\n", p->id, len, buf[0],
					lowlat_status_string(s));
		}
	}
}

// The host's write, back to the pad it names. False when the pad is not one
// read here.
static bool raw_write(const lowlat_pad_report_event *e)
{
	raw_pad *p = raw_by_id(e->pad);
	if (p == NULL)
		return false;
	int done;
	if (e->kind == LOWLAT_PAD_REPORT_FEATURE)
		done = SDL_hid_send_feature_report(p->dev, e->report, e->len);
	else
		done = SDL_hid_write(p->dev, e->report, e->len);
	if (done < 0)
		printf("demo: pad %u write of %u bytes failed (%s)\n", p->id, (unsigned) e->len,
			SDL_GetError());
	else
		_raw_outputs++;
	if (config::get().trace_pads) {
		// The head of the report, where the flags and the motors are.
		printf("demo: pad %u <- %s %u bytes:", p->id,
			e->kind == LOWLAT_PAD_REPORT_FEATURE ? "feature" : "output", (unsigned) e->len);
		for (uint32_t i = 0; i < e->len && i < 12; i++)
			printf(" %02x", e->report[i]);
		printf("\n");
	}
	return true;
}

// The wireless checksum: the direction's seed, then everything before the
// last four bytes, little endian at the end.
static void seal(uint8_t *report, size_t len)
{
	uint8_t seed = 0xA2;
	Uint32 crc = SDL_crc32(0, &seed, 1);
	crc = SDL_crc32(crc, report, len - 4);
	memcpy(report + len - 4, &crc, 4);
}

// The host's rumble message for a pad read here: a motor-only report, the
// lights untouched. False when the pad is not one read here.
static bool raw_rumble(uint32_t pad, uint8_t low, uint8_t high)
{
	raw_pad *p = raw_by_id(pad);
	if (p == NULL)
		return false;
	// The motors alone: the flags name them and nothing else, so a lightbar
	// the host set stays as it is.
	uint8_t report[78];
	memset(report, 0, sizeof report);
	size_t len;
	if (p->type == LOWLAT_PAD_TYPE_DS4) {
		size_t at = p->wireless ? 3 : 1;
		report[0] = p->wireless ? 0x11 : 0x05;
		if (p->wireless)
			report[1] = 0xC0;
		report[at] = 0x01;         // motors valid
		report[at + 3] = high;     // right, the small one
		report[at + 4] = low;      // left, the large one
		len = p->wireless ? 78 : 32;
	} else {
		size_t at = p->wireless ? 3 : 1;
		report[0] = p->wireless ? 0x31 : 0x02;
		if (p->wireless) {
			// The sequence is the library's for its own writes; this one
			// interleaves with it, which the pad tolerates.
			report[1] = (uint8_t) (p->seq << 4);
			report[2] = 0x10;
			p->seq = (uint8_t) ((p->seq + 1) & 0x0F);
		}
		report[at] = 0x03;         // both motors, the compatible vibration
		report[at + 2] = high;     // right
		report[at + 3] = low;      // left
		len = p->wireless ? 78 : 48;
	}
	if (p->wireless)
		seal(report, len);
	if (SDL_hid_write(p->dev, report, len) < 0)
		printf("demo: pad %u rumble write failed (%s)\n", p->id, SDL_GetError());
	else
		_raw_outputs++;
	return true;
}

// The gamepads SDL reports, sent as the sixteen-button pad: the latest state
// of each that changed, once a pass, since SDL reports every axis change --
// several hundred a second from a moving stick.

#define PADS 8

struct gamepad {
	SDL_Gamepad *pad;          // NULL when the slot is empty
	SDL_JoystickID which;      // SDL's name for it
	uint32_t id;               // as named to the library: the slot's number
	bool pending;              // changed since it was last sent
	bool has_last;             // `last` is what this session's host was sent
	lowlat_pad_state last;     // the state sent last
};

static gamepad _pads[PADS];
static uint32_t _events;
static uint32_t _sent;
static uint32_t _rumbles;

static gamepad *by_which(SDL_JoystickID which)
{
	for (size_t i = 0; i < PADS; i++)
		if (_pads[i].pad != NULL && _pads[i].which == which)
			return &_pads[i];
	return NULL;
}

static gamepad *by_id(uint32_t id)
{
	for (size_t i = 0; i < PADS; i++)
		if (_pads[i].pad != NULL && _pads[i].id == id)
			return &_pads[i];
	return NULL;
}

// Whether a gamepad is one this side leaves alone: every one with the knob at
// "only", a Sony pad read raw instead -- never opened as a gamepad, since
// that makes SDL write its own lights on it -- and one the host on this
// machine made.
static bool left_alone(SDL_JoystickID which)
{
	const config::Settings &knobs = config::get();
	if (knobs.raw_pads_only)
		return true;
	if (knobs.raw_pads && SDL_GetGamepadVendorForID(which) == VENDOR_SONY
		&& type_of(SDL_GetGamepadProductForID(which)) != 0)
		return true;
	const char *path = SDL_GetGamepadPathForID(which);
	if (path != NULL && hosts_own(path)) {
		printf("demo: %s is the host's own on this machine; not sent\n", path);
		return true;
	}
	return false;
}

static void added(SDL_JoystickID which)
{
	if (by_which(which) != NULL || left_alone(which))
		return;
	gamepad *slot = NULL;
	for (size_t i = 0; i < PADS && slot == NULL; i++)
		if (_pads[i].pad == NULL)
			slot = &_pads[i];
	if (slot == NULL)
		return;
	SDL_Gamepad *pad = SDL_OpenGamepad(which);
	if (pad == NULL) {
		printf("demo: gamepad %u not opened (%s)\n", (unsigned) which, SDL_GetError());
		return;
	}
	// Pending from the start, so the host has the pad before it is touched.
	*slot = gamepad{
		.pad = pad,
		.which = which,
		.id = (uint32_t) (slot - _pads),
		.pending = true,
	};
	const char *name = SDL_GetGamepadName(pad);
	printf("demo: pad %u is %s (%04x:%04x)\n", slot->id, name != NULL ? name : "a gamepad",
		SDL_GetGamepadVendor(pad), SDL_GetGamepadProduct(pad));
}

static void removed(SDL_JoystickID which)
{
	gamepad *p = by_which(which);
	if (p == NULL)
		return;
	printf("demo: pad %u gone\n", p->id);
	lowlat_client_send_pad_unplug(session::client(), p->id);
	SDL_CloseGamepad(p->pad);
	*p = gamepad{};
}

static void changed(SDL_JoystickID which)
{
	_events++;
	gamepad *p = by_which(which);
	if (p != NULL)
		p->pending = true;
}

// SDL's sticks grow down and to the right; the wire's up is positive, and
// -32768 has no negation in sixteen bits.
static int16_t up(int16_t v)
{
	return v == INT16_MIN ? INT16_MAX : (int16_t) -v;
}

// A stick within 2000 of its centre is centred and the rest lose their lowest
// bit, the ends kept exact: what an established client sends, and what keeps
// a pad at rest from sending its jitter.
static int16_t clean(int16_t v)
{
	if (v > -2000 && v < 2000)
		return 0;
	if (v == INT16_MIN || v == INT16_MAX)
		return v;
	return (int16_t) (v & 0xFFFE);
}

static lowlat_pad_state state_of(SDL_Gamepad *pad)
{
	static constexpr struct {
		SDL_GamepadButton from;
		uint16_t to;
	} BUTTONS[] = {
		{SDL_GAMEPAD_BUTTON_SOUTH, LOWLAT_PAD_STATE_A},
		{SDL_GAMEPAD_BUTTON_EAST, LOWLAT_PAD_STATE_B},
		{SDL_GAMEPAD_BUTTON_WEST, LOWLAT_PAD_STATE_X},
		{SDL_GAMEPAD_BUTTON_NORTH, LOWLAT_PAD_STATE_Y},
		{SDL_GAMEPAD_BUTTON_BACK, LOWLAT_PAD_STATE_BACK},
		{SDL_GAMEPAD_BUTTON_START, LOWLAT_PAD_STATE_START},
		{SDL_GAMEPAD_BUTTON_LEFT_STICK, LOWLAT_PAD_STATE_LSTICK},
		{SDL_GAMEPAD_BUTTON_RIGHT_STICK, LOWLAT_PAD_STATE_RSTICK},
		{SDL_GAMEPAD_BUTTON_LEFT_SHOULDER, LOWLAT_PAD_STATE_LSHOULDER},
		{SDL_GAMEPAD_BUTTON_RIGHT_SHOULDER, LOWLAT_PAD_STATE_RSHOULDER},
		{SDL_GAMEPAD_BUTTON_GUIDE, LOWLAT_PAD_STATE_GUIDE},
		{SDL_GAMEPAD_BUTTON_TOUCHPAD, LOWLAT_PAD_STATE_TOUCHPAD},
		{SDL_GAMEPAD_BUTTON_DPAD_UP, LOWLAT_PAD_STATE_DPAD_UP},
		{SDL_GAMEPAD_BUTTON_DPAD_DOWN, LOWLAT_PAD_STATE_DPAD_DOWN},
		{SDL_GAMEPAD_BUTTON_DPAD_LEFT, LOWLAT_PAD_STATE_DPAD_LEFT},
		{SDL_GAMEPAD_BUTTON_DPAD_RIGHT, LOWLAT_PAD_STATE_DPAD_RIGHT},
	};
	lowlat_pad_state st{.size = sizeof(lowlat_pad_state)};
	for (const auto &b : BUTTONS)
		if (SDL_GetGamepadButton(pad, b.from))
			st.buttons |= b.to;
	st.lx = clean(SDL_GetGamepadAxis(pad, SDL_GAMEPAD_AXIS_LEFTX));
	st.ly = clean(up(SDL_GetGamepadAxis(pad, SDL_GAMEPAD_AXIS_LEFTY)));
	st.rx = clean(SDL_GetGamepadAxis(pad, SDL_GAMEPAD_AXIS_RIGHTX));
	st.ry = clean(up(SDL_GetGamepadAxis(pad, SDL_GAMEPAD_AXIS_RIGHTY)));
	// The triggers run 0 to 32767, the wire's 0 to 255.
	st.lt = (uint8_t) (SDL_GetGamepadAxis(pad, SDL_GAMEPAD_AXIS_LEFT_TRIGGER) >> 7);
	st.rt = (uint8_t) (SDL_GetGamepadAxis(pad, SDL_GAMEPAD_AXIS_RIGHT_TRIGGER) >> 7);
	return st;
}

// Field by field, since the structure's padding is not part of its state.
static bool same(const lowlat_pad_state &a, const lowlat_pad_state &b)
{
	return a.buttons == b.buttons && a.lx == b.lx && a.ly == b.ly && a.rx == b.rx &&
		a.ry == b.ry && a.lt == b.lt && a.rt == b.rt;
}

// The latest state of each gamepad that changed, while the window is in
// front: a pad reaches the host only then, as the keys and the mouse do. What
// changed meanwhile waits, so the state a pad holds goes when the window comes
// back, and a state with no session up waits for the session.
//
// A state the host was already sent is not sent again. A stick's jitter inside
// the deadzone marks the pad changed while what it reads, once centred, is
// what went last time: a pad at rest with a worn stick sent its same state
// dozens of times a second.
static void flush()
{
	if (!MTY_AppIsActive(app::toolkit()))
		return;
	for (size_t i = 0; i < PADS; i++) {
		gamepad *p = &_pads[i];
		if (p->pad == NULL || !p->pending)
			continue;
		lowlat_pad_state st = state_of(p->pad);
		if (p->has_last && same(st, p->last)) {
			p->pending = false;
			continue;
		}
		lowlat_status s = lowlat_client_send_pad_state(session::client(), p->id, &st);
		if (s == LOWLAT_ERR_NOT_STARTED)
			continue;
		p->pending = false;
		if (s == LOWLAT_OK) {
			_sent++;
			p->last = st;
			p->has_last = true;
		}
		if (config::get().trace_pads)
			printf("pad %u: lx=%d ly=%d rx=%d ry=%d lt=%u rt=%u buttons=%04x -> %s\n", p->id,
				st.lx, st.ly, st.rx, st.ry, st.lt, st.rt, st.buttons, lowlat_status_string(s));
	}
}

void start()
{
	if (!SDL_InitSubSystem(SDL_INIT_GAMEPAD))
		fprintf(stderr, "demo: no gamepads (%s)\n", SDL_GetError());
	if (config::get().raw_pads && SDL_hid_init() != 0)
		fprintf(stderr, "demo: no raw pads (%s)\n", SDL_GetError());
}

void poll(bool established)
{
	SDL_PumpEvents();
	SDL_Event ev;
	while (SDL_PeepEvents(&ev, 1, SDL_GETEVENT, SDL_EVENT_FIRST, SDL_EVENT_LAST) > 0) {
		switch (ev.type) {
			case SDL_EVENT_GAMEPAD_ADDED:
				added(ev.gdevice.which);
				break;
			case SDL_EVENT_GAMEPAD_REMOVED:
				removed(ev.gdevice.which);
				break;
			case SDL_EVENT_GAMEPAD_AXIS_MOTION:
				changed(ev.gaxis.which);
				break;
			case SDL_EVENT_GAMEPAD_BUTTON_DOWN:
			case SDL_EVENT_GAMEPAD_BUTTON_UP:
				changed(ev.gbutton.which);
				break;
			default:
				break;
		}
	}
	// A new session's host has been sent no state yet, so each pad's next goes.
	if (established && !_was_established)
		for (size_t i = 0; i < PADS; i++)
			_pads[i].has_last = false;
	if (config::get().raw_pads) {
		// A new session has none of the pads' feature reports.
		if (established && !_was_established)
			for (size_t i = 0; i < RAW_PADS; i++)
				_raw[i].features_due = true;
		raw_scan(established);
		raw_pump(established);
	}
	_was_established = established;
	flush();
}

void rumble(const lowlat_rumble_event *r)
{
	// Eight bits each on the wire and sixteen in SDL: a byte broadcast into
	// both halves maps the ends exactly. It runs until the host's next word,
	// which stops it with zeros. A pad read raw gets a motor-only report of
	// its own instead.
	_rumbles++;
	if (config::get().raw_pads && raw_rumble(r->pad, r->low, r->high))
		return;
	gamepad *p = by_id(r->pad);
	if (p != NULL)
		SDL_RumbleGamepad(p->pad, (Uint16) (r->low | (r->low << 8)),
			(Uint16) (r->high | (r->high << 8)), 0);
}

void written(const lowlat_pad_report_event *e)
{
	if (!raw_write(e))
		printf("demo: a write for pad %u, which is not read here\n", (unsigned) e->pad);
}

void close()
{
	for (size_t i = 0; i < RAW_PADS; i++)
		if (_raw[i].dev != NULL)
			drop(&_raw[i]);
	for (size_t i = 0; i < PADS; i++)
		if (_pads[i].pad != NULL) {
			SDL_CloseGamepad(_pads[i].pad);
			_pads[i] = gamepad{};
		}
	if (config::get().raw_pads)
		SDL_hid_exit();
	SDL_QuitSubSystem(SDL_INIT_GAMEPAD);
}

Second take_second()
{
	Second s = {_sent, _events, _raw_reports, _raw_outputs};
	_sent = 0;
	_events = 0;
	_raw_reports = 0;
	_raw_outputs = 0;
	return s;
}

uint32_t rumbles()
{
	return _rumbles;
}

}
