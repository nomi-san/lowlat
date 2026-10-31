/* The header, and nothing else, in one translation unit.
 *
 * This is the whole of what "compiles standalone" means: a header that needs
 * something included before it works only in the file its author tried it in.
 * Compiled as C and as C++, warnings as errors, and it declares nothing of its
 * own so that the only thing under test is the header.
 */

#include "lowlat.h"

/* `static_assert` is the spelling both languages share: C11 gets it from here,
 * C++11 has it built in. */
#include <assert.h>

/* **A boolean field must be one byte**, which the C standard does not promise
 * and every ABI this targets does. If it is ever not, every field after one in
 * a structure moves and the two sides silently disagree about where everything
 * is -- so it is asserted at compile time rather than discovered by a guest
 * whose permissions came out wrong. */
static_assert(sizeof(bool) == 1, "a bool is not one byte on this target");

/* The other direction of the same worry is already covered elsewhere: every
 * configuration carries its own `sizeof` and the library refuses one smaller
 * than it expects, so a C translation unit that disagreed with the library
 * about a structure's size could not start a host at all. */

/* **Every enumeration a call returns or a structure holds is four bytes**,
 * whichever language reads the header: in C++ by the width it declares, in C
 * as the 32-bit integer its name is. One sized otherwise would put every field
 * after it at the wrong offset, so it is asserted here rather than discovered
 * at the first event. */
static_assert(sizeof(lowlat_status) == 4, "lowlat_status is not four bytes");
static_assert(sizeof(lowlat_event_type) == 4, "lowlat_event_type is not four bytes");
static_assert(sizeof(lowlat_outcome) == 4, "lowlat_outcome is not four bytes");
static_assert(sizeof(lowlat_codec) == 4, "lowlat_codec is not four bytes");
static_assert(sizeof(lowlat_rotation) == 4, "lowlat_rotation is not four bytes");
static_assert(sizeof(lowlat_pad_report) == 4, "lowlat_pad_report is not four bytes");
static_assert(sizeof(lowlat_log_level) == 4, "lowlat_log_level is not four bytes");
#if defined(LOWLAT_CLIENT)
static_assert(sizeof(lowlat_frame_kind) == 4, "lowlat_frame_kind is not four bytes");
static_assert(sizeof(lowlat_format) == 4, "lowlat_format is not four bytes");
static_assert(sizeof(lowlat_handle_kind) == 4, "lowlat_handle_kind is not four bytes");
static_assert(sizeof(lowlat_client_state) == 4, "lowlat_client_state is not four bytes");
static_assert(sizeof(lowlat_decoder_state) == 4, "lowlat_decoder_state is not four bytes");
static_assert(sizeof(lowlat_decoder) == 4, "lowlat_decoder is not four bytes");
static_assert(sizeof(lowlat_audio_codec) == 4, "lowlat_audio_codec is not four bytes");
static_assert(sizeof(lowlat_mapping) == 4, "lowlat_mapping is not four bytes");
static_assert(sizeof(lowlat_nat_state) == 4, "lowlat_nat_state is not four bytes");
static_assert(sizeof(lowlat_nat_type) == 4, "lowlat_nat_type is not four bytes");
static_assert(sizeof(lowlat_nat_mapping) == 4, "lowlat_nat_mapping is not four bytes");
static_assert(sizeof(lowlat_nat_source) == 4, "lowlat_nat_source is not four bytes");
#endif
#if defined(LOWLAT_HOST)
static_assert(sizeof(lowlat_chroma) == 4, "lowlat_chroma is not four bytes");
#endif

/* **In C++ the tag survives the typedef**, so an application may write either
 * `lowlat_status` or `enum lowlat_status` and mean the same type. Repeating a
 * typedef is legal only when it names an identical type, which is what this
 * asks. In C the name is the integer and the tag a separate enumeration, so
 * the two are not one type there and this is not asked. */
#if defined(__cplusplus)
typedef enum lowlat_status lowlat_status;
#endif

/* **The shared types are there whichever half is hidden**, and a half that is
 * hidden declares nothing: the seam's credentials and events belong to both,
 * the handles to one each. */
static_assert(sizeof(lowlat_credentials) > 0, "the credentials are missing");
static_assert(sizeof(lowlat_event) > 0, "the event is missing");
#if defined(LOWLAT_HOST)
typedef lowlat_host *lowlat_host_handle_is_declared;
#endif
#if defined(LOWLAT_CLIENT)
typedef lowlat_client *lowlat_client_handle_is_declared;
static_assert(sizeof(lowlat_client_status) > 0, "the client status is missing");
#endif

/* Twice on purpose. Including a header a second time is the only thing a guard
 * has to survive, and this one uses `#pragma once` rather than a macro name an
 * application could collide with. */
#include "lowlat.h"
