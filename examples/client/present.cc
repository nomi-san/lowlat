// The presenting thread. See present.h.

#include "present.h"

#include <inttypes.h>
#include <stdio.h>
#include <string.h>

#include <atomic>

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif

#include "lowlat.h"
#include "matoya.h"

#include "app.h"
#include "clocks.h"
#include "config.h"
#include "input.h"
#include "session.h"
#include "view.h"

namespace present {

static MTY_Thread *_thread;

// The picture on the screen, held until the next replaces it, and what was
// polled and presented when: the presenting thread's alone.
static lowlat_frame _shown;
static bool _showing;
static uint64_t _last_sequence;
static double _last_poll_ms;
// The display's refresh period and when the picture on screen was first
// presented.
static double _refresh_ms;
static double _shown_at_ms;
#if defined(_WIN32)
// The GPU the Direct3D 11 context is made on, as the pictures name it (the
// adapter's identity, low part first), zero for the system's default. A
// picture of textures from another GPU remakes the context there, since a
// shared texture opens only on its own GPU.
static uint64_t _gfx_adapter;
#endif
// Whether the picture on the screen is in the renderer's textures already,
// and when it was last loaded into them. A repeat draws those again with no
// upload; at least once a second it is loaded all the same, since a context
// the toolkit makes again by itself, after its device was lost, starts with
// none.
static bool _loaded;
static double _loaded_ms;

static std::atomic_uint _width;
static std::atomic_uint _height;
static std::atomic_uint _rotation;
static std::atomic_uint _format;
static std::atomic_uint _kind;
static std::atomic_bool _full_range;

static std::atomic_uint _presents;
static std::atomic_uint _polls;
static std::atomic_uint _pictures;
static std::atomic_uint _repeats;
static std::atomic_uint _skips;
static std::atomic_uint _swap_sum_us;
static std::atomic_uint _swap_count;
static std::atomic_uint _swap_max_us;
static std::atomic_uint _held[4];
static std::atomic_uint _lat_sum_us;
static std::atomic_uint _lat_count;
static std::atomic_uint _lat_max_us;
static std::atomic_uint _lib_sum_us;
static std::atomic_uint _lib_max_us;

// The layouts by what drawing one asks: ten bits in sixteen, chroma at full
// resolution, and each chroma component in a plane of its own.
static bool deep(uint32_t format)
{
	return format == LOWLAT_FORMAT_P010 || format == LOWLAT_FORMAT_YUV444_16
		|| format == LOWLAT_FORMAT_P410;
}

static bool full(uint32_t format)
{
	return format == LOWLAT_FORMAT_YUV444 || format == LOWLAT_FORMAT_YUV444_16
		|| format == LOWLAT_FORMAT_NV24 || format == LOWLAT_FORMAT_P410;
}

static bool three_planes(uint32_t format)
{
	return format == LOWLAT_FORMAT_YUV444 || format == LOWLAT_FORMAT_YUV444_16;
}

// `LOWLAT_DUMP_FRAME=<path>` writes the tenth picture's planes, each row at
// its own width with the pitch's padding left out, so the bytes a renderer
// is handed can be looked at with another tool.
static void dump_once(const lowlat_frame *f)
{
	static unsigned seen;
	const char *path = config::get().dump_frame;
	if (path == NULL || ++seen != 10)
		return;
	uint32_t row = f->width * (deep(f->format) ? 2 : 1);
	FILE *out = fopen(path, "wb");
	if (out == NULL)
		return;
	for (uint32_t p = 0; p < 3; p++) {
		if (f->planes[p].data == NULL)
			continue;
		uint32_t rows = p == 0 || full(f->format) ? f->height : f->height / 2;
		// Full chroma in two planes: both components, side by side.
		uint32_t bytes = p == 1 && full(f->format) && !three_planes(f->format) ? 2 * row : row;
		for (uint32_t r = 0; r < rows; r++)
			fwrite(f->planes[p].data + (size_t) r * f->planes[p].pitch, 1, bytes, out);
	}
	fclose(out);
	printf("demo: dumped picture %ux%u format=%u full_range=%d to %s\n", f->width, f->height,
		f->format, (int) f->full_range, path);
}

static void *present_loop(void *opaque) noexcept
{
	(void) opaque;
	const config::Settings &knobs = config::get();
	lowlat_client *client = session::client();
	MTY_App *toolkit = app::toolkit();
	MTY_Window window = app::window();
#if defined(_WIN32)
	_gfx_adapter = knobs.gfx_adapter;
	MTY_SetGFXAdapter(_gfx_adapter);
#endif
	if (!MTY_WindowSetGFX(toolkit, window, knobs.gfx, knobs.vsync)) {
		fprintf(stderr, "demo: no graphics context\n");
		app::quit();
		return NULL;
	}
	uint32_t refresh = MTY_WindowGetRefreshRate(toolkit, window);
	_refresh_ms = 1000.0 / (double) (refresh > 0 ? refresh : 60);
#if defined(_WIN32)
	if (knobs.gfx == MTY_GFX_D3D11 && _gfx_adapter != 0)
		printf("demo: drawing on luid:%08x:%08x\n", (unsigned) (_gfx_adapter >> 32),
			(unsigned) _gfx_adapter);
	else
		printf("demo: drawing through %s on the system's default adapter\n",
			config::gfx_name(knobs.gfx));
#endif
#if defined(__linux__) || defined(__APPLE__)
	// A handle is only drawable on a context that imports one: asked once,
	// before any picture, rather than found out per frame. Direct3D 11 opens
	// every shared texture of its own GPU.
	if (knobs.handles && !MTY_WindowIsValidHardwareFrame(toolkit, window, NULL, NULL)) {
		fprintf(stderr, "demo: this graphics context does not import device handles\n");
		app::quit();
		return NULL;
	}
#endif
	while (!app::quitting()) {
		double t = clocks::now_ms();
		double taken_ms = 0.0;
		uint64_t arrived_us = 0;
		// The poll: the newest picture, or nothing new. **It waits for the
		// picture** rather than presenting the one on screen again and
		// looking afterwards: a present blocks until the display's next
		// refresh, so a picture arriving meanwhile would be taken a refresh
		// later and shown a refresh after that. Waiting, it is drawn the
		// moment it arrives and shown at the next refresh; the one on
		// screen is presented again only when nothing came in the wait, which
		// keeps the window drawn while the stream is still. Under a cap it
		// runs on the first refresh at or past the cap's period (three
		// quarters of it, so a refresh a little early still counts), without
		// waiting, so the display shows every refresh and the picture changes
		// at the cap's cadence.
		if (knobs.poll_period_ms <= 0.0 || t - _last_poll_ms >= knobs.poll_period_ms * 0.75) {
			_last_poll_ms = t;
			_polls.fetch_add(1);
			lowlat_frame fresh{.size = sizeof(lowlat_frame)};
			uint32_t wait_ms = knobs.poll_period_ms > 0.0 ? 0 : 20;
			lowlat_status s = lowlat_client_acquire_frame(client, 0, wait_ms, &fresh);
			if (s == LOWLAT_OK) {
				taken_ms = clocks::now_ms();
				arrived_us = fresh.arrived_us;
				if (_showing)
					lowlat_client_release_frame(client, &_shown, NULL);
				if (_showing && fresh.sequence > _last_sequence + 1)
					_skips.fetch_add((uint32_t) (fresh.sequence - _last_sequence - 1));
				_last_sequence = fresh.sequence;
				_shown = fresh;
				_showing = true;
				_loaded = false;
#if defined(_WIN32)
				// A shared texture opens only on its own GPU, and the decoder
				// starts on the renderer's (session.cc): pictures from another
				// one -- the decoder moved by the chord or by a row picked, a
				// device lost and found again under a new identity -- remake
				// the context there.
				if (fresh.kind == LOWLAT_FRAME_HANDLE && fresh.adapter != 0
					&& fresh.adapter != _gfx_adapter) {
					_gfx_adapter = fresh.adapter;
					MTY_SetGFXAdapter(_gfx_adapter);
					MTY_WindowSetGFX(toolkit, window, MTY_GFX_NONE, false);
					bool made = MTY_WindowSetGFX(toolkit, window, knobs.gfx, knobs.vsync);
					printf("demo: pictures on adapter %08x:%08x, the renderer follows: %s\n",
						(unsigned) (_gfx_adapter >> 32), (unsigned) _gfx_adapter,
						made ? "ok" : "no context there");
				}
#endif
				_height.store(fresh.height);
				_rotation.store(fresh.rotation);
				_format.store(fresh.format);
				_kind.store(fresh.kind);
				_full_range.store(fresh.full_range);
				_width.store(fresh.width);
				_pictures.fetch_add(1);
				dump_once(&fresh);
				session::first_picture();
			} else {
				_repeats.fetch_add(1);
				// Between one session and the next the library has nothing
				// to wait on and answers at once, so the wait is made here
				// rather than spun.
				if (s == LOWLAT_ERR_NOT_STARTED)
					MTY_Sleep(20);
			}
		}

		// Drawn every iteration, new or not: after a new picture, or after
		// a wait that brought none, so a still stream's window is drawn too.
		// A repeat names no format, which draws the renderer's textures as
		// they are; a new picture, or one not loaded for a second, is loaded.
		if (_showing) {
			const lowlat_frame *f = &_shown;
			bool load = !_loaded || t - _loaded_ms >= 1000.0;
			MTY_RenderDesc desc;
			memset(&desc, 0, sizeof desc);
			desc.format = !load ? MTY_COLOR_FORMAT_UNKNOWN
				: three_planes(f->format)
					? (deep(f->format) ? MTY_COLOR_FORMAT_3PLANES_16 : MTY_COLOR_FORMAT_3PLANES)
					: (deep(f->format) ? MTY_COLOR_FORMAT_2PLANES_16 : MTY_COLOR_FORMAT_2PLANES);
			desc.chroma = full(f->format) ? MTY_CHROMA_444 : MTY_CHROMA_420;
			desc.filter = MTY_FILTER_LINEAR;
			// A handle is drawn from the toolkit's import of it, made once
			// per allocation and kept while that allocation keeps coming:
			// on Windows a texture per plane, by its shared handle; on macOS
			// each plane of the picture's surface by its index; elsewhere
			// at each plane's offset and pitch, planes being one image with
			// the planes in sequence and the row length as a width, each
			// plane's offset the rows before it times that width, which is
			// how the slot is laid out.
			MTY_HardwareFrame hw;
			memset(&hw, 0, sizeof hw);
			if (f->kind == LOWLAT_FRAME_HANDLE) {
				hw.id = f->allocation;
#if defined(_WIN32)
				for (uint32_t p = 0; p < 3; p++)
					hw.texture[p] = (void *) (uintptr_t) f->textures[p];
#elif defined(__APPLE__)
				hw.surface = const_cast<void *>(f->surface);
#else
				hw.fd = f->fd;
				hw.size = f->handle_size;
				for (uint32_t p = 0; p < 3; p++) {
					hw.offset[p] = f->planes[p].offset;
					hw.pitch[p] = f->planes[p].pitch;
				}
#endif
				desc.hardware = true;
				desc.imageWidth = f->width;
				desc.imageHeight = f->height;
			} else {
				// Each plane where it lies, at its own pitch: in sequence in
				// the library's own memory, apart and each aligned on its own
				// in a buffer of the system's decoder on macOS.
				for (uint32_t p = 0; p < 3; p++) {
					desc.planes[p] = f->planes[p].data;
					desc.pitches[p] = f->planes[p].pitch;
				}
				desc.imageWidth = f->width;
				desc.imageHeight = f->height;
			}
			desc.cropWidth = f->width;
			desc.cropHeight = f->height;
			desc.rotation = f->rotation == LOWLAT_ROTATION_90 ? MTY_ROTATION_90
				: f->rotation == LOWLAT_ROTATION_180 ? MTY_ROTATION_180
				: f->rotation == LOWLAT_ROTATION_270 ? MTY_ROTATION_270 : MTY_ROTATION_NONE;
			desc.aspectRatio = (float) f->width / (float) f->height;
			// Fitted to the window, or at its own size when it fits.
			desc.scale = view::stretched() ? 0.0f : 1.0f;
			// Ten-bit samples arrive in the high bits of sixteen, which is
			// already the scale a sixteen-bit texture normalises; the
			// toolkit's multiply is for samples in the low bits, and applied
			// here it saturates the chroma into a uniform magenta.
			desc.multiplyYUV = false;
			// The range is the stream's, per picture. Left at the video
			// range, a full-range picture is drawn darker, its blacks
			// crushed and its contrast raised.
			desc.fullRangeYUV = f->full_range;
			MTY_WindowDrawQuad(toolkit, window,
				f->kind == LOWLAT_FRAME_HANDLE ? (const void *) &hw : (const void *) f->planes[0].data,
				&desc);
			if (load) {
				_loaded = true;
				_loaded_ms = t;
			}
		} else {
			MTY_WindowClear(toolkit, window, 0.0f, 0.0f, 0.0f, 1.0f);
		}
		MTY_WindowPresent(toolkit, window);
		_presents.fetch_add(1);
		// A new picture's wait from hand to screen, and how long the one it
		// replaced stayed up, in refreshes.
		if (taken_ms > 0.0) {
			double shown = clocks::now_ms();
			uint32_t swap_us = (uint32_t) ((shown - taken_ms) * 1000.0);
			_swap_sum_us.fetch_add(swap_us);
			_swap_count.fetch_add(1);
			if (swap_us > _swap_max_us.load())
				_swap_max_us.store(swap_us);
			if (_shown_at_ms > 0.0) {
				uint32_t n = (uint32_t) ((shown - _shown_at_ms) / _refresh_ms + 0.5);
				_held[n >= 4 ? 3 : (n > 0 ? n - 1 : 0)].fetch_add(1);
			}
			_shown_at_ms = shown;
			// Zero from a library that does not stamp; both clocks are the
			// monotonic one, so a stamp past the acquire is not one.
			double arrived_ms = (double) arrived_us / 1000.0;
			if (arrived_us != 0 && arrived_ms <= taken_ms) {
				uint32_t lib_us = (uint32_t) ((taken_ms - arrived_ms) * 1000.0);
				uint32_t lat_us = (uint32_t) ((shown - arrived_ms) * 1000.0);
				_lat_sum_us.fetch_add(lat_us);
				_lat_count.fetch_add(1);
				if (lat_us > _lat_max_us.load())
					_lat_max_us.store(lat_us);
				_lib_sum_us.fetch_add(lib_us);
				if (lib_us > _lib_max_us.load())
					_lib_max_us.store(lib_us);
				if (knobs.trace_latency)
					printf("lat: t_ms=%.1f seq=%" PRIu64 " lib_us=%u present_us=%u total_us=%u "
						"motions=%u\n", shown - clocks::started_ms(), _last_sequence, lib_us,
						lat_us - lib_us, lat_us, input::motions_total());
			}
		}
	}
	if (_showing)
		lowlat_client_release_frame(client, &_shown, NULL);
	MTY_WindowSetGFX(toolkit, window, MTY_GFX_NONE, false);
	return NULL;
}

void start()
{
	_thread = MTY_ThreadCreate(present_loop, NULL);
}

void stop()
{
	MTY_ThreadDestroy(&_thread);
}

Picture picture()
{
	Picture p;
	p.width = _width.load();
	p.height = _height.load();
	p.rotation = _rotation.load();
	p.format = _format.load();
	p.kind = _kind.load();
	p.full_range = _full_range.load();
	return p;
}

void forget_picture()
{
	_width.store(0);
}

Second take_second()
{
	Second s;
	s.presents = _presents.exchange(0);
	s.polls = _polls.exchange(0);
	s.pictures = _pictures.exchange(0);
	s.repeats = _repeats.exchange(0);
	s.skips = _skips.exchange(0);
	s.swap_sum_us = _swap_sum_us.exchange(0);
	s.swap_count = _swap_count.exchange(0);
	s.swap_max_us = _swap_max_us.exchange(0);
	for (uint32_t i = 0; i < 4; i++)
		s.held[i] = _held[i].exchange(0);
	s.lat_sum_us = _lat_sum_us.exchange(0);
	s.lat_count = _lat_count.exchange(0);
	s.lat_max_us = _lat_max_us.exchange(0);
	s.lib_sum_us = _lib_sum_us.exchange(0);
	s.lib_max_us = _lib_max_us.exchange(0);
	return s;
}

}
