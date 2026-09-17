// The client demo: a window that shows a host's desktop.
//
// Pure C on the application toolkit. The library decodes; this presents.
// One file for the session and the window, one for signaling. Nothing else:
// input, sound and the cursor come with their phases.
//
//   LOWLAT_PEER=<the host's peer id> LOWLAT_SESSION=<a session token> ./client
//
// `LOWLAT_SERVER` names the signaling service (kessel-ws.parsec.app by
// default), `LOWLAT_DEVICE` a render node for the decoder (the first that
// decodes by default), `LOWLAT_DECODER` one of `auto`, `open`, `none`.
// `LOWLAT_FPS` asks the host for that rate through the application
// protocol once the first picture is in; `LOWLAT_PRESENT_HZ` caps how often
// a new picture is taken (the cached one is still drawn every refresh), so
// a stream faster than the presentation can be measured on one display;
// `LOWLAT_SECONDS` leaves cleanly after that long.
//
// Once a second a line goes to stdout with the presentation cadence as
// numbers rather than a judgement: presents and pictures in the second,
// repeats (a present with no new picture) and skips (pictures published and
// never shown, because a newer one had arrived), the decoder's figures, the
// reader's lag, and the process's resident set.

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "lowlat.h"
#include "matoya.h"
#include "signaling.h"

struct demo {
	lowlat_client *client;
	struct signaling sig;
	char attempt[LOWLAT_ATTEMPT_MAX];
	MTY_App *app;
	MTY_Window window;
	bool begun;
	bool quit;

	// The picture on the screen, held until the next replaces it.
	lowlat_frame shown;
	bool showing;
	uint64_t last_sequence;

	// The knobs: the rate asked of the host, the presentation cap, the leave.
	uint32_t ask_fps;
	bool asked;
	double poll_period_ms;
	double last_poll_ms;
	double leave_at_ms;

	// The second's figures.
	double second_began;
	uint32_t presents;
	uint32_t polls;
	uint32_t pictures;
	uint32_t repeats;
	uint32_t skips;
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

static void event_func(const MTY_Event *evt, void *opaque)
{
	struct demo *d = opaque;
	if (evt->type == MTY_EVENT_CLOSE || evt->type == MTY_EVENT_QUIT)
		d->quit = true;
}

// Forward what the library found to the host, and act on what ended.
static void pump_library(struct demo *d)
{
	for (;;) {
		lowlat_event e;
		uint32_t body_len = 0;
		lowlat_status s = lowlat_client_poll_events(d->client, 0, &e, NULL, &body_len);
		if (s != LOWLAT_OK)
			break;
		switch (e.kind) {
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
				d->quit = true;
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
				d->quit = true;
			break;
		}
		if (e == SIGNALING_ANSWER && !d->begun) {
			lowlat_status s = lowlat_client_begin_p2p(d->client, d->attempt, &theirs);
			if (s != LOWLAT_OK) {
				fprintf(stderr, "demo: begin refused: %s\n", lowlat_status_string(s));
				d->quit = true;
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
	printf("demo: t=%" PRIu64 " presents=%u polls=%u pictures=%u repeats=%u skips=%u "
		"codec=%s decode_us=%u readback_us=%u encode_us=%u queue=%u behind=%u behind_ms=%u "
		"rtt_ms=%u mbit=%.1f decoded=%" PRIu64 " rss_mb=%" PRIu64 "\n",
		d->seconds, d->presents, d->polls, d->pictures, d->repeats, d->skips, codec,
		st.decode_us, st.readback_us, st.encode_us, st.queue_depth, st.behind, st.behind_ms,
		st.rtt_ms, mbit, st.decoded, rss);
	fflush(stdout);

	char title[256];
	if (d->showing) {
		const lowlat_frame *f = &d->shown;
		snprintf(title, sizeof title,
			"lowlat | %ux%u %s %s%s | %s | %u fps | rtt %u ms | enc %.1f ms | dec %.1f ms | "
			"rb %.1f ms | q %u behind %u | skips %u | %.1f Mbit/s | rss %" PRIu64 " MB",
			f->width, f->height, codec, f->format == LOWLAT_FORMAT_P010 ? "10bit" : "8bit",
			f->rotation == LOWLAT_ROTATION_90 ? " 90deg"
				: f->rotation == LOWLAT_ROTATION_180 ? " 180deg"
				: f->rotation == LOWLAT_ROTATION_270 ? " 270deg" : "",
			st.backend == LOWLAT_DECODER_OPEN ? "open CPU" : "no decoder",
			d->pictures, st.rtt_ms, (double) st.encode_us / 1000.0,
			(double) st.decode_us / 1000.0, (double) st.readback_us / 1000.0, st.queue_depth,
			st.behind, d->skips, mbit, rss);
	} else {
		snprintf(title, sizeof title, "lowlat | %s",
			st.state == LOWLAT_CLIENT_ESTABLISHED ? "established, no picture yet"
			: st.state == LOWLAT_CLIENT_OVER ? "over" : "connecting...");
	}
	MTY_WindowSetTitle(d->app, d->window, title);

	d->presents = 0;
	d->polls = 0;
	d->pictures = 0;
	d->repeats = 0;
	d->skips = 0;
}

static bool app_func(void *opaque)
{
	struct demo *d = opaque;
	pump_signaling(d);
	pump_library(d);
	double t = now_ms();
	if (d->leave_at_ms > 0.0 && t >= d->leave_at_ms) {
		printf("demo: leaving after %" PRIu64 " s\n", d->seconds);
		d->quit = true;
	}
	if (d->quit)
		return false;

	// The poll: the newest picture, or nothing new. Under a cap it runs on
	// the first refresh at or past the cap's period (three quarters of it,
	// so a refresh a little early still counts), so the display shows every
	// refresh and the picture changes at the cap's cadence.
	if (d->poll_period_ms <= 0.0 || t - d->last_poll_ms >= d->poll_period_ms * 0.75) {
		d->last_poll_ms = t;
		d->polls++;
		lowlat_frame fresh;
		memset(&fresh, 0, sizeof fresh);
		fresh.size = (uint32_t) sizeof fresh;
		lowlat_status s = lowlat_client_acquire_frame(d->client, 0, 0, &fresh);
		if (s == LOWLAT_OK) {
			if (d->showing)
				lowlat_client_release_frame(d->client, &d->shown, NULL);
			if (d->showing && fresh.sequence > d->last_sequence + 1)
				d->skips += (uint32_t) (fresh.sequence - d->last_sequence - 1);
			d->last_sequence = fresh.sequence;
			d->shown = fresh;
			d->showing = true;
			d->pictures++;
			if (d->ask_fps != 0 && !d->asked) {
				d->asked = true;
				ask_rate(d);
			}
		} else {
			d->repeats++;
		}
	}

	// Drawn every iteration, new or not: a renderer that re-presents the
	// cached picture on every refresh is what keeps the window's cadence
	// the display's rather than the stream's.
	if (d->showing) {
		const lowlat_frame *f = &d->shown;
		uint32_t sample = f->format == LOWLAT_FORMAT_P010 ? 2 : 1;
		MTY_RenderDesc desc;
		memset(&desc, 0, sizeof desc);
		desc.format = f->format == LOWLAT_FORMAT_P010 ? MTY_COLOR_FORMAT_2PLANES_16
			: MTY_COLOR_FORMAT_2PLANES;
		desc.chroma = MTY_CHROMA_420;
		desc.filter = MTY_FILTER_LINEAR;
		// The toolkit takes one image with the planes in sequence and the
		// row length as a width; the second plane's offset is the first's
		// rows times that width, which is how the slot is laid out.
		desc.imageWidth = f->planes[0].pitch / sample;
		desc.imageHeight = (uint32_t) ((f->planes[1].data - f->planes[0].data)
			/ f->planes[0].pitch);
		desc.cropWidth = f->width;
		desc.cropHeight = f->height;
		desc.rotation = f->rotation == LOWLAT_ROTATION_90 ? MTY_ROTATION_90
			: f->rotation == LOWLAT_ROTATION_180 ? MTY_ROTATION_180
			: f->rotation == LOWLAT_ROTATION_270 ? MTY_ROTATION_270 : MTY_ROTATION_NONE;
		desc.aspectRatio = (float) f->width / (float) f->height;
		desc.multiplyYUV = sample == 2;
		MTY_WindowDrawQuad(d->app, d->window, f->planes[0].data, &desc);
	} else {
		MTY_WindowClear(d->app, d->window, 0.0f, 0.0f, 0.0f, 1.0f);
	}
	MTY_WindowPresent(d->app, d->window);
	d->presents++;

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
	s = lowlat_client_new_attempt(d.client, NULL, d.attempt, LOWLAT_TRANSPORT_BUD, &ours);
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
	MTY_WindowSetGFX(d.app, d.window, MTY_GFX_GL, true);
	MTY_AppSetTimeout(d.app, 1);
	d.second_began = now_ms();
	d.leave_at_ms = seconds > 0 ? d.second_began + (double) seconds * 1000.0 : 0.0;

	MTY_AppRun(d.app);

	if (d.showing)
		lowlat_client_release_frame(d.client, &d.shown, NULL);
	lowlat_client_end_connection(d.client);
	signaling_close(&d.sig);
	lowlat_client_destroy(d.client);
	MTY_AppDestroy(&d.app);
	return 0;
}
