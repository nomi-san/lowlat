cmake_minimum_required(VERSION 3.21)

# The client demo on Windows: the C++ example on the application toolkit and SDL, built as an
# application that links the library into itself is -- the library from its static build, the
# static runtime, the same presets -- with its own code optimised across its files at the link
# in Release. The GNU build is GNUmakefile beside this one.
project(lowlat_demo LANGUAGES CXX)

if(NOT WIN32)
    message(FATAL_ERROR "This is the Windows build; elsewhere the demo is GNUmakefile's")
endif()

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# The static runtime, as the library and the toolkit link it, so nothing asks the machine for
# a redistributable.
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")

# The demo's own code optimised whole at the link in Release: /GL and /LTCG with cl,
# -flto=thin with clang-cl and its own linker. The library comes whole-optimised already.
set(CMAKE_INTERPROCEDURAL_OPTIMIZATION_RELEASE ON)

get_filename_component(ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.." ABSOLUTE)
set(MATOYA "${ROOT}/third_party/matoya")
set(MATOYA_LIB "${MATOYA}/bin/windows/x64/matoya.lib")
set(LOWLAT_LIB "${ROOT}/target/release/lowlat.lib" CACHE FILEPATH
    "The library's static build; made here when it is this checkout's")

# --- the toolkit ------------------------------------------------------------------------------
# Built once, from its own tree with its own makefile, when this configure finds none: that
# takes a developer environment's nmake and fxc, Khronos' glslangValidator on the path, and the
# platform it names its output directory by. Once it is there nothing builds it again.
if(NOT EXISTS "${MATOYA_LIB}")
    add_custom_command(OUTPUT "${MATOYA_LIB}"
        COMMAND nmake /nologo
        WORKING_DIRECTORY "${MATOYA}"
        COMMENT "The toolkit, built once from its own tree"
        VERBATIM)
    add_custom_target(matoya_build DEPENDS "${MATOYA_LIB}")
endif()
# Its header is C and outside the demo's warnings, which call what C allows there an extension.
add_library(matoya STATIC IMPORTED)
set_target_properties(matoya PROPERTIES
    IMPORTED_LOCATION "${MATOYA_LIB}"
    INTERFACE_INCLUDE_DIRECTORIES "${MATOYA}/src")

# --- the library ------------------------------------------------------------------------------
# This checkout's static build, asked of cargo on every build: it is built when a crate changed
# and answered at once when nothing did, and the demo relinks only when it changed. It names the
# system libraries it calls into itself. It carries the static release runtime, so a debug link
# sets that runtime's name aside for the debug one.
if(LOWLAT_LIB STREQUAL "${ROOT}/target/release/lowlat.lib")
    add_custom_target(lowlat_build
        COMMAND cargo rustc --manifest-path "${ROOT}/Cargo.toml" -p lowlat-sdk --release
            --crate-type staticlib
        BYPRODUCTS "${LOWLAT_LIB}"
        COMMENT "The library's static build"
        VERBATIM)
endif()
add_library(lowlat STATIC IMPORTED)
set_target_properties(lowlat PROPERTIES
    IMPORTED_LOCATION "${LOWLAT_LIB}"
    INTERFACE_LINK_OPTIONS "$<$<CONFIG:Debug>:/NODEFAULTLIB:libcmt.lib>")

# --- SDL --------------------------------------------------------------------------------------
# The pads and the sound, static from vcpkg: vcpkg.json beside this file names the version, the
# presets the toolchain and the static triplet.
find_package(SDL3 CONFIG REQUIRED)

# --- the demo ---------------------------------------------------------------------------------
add_executable(client
    main.cc
    app.cc
    clocks.cc
    config.cc
    input.cc
    logs.cc
    pads.cc
    present.cc
    report.cc
    session.cc
    signaling.cc
    sound.cc
    view.cc)

# The library's header is the demo's to be warned about; the toolkit's comes in as a system one.
# Windows builds of the library carry the client half alone, so the host half stays undeclared.
target_include_directories(client PRIVATE "${ROOT}/include")
target_compile_definitions(client PRIVATE _CRT_SECURE_NO_WARNINGS LOWLAT_NO_HOST)
target_compile_options(client PRIVATE /W4 /WX /wd4100)
# clang-cl counts the members a designated initializer leaves out, which C++ zeroes by
# definition: the library's structs are set up by their size alone.
if(CMAKE_CXX_COMPILER_ID STREQUAL "Clang")
    target_compile_options(client PRIVATE -Wno-missing-designated-field-initializers)
endif()
# Release writes its symbols beside it, for a crash dump read against the demo as shipped: the
# debug information in the objects and a PDB from the link, which still drops unreferenced code.
target_compile_options(client PRIVATE $<$<CONFIG:Release>:/Z7>)
target_link_options(client PRIVATE $<$<CONFIG:Release>:/DEBUG /OPT:REF /OPT:ICF>)

# SDL names what it calls into itself; the rest is what the toolkit calls into on this platform.
target_link_libraries(client PRIVATE lowlat matoya SDL3::SDL3
    user32 gdi32 shell32 ole32 oleaut32 uuid advapi32 ws2_32 bcrypt crypt32 winmm wininet
    winhttp userenv setupapi cfgmgr32 iphlpapi shlwapi imm32 hid dxgi d3d11 d3d12 d3dcompiler
    dxguid dwmapi dwrite propsys shcore xinput windowscodecs)

if(TARGET matoya_build)
    add_dependencies(client matoya_build)
endif()
if(TARGET lowlat_build)
    add_dependencies(client lowlat_build)
endif()
