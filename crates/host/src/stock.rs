//! Stock pointers, read from the desktop's own icon theme.
//!
//! Some pointers a host has to show are not ones the display ever draws. The
//! refused pointer is the case: a guest that does not hold the arbitrated
//! pointer has to be shown that it does not ([05-host.md §7.1](../../../docs/05-host.md)),
//! and no application on the host is asking for that shape, so there is
//! nothing on the pointer plane to read.
//!
//! **It is loaded rather than drawn.** The shapes a desktop uses live in the
//! icon theme as files in a format that carries the picture *and its hotspot*,
//! which is the part that matters: a drawn glyph would need a hotspot invented
//! for it, and this backend has no way to derive one for a shape the display
//! never puts on screen. It also looks like the pointer the person at the desk
//! would see, which a hand-drawn one does not.
//!
//! The same files serve both display stacks, so this is not specific to
//! either.

use std::path::{Path, PathBuf};

/// Chunk kind for an image, as the format numbers them.
const IMAGE: u32 = 0xfffd_0002;
/// Fixed header, then twelve bytes of table per entry.
const TOC_AT: usize = 16;
const TOC_ENTRY: usize = 12;
/// Fields before the pixels in an image chunk.
const IMAGE_HEADER: usize = 36;

/// Where themes are kept when the environment does not say.
const SEARCH: [&str; 3] = [
    "/usr/share/icons",
    "/usr/local/share/icons",
    "/usr/share/pixmaps",
];

/// Themes to try when the environment does not name one.
///
/// **`default` first, and it is usually a redirection rather than a theme**: it
/// carries an inherits line naming whichever theme the desktop actually uses,
/// which is followed. The rest are what is present on an ordinary desktop, so
/// a host that cannot see the session's own settings still finds a shape.
const THEMES: [&str; 4] = ["default", "Adwaita", "breeze_cursors", "breeze"];

/// Names the refused pointer goes by. **All four occur**, and a theme that has
/// one may not have the others.
pub const REFUSED: [&str; 4] = ["not-allowed", "crossed_circle", "no-drop", "forbidden"];

/// One stock pointer, in the form the cursor path wants it.
#[derive(Debug, Clone)]
pub struct Stock {
    pub width: u16,
    pub height: u16,
    /// **Read from the file, not derived.** A shape the display never draws
    /// cannot have its hotspot learned from where the display drew it.
    pub hot_x: u16,
    pub hot_y: u16,
    /// Red, green, blue, alpha, not premultiplied.
    pub rgba: Vec<u8>,
}

/// Load the first of `names` any theme has, at the size nearest `wanted`.
///
/// Returns nothing when no theme on this machine carries any of them, which is
/// an ordinary state on a system with no desktop installed rather than a
/// fault: a host with no shape to show simply shows none.
pub fn load(names: &[&str], wanted: u32) -> Option<Stock> {
    for directory in themes() {
        for name in names {
            let path = directory.join("cursors").join(name);
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if let Some(stock) = parse(&bytes, wanted) {
                lowlat_common::log_info!(
                    "cursor: stock shape {} is {}x{} hot=({},{})",
                    path.display(),
                    stock.width,
                    stock.height,
                    stock.hot_x,
                    stock.hot_y
                );
                return Some(stock);
            }
        }
    }
    None
}

/// Theme directories to look in, most specific first.
fn themes() -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = match std::env::var("XCURSOR_PATH") {
        Ok(path) => path.split(':').map(PathBuf::from).collect(),
        Err(_) => {
            let mut roots = Vec::new();
            if let Ok(home) = std::env::var("HOME") {
                roots.push(PathBuf::from(home).join(".icons"));
            }
            roots.extend(SEARCH.iter().map(PathBuf::from));
            roots
        }
    };

    let mut wanted: Vec<String> = Vec::new();
    if let Ok(theme) = std::env::var("XCURSOR_THEME") {
        wanted.push(theme);
    }
    wanted.extend(THEMES.iter().map(|name| (*name).to_string()));

    let mut found = Vec::new();
    let mut at = 0;
    // A theme that only redirects adds what it names, which is followed in
    // turn. Bounded because a pair of themes can inherit each other.
    while at < wanted.len() && at < 8 {
        let name = wanted[at].clone();
        at += 1;
        for root in &roots {
            let directory = root.join(&name);
            if !directory.is_dir() {
                continue;
            }
            found.push(directory.clone());
            if let Some(parent) = inherits(&directory)
                && !wanted.contains(&parent)
            {
                wanted.push(parent);
            }
        }
    }
    found
}

/// The theme a redirection names, if it is one.
fn inherits(directory: &Path) -> Option<String> {
    let text = std::fs::read_to_string(directory.join("index.theme")).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("Inherits=") {
            return rest.split(',').next().map(|name| name.trim().to_string());
        }
    }
    None
}

/// Read the image nearest `wanted` out of a theme file.
///
/// **Nominal size is not pixel size** and the two are chosen separately by the
/// theme, so the nearest nominal is picked and whatever pixels it carries are
/// taken as they come.
fn parse(bytes: &[u8], wanted: u32) -> Option<Stock> {
    if bytes.get(..4)? != b"Xcur" {
        return None;
    }
    let entries = read(bytes, 12)?;

    let mut best: Option<(u32, usize)> = None;
    for index in 0..entries as usize {
        let at = TOC_AT.checked_add(index.checked_mul(TOC_ENTRY)?)?;
        if read(bytes, at)? != IMAGE {
            continue;
        }
        let nominal = read(bytes, at + 4)?;
        let position = read(bytes, at + 8)? as usize;
        let distance = nominal.abs_diff(wanted);
        if best.is_none_or(|(closest, _)| distance < closest) {
            best = Some((distance, position));
        }
    }

    let (_, at) = best?;
    let width = read(bytes, at + 16)?;
    let height = read(bytes, at + 20)?;
    let hot_x = read(bytes, at + 24)?;
    let hot_y = read(bytes, at + 28)?;
    // A shape larger than a pointer plane can carry is not one this path can
    // use, and the multiplication below has to be bounded regardless.
    if width == 0 || height == 0 || width > 256 || height > 256 {
        return None;
    }

    let pixels = (width as usize).checked_mul(height as usize)?;
    let mut rgba = vec![0u8; pixels.checked_mul(4)?];
    for index in 0..pixels {
        let value = read(bytes, at.checked_add(IMAGE_HEADER)?.checked_add(index * 4)?)?;
        // Masked rather than cast: the intent is one byte of a packed word,
        // and a truncating cast says the same thing less clearly.
        let byte = |shift: u32| u8::try_from((value >> shift) & 0xFF).unwrap_or(0);
        let (a, r, g, b) = (byte(24), byte(16), byte(8), byte(0));
        let slot = rgba.get_mut(index * 4..index * 4 + 4)?;
        // **The file's colours are multiplied by their own alpha and a picture's
        // are not.** Copying them across unchanged darkens every edge of the
        // shape against whatever it is drawn over, which on a mostly white
        // pointer is a grey halo.
        slot[0] = straighten(r, a);
        slot[1] = straighten(g, a);
        slot[2] = straighten(b, a);
        slot[3] = a;
    }

    Some(Stock {
        width: u16::try_from(width).ok()?,
        height: u16::try_from(height).ok()?,
        hot_x: u16::try_from(hot_x).ok()?,
        hot_y: u16::try_from(hot_y).ok()?,
        rgba,
    })
}

/// Undo the alpha a stored colour was multiplied by.
fn straighten(value: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        return 0;
    }
    // Rounded to nearest rather than truncated: a colour stored at exactly
    // half its alpha comes back one short otherwise, on every pixel.
    let alpha = u32::from(alpha);
    let raised = (u32::from(value) * 255 + alpha / 2) / alpha;
    u8::try_from(raised.min(255)).unwrap_or(255)
}

/// One little-endian word.
fn read(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(word.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a file with two sizes in it, the second carrying the pixels.
    fn theme_file() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"Xcur");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&0x0001_0000u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());

        let word = |value: usize| u32::try_from(value).expect("a small offset");
        let first = word(TOC_AT + 2 * TOC_ENTRY);
        // A 1x1 at nominal 12, then a 2x2 at nominal 24.
        let second = first + word(IMAGE_HEADER) + 4;
        for (nominal, position) in [(12u32, first), (24, second)] {
            bytes.extend_from_slice(&IMAGE.to_le_bytes());
            bytes.extend_from_slice(&nominal.to_le_bytes());
            bytes.extend_from_slice(&position.to_le_bytes());
        }

        let mut image = |width: u32, height: u32, hot: (u32, u32), pixels: &[u32]| {
            for value in [
                word(IMAGE_HEADER),
                IMAGE,
                width,
                0x0001_0000,
                width,
                height,
                hot.0,
                hot.1,
                0,
            ] {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            for pixel in pixels {
                bytes.extend_from_slice(&pixel.to_le_bytes());
            }
        };
        image(1, 1, (0, 0), &[0xFF00_0000]);
        // Half-transparent white, stored multiplied by its own alpha.
        image(2, 2, (1, 1), &[0x8040_4040; 4]);
        bytes
    }

    /// **The nearest nominal size wins**, and the file carries several. Taking
    /// the first would send a pointer at whatever size the theme happened to
    /// list first, which is usually the smallest.
    #[test]
    fn the_size_nearest_the_one_asked_for_is_taken() {
        let bytes = theme_file();
        let small = parse(&bytes, 12).expect("a shape");
        assert_eq!((small.width, small.height), (1, 1));

        let large = parse(&bytes, 30).expect("a shape");
        assert_eq!((large.width, large.height), (2, 2));
        assert_eq!((large.hot_x, large.hot_y), (1, 1), "the file's own hotspot");
    }

    /// **The file's colours are multiplied by their alpha and a picture's are
    /// not.** Copied across unchanged, every edge of the shape darkens against
    /// what it is drawn over.
    #[test]
    fn colours_come_back_unmultiplied() {
        let large = parse(&theme_file(), 24).expect("a shape");
        // 0x40 stored at half alpha is 0x80 straight, and the alpha rides
        // along unchanged.
        assert_eq!(large.rgba.get(..4), Some(&[0x80, 0x80, 0x80, 0x80][..]));
    }

    /// Anything that is not one of these files is refused rather than read as
    /// one.
    #[test]
    fn a_file_that_is_not_one_is_refused() {
        assert!(parse(b"", 24).is_none());
        assert!(parse(b"not a cursor at all", 24).is_none());
        let mut truncated = theme_file();
        truncated.truncate(20);
        assert!(parse(&truncated, 24).is_none());
    }

    /// The real thing, on a machine that has a desktop installed.
    #[test]
    #[ignore = "requires an icon theme"]
    fn a_stock_refused_pointer_is_found() {
        let stock = load(&REFUSED, 24).expect("a refused pointer");
        assert!(stock.width >= 16 && stock.height >= 16);
        assert!(stock.hot_x < stock.width && stock.hot_y < stock.height);
        assert_eq!(
            stock.rgba.len(),
            (stock.width as usize) * (stock.height as usize) * 4
        );
    }
}
