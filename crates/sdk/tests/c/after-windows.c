/* The header after the platform's own, included whole, on Windows.
 *
 * Windows' headers define plain words as macros -- `small`, `hyper`, `near`,
 * `far`, `interface` among them -- and an application that includes them whole,
 * as most do, has any such word in this header rewritten under it. The header
 * is compiled after them, as C and as C++, warnings as errors, so a field or a
 * parameter named with one of those words fails here and not in an
 * application's build.
 */

#include <windows.h>

#include "lowlat.h"
