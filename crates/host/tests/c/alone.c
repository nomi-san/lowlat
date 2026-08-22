/* The header, and nothing else, in one translation unit.
 *
 * This is the whole of what "compiles standalone" means: a header that needs
 * something included before it works only in the file its author tried it in.
 * Compiled as C and as C++, warnings as errors, and it declares nothing of its
 * own so that the only thing under test is the header.
 */

#include "lowlat.h"

/* Twice on purpose. Including a header a second time is the only thing a guard
 * has to survive, and this one uses `#pragma once` rather than a macro name an
 * application could collide with. */
#include "lowlat.h"
