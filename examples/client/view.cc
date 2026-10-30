// What the window shows and where. See view.h.

#include "view.h"

#include <math.h>
#include <stdio.h>
#include <string.h>

#include <atomic>
#include <utility>
#include <vector>

#include "matoya.h"

#include "app.h"
#include "present.h"
#include "session.h"

namespace view {

// Where the picture is drawn: stretched to the window, or at its own size
// when it fits. The rectangle last told to the library, and the window's
// size it was fitted to.
static std::atomic_bool _stretch{true};
static int32_t _viewport[4];
static MTY_Size _window_size;

// The host's pointer picture, kept at its native size; the toolkit is
// given it at the size it has in the drawn picture, in the drawable's pixels
// (the toolkit's own cursor-size call is a no-op on Linux and Windows, so
// the resample is done here, as an established client does it; on macOS the
// toolkit draws an unsized cursor's pixels as the screen's). The target last
// applied says when to redo it.
static std::vector<uint8_t> _cursor_rgba;
static uint32_t _cursor_width;
static uint32_t _cursor_height;
static uint32_t _cursor_hot_x;
static uint32_t _cursor_hot_y;
static uint32_t _cursor_target_w;
static uint32_t _cursor_target_h;
static bool _cursor_suppressed;

// The drawn size of the picture as shown (a quarter turn swaps its sides),
// or zero before there is one.
static void drawn_size(uint32_t *across, uint32_t *down)
{
	present::Picture p = present::picture();
	bool turned = p.rotation == LOWLAT_ROTATION_90 || p.rotation == LOWLAT_ROTATION_270;
	*across = turned ? p.height : p.width;
	*down = turned ? p.width : p.height;
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
static void apply_cursor()
{
	if (_cursor_rgba.empty())
		return;
	uint32_t across, down;
	drawn_size(&across, &down);
	uint32_t cw = _cursor_width, ch = _cursor_height;
	uint32_t tw = cw, th = ch;
	if (across != 0 && down != 0 && _viewport[2] > 0 && _viewport[3] > 0) {
		tw = (uint32_t) ((uint64_t) cw * (uint64_t) _viewport[2] + across / 2) / across;
		th = (uint32_t) ((uint64_t) ch * (uint64_t) _viewport[3] + down / 2) / down;
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
	if (tw == _cursor_target_w && th == _cursor_target_h)
		return;
	_cursor_target_w = tw;
	_cursor_target_h = th;
	uint32_t hot_x = (uint32_t) (((uint64_t) tw * _cursor_hot_x + cw / 2) / cw);
	uint32_t hot_y = (uint32_t) (((uint64_t) th * _cursor_hot_y + ch / 2) / ch);
	if (tw == cw && th == ch) {
		MTY_AppSetRGBACursor(app::toolkit(), _cursor_rgba.data(), cw, ch, _cursor_hot_x,
			_cursor_hot_y);
		return;
	}
	// The chain of halvings, then the bilinear step.
	std::vector<uint8_t> halved;
	const uint8_t *cur = _cursor_rgba.data();
	uint32_t sw = cw, sh = ch;
	while ((sw > sh ? sw : sh) >= 2 * tw && sw >= 2 && sh >= 2) {
		uint32_t hw = sw / 2, hh = sh / 2;
		std::vector<uint8_t> next((size_t) hw * hh * 4);
		halve_rgba(cur, sw, sh, next.data(), hw, hh);
		halved = std::move(next);
		cur = halved.data();
		sw = hw;
		sh = hh;
	}
	std::vector<uint8_t> scaled((size_t) tw * th * 4);
	if (sw == tw && sh == th)
		memcpy(scaled.data(), cur, scaled.size());
	else
		bilinear_rgba(cur, sw, sh, scaled.data(), tw, th);
	MTY_AppSetRGBACursor(app::toolkit(), scaled.data(), tw, th, hot_x, hot_y);
}

void cursor(const lowlat_cursor_event *c)
{
	if (c->image_update && c->image != NULL && c->image_len == (uint32_t) c->width * c->height * 4) {
		_cursor_rgba.assign(c->image, c->image + c->image_len);
		_cursor_width = c->width;
		_cursor_height = c->height;
		_cursor_hot_x = c->hot_x;
		_cursor_hot_y = c->hot_y;
		_cursor_target_w = 0;
		_cursor_target_h = 0;
	}
	apply_cursor();
	// A pointer the host withholds for touch is not drawn here either; it
	// is not relative mode, which the relative event handles.
	if (c->suppressed != _cursor_suppressed) {
		_cursor_suppressed = c->suppressed;
		MTY_AppShowCursor(app::toolkit(), !c->suppressed);
	}
}

void window_changed()
{
	_window_size = MTY_WindowGetSize(app::toolkit(), app::window());
}

// The rectangle the picture is drawn into, as the toolkit computes it:
// fitted to the window keeping its shape, or at its own size when it fits;
// centred either way. Told to the library when it changes.
void place()
{
	int32_t rect[4] = {0, 0, 0, 0};
	present::Picture p = present::picture();
	if (p.width != 0) {
		MTY_Size size = _window_size;
		bool turned = p.rotation == LOWLAT_ROTATION_90 || p.rotation == LOWLAT_ROTATION_270;
		double w = turned ? p.height : p.width;
		double h = turned ? p.width : p.height;
		double ar = w / h;
		bool stretch = _stretch.load();
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
	if (memcmp(rect, _viewport, sizeof rect) != 0) {
		memcpy(_viewport, rect, sizeof rect);
		lowlat_client_set_viewport(session::client(), rect[0], rect[1], rect[2], rect[3]);
		// The pointer is drawn at the picture's ratio, so it follows.
		apply_cursor();
	}
}

void toggle_stretch()
{
	_stretch.store(!_stretch.load());
	printf("demo: %s\n", _stretch ? "stretched to the window" : "at its own size");
}

bool stretched()
{
	return _stretch.load();
}

void forget_session()
{
	memset(_viewport, 0, sizeof _viewport);
}

}
