//! Runtime loading of a shared library.
//!
//! Backends that need a vendor runtime resolve it here rather than linking it,
//! so a machine without that runtime has **a missing backend rather than a
//! process that will not start**. A link-time dependency on a driver library
//! turns "no GPU here" into a loader error before `main` runs.
//!
//! Two reasons this lives in `lowlat-common` rather than beside the backend
//! that first needed it. It is the one piece of the platform that differs
//! between the operating systems we target while the code above it does not,
//! so the split belongs at the bottom. And this crate is the only one in the
//! workspace containing `unsafe`, which is what keeps that obligation in one
//! auditable place; a loader written into each backend would spread it.
//!
//! **`miri` cannot reach any of this**, because it cannot execute a loader
//! syscall or run foreign code. The tests below check the observable contract
//! -- a library opens, a symbol resolves, an absent one reports absence -- and
//! the handle discipline is what review has to cover.

use core::ffi::{CStr, c_void};
use core::ptr::NonNull;

/// An open shared library. Closed on drop.
pub struct Library {
    handle: NonNull<c_void>,
}

impl core::fmt::Debug for Library {
    /// Deliberately opaque. The handle is an address in this process and
    /// printing it puts a mapping address into logs for no diagnostic gain.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Library(open)")
    }
}

// SAFETY: a library handle is a process-wide token, not thread-affine. Both
// platforms' loaders are internally synchronised, and neither the handle nor
// the symbol addresses derived from it depend on the calling thread. Sending a
// handle is what lets a backend be constructed on one thread and run on
// another, which is the arrangement the pipeline uses.
unsafe impl Send for Library {}
// SAFETY: as above, and every method taking `&self` only reads the handle.
unsafe impl Sync for Library {}

impl Library {
    /// Open a library by name, or by absolute path.
    ///
    /// Returns `None` if it is absent or fails to load. The caller knows which
    /// names it tried and is the one that can say so usefully in a log, which
    /// is why no error detail comes back: carrying one would mean either
    /// allocating or handing out a pointer into the loader's own buffer.
    pub fn open(name: &CStr) -> Option<Self> {
        let handle = imp::open(name);
        NonNull::new(handle).map(|handle| Self { handle })
    }

    /// Open the first name that loads.
    ///
    /// Vendor runtimes are versioned in their file name and the unversioned
    /// alias is frequently absent on a machine without the development
    /// package, so a backend needs to try a short list rather than one name.
    pub fn open_first(names: &[&CStr]) -> Option<Self> {
        names.iter().find_map(|name| Self::open(name))
    }

    /// Resolve a symbol, or `None` if the library does not export it.
    ///
    /// # Safety
    ///
    /// `T` must be a function pointer type whose signature, calling convention
    /// and lifetime expectations match the symbol as the library actually
    /// defines it. Nothing here can check that, and getting it wrong is
    /// undefined behaviour at the first call rather than at resolution.
    ///
    /// The returned pointer borrows the library: calling it after the
    /// `Library` is dropped is a use-after-unmap.
    pub unsafe fn symbol<T: Copy>(&self, name: &CStr) -> Option<T> {
        const {
            assert!(
                size_of::<T>() == size_of::<*mut c_void>(),
                "symbol type must be pointer sized"
            );
        }
        // SAFETY: the handle is valid for the life of `self`.
        let address = unsafe { imp::symbol(self.handle.as_ptr(), name) };
        if address.is_null() {
            return None;
        }
        // SAFETY: `address` is non-null and pointer sized, asserted above. The
        // caller carries the obligation that `T` describes it correctly.
        Some(unsafe { core::mem::transmute_copy::<*mut c_void, T>(&address) })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful open and is closed once,
        // because `Library` is neither `Copy` nor `Clone`.
        unsafe { imp::close(self.handle.as_ptr()) };
    }
}

#[cfg(unix)]
mod imp {
    use core::ffi::{CStr, c_void};

    /// Resolve every symbol at open rather than lazily.
    ///
    /// A library whose own dependencies are missing then fails here, where the
    /// caller can fall back to another backend, instead of at the first call
    /// through a function pointer, where it is a crash.
    ///
    /// Local, not global: this ships inside a shared library loaded into other
    /// people's processes, and adding a vendor runtime's symbols to the global
    /// namespace can capture lookups that were never meant for us.
    const FLAGS: i32 = libc::RTLD_NOW | libc::RTLD_LOCAL;

    pub(super) fn open(name: &CStr) -> *mut c_void {
        // SAFETY: `name` is a valid NUL-terminated string for the call.
        unsafe { libc::dlopen(name.as_ptr(), FLAGS) }
    }

    pub(super) unsafe fn symbol(handle: *mut c_void, name: &CStr) -> *mut c_void {
        // SAFETY: the caller guarantees `handle` is open; `name` is valid.
        unsafe { libc::dlsym(handle, name.as_ptr()) }
    }

    pub(super) unsafe fn close(handle: *mut c_void) {
        // SAFETY: the caller guarantees `handle` came from `open` and is
        // closed exactly once.
        unsafe { libc::dlclose(handle) };
    }
}

#[cfg(windows)]
mod imp {
    use core::ffi::{CStr, c_void};

    // The system loader is present in every process, so these need no crate
    // dependency and no link attribute.
    unsafe extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }

    pub(super) fn open(name: &CStr) -> *mut c_void {
        // SAFETY: `name` is a valid NUL-terminated string for the call.
        unsafe { LoadLibraryA(name.as_ptr().cast()) }
    }

    pub(super) unsafe fn symbol(handle: *mut c_void, name: &CStr) -> *mut c_void {
        // SAFETY: the caller guarantees `handle` is open; `name` is valid.
        unsafe { GetProcAddress(handle, name.as_ptr().cast()) }
    }

    pub(super) unsafe fn close(handle: *mut c_void) {
        // SAFETY: the caller guarantees `handle` came from `open` and is
        // closed exactly once.
        unsafe { FreeLibrary(handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Present on any glibc system, which is every platform this workspace is
    /// built and tested on. Versioned, because the unversioned alias is a
    /// linker script rather than an object and will not load.
    const LIBC: &CStr = c"libc.so.6";

    #[test]
    fn opens_a_library_and_resolves_a_symbol() {
        let library = Library::open(LIBC).expect("libc did not open");
        // SAFETY: the signature is not called, only resolved, and the test
        // asserts resolution rather than invocation.
        let symbol: Option<unsafe extern "C" fn(usize) -> *mut c_void> =
            unsafe { library.symbol(c"malloc") };
        assert!(symbol.is_some(), "a symbol libc certainly exports");
    }

    /// The counterpart that proves the test above is not vacuous: the same
    /// call against a name no library exports must report absence rather than
    /// hand back something.
    #[test]
    fn an_absent_symbol_reports_none() {
        let library = Library::open(LIBC).expect("libc did not open");
        // SAFETY: as above; resolution only.
        let symbol: Option<unsafe extern "C" fn()> =
            unsafe { library.symbol(c"lowlat_no_such_symbol_exists") };
        assert!(symbol.is_none());
    }

    #[test]
    fn an_absent_library_reports_none() {
        assert!(Library::open(c"liblowlat-no-such-library.so.999").is_none());
    }

    /// A backend hands over a short list because the versioned name is the one
    /// that exists on a machine without the development package. The first
    /// entry failing must not abandon the search.
    #[test]
    fn open_first_skips_names_that_do_not_load() {
        let library = Library::open_first(&[c"liblowlat-absent.so.999", LIBC]);
        assert!(library.is_some(), "the fallback name was never tried");
        assert!(Library::open_first(&[c"liblowlat-absent.so.999"]).is_none());
        assert!(Library::open_first(&[]).is_none());
    }

    #[test]
    fn a_handle_crosses_threads() {
        let library = Library::open(LIBC).expect("libc did not open");
        std::thread::spawn(move || drop(library))
            .join()
            .expect("the handle did not survive the move");
    }
}
