// The toolkit's app and its window. See app.h.

#include "app.h"

#include <stdio.h>

#include <atomic>

#include "clocks.h"
#include "input.h"
#include "pads.h"
#include "report.h"
#include "session.h"
#include "view.h"

namespace app {

static MTY_App *_app;
static MTY_Window _window;
static std::atomic_bool _quit;

// The main thread: the two pumps, the leave, the pads, the rectangle and
// the figures, at the toolkit's own cadence.
static bool app_func(void *opaque) noexcept
{
	(void) opaque;
	session::pump();
	double t = clocks::now_ms();
	input::tick(t);
	session::leave_when_due(t);
	if (_quit.load())
		return false;
	pads::poll(report::established());
	view::place();
	if (report::tick(t))
		session::walk(report::seconds(), report::established());
	return true;
}

static void event_func(const MTY_Event *evt, void *opaque) noexcept
{
	(void) opaque;
	switch (evt->type) {
		case MTY_EVENT_CLOSE:
		case MTY_EVENT_QUIT:
			_quit.store(true);
			break;
		case MTY_EVENT_KEY:
			input::key(&evt->key);
			break;
		case MTY_EVENT_BUTTON:
			input::button(&evt->button);
			break;
		case MTY_EVENT_SCROLL:
			input::scroll(&evt->scroll);
			break;
		case MTY_EVENT_MOTION:
			input::motion(&evt->motion);
			break;
		case MTY_EVENT_FOCUS:
			input::focus(evt->focus);
			break;
		case MTY_EVENT_SIZE:
		case MTY_EVENT_MOVE:
			// A notice that both resizes and moves the window arrives as a
			// move alone, so either one reads the size again.
			view::window_changed();
			break;
		default:
			break;
	}
}

bool create()
{
	// The pads are SDL's, so the toolkit leaves them alone.
	_app = MTY_AppCreate(MTY_APP_FLAG_NO_CONTROLLERS, app_func, event_func, NULL);
	if (_app == NULL) {
		fprintf(stderr, "demo: no app\n");
		return false;
	}
	MTY_Frame frame = MTY_MakeDefaultFrame(0, 0, 1280, 720, 0.8f);
	_window = MTY_WindowCreate(_app, "lowlat", &frame, 0);
	if (_window < 0) {
		fprintf(stderr, "demo: no window\n");
		return false;
	}
	view::window_changed();
	MTY_AppSetTimeout(_app, 1);
	return true;
}

void run()
{
	MTY_AppRun(_app);
}

void destroy()
{
	MTY_AppDestroy(&_app);
}

void quit()
{
	_quit.store(true);
}

bool quitting()
{
	return _quit.load();
}

MTY_App *toolkit()
{
	return _app;
}

MTY_Window window()
{
	return _window;
}

}
