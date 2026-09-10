//! Which session owns the seat, asked of the login manager.
//!
//! **A session going inactive is not a session ending.** A user switch keeps
//! the first session alive, with its helper connected and its layout still
//! describing a desktop nobody is scanning out, and puts a greeter in front
//! of the display that has no helper at all. Which session the display
//! belongs to is the login manager's fact, so it is asked -- through its own
//! library, one symbol, opened at runtime like every other vendor interface
//! here -- rather than inferred from who is connected.

use std::ffi::{c_char, c_int};
use std::sync::OnceLock;

/// `int sd_seat_get_active(const char *seat, char **session, uid_t *uid)`.
type SeatGetActive =
    unsafe extern "C" fn(*const c_char, *mut *mut c_char, *mut libc::uid_t) -> c_int;

fn seat_get_active() -> Option<SeatGetActive> {
    static SYMBOL: OnceLock<Option<(lowlat_common::dynlib::Library, SeatGetActive)>> =
        OnceLock::new();
    SYMBOL
        .get_or_init(|| {
            let library = lowlat_common::dynlib::Library::open(c"libsystemd.so.0")?;
            // SAFETY: the signature above is the library's own declaration.
            let symbol: SeatGetActive = unsafe { library.symbol(c"sd_seat_get_active")? };
            Some((library, symbol))
        })
        .as_ref()
        .map(|(_, symbol)| *symbol)
}

/// The account whose session is in front of the display, or nothing where
/// no login manager answers.
///
/// **Nothing is not "nobody".** A machine without the library, or a seat
/// with no session on it yet, answers nothing, and the caller falls back to
/// what it knew rather than concluding the display is unowned.
pub(crate) fn active_uid() -> Option<u32> {
    let symbol = seat_get_active()?;
    let mut uid: libc::uid_t = 0;
    // SAFETY: the seat name is a terminated string, the session out-parameter
    // is null so nothing is allocated for us, and the uid is written in place.
    let rc = unsafe { symbol(c"seat0".as_ptr(), std::ptr::null_mut(), &raw mut uid) };
    (rc >= 0).then_some(uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Off by default: it needs a login manager and a seat.** What it
    /// asserts is that the answer is the account running this test, which is
    /// the one logged in at the desk.
    #[test]
    #[ignore = "needs a seat"]
    fn the_seat_belongs_to_whoever_is_logged_in() {
        // SAFETY: a plain read of this process's own identity.
        let me = unsafe { libc::getuid() };
        assert_eq!(active_uid(), Some(me));
    }
}
