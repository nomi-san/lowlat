// The line each second. See report.h.

#include "report.h"

#include <inttypes.h>
#include <stdio.h>
#include <string.h>

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <psapi.h>
#elif defined(__APPLE__)
#include <mach/mach.h>
#else
#include <unistd.h>
#endif

#include "lowlat.h"
#include "matoya.h"

#include "app.h"
#include "config.h"
#include "input.h"
#include "pads.h"
#include "present.h"
#include "session.h"
#include "sound.h"

namespace report {

static double _second_began;
static uint64_t _seconds;
static uint64_t _last_video_bytes;
static bool _established;
// The reader's lag, sampled once a second: thirty or more messages
// behind for sixty consecutive seconds is the warning every client
// shows, and it clears the moment the lag drops under.
static unsigned _behind_seconds;
static bool _behind_warned;
// The gateway's mapping as last printed: by what, where, and the last
// refusal.
static uint32_t _mapping;
static uint16_t _mapped_port;
static char _mapped_address[LOWLAT_ADDRESS_MAX];
static uint16_t _mapping_refusal;
static uint32_t _mapping_refused_by;

#if defined(__APPLE__)
// The task's own accounting: what it has resident, and its footprint, which
// is what the system charges it and what its memory pressure is judged by.
static bool vm_info(task_vm_info_data_t *info)
{
	mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
	return task_info(mach_task_self(), TASK_VM_INFO, (task_info_t) info, &count) == KERN_SUCCESS;
}
#endif

static uint64_t resident_mb()
{
#if defined(_WIN32)
	PROCESS_MEMORY_COUNTERS c;
	memset(&c, 0, sizeof c);
	c.cb = (DWORD) sizeof c;
	if (!GetProcessMemoryInfo(GetCurrentProcess(), &c, sizeof c))
		return 0;
	return (uint64_t) c.WorkingSetSize / (1024 * 1024);
#elif defined(__APPLE__)
	task_vm_info_data_t info;
	if (!vm_info(&info))
		return 0;
	return (uint64_t) info.resident_size / (1024 * 1024);
#else
	FILE *f = fopen("/proc/self/statm", "r");
	if (f == NULL)
		return 0;
	unsigned long size = 0, resident = 0;
	int n = fscanf(f, "%lu %lu", &size, &resident);
	fclose(f);
	if (n != 2)
		return 0;
	return (uint64_t) resident * (uint64_t) sysconf(_SC_PAGESIZE) / (1024 * 1024);
#endif
}

void memory_words(char *out, size_t size)
{
#if defined(_WIN32)
	PROCESS_MEMORY_COUNTERS_EX c;
	memset(&c, 0, sizeof c);
	c.cb = (DWORD) sizeof c;
	uint64_t committed = 0;
	if (GetProcessMemoryInfo(GetCurrentProcess(), (PROCESS_MEMORY_COUNTERS *) &c, sizeof c))
		committed = (uint64_t) c.PrivateUsage / (1024 * 1024);
	snprintf(out, size, "rss_mb=%" PRIu64 " commit_mb=%" PRIu64, resident_mb(), committed);
#elif defined(__APPLE__)
	task_vm_info_data_t info;
	uint64_t footprint = vm_info(&info) ? (uint64_t) info.phys_footprint / (1024 * 1024) : 0;
	snprintf(out, size, "rss_mb=%" PRIu64 " footprint_mb=%" PRIu64, resident_mb(), footprint);
#else
	snprintf(out, size, "rss_mb=%" PRIu64, resident_mb());
#endif
}

// The second's figures on the log, and the same in the title bar.
static void second()
{
	lowlat_client *client = session::client();
	const config::Settings &knobs = config::get();
	lowlat_client_status st{.size = sizeof(lowlat_client_status)};
	lowlat_client_get_status(client, &st);
	_seconds++;
	_established = st.state == LOWLAT_CLIENT_ESTABLISHED;
	double mbit = (double) (st.video_bytes - _last_video_bytes) * 8.0 / 1.0e6;
	_last_video_bytes = st.video_bytes;
	uint64_t rss = resident_mb();
	char memory[64];
	memory_words(memory, sizeof memory);
	const char *codec = st.codec == LOWLAT_CODEC_HEVC ? "HEVC"
		: st.codec == LOWLAT_CODEC_H264 ? "H264" : "-";
	present::Second p = present::take_second();
	sound::Second snd = sound::take_second();
	if (snd.packets == 0)
		snd.q_min = 0;
	// This side's figures for the video channel, and beside them the host's
	// for this guest as its last guest list carried them: the two ends of
	// one path, each measured where it can be.
	lowlat_client_metrics m{.size = sizeof(lowlat_client_metrics)};
	lowlat_client_get_metrics(client, &m);
	input::Second in = input::take_second();
	pads::Second pad = pads::take_second();
	const session::Roster &room = session::roster();
	printf("demo: t=%" PRIu64 " presents=%u polls=%u pictures=%u repeats=%u skips=%u "
		"gfx=%s vsync=%d swap_ms=%.1f/%.1f held=%u/%u/%u/%u lat_ms=%.2f/%.2f lib_ms=%.2f/%.2f "
		"codec=%s decode_us=%u readback_us=%u encode_us=%u queue=%u behind=%u behind_ms=%u "
		"rtt_ms=%u mbit=%.1f decoded=%" PRIu64 " %s keys=%u btn=%u wheel=%u "
		"motion=%u pad=%u pad_events=%u pad_raw=%u pad_out=%u pad_in=%u pad_in_dropped=%u "
		"input_dropped=%u "
		"snd=%u snd_frames=%u snd_q_ms=%u snd_q_min=%u snd_q_max=%u snd_age_ms=%u "
		"snd_queued=%u snd_dropped=%u snd_refused=%u snd_resync=%u snd_codec=%s "
		"reported_us=%u snd_reported_us=%u asked=%#x declared=%#x stream_format=%u "
		"frag=%" PRIu64 " late=%" PRIu64 " dup=%" PRIu64 " oow=%" PRIu64 " nacks=%" PRIu64
		" loss30=%.4f cursor=%u misses=%u refused=%u rumble=%u "
		"guest=%u owner=%d rosters=%u h_rtt=%.1f h_mbps=%.2f h_enc=%.2f h_dec=%.2f "
		"h_packets=%d h_fast=%d h_slow=%d h_cg=%d mic=%u mic_dropped=%u mic_open=%d "
		"mic_refused=%u\n",
		_seconds, p.presents, p.polls, p.pictures, p.repeats, p.skips,
		config::gfx_name(knobs.gfx), (int) knobs.vsync,
		p.swap_count > 0 ? (double) p.swap_sum_us / (double) p.swap_count / 1000.0 : 0.0,
		(double) p.swap_max_us / 1000.0, p.held[0], p.held[1], p.held[2], p.held[3],
		p.lat_count > 0 ? (double) p.lat_sum_us / (double) p.lat_count / 1000.0 : 0.0,
		(double) p.lat_max_us / 1000.0,
		p.lat_count > 0 ? (double) p.lib_sum_us / (double) p.lat_count / 1000.0 : 0.0,
		(double) p.lib_max_us / 1000.0, codec,
		st.decode_us, st.readback_us, st.encode_us, st.queue_depth, st.behind, st.behind_ms,
		st.rtt_ms, mbit, st.decoded, memory, in.keys, in.buttons, in.wheels,
		in.motions, pad.sent, pad.events, pad.raw_reports, pad.raw_outputs,
		st.pad_reports_received, st.pad_reports_dropped, st.input_dropped,
		snd.packets, snd.frames, snd.q_ms, snd.q_min, snd.q_max, snd.age_max,
		st.audio_queued, st.audio_dropped, st.audio_refused, snd.resyncs,
		st.audio_codec == LOWLAT_AUDIO_OPUS ? "opus"
			: st.audio_codec == LOWLAT_AUDIO_PCM ? "pcm" : "-",
		st.decode_reported_us, st.audio_reported_us, st.asked_flags, st.declared_flags,
		st.stream_format,
		m.video.fragments, m.video.late, m.video.duplicates, m.video.out_of_window,
		m.video.nacks_sent, (double) m.video.loss_30s, st.cursor_images, st.cursor_misses,
		st.cursor_refused, pads::rumbles(), st.number, room.owner, room.rosters, room.host_rtt_ms,
		room.host_mbps, room.host_encode_ms, room.host_decode_ms, room.host_packets,
		room.host_fast_rts, room.host_slow_rts, room.host_cg_events, st.microphone_sent,
		st.microphone_dropped, (int) st.microphone, snd.mic_refused);
	// The warning every client shows for hardware that cannot keep up,
	// gated on the reader's lag rather than on any figure of the decoder's:
	// thirty or more messages behind for sixty consecutive seconds, cleared
	// the moment it drops under.
	if (st.behind >= 30) {
		_behind_seconds++;
		if (_behind_seconds >= 60 && !_behind_warned) {
			_behind_warned = true;
			printf("demo: WARNING the host's resolution or rate is too high for this "
				"hardware to keep up (behind=%u for %u s)\n", st.behind, _behind_seconds);
		}
	} else {
		if (_behind_warned)
			printf("demo: the reader caught up\n");
		_behind_seconds = 0;
		_behind_warned = false;
	}
	// The gateway's mapping of the port, whenever any of it changes: by what,
	// where, or the last refusal while nothing is mapped.
	bool unmapped = st.mapping == LOWLAT_MAPPING_NONE;
	if (st.mapping != _mapping || st.mapped_port != _mapped_port
		|| strcmp(st.mapped_address, _mapped_address) != 0
		|| (unmapped && (st.mapping_refusal != _mapping_refusal
			|| st.mapping_refused_by != _mapping_refused_by))) {
		static const char *const by[] = {"nothing", "PCP", "NAT-PMP", "UPnP"};
		if (st.mapping != LOWLAT_MAPPING_NONE && st.mapping < 4)
			printf("demo: port mapped by %s, external %s:%u\n", by[st.mapping],
				st.mapped_address[0] ? st.mapped_address : "(not stated)",
				(unsigned) st.mapped_port);
		else if (st.mapping_refusal != 0 && st.mapping_refused_by < 4)
			printf("demo: port not mapped, %s refused with %u\n", by[st.mapping_refused_by],
				(unsigned) st.mapping_refusal);
		else
			printf("demo: port not mapped\n");
		_mapping = st.mapping;
		_mapped_port = st.mapped_port;
		memcpy(_mapped_address, st.mapped_address, sizeof _mapped_address);
		_mapping_refusal = st.mapping_refusal;
		_mapping_refused_by = st.mapping_refused_by;
	}
	fflush(stdout);

	char title[320];
	present::Picture pic = present::picture();
	// The decoder by its row's label where the session is on that row's
	// backend, else by the backend alone.
	const lowlat_decoder_info *row = session::decoder_row();
	if (pic.width != 0) {
		const char *colour = pic.format == LOWLAT_FORMAT_P010 ? "10bit"
			: pic.format == LOWLAT_FORMAT_YUV444 || pic.format == LOWLAT_FORMAT_NV24 ? "444"
			: pic.format == LOWLAT_FORMAT_YUV444_16 || pic.format == LOWLAT_FORMAT_P410 ? "444 10bit"
			: "8bit";
		snprintf(title, sizeof title,
			"lowlat | %ux%u %s %s%s%s | asked %s | %s %s %s%s | %u fps | rtt %u/%.0f ms | enc %.1f ms | "
			"dec %.1f ms | rb %.1f ms | q %u behind %u | skips %u | %.1f/%.1f Mbit/s | "
			"loss %.2f%% | snd %u ms | rss %" PRIu64 " MB | guest %u%s%s%s%s%s",
			pic.width, pic.height, codec, colour,
			pic.full_range ? " full" : "",
			pic.rotation == LOWLAT_ROTATION_90 ? " 90deg"
				: pic.rotation == LOWLAT_ROTATION_180 ? " 180deg"
				: pic.rotation == LOWLAT_ROTATION_270 ? " 270deg" : "",
			session::video_words(),
			row != NULL && row->decoder == st.backend ? row->name
				: st.backend == LOWLAT_DECODER_OPEN ? "open"
				: st.backend == LOWLAT_DECODER_VENDOR ? "vendor"
				: st.backend == LOWLAT_DECODER_SOFTWARE ? "software"
				: st.backend == LOWLAT_DECODER_SYSTEM ? "system"
				: "no decoder",
			pic.kind == LOWLAT_FRAME_HANDLE ? "handles" : "planes",
			config::gfx_name(knobs.gfx), knobs.vsync ? "" : " no vsync",
			p.pictures, st.rtt_ms, room.host_rtt_ms, (double) st.encode_us / 1000.0,
			(double) st.decode_us / 1000.0, (double) st.readback_us / 1000.0, st.queue_depth,
			st.behind, p.skips, mbit, room.host_mbps, (double) m.video.loss_30s * 100.0,
			snd.q_ms, rss, st.number, room.owner ? " owner" : "",
			room.perm_mouse ? " m" : "", room.perm_keyboard ? " k" : "", room.perm_gamepad ? " g" : "",
			_behind_warned ? " | CANNOT KEEP UP" : "");
	} else {
		snprintf(title, sizeof title, "lowlat | %s",
			st.state == LOWLAT_CLIENT_ESTABLISHED ? "established, no picture yet"
			: st.state == LOWLAT_CLIENT_OVER ? "over" : "connecting...");
	}
	MTY_WindowSetTitle(app::toolkit(), app::window(), title);
}

void start(double now_ms)
{
	_second_began = now_ms;
}

bool tick(double now_ms)
{
	if (now_ms - _second_began < 1000.0)
		return false;
	_second_began = now_ms;
	second();
	return true;
}

uint64_t seconds()
{
	return _seconds;
}

bool established()
{
	return _established;
}

void forget_session()
{
	_established = false;
	_last_video_bytes = 0;
	_behind_seconds = 0;
	_behind_warned = false;
}

}
