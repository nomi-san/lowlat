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

/* Twice on purpose. Including a header a second time is the only thing a guard
 * has to survive, and this one uses `#pragma once` rather than a macro name an
 * application could collide with. */
#include "lowlat.h"
