/* lowlat - the public C ABI.
 *
 * Generated from the Rust definitions. Do not edit; see crates/host/cbindgen.toml.
 */

#ifndef LOWLAT_H
#define LOWLAT_H

#include <stdint.h>

// The major version, raised only when something already published changes.
#define LOWLAT_ABI_MAJOR 0

// The minor version, raised when surface is appended.
#define LOWLAT_ABI_MINOR 1

// A status code.
//
// **An integer with named constants rather than an enumeration**, and the
// reason is soundness rather than taste: a status travels back into this
// library -- ending a guest carries one as its reason -- and an application is
// free to hand back a number we never defined. Reading an undefined
// discriminant into a Rust enumeration is undefined behaviour, so the type
// that crosses the boundary is one where every bit pattern is valid.
//
// Zero succeeds, positive is a non-fatal condition, negative is an error, and
// the error space is partitioned by subsystem so that a number says where it
// came from without a lookup:
//
// ```text
//   -1 to -99      the boundary itself: arguments, state, contained faults
//   -100 to -199   signaling and admission
//   -200 to -299   capture
//   -300 to -399   encode
//   -400 to -499   transport
// ```
//
// A value is assigned once and never reused, including for a condition that
// is removed.
typedef int32_t lowlat_status;

// The call succeeded.
#define LOWLAT_OK 0

// No event arrived within the timeout. Not an error.
#define LOWLAT_TIMEOUT 1

// A fault was contained at the boundary. The handle no longer runs.
#define LOWLAT_ERR_INTERNAL -1

// An argument was missing, out of range, or contradicted another.
#define LOWLAT_ERR_INVALID_ARGUMENT -2

// The buffer was too small. What it would have taken has been written back,
// and nothing has been consumed.
#define LOWLAT_ERR_TOO_SMALL -3

// A previous call was contained at the boundary, so this handle is no longer
// trusted to describe its own state. Only destroying it still works.
#define LOWLAT_ERR_POISONED -4

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// Major and minor, packed.
//
// **The one function whose signature can never change**, because it is what a
// loader calls to decide whether it may call anything else.
uint32_t lowlat_abi_version(void);

// Describe a status.
//
// The pointer is to storage that outlives the library, so it is never freed
// and never copied out of. An unrecognised value is described as one rather
// than refused: this is what a caller reaches for while diagnosing, and
// returning nothing there is the least useful thing it could do.
const char *lowlat_status_string(lowlat_status status);

// Panic on purpose, and prove the boundary contains it.
//
// **Exported by the shipped library rather than hidden behind a build
// option**, because what has to be tested is that *this* object still
// unwinds. Building it to abort on panic silently disables containment
// everywhere, and the same code linked into a test binary answers for the
// test's build rather than for this one.
lowlat_status lowlat_debug_panic(void);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* LOWLAT_H */
