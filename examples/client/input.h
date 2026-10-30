// Keyboard and mouse to the host, as the toolkit reports them; the demo's own
// chords; the pointer mode the host asks for. The app thread's, but
// motions_total(), any thread.
//
// The chords are Ctrl+Shift and a letter, never sent: the release that
// follows one is sent and names a key the host never saw down, which it
// drops. Off macOS a bare GUI key is the desktop's unless the keyboard is
// grabbed; on macOS Command is sent as the GUI key, and Caps Lock as a press
// and a release each time the lock changes.

#pragma once

#include <stdint.h>

#include "lowlat.h"
#include "matoya.h"

namespace input {

void key(const MTY_KeyEvent *k);
void button(const MTY_ButtonEvent *b);
void scroll(const MTY_ScrollEvent *s);
void motion(const MTY_MotionEvent *m);
// Nothing stays held on a host whose window is no longer in front.
void focus(bool focused);

// The host took the pointer, or gave it back.
void host_relative(const lowlat_relative_event *r);
// The session left: a pointer the host held is given back here.
void forget_session();

// What went to the library in the second, by kind, so the host's own count
// of what it received can be read against this; read and cleared. And how
// many keys the host was last told are down -- a count, never which -- which
// is zero again once the hands are off the keyboard.
struct Second {
	uint32_t keys;
	uint32_t buttons;
	uint32_t wheels;
	uint32_t motions;
	uint32_t keys_held;
};
Second take_second();
// Every motion sent, never cleared, for the per-picture line to say how
// much input went out while the picture was on its way.
uint32_t motions_total();

}
