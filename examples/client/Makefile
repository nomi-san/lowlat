# The client demo: the C example on the application toolkit.
#
#   make            builds the toolkit, the library and the demo
#   make run        runs it; LOWLAT_PEER and LOWLAT_SESSION must be set
#
# The toolkit is built from its own tree with its own makefile; the library
# is the SDK crate's release build; the demo links the two. Nothing here
# knows a package manager.
#
# LOWLAT_LIB names a directory holding a built liblowlat.so instead: no
# library is built, the demo links the one there, and its run path is
# ../lib relative to itself, which is where a packaged demo finds it.

ROOT    := $(abspath ../..)
MATOYA  := $(ROOT)/third_party/matoya
ARCH    := $(shell uname -m)
TARGET  := $(ROOT)/target/release

ifeq ($(LOWLAT_LIB),)
LIBDIR  := $(TARGET)
LIBDEP  := $(TARGET)/liblowlat.so
RPATH   := $(TARGET)
else
LIBDIR  := $(LOWLAT_LIB)
LIBDEP  :=
RPATH   := '$$ORIGIN/../lib'
endif

CFLAGS  := -std=c11 -D_POSIX_C_SOURCE=200809L -O2 -Wall -Wextra -Wno-unused-parameter \
           -I$(ROOT)/include -I$(MATOYA)/src
LIBS    := $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a \
           -L$(LIBDIR) -llowlat -Wl,-rpath,$(RPATH) \
           -lc -ldl -lpthread -lm

SOURCES := main.c signaling.c pads.c

all: client

client: $(SOURCES) signaling.h pads.h keys.h $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a $(LIBDEP)
	$(CC) $(CFLAGS) -o $@ $(SOURCES) $(LIBS)

$(MATOYA)/bin/linux/$(ARCH)/libmatoya.a:
	TERM=xterm $(MAKE) -C $(MATOYA)

$(TARGET)/liblowlat.so: FORCE
	cd $(ROOT) && cargo build -p lowlat-sdk --release

run: client
	./client

# A one-off: does the toolkit's GL context import what the compute runtime
# exports? Needs no library build.
probe-import: probe-import.c $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a
	$(CC) $(CFLAGS) -I$(MATOYA)/deps -o $@ probe-import.c \
		$(MATOYA)/bin/linux/$(ARCH)/libmatoya.a -lc -ldl -lpthread -lm

clean:
	rm -f client probe-import

FORCE:

.PHONY: all run clean FORCE
