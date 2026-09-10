//! Let one account into a directory, by name.
//!
//! **The session's runtime directory is the user's alone**, and the sound
//! server's socket lives inside it. Root traverses it regardless; the account
//! a service runs as does not, and pointing it at the socket answers "refused"
//! (docs/07-platforms.md section 6). So the session lends the way in: an
//! access-list entry granting traverse -- execute on a directory, nothing
//! more -- to exactly the account the service connected as. It is the
//! session's own directory and the session's own decision, which is what a
//! helper is for.
//!
//! **Written as the attribute the kernel keeps it in**, because the list is
//! four kinds of entry in a fixed order and nothing about it wants a
//! library. Where no list exists yet, one is made from the mode bits, which
//! is what the kernel would report for it.

use std::ffi::{CStr, CString};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

const ATTRIBUTE: &CStr = c"system.posix_acl_access";
const VERSION: u32 = 2;

const USER_OBJ: u16 = 0x01;
const USER: u16 = 0x02;
const GROUP_OBJ: u16 = 0x04;
const GROUP: u16 = 0x08;
const MASK: u16 = 0x10;
const OTHER: u16 = 0x20;
const UNDEFINED: u32 = u32::MAX;

const EXECUTE: u16 = 0o1;

/// Grant `uid` traverse on the directory at `path`, keeping everything else
/// the list already says.
pub(crate) fn grant_traverse(path: &Path, uid: u32) -> std::io::Result<()> {
    let mode = u16::try_from(std::fs::metadata(path)?.mode() & 0o777).unwrap_or(0);
    let existing = read(path)?;
    let entries = match existing {
        Some(bytes) => parse(&bytes).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "an access list not understood",
            )
        })?,
        None => from_mode(mode),
    };
    write(path, &encode(&granted(entries, uid, EXECUTE)))
}

/// One entry: what kind, what it allows, and whom it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    tag: u16,
    perm: u16,
    id: u32,
}

/// The list a directory with no list would report: its mode bits.
fn from_mode(mode: u16) -> Vec<Entry> {
    vec![
        Entry {
            tag: USER_OBJ,
            perm: (mode >> 6) & 7,
            id: UNDEFINED,
        },
        Entry {
            tag: GROUP_OBJ,
            perm: (mode >> 3) & 7,
            id: UNDEFINED,
        },
        Entry {
            tag: OTHER,
            perm: mode & 7,
            id: UNDEFINED,
        },
    ]
}

/// The list with `uid` allowed `perm`, and the mask widened to cover it.
///
/// **The mask bounds every named entry**, so an entry the mask does not
/// cover is an entry that grants nothing; a list that had none gets one,
/// which is what a tool doing the same would add.
fn granted(mut entries: Vec<Entry>, uid: u32, perm: u16) -> Vec<Entry> {
    entries.retain(|entry| !(entry.tag == USER && entry.id == uid));
    entries.push(Entry {
        tag: USER,
        perm,
        id: uid,
    });
    let covered = entries
        .iter()
        .filter(|entry| matches!(entry.tag, GROUP_OBJ | USER | GROUP))
        .fold(0, |mask, entry| mask | entry.perm);
    match entries.iter_mut().find(|entry| entry.tag == MASK) {
        Some(mask) => mask.perm |= perm,
        None => entries.push(Entry {
            tag: MASK,
            perm: covered,
            id: UNDEFINED,
        }),
    }
    entries.sort_by_key(|entry| (entry.tag, entry.id));
    entries
}

fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = VERSION.to_ne_bytes().to_vec();
    for entry in entries {
        out.extend_from_slice(&entry.tag.to_ne_bytes());
        out.extend_from_slice(&entry.perm.to_ne_bytes());
        out.extend_from_slice(&entry.id.to_ne_bytes());
    }
    out
}

fn parse(bytes: &[u8]) -> Option<Vec<Entry>> {
    let version = u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    if version != VERSION || (bytes.len() - 4) % 8 != 0 {
        return None;
    }
    bytes
        .get(4..)?
        .chunks_exact(8)
        .map(|chunk| {
            Some(Entry {
                tag: u16::from_ne_bytes(chunk.get(..2)?.try_into().ok()?),
                perm: u16::from_ne_bytes(chunk.get(2..4)?.try_into().ok()?),
                id: u32::from_ne_bytes(chunk.get(4..8)?.try_into().ok()?),
            })
        })
        .collect()
}

/// The list as stored, or nothing where the directory has none.
fn read(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a path with a nul"))?;
    let mut buffer = vec![0u8; 4096];
    // SAFETY: the path is a terminated string and the buffer is as long as it
    // says it is.
    let length = unsafe {
        libc::getxattr(
            path.as_ptr(),
            ATTRIBUTE.as_ptr(),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            buffer.len(),
        )
    };
    if length < 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENODATA) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    buffer.truncate(usize::try_from(length).unwrap_or(0));
    Ok(Some(buffer))
}

fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a path with a nul"))?;
    // SAFETY: the path is a terminated string and the bytes are as long as
    // they say they are.
    let rc = unsafe {
        libc::setxattr(
            path.as_ptr(),
            ATTRIBUTE.as_ptr(),
            bytes.as_ptr().cast::<libc::c_void>(),
            bytes.len(),
            0,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A directory nobody else may enter gets exactly one more entry**, and
    /// the mask that makes it count; the owner, the group and everyone else
    /// keep what the mode said.
    #[test]
    fn a_grant_adds_one_entry_and_a_mask_and_nothing_else() {
        let entries = granted(from_mode(0o700), 985, EXECUTE);
        assert_eq!(
            entries,
            [
                Entry {
                    tag: USER_OBJ,
                    perm: 7,
                    id: UNDEFINED
                },
                Entry {
                    tag: USER,
                    perm: 1,
                    id: 985
                },
                Entry {
                    tag: GROUP_OBJ,
                    perm: 0,
                    id: UNDEFINED
                },
                Entry {
                    tag: MASK,
                    perm: 1,
                    id: UNDEFINED
                },
                Entry {
                    tag: OTHER,
                    perm: 0,
                    id: UNDEFINED
                },
            ]
        );
        // Granted twice is granted once.
        let again = granted(entries.clone(), 985, EXECUTE);
        assert_eq!(again, entries);
        // And the bytes read back as what was written.
        assert_eq!(
            parse(&encode(&entries)).as_deref(),
            Some(entries.as_slice())
        );
    }

    /// **The kernel's own reading of the result**, where the machine has the
    /// tool to ask it with. Run with `--ignored`.
    #[test]
    #[ignore = "needs getfacl and a filesystem with access lists"]
    fn the_kernel_reads_the_grant_back() {
        let dir = std::env::temp_dir().join(format!("lowlat-acl-{}", std::process::id()));
        std::fs::create_dir(&dir).expect("a directory");
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("its mode");
        grant_traverse(&dir, 985).expect("granted");
        let listed = std::process::Command::new("getfacl")
            .args(["-p", "-n"])
            .arg(&dir)
            .output()
            .expect("getfacl");
        let text = String::from_utf8_lossy(&listed.stdout);
        assert!(text.contains("user:985:--x"), "{text}");
        assert!(text.contains("mask::--x"), "{text}");
        assert!(text.contains("user::rwx"), "{text}");
        assert!(text.contains("other::---"), "{text}");
        let _ = std::fs::remove_dir(&dir);
    }
}
