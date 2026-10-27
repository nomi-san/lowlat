// Every knob the demo reads. See config.h.

#include "config.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <dxgi.h>
#endif

namespace config {

static Settings _settings;

// The interface that imports the library's device handles.
#if defined(_WIN32)
static const MTY_GFX HANDLES_GFX = MTY_GFX_D3D11;
#else
static const MTY_GFX HANDLES_GFX = MTY_GFX_GL;
#endif

static const char *env_or(const char *name, const char *fallback)
{
	const char *v = getenv(name);
	return v != NULL && v[0] != '\0' ? v : fallback;
}

// The graphics interfaces by the names the knob takes; none for a name this
// toolkit has not.
static MTY_GFX gfx_named(const char *name)
{
	return strcmp(name, "gl") == 0 ? MTY_GFX_GL
		: strcmp(name, "vk") == 0 ? MTY_GFX_VK
		: strcmp(name, "d3d11") == 0 ? MTY_GFX_D3D11
		: strcmp(name, "d3d12") == 0 ? MTY_GFX_D3D12 : MTY_GFX_NONE;
}

const char *gfx_name(MTY_GFX gfx)
{
	return gfx == MTY_GFX_GL ? "gl" : gfx == MTY_GFX_VK ? "vk"
		: gfx == MTY_GFX_D3D11 ? "d3d11" : gfx == MTY_GFX_D3D12 ? "d3d12" : "-";
}

bool draws_handles()
{
	return _settings.gfx == HANDLES_GFX;
}

#if defined(_WIN32)
// The system's first adapter, which the toolkit's device is made on when
// none is named: its identity, low part first, as the pictures name one;
// zero for none, and for a software one, which decodes nothing.
static uint64_t first_adapter()
{
	IDXGIFactory1 *factory = NULL;
	if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void **) &factory)))
		return 0;
	uint64_t luid = 0;
	IDXGIAdapter1 *adapter = NULL;
	if (SUCCEEDED(factory->EnumAdapters1(0, &adapter))) {
		DXGI_ADAPTER_DESC1 desc;
		if (SUCCEEDED(adapter->GetDesc1(&desc)) && (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) == 0)
			luid = ((uint64_t) (uint32_t) desc.AdapterLuid.HighPart << 32) | desc.AdapterLuid.LowPart;
		adapter->Release();
	}
	factory->Release();
	return luid;
}
#endif

int read()
{
	Settings &s = _settings;
	const char *peer = getenv("LOWLAT_PEER");
	s.session = getenv("LOWLAT_SESSION");
	if (peer == NULL || s.session == NULL) {
		fprintf(stderr, "demo: LOWLAT_PEER and LOWLAT_SESSION are required\n");
		return 2;
	}
	s.server = env_or("LOWLAT_SERVER", "kessel-ws.parsec.app");
	s.device = env_or("LOWLAT_DEVICE", "");
	const char *decoder = env_or("LOWLAT_DECODER", "auto");
	unsigned long ask_fps = strtoul(env_or("LOWLAT_FPS", "0"), NULL, 10);
	unsigned long present_hz = strtoul(env_or("LOWLAT_PRESENT_HZ", "0"), NULL, 10);
	s.seconds = strtoul(env_or("LOWLAT_SECONDS", "0"), NULL, 10);
	s.switch_every = strtoul(env_or("LOWLAT_SWITCH_EVERY", "0"), NULL, 10);
	s.decoder_every = strtoul(env_or("LOWLAT_DECODER_EVERY", "0"), NULL, 10);
	s.kind_every = strtoul(env_or("LOWLAT_KIND_EVERY", "0"), NULL, 10);
	s.output = getenv("LOWLAT_OUTPUT");
	s.modes = getenv("LOWLAT_MODES");
	s.mode_every = strtoul(env_or("LOWLAT_MODE_EVERY", "5"), NULL, 10);

	// One peer, or several separated by commas: visited in order on the one
	// handle, each for LOWLAT_SECONDS, which a list therefore needs.
	for (const char *at = peer; *at != '\0' && s.peer_count < sizeof s.peers / sizeof s.peers[0];) {
		size_t len = strcspn(at, ",");
		if (len > 0 && len < sizeof s.peers[0])
			snprintf(s.peers[s.peer_count++], sizeof s.peers[0], "%.*s", (int) len, at);
		at += len + (at[len] == ',');
	}
	if (s.peer_count == 0 || (s.peer_count > 1 && s.seconds == 0)) {
		fprintf(stderr, "demo: LOWLAT_PEER names no peer, or names several without LOWLAT_SECONDS\n");
		return 2;
	}
	s.stay_ms = (double) s.seconds * 1000.0;
	s.ask_fps = (uint32_t) ask_fps;
	s.poll_period_ms = present_hz > 0 ? 1000.0 / (double) present_hz : 0.0;
	s.trace_pads = getenv("LOWLAT_PAD_TRACE") != NULL;
	s.trace_latency = getenv("LOWLAT_LATENCY_TRACE") != NULL;
	s.trace_audio = getenv("LOWLAT_AUDIO_TRACE") != NULL;
	// A DualShock 4 or a DualSense sent as its own report, which an
	// established host takes only in a mode its owner set; this library's
	// hosts take it as it is. The peer's kind is the application's to know,
	// and this one is told.
	s.raw_pads = getenv("LOWLAT_PAD_RAW") != NULL;
	s.raw_pads_only = s.raw_pads && strcmp(getenv("LOWLAT_PAD_RAW"), "only") == 0;

	s.decoder = strcmp(decoder, "none") == 0 ? LOWLAT_DECODER_NONE
		: strcmp(decoder, "open") == 0 ? LOWLAT_DECODER_OPEN
		: strcmp(decoder, "vendor") == 0 ? LOWLAT_DECODER_VENDOR
		: strcmp(decoder, "software") == 0 ? LOWLAT_DECODER_SOFTWARE
		: strcmp(decoder, "system") == 0 ? LOWLAT_DECODER_SYSTEM : LOWLAT_DECODER_AUTO;
	// Pictures as device handles the renderer imports, on a decoder that
	// exports them: on Linux the vendor's whatever was asked.
	s.handles = getenv("LOWLAT_HANDLE") != NULL;
#if defined(_WIN32)
	// The renderer starts on the GPU the decoder is named on, where its
	// shared textures open; with none named, on the system's first adapter,
	// named to the toolkit all the same, so the decoder can be created where
	// it is drawn (session.cc).
	unsigned luid_high = 0, luid_low = 0;
	if (sscanf(s.device, "luid:%8x:%8x", &luid_high, &luid_low) == 2)
		s.gfx_adapter = ((uint64_t) luid_high << 32) | luid_low;
	else if (s.device[0] == '\0')
		s.gfx_adapter = first_adapter();
#endif
#if defined(_WIN32)
	const char *gfx = env_or("LOWLAT_GFX", "d3d11");
#else
	const char *gfx = env_or("LOWLAT_GFX", "gl");
#endif
	s.gfx = gfx_named(gfx);
	if (s.gfx == MTY_GFX_NONE) {
		fprintf(stderr, "demo: LOWLAT_GFX names no graphics interface: %s\n", gfx);
		return 2;
	}
	if (s.handles && !draws_handles()) {
		fprintf(stderr, "demo: device handles are drawn through %s only; LOWLAT_GFX=%s takes planes\n",
			gfx_name(HANDLES_GFX), gfx);
		return 2;
	}
	// Without the refresh the capped loop, which does not wait for a
	// picture, would be paced by nothing at all.
	s.vsync = strcmp(env_or("LOWLAT_VSYNC", "1"), "0") != 0;
	if (!s.vsync && present_hz > 0) {
		fprintf(stderr, "demo: LOWLAT_PRESENT_HZ is paced by the display's refresh and needs vsync\n");
		return 2;
	}
	// The port and its mapping: mapped here, as a person running a client
	// expects, unless asked not to.
	unsigned long port = strtoul(env_or("LOWLAT_PORT", "0"), NULL, 10);
	if (port > UINT16_MAX) {
		fprintf(stderr, "demo: LOWLAT_PORT is past the last port: %lu\n", port);
		return 2;
	}
	s.port = (uint16_t) port;
	s.port_seed = env_or("LOWLAT_PORT_SEED", "");
	s.port_mapping = strcmp(env_or("LOWLAT_PORT_MAPPING", "1"), "0") != 0;
	s.decoder_index = getenv("LOWLAT_DECODER_INDEX");

	s.video.hevc = getenv("LOWLAT_HEVC") != NULL;
	s.video.ten_bit = getenv("LOWLAT_10BIT") != NULL;
	s.video.chroma_444 = getenv("LOWLAT_444") != NULL;
	// The renderer converts with each picture's range, so it takes either.
	const char *range = getenv("LOWLAT_FULL_RANGE");
	s.video.full_range = range == NULL || strcmp(range, "0") != 0;
	// A size asked of the host's display, `WxH`, which a host that sizes its
	// own display takes from an owner arriving into an empty room; none by
	// default, since it changes the person's monitor.
	unsigned width = 0, height = 0;
	if (sscanf(env_or("LOWLAT_RESOLUTION", ""), "%ux%u", &width, &height) == 2) {
		s.video.resolution_x = width;
		s.video.resolution_y = height;
	}

	s.stun = getenv("LOWLAT_STUN");
	s.raw_audio = getenv("LOWLAT_RAW_AUDIO") != NULL;
	s.legacy_cipher = getenv("LOWLAT_LEGACY_CIPHER") != NULL;
	s.ipv4_only = getenv("LOWLAT_IPV4_ONLY") != NULL;
	s.relay = env_or("LOWLAT_RELAY", "");
	s.relay_user = env_or("LOWLAT_RELAY_USER", "");
	const char *carrier = env_or("LOWLAT_RELAY_CARRIER", "udp");
	if (strcmp(carrier, "udp") == 0) {
		s.relay_carrier = LOWLAT_RELAY_CARRIER_UDP;
	} else if (strcmp(carrier, "tcp") == 0) {
		s.relay_carrier = LOWLAT_RELAY_CARRIER_TCP;
	} else {
		fprintf(stderr, "demo: LOWLAT_RELAY_CARRIER is udp or tcp: %s\n", carrier);
		return 2;
	}
	s.nat = strcmp(env_or("LOWLAT_NAT", "0"), "0") != 0;
	s.mic_hz = atof(env_or("LOWLAT_MIC_TONE", "0"));
	s.dump_frame = getenv("LOWLAT_DUMP_FRAME");
	return 0;
}

const Settings &get()
{
	return _settings;
}

}
