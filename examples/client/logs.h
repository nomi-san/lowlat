// The demo's stamped lines, and the sinks for the library's and the toolkit's.
// Any thread: each line is written in one call.

#pragma once

#include <stdint.h>

#include "lowlat.h"

namespace logs {

// A line of the demo's own, stamped with the time of day. Formatted whole
// and written in one call, so a line from another thread cannot land
// inside it.
void line(const char *format, ...);

// A candidate as it goes to the host or comes from it, an IPv6 address
// bracketed so its port does not read as one more group of it.
void candidate(const char *whose, const char *address, uint16_t port, bool lan, bool reflexive);

// Every function the library or the toolkit calls back is noexcept: nothing
// may unwind through their frames, so a throw ends the process where it is
// thrown -- running out of memory for the pointer's picture among them.
void library_line(lowlat_log_level level, const char *message, void *opaque) noexcept;

// The toolkit's own lines, which say when a draw could not be made: a
// picture that failed to import would otherwise leave the last one on the
// screen with every counter looking healthy.
void toolkit_line(const char *message, void *opaque) noexcept;

}
