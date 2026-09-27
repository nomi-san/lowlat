# The client demo on Windows: the C example on the application toolkit, built
# with Visual Studio's own tools from its x64 developer environment. The GNU
# build is GNUmakefile beside this one.
#
#   nmake           builds the toolkit, the library and the demo
#   nmake run       runs it; LOWLAT_PEER and LOWLAT_SESSION must be set
#
# The toolkit is built once, from its own tree with its own makefile, which
# needs its shader compiler (glslangValidator) on the path; the library is the
# SDK crate's release build; the demo links the two. The library's DLL is
# copied beside the demo, the first place the system's loader looks. The C
# runtime is linked statically in every build, as the library links it, so
# nothing asks the machine for a redistributable.
#
# LOWLAT_LIB names a directory holding a built lowlat.dll and its import
# library, lowlat.dll.lib, instead: no library is built, and the DLL is copied
# from there.

ROOT = ..\..
MATOYA = $(ROOT)\third_party\matoya
TARGET = $(ROOT)\target\release
MATOYA_LIB = $(MATOYA)\bin\windows\x64\matoya.lib

!IFNDEF LOWLAT_LIB
LIBDIR = $(TARGET)
LIBDEP = $(TARGET)\lowlat.dll
!ELSE
LIBDIR = $(LOWLAT_LIB)
LIBDEP =
!ENDIF

# C17 with the standard's atomics, which this compiler still calls
# experimental; its advice against the standard library's string and file
# calls is declined, as the toolkit's own build declines it.
CFLAGS = /nologo /std:c17 /experimental:c11atomics /O2 /MT /W4 /wd4100 \
	/D_CRT_SECURE_NO_WARNINGS /I$(ROOT)\include /I$(MATOYA)\src

# What the toolkit calls into on this platform.
SYSTEM = user32.lib gdi32.lib shell32.lib ole32.lib oleaut32.lib uuid.lib \
	advapi32.lib ws2_32.lib bcrypt.lib crypt32.lib winmm.lib wininet.lib \
	winhttp.lib userenv.lib setupapi.lib cfgmgr32.lib iphlpapi.lib shlwapi.lib \
	imm32.lib hid.lib dxgi.lib d3d11.lib d3d12.lib d3dcompiler.lib dxguid.lib \
	dwmapi.lib dwrite.lib propsys.lib shcore.lib xinput.lib windowscodecs.lib

SOURCES = main.c signaling.c pads.c

all: client.exe lowlat.dll

client.exe: $(SOURCES) signaling.h pads.h keys.h $(MATOYA_LIB) $(LIBDEP)
	cl $(CFLAGS) /Fe$@ $(SOURCES) /link $(MATOYA_LIB) $(LIBDIR)\lowlat.dll.lib $(SYSTEM)

lowlat.dll: $(LIBDEP)
	copy /y $(LIBDIR)\lowlat.dll .

$(MATOYA_LIB):
	cmd /c "cd /d $(MATOYA) && nmake /nologo"

$(TARGET)\lowlat.dll: FORCE
	cargo build --manifest-path $(ROOT)\Cargo.toml -p lowlat-sdk --release

run: all
	client.exe

clean:
	-del /q client.exe main.obj signaling.obj pads.obj lowlat.dll 2>nul

FORCE:
