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

/* **The three enumerations that are types are four bytes.** The rest only name
 * the values of fields carried as plain integers, so their width reaches
 * nothing; these three do -- one is returned by almost every call, and two are
 * structure fields, where a translation unit that sized one differently would
 * read every field after it from the wrong offset. C leaves the width to the
 * implementation and `-fshort-enums` takes it, so it is asserted here rather
 * than discovered at the first event. */
static_assert(sizeof(lowlat_status) == 4, "lowlat_status is not four bytes");
static_assert(sizeof(lowlat_event_type) == 4, "lowlat_event_type is not four bytes");
static_assert(sizeof(lowlat_outcome) == 4, "lowlat_outcome is not four bytes");

/* **The tag survives the typedef**, so an application may write either
 * `lowlat_status` or `enum lowlat_status` and mean the same type, and may
 * forward-declare one. Repeating a typedef is legal only when it names an
 * identical type, in C11 and in C++ alike, which is what this asks. */
typedef enum lowlat_status lowlat_status;

/* Twice on purpose. Including a header a second time is the only thing a guard
 * has to survive, and this one uses `#pragma once` rather than a macro name an
 * application could collide with. */
#include "lowlat.h"
