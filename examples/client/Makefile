# The client demo: the C example on the application toolkit.
#
#   make            builds the toolkit, the library and the demo
#   make run        runs it; LOWLAT_PEER and LOWLAT_SESSION must be set
#
# The toolkit is built from its own tree with its own makefile; the library
# is the SDK crate's release build; the demo links the two. Nothing here
# knows a package manager.

ROOT    := $(abspath ../..)
MATOYA  := $(ROOT)/third_party/matoya
ARCH    := $(shell uname -m)
TARGET  := $(ROOT)/target/release

CFLAGS  := -std=c11 -D_POSIX_C_SOURCE=200809L -O2 -Wall -Wextra -Wno-unused-parameter \
           -I$(ROOT)/include -I$(MATOYA)/src
LIBS    := $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a \
           -L$(TARGET) -llowlat -Wl,-rpath,$(TARGET) \
           -lc -ldl -lpthread -lm

SOURCES := main.c signaling.c

all: client

client: $(SOURCES) signaling.h keys.h $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a $(TARGET)/liblowlat.so
	$(CC) $(CFLAGS) -o $@ $(SOURCES) $(LIBS)

$(MATOYA)/bin/linux/$(ARCH)/libmatoya.a:
	TERM=xterm $(MAKE) -C $(MATOYA)

$(TARGET)/liblowlat.so: FORCE
	cd $(ROOT) && cargo build -p lowlat-sdk --release

run: client
	./client

clean:
	rm -f client

FORCE:

.PHONY: all run clean FORCE
