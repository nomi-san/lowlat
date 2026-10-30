# The client demo: the C++ example on the application toolkit and SDL.
#
#   make            builds the toolkit, the library and the demo
#   make run        runs it; LOWLAT_PEER and LOWLAT_SESSION must be set
#
# The toolkit is built from its own tree with its own makefile; the library
# is the SDK crate's release build; SDL, for the pads and the sound, is the
# system's, found by pkg-config; the demo links the three. Nothing here
# knows a package manager.
#
# LOWLAT_LIB names a directory holding a built liblowlat.so (liblowlat.dylib
# on macOS) instead: no library is built, the demo links the one there, and
# its run path is ../lib relative to itself, which is where a packaged demo
# finds it.
#
# On macOS the demo is built for the machine's architecture or the one ARCH
# names -- `make ARCH=x86_64` on Apple Silicon, run under Rosetta -- and the
# toolkit and the library for the same. The toolkit builds its objects in its
# own tree, so one architecture's archive is there at a time.

ROOT    := $(abspath ../..)
MATOYA  := $(ROOT)/third_party/matoya
ARCH    := $(shell uname -m)

ifeq ($(shell uname -s),Darwin)
TRIPLE  := $(if $(filter arm64,$(ARCH)),aarch64,$(ARCH))-apple-darwin
TARGET  := $(ROOT)/target/$(TRIPLE)/release
TOOLKIT := $(MATOYA)/bin/macosx/$(ARCH)/libmatoya.a
OBJECT  := liblowlat.dylib
ORIGIN  := @loader_path
CARGO_FLAGS := --target $(TRIPLE)
ARCH_FLAGS  := -arch $(ARCH)
# The system's own names beside the POSIX level the C probes ask for, which
# hides them here: the loopback's address among them.
C_SOURCE    := -D_DARWIN_C_SOURCE
# The toolkit's frameworks; a static SDL names its own in its package's
# static list. The C++ runtime is the system's.
SYSLIBS := -framework AppKit -framework Carbon -framework IOKit -framework Metal \
           -framework QuartzCore -framework WebKit
SDL_STATIC  := --static
CXX_RUNTIME :=
else
TARGET  := $(ROOT)/target/release
TOOLKIT := $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a
OBJECT  := liblowlat.so
ORIGIN  := $$ORIGIN
CARGO_FLAGS :=
ARCH_FLAGS  :=
C_SOURCE    :=
SYSLIBS := -lc -ldl -lpthread -lm
SDL_STATIC  :=
# The C++ runtime is linked in, so a packaged demo needs no particular one on
# the machine it lands on.
CXX_RUNTIME := -static-libstdc++
endif

ifeq ($(LOWLAT_LIB),)
LIBDIR  := $(TARGET)
LIBDEP  := $(TARGET)/$(OBJECT)
RPATH   := $(TARGET)
else
LIBDIR  := $(LOWLAT_LIB)
LIBDEP  :=
RPATH   := '$(ORIGIN)/../lib'
endif

# g++ counts the members a designated initializer leaves out, which C++ zeroes
# by definition: the library's structs are set up by their size alone.
CXXFLAGS := -std=c++20 -O2 -Wall -Wextra -Wno-unused-parameter -Wno-missing-field-initializers \
           -I$(ROOT)/include -I$(MATOYA)/src $(ARCH_FLAGS)
# The one-off probes below are C.
CFLAGS  := -std=c11 -D_POSIX_C_SOURCE=200809L $(C_SOURCE) -O2 -Wall -Wextra -Wno-unused-parameter \
           -I$(ROOT)/include -I$(MATOYA)/src $(ARCH_FLAGS)
LIBS    := $(TOOLKIT) -L$(LIBDIR) -llowlat -Wl,-rpath,$(RPATH)
# Asked only when the demo is built: the probes and the check need no SDL. A
# static SDL (scripts/build-sdl3.sh) takes the system's libraries after it.
SDL_CFLAGS = $(shell pkg-config --cflags sdl3)
SDL_LIBS   = $(shell pkg-config $(SDL_STATIC) --libs sdl3)

SOURCES := main.cc app.cc clocks.cc config.cc input.cc logs.cc pads.cc present.cc report.cc \
           session.cc signaling.cc sound.cc view.cc
HEADERS := app.h clocks.h config.h input.h keys.h logs.h pads.h present.h report.h session.h \
           signaling.h sound.h view.h

all: client

client: $(SOURCES) $(HEADERS) $(TOOLKIT) $(LIBDEP)
	$(CXX) $(CXXFLAGS) $(SDL_CFLAGS) -o $@ $(SOURCES) $(LIBS) $(SDL_LIBS) $(SYSLIBS) \
		$(CXX_RUNTIME)

# **Built again when anything it is built from has changed**, not only when
# it is missing: an archive kept across a change to the toolkit's headers
# links a layout the demo's sources no longer agree with, and neither the
# compiler nor the linker says so. Its own makefile rebuilds the whole toolkit
# every time it runs, about a second, so this decides only whether to run it.
MATOYA_SOURCES := $(shell find $(MATOYA)/src $(MATOYA)/deps -type f \( -name '*.c' -o -name '*.h' \
                  -o -name '*.m' -o -name '*.vert' -o -name '*.frag' -o -name '*.vertvk' \
                  -o -name '*.fragvk' -o -name '*.metal' \))

$(TOOLKIT): $(MATOYA_SOURCES)
	TERM=xterm $(MAKE) -C $(MATOYA) ARCH=$(ARCH)

$(TARGET)/$(OBJECT): FORCE
	cd $(ROOT) && cargo build -p lowlat-sdk --release $(CARGO_FLAGS)

run: client
	./client

# A one-off: does the toolkit's GL context import what the compute runtime
# exports? Needs no library build. Linux's.
probe-import: probe-import.c $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a
	$(CC) $(CFLAGS) -I$(MATOYA)/deps -o $@ probe-import.c \
		$(MATOYA)/bin/linux/$(ARCH)/libmatoya.a -lc -ldl -lpthread -lm

# The toolkit's websocket reader against a loopback server that sends
# messages in fragments. Needs no library build and no network.
ws-check: ws-check.c $(TOOLKIT)
	$(CC) $(CFLAGS) -o $@ ws-check.c $(TOOLKIT) $(SYSLIBS)

check: ws-check
	./ws-check

clean:
	rm -f client probe-import ws-check

FORCE:

.PHONY: all run clean FORCE
