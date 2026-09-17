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

	// The second's figures.
	double second_began;
	uint32_t presents;
	uint32_t pictures;
	uint32_t repeats;
	uint32_t skips;
	uint64_t seconds;
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

static void report(struct demo *d)
{
	lowlat_client_status st;
	memset(&st, 0, sizeof st);
	st.size = (uint32_t) sizeof st;
	lowlat_client_get_status(d->client, &st);
	d->seconds++;
	printf("demo: t=%" PRIu64 " presents=%u pictures=%u repeats=%u skips=%u "
		"decode_us=%u readback_us=%u queue=%u behind=%u behind_ms=%u rtt_ms=%u "
		"decoded=%" PRIu64 " rss_mb=%" PRIu64 "\n",
		d->seconds, d->presents, d->pictures, d->repeats, d->skips, st.decode_us,
		st.readback_us, st.queue_depth, st.behind, st.behind_ms, st.rtt_ms, st.decoded,
		resident_mb());
	fflush(stdout);
	d->presents = 0;
	d->pictures = 0;
	d->repeats = 0;
	d->skips = 0;
}

static bool app_func(void *opaque)
{
	struct demo *d = opaque;
	pump_signaling(d);
	pump_library(d);
	if (d->quit)
		return false;

	// The poll: the newest picture, or nothing new.
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
	} else {
		d->repeats++;
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

	double t = now_ms();
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

	if ((lowlat_features() & LOWLAT_FEATURE_CLIENT) == 0) {
		fprintf(stderr, "demo: this library carries no client half\n");
		return 2;
	}
	lowlat_set_log_callback(log_line, NULL);
	lowlat_set_log_level(LOWLAT_LOG_INFO);

	struct demo d;
	memset(&d, 0, sizeof d);

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

	MTY_AppRun(d.app);

	if (d.showing)
		lowlat_client_release_frame(d.client, &d.shown, NULL);
	lowlat_client_end_connection(d.client);
	signaling_close(&d.sig);
	lowlat_client_destroy(d.client);
	MTY_AppDestroy(&d.app);
	return 0;
}
