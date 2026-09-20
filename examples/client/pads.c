// The Sony pads read raw from their own nodes. See pads.h.

#include "pads.h"

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include "matoya.h"

#define VENDOR_SONY 0x054C
#define BUS_USB 3
#define BUS_BLUETOOTH 5
// The pads are named to the library apart from the toolkit's controller
// numbers, so a rumble message can be told to the right side, and **below
// 256**: an established host keys its pads on the low eight bits of the
// identifier for the state, button, axis and unplug messages and on the
// whole identifier for the report message, so a report under a wider
// identifier never reaches the pad the states made (docs/01-protocol.md
// 11.1), and its rumble comes back under the eight-bit one. The toolkit
// numbers its controllers by descriptor, so this starts well above where
// those sit.
#define ID_BASE 200
#define SCAN_EVERY_MS 1000.0

// The raw node's feature-report reads and writes.
#define HIDIOCGFEATURE(len) _IOC(_IOC_WRITE | _IOC_READ, 'H', 0x07, len)
#define HIDIOCSFEATURE(len) _IOC(_IOC_WRITE | _IOC_READ, 'H', 0x06, len)

// The two feature reports the host's driver asks for, per product and
// transport: calibration and firmware, identifier and length.
struct feature {
	uint8_t id;
	uint32_t len;
};

static void features_of(uint32_t type, bool wireless, struct feature out[2])
{
	if (type == LOWLAT_PAD_TYPE_DS4) {
		out[0] = (struct feature) {wireless ? 0x05 : 0x02, wireless ? 41 : 37};
		out[1] = (struct feature) {0xA3, 49};
	} else {
		out[0] = (struct feature) {0x05, 41};
		out[1] = (struct feature) {0x20, 64};
	}
}

static uint32_t type_of(unsigned product)
{
	switch (product) {
		case 0x05C4:
		case 0x09CC: return LOWLAT_PAD_TYPE_DS4;
		case 0x0CE6: return LOWLAT_PAD_TYPE_DS5;
		default: return 0;
	}
}

// The node's identity from its device's uevent: bus, vendor, product, and
// whether it is one of a host's own virtual pads. A demo run on the host's
// machine would otherwise read the pad the host made from its reports and
// send it back, which is an echo.
static bool identity(unsigned node, unsigned *bus, unsigned *vendor, unsigned *product,
	bool *virtual_pad)
{
	char path[96];
	snprintf(path, sizeof path, "/sys/class/hidraw/hidraw%u/device/uevent", node);
	FILE *f = fopen(path, "r");
	if (f == NULL)
		return false;
	bool found = false;
	*virtual_pad = false;
	char line[160];
	while (fgets(line, sizeof line, f) != NULL) {
		if (sscanf(line, "HID_ID=%x:%x:%x", bus, vendor, product) == 3)
			found = true;
		if (strncmp(line, "HID_PHYS=lowlat/", 16) == 0)
			*virtual_pad = true;
	}
	fclose(f);
	return found;
}

static struct raw_pad *by_id(struct raw_pads *r, uint32_t id)
{
	for (size_t i = 0; i < RAW_PADS; i++)
		if (r->pads[i].fd >= 0 && r->pads[i].id == id)
			return &r->pads[i];
	return NULL;
}

static void drop(struct raw_pads *r, struct raw_pad *p, lowlat_client *client)
{
	printf("demo: pad %u on hidraw%u gone\n", p->id, p->node);
	close(p->fd);
	p->fd = -1;
	if (client != NULL)
		lowlat_client_send_pad_unplug(client, p->id);
}

// The feature reports, read from the pad and sent ahead of its first input
// report; one the pad does not answer is left to the host's default.
static void send_features(struct raw_pads *r, struct raw_pad *p, lowlat_client *client)
{
	struct feature features[2];
	features_of(p->type, p->wireless, features);
	for (size_t i = 0; i < 2; i++) {
		uint8_t buf[64];
		memset(buf, 0, sizeof buf);
		buf[0] = features[i].id;
		int got = ioctl(p->fd, HIDIOCGFEATURE(features[i].len), buf);
		if (got <= 0) {
			printf("demo: pad %u feature 0x%02x not answered (%s)\n", p->id, features[i].id,
				strerror(errno));
			continue;
		}
		lowlat_status s = lowlat_client_send_pad_report(client, p->id, p->type,
			LOWLAT_PAD_REPORT_FEATURE, buf, (uint32_t) got);
		if (r->trace || s != LOWLAT_OK)
			printf("demo: pad %u feature 0x%02x %d bytes -> %s\n", p->id, features[i].id, got,
				lowlat_status_string(s));
	}
}

void raw_pads_init(struct raw_pads *r, bool trace)
{
	memset(r, 0, sizeof *r);
	for (size_t i = 0; i < RAW_PADS; i++)
		r->pads[i].fd = -1;
	r->trace = trace;
}

void raw_pads_scan(struct raw_pads *r, lowlat_client *client, double now_ms, bool established)
{
	if (!established || (r->scanned_ms != 0.0 && now_ms - r->scanned_ms < SCAN_EVERY_MS))
		return;
	r->scanned_ms = now_ms;

	DIR *dir = opendir("/sys/class/hidraw");
	if (dir == NULL)
		return;
	struct dirent *entry;
	while ((entry = readdir(dir)) != NULL) {
		unsigned node;
		if (sscanf(entry->d_name, "hidraw%u", &node) != 1)
			continue;
		bool open_already = false;
		for (size_t i = 0; i < RAW_PADS; i++)
			if (r->pads[i].fd >= 0 && r->pads[i].node == node)
				open_already = true;
		if (open_already)
			continue;
		unsigned bus, vendor, product;
		bool virtual_pad;
		if (!identity(node, &bus, &vendor, &product, &virtual_pad) || vendor != VENDOR_SONY)
			continue;
		if (virtual_pad)
			continue;
		if ((bus != BUS_USB && bus != BUS_BLUETOOTH) || type_of(product) == 0)
			continue;
		struct raw_pad *slot = NULL;
		for (size_t i = 0; i < RAW_PADS && slot == NULL; i++)
			if (r->pads[i].fd < 0)
				slot = &r->pads[i];
		if (slot == NULL)
			break;
		char path[32];
		snprintf(path, sizeof path, "/dev/hidraw%u", node);
		int fd = open(path, O_RDWR | O_NONBLOCK | O_CLOEXEC);
		if (fd < 0) {
			// Once, not every second: the seat's access rule is what
			// grants it, and a node without it stays without it.
			static bool said;
			if (!said)
				printf("demo: %s not readable (%s); the seat's access rule grants it\n", path,
					strerror(errno));
			said = true;
			continue;
		}
		*slot = (struct raw_pad) {
			.fd = fd,
			.node = node,
			.id = ID_BASE + node,
			.type = type_of(product),
			.wireless = bus == BUS_BLUETOOTH,
			.seq = 0,
		};
		printf("demo: pad %u is %s %04x on hidraw%u over %s, read raw\n", slot->id,
			slot->type == LOWLAT_PAD_TYPE_DS4 ? "DualShock 4" : "DualSense", product, node,
			slot->wireless ? "Bluetooth" : "USB");
		send_features(r, slot, client);
	}
	closedir(dir);
}

void raw_pads_pump(struct raw_pads *r, lowlat_client *client)
{
	for (size_t i = 0; i < RAW_PADS; i++) {
		struct raw_pad *p = &r->pads[i];
		if (p->fd < 0)
			continue;
		// Everything queued, not one report a pass: a wired DualSense
		// reports several hundred times a second.
		for (;;) {
			uint8_t buf[LOWLAT_PAD_REPORT_MAX];
			ssize_t n = read(p->fd, buf, sizeof buf);
			if (n < 0) {
				if (errno == EAGAIN || errno == EINTR)
					break;
				drop(r, p, client);
				break;
			}
			if (n == 0) {
				drop(r, p, client);
				break;
			}
			lowlat_status s = lowlat_client_send_pad_report(client, p->id, p->type,
				LOWLAT_PAD_REPORT_INPUT, buf, (uint32_t) n);
			if (s == LOWLAT_OK)
				r->reports++;
			else if (r->trace)
				printf("demo: pad %u report %zd bytes id 0x%02x -> %s\n", p->id, n, buf[0],
					lowlat_status_string(s));
		}
	}
}

bool raw_pads_owns_vendor(const struct raw_pads *r, uint16_t vid)
{
	(void) r;
	return vid == VENDOR_SONY;
}

bool raw_pads_write(struct raw_pads *r, const lowlat_pad_report_event *e)
{
	struct raw_pad *p = by_id(r, e->pad);
	if (p == NULL)
		return false;
	int done;
	if (e->kind == LOWLAT_PAD_REPORT_FEATURE) {
		uint8_t buf[LOWLAT_PAD_REPORT_MAX];
		memcpy(buf, e->report, e->len);
		done = ioctl(p->fd, HIDIOCSFEATURE(e->len), buf);
	} else {
		done = (int) write(p->fd, e->report, e->len);
	}
	if (done < 0)
		printf("demo: pad %u write of %u bytes failed (%s)\n", p->id, (unsigned) e->len,
			strerror(errno));
	else
		r->outputs++;
	if (r->trace) {
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
	uint32_t crc = MTY_CRC32(0, &seed, 1);
	crc = MTY_CRC32(crc, report, len - 4);
	memcpy(report + len - 4, &crc, 4);
}

bool raw_pads_rumble(struct raw_pads *r, uint32_t pad, uint8_t large, uint8_t small)
{
	struct raw_pad *p = by_id(r, pad);
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
		report[at + 3] = small;    // right, the small one
		report[at + 4] = large;    // left, the large one
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
		report[at + 2] = small;    // right
		report[at + 3] = large;    // left
		len = p->wireless ? 78 : 48;
	}
	if (p->wireless)
		seal(report, len);
	if (write(p->fd, report, len) < 0)
		printf("demo: pad %u rumble write failed (%s)\n", p->id, strerror(errno));
	else
		r->outputs++;
	return true;
}

void raw_pads_close(struct raw_pads *r, lowlat_client *client)
{
	for (size_t i = 0; i < RAW_PADS; i++)
		if (r->pads[i].fd >= 0)
			drop(r, &r->pads[i], client);
}
