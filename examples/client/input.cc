// Keyboard and mouse to the host. See input.h.

#include "input.h"

#include <stdio.h>

#include <atomic>
#include <bitset>

#include "app.h"
#include "keys.h"
#include "session.h"
#include "view.h"

namespace input {

// The host's pointer mode, and whether the chord let go of it.
static bool _relative;
static bool _released;
// Whether the keyboard is grabbed: the desktop's own keys -- the GUI key,
// its task switch -- go to the host instead of to the desktop here.
static bool _grabbed;
static uint32_t _keys;
static uint32_t _buttons;
static uint32_t _wheels;
static uint32_t _motions;
static std::atomic_uint _motions_total;
// The usage codes last sent down and not yet up: a press the library took
// sets one, any release clears it, and so does letting go of everything.
static std::bitset<256> _held;

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
static void sent(lowlat_status s, uint32_t *counter)
{
	if (s == LOWLAT_OK)
		(*counter)++;
}

// The host's pointer mode, as the toolkit is told it. The chord can let go
// of a captured pointer; the host's next transition takes it back.
static void apply_relative()
{
	bool want = _relative && !_released;
	if (MTY_AppGetRelativeMouse(app::toolkit()) != want)
		MTY_AppSetRelativeMouse(app::toolkit(), want);
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

void key(const MTY_KeyEvent *k)
{
	// The demo's own chords, Ctrl+Shift and a letter, never sent. The
	// release that follows one is sent and names a key the host never saw
	// down, which it drops.
	if (k->pressed && (k->mod & (MTY_MOD_LCTRL | MTY_MOD_RCTRL))
		&& (k->mod & (MTY_MOD_LSHIFT | MTY_MOD_RSHIFT))) {
		switch (k->key) {
			case MTY_KEY_I:
				// The toolkit grabs the keyboard while the window has the
				// focus, so the desktop's own keys reach the host; the
				// grab follows the focus, and the release-all on losing
				// it still runs.
				_grabbed = MTY_AppGrabKeyboard(app::toolkit(), !_grabbed);
				printf("demo: keyboard %s\n", _grabbed ? "grabbed, the desktop's keys go to the host"
					: "let go");
				return;
			case MTY_KEY_W: {
				bool full = !MTY_WindowIsFullscreen(app::toolkit(), app::window());
				MTY_WindowSetFullscreen(app::toolkit(), app::window(), full);
				printf("demo: %s\n", full ? "fullscreen" : "windowed");
				return;
			}
			case MTY_KEY_D:
				session::ask_outputs();
				return;
			case MTY_KEY_F:
				view::toggle_stretch();
				return;
			case MTY_KEY_R:
				_released = !_released;
				apply_relative();
				printf("demo: pointer %s\n", _released ? "let go" : "taken");
				return;
			case MTY_KEY_C:
				session::cycle_video();
				return;
			case MTY_KEY_X:
				session::cycle_decoder();
				return;
			case MTY_KEY_H:
				session::toggle_kind();
				return;
			case MTY_KEY_Q:
				// Leave and quit, as closing the window does: the loop stops
				// here, and the departure goes out on the way down with its
				// grace.
				printf("demo: leaving and quitting\n");
				app::quit();
				return;
			default:
				break;
		}
	}
#if !defined(__APPLE__)
	// A bare GUI key is the desktop's unless the keyboard is grabbed: sent
	// then, the host would be left holding the modifier the desktop took. On
	// macOS it is Command, sent as the GUI key: the system's own Command
	// chords never reach the window, and its losing the focus lets go of
	// everything.
	if ((k->key == MTY_KEY_LWIN || k->key == MTY_KEY_RWIN) && !_grabbed)
		return;
#endif
	if (k->key >= MTY_KEY_MAX || KEY_USAGE[k->key] == 0)
		return;
	uint16_t usage = KEY_USAGE[k->key];
#if defined(__APPLE__)
	// The window reports Caps Lock as the lock's state, down as it goes on
	// and up as it goes off; a host that toggles on a press would toggle
	// once every two. Each change goes as a press and a release, the mask
	// carrying the new state.
	if (k->key == MTY_KEY_CAPS) {
		uint32_t mods = mods_of(k->mod);
		sent(lowlat_client_send_key(session::client(), usage, mods, true), &_keys);
		sent(lowlat_client_send_key(session::client(), usage, mods, false), &_keys);
		return;
	}
#endif
	lowlat_status s = lowlat_client_send_key(session::client(), usage, mods_of(k->mod), k->pressed);
	sent(s, &_keys);
	if (!k->pressed)
		_held.reset(usage);
	else if (s == LOWLAT_OK)
		_held.set(usage);
}

void button(const MTY_ButtonEvent *b)
{
	if (button_of(b->button) != 0)
		sent(lowlat_client_send_mouse_button(session::client(), button_of(b->button), b->pressed,
			b->x, b->y), &_buttons);
}

void scroll(const MTY_ScrollEvent *s)
{
	sent(lowlat_client_send_mouse_wheel(session::client(), s->x, s->y), &_wheels);
}

void motion(const MTY_MotionEvent *m)
{
	sent(lowlat_client_send_mouse_motion(session::client(), m->x, m->y, m->relative), &_motions);
	_motions_total.fetch_add(1);
}

void focus(bool focused)
{
	if (!focused) {
		lowlat_client_send_release_all(session::client());
		_held.reset();
	}
	apply_relative();
}

void host_relative(const lowlat_relative_event *r)
{
	// On the way out the pointer reappears where the host says, once.
	_relative = r->relative;
	_released = false;
	apply_relative();
	if (!_relative && r->x >= 0 && r->y >= 0)
		MTY_WindowWarpCursor(app::toolkit(), app::window(), (uint32_t) r->x, (uint32_t) r->y);
	printf("demo: pointer %s\n", _relative ? "captured by the host" : "returned");
}

void forget_session()
{
	_held.reset();
	if (_relative) {
		_relative = false;
		apply_relative();
	}
}

Second take_second()
{
	Second s = {_keys, _buttons, _wheels, _motions, (uint32_t) _held.count()};
	_keys = 0;
	_buttons = 0;
	_wheels = 0;
	_motions = 0;
	return s;
}

uint32_t motions_total()
{
	return _motions_total.load();
}

}
