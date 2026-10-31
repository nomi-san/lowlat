// The demo's stamped lines. See logs.h.

#include "logs.h"

#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#include "lowlat.h"

#include "clocks.h"

namespace logs {

void line(const char *format, ...)
{
	char stamp[16];
	char text[512];
	clocks::wall_time(stamp, sizeof stamp);
	va_list args;
	va_start(args, format);
	vsnprintf(text, sizeof text, format, args);
	va_end(args);
	printf("%s demo: %s\n", stamp, text);
}

void candidate(const char *whose, const char *address, uint16_t port, bool lan, bool reflexive)
{
	bool v6 = strchr(address, ':') != NULL;
	line("%s candidate %s%s%s:%u%s%s", whose, v6 ? "[" : "", address, v6 ? "]" : "",
		(unsigned) port, lan ? " lan" : "", reflexive ? " reflexive" : "");
}

void library_line(lowlat_log_level level, const char *message, void *opaque) noexcept
{
	(void) opaque;
	char stamp[16];
	clocks::wall_time(stamp, sizeof stamp);
	fprintf(stderr, "%s %s%s\n", stamp, level <= LOWLAT_LOG_WARN ? "! " : "  ", message);
}

void toolkit_line(const char *message, void *opaque) noexcept
{
	(void) opaque;
	fprintf(stderr, "! toolkit: %s\n", message);
}

}
