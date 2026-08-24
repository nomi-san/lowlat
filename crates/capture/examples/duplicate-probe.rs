//! Duplicate diagnostic. Asks whether an unchanged display really produces an
//! unchanged conversion, and if it does not, which side moved.
//!
//!   sudo duplicate-probe [/dev/dri/card0] [frames] [depth]
//!
//! **Three separate questions, because the obvious one answers none of them.**
//! A framebuffer's identity says the buffer was not swapped; it says nothing
//! about the bytes inside it, and a compositor that redraws in place changes
//! the picture without changing the identity. So each iteration asks:
//!
//!   A. the same imported source converted into two targets back to back.
//!      A difference here is ours: the conversion is not a function of its
//!      input, or a target carries state a frame does not overwrite.
//!   B. this iteration's conversion against the previous one, into the same
//!      target slot. A difference with A clean means the source moved.
//!   C. the exported buffer's identity, which is what a duplicate check would
//!      key on. A difference means the display flipped.
//!
//! Reading B against C is the point. Identity steady with B differing is a
//! compositor rewriting the buffer it already had, which is the case an
//! identity-keyed duplicate check cannot see; identity steady with B clean is
//! a genuinely still picture and a duplicate that could have been suppressed.
//!
//! Differences are reported by row so a fault confined to the edge -- padding
//! a shader never wrote, an odd last block -- is distinguishable from one
//! spread over the picture.
//!
//! Two runs, and the second is the one that matters.
//!
//! **An idle display**, 2560x1440, 300 frames: two flips, A and B both clean
//! (0 of 300, 0 of 294). Nothing was drawn, so identity and content agreed
//! trivially; this shows only that the probe is quiet when the picture is.
//!
//! **A display in use**, 2560x1440, 400 frames, twice: 191 and 145 flips, and
//! of the frames where the identity held steady, **206 of 206 and 252 of 252
//! changed anyway**. The compositor rewrites the buffer it already had rather
//! than flipping to a new one, so **the exported identity is not a sufficient
//! duplicate key**: suppressing on it drops real frames at roughly half the
//! frame rate. A duplicate check has to compare what was drawn.

use std::collections::HashMap;
use std::os::fd::IntoRawFd;
use std::path::PathBuf;

use lowlat_capture::convert::{Converter, Nv12};
use lowlat_capture::scanout::{Card, Framebuffer};
use lowlat_capture::vulkan::{Device, Imported, Imports, PlaneLayout};

/// How many rows of a difference to name before summarizing the rest.
const NAMED_ROWS: usize = 4;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let node = args
        .first()
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);
    let frames: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(300);
    let depth: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(4);

    let card = Card::open(&node).unwrap_or_else(|error| fail(&format!("open {node:?}: {error}")));
    let layout = card
        .scan()
        .unwrap_or_else(|error| fail(&format!("scan: {error}")));
    let plane = layout.primary_plane;
    let fb = &layout.primary;
    println!(
        "duplicate-probe: {node:?} {}x{} {:?} modifier {:#018x}, {frames} frames, depth {depth}",
        fb.width,
        fb.height,
        fb.format,
        fb.modifier.map_or(0, u64::from)
    );

    let device =
        Device::for_display(&node).unwrap_or_else(|error| fail(&format!("device: {error}")));
    let converter =
        Converter::new(&device).unwrap_or_else(|error| fail(&format!("pipeline: {error}")));

    // The same ring the loop keeps, so a target's own history is exercised
    // rather than one target being reused every frame.
    let targets: Vec<Nv12> = (0..depth.max(2))
        .map(|_| {
            device
                .allocate_nv12(fb.width, fb.height)
                .unwrap_or_else(|error| fail(&format!("allocate: {error}")))
        })
        .collect();
    println!(
        "device {}, {} targets of {}x{}",
        device.name(),
        targets.len(),
        targets[0].width,
        targets[0].height
    );

    let mut imports: HashMap<u64, Imported> = HashMap::new();
    let mut previous: HashMap<usize, Vec<u8>> = HashMap::new();
    let mut last_key: Option<u64> = None;

    let mut flips = 0usize;
    let mut a_differed = 0usize;
    let mut b_differed = 0usize;
    let mut b_compared = 0usize;
    let mut b_still_differed = 0usize;
    let mut b_still_compared = 0usize;
    let mut worst: Option<String> = None;

    for index in 0..frames {
        let began = std::time::Instant::now();
        let fb = card
            .framebuffer_on(plane)
            .unwrap_or_else(|error| fail(&format!("re-read: {error}")));
        let key = import(&card, &device, &fb, &mut imports);
        let flipped = last_key.is_some_and(|last| last != key);
        if flipped {
            flips += 1;
        }
        last_key = Some(key);
        let source = &imports[&key];

        // A. The same source into two different targets, back to back, with
        //    nothing read from the display in between.
        let first = index % targets.len();
        let second = (index + 1) % targets.len();
        let a = convert_and_read(&converter, &device, source, &targets[first]);
        let b = convert_and_read(&converter, &device, source, &targets[second]);
        if a != b {
            a_differed += 1;
            if worst.is_none() {
                worst = Some(format!(
                    "A at frame {index}: {}",
                    describe(&a, &b, targets[first].width as usize)
                ));
            }
        }

        // B. This iteration's conversion against the last one into the same
        //    slot. Only the first target is tracked; the second is the control
        //    above and its history would confuse the comparison.
        if let Some(before) = previous.get(&first) {
            b_compared += 1;
            let changed = *before != a;
            if changed {
                b_differed += 1;
            }
            // The subset that matters: the display did not flip, so anything
            // that moved moved inside a buffer nobody replaced.
            if !flipped {
                b_still_compared += 1;
                if changed {
                    b_still_differed += 1;
                    if worst.as_ref().is_none_or(|w| w.starts_with('A')) {
                        worst = Some(format!(
                            "B at frame {index}, identity steady: {}",
                            describe(before, &a, targets[first].width as usize)
                        ));
                    }
                }
            }
        }
        previous.insert(first, a);

        let took = began.elapsed();
        let tick = std::time::Duration::from_micros(16_667);
        if took < tick {
            std::thread::sleep(tick - took);
        }
    }

    println!();
    println!("over {frames} frames:");
    println!("  C. the display flipped                     {flips}");
    println!("  A. one source, two targets, differed       {a_differed} of {frames}");
    println!("  B. across frames, same target, differed    {b_differed} of {b_compared}");
    println!(
        "     of those, with the identity steady        {b_still_differed} of {b_still_compared}"
    );
    if let Some(worst) = worst {
        println!("  first difference seen -- {worst}");
    }
    println!();
    // The display was being drawn to if it flipped or if an unflipped buffer
    // ever changed. Either says the source is live, which is what makes A
    // unreadable.
    let moving = flips > 0 || b_still_differed > 0;
    println!(
        "{}",
        verdict(a_differed, b_still_differed, b_still_compared, moving)
    );
}

/// Import this framebuffer if it is not already imported, and say which it is.
fn import(
    card: &Card,
    device: &Device,
    fb: &Framebuffer,
    imports: &mut HashMap<u64, Imported>,
) -> u64 {
    let first = fb.planes().next().unwrap_or_else(|| fail("no buffers"));
    let fd = card
        .export(first)
        .unwrap_or_else(|error| fail(&format!("export: {error}")));
    let key = lowlat_capture::scanout::identity(&fd).unwrap_or_else(|| fail("no identity"));
    if imports.contains_key(&key) {
        return key;
    }
    let planes: Vec<PlaneLayout> = fb
        .planes()
        .map(|buffer| PlaneLayout {
            offset: buffer.offset,
            pitch: buffer.pitch,
        })
        .collect();
    let imported = device
        .import(&Imports {
            width: fb.width,
            height: fb.height,
            format: fb.format,
            modifier: fb.modifier.map_or(0, u64::from),
            fd: fd.into_raw_fd(),
            planes: &planes,
        })
        .unwrap_or_else(|error| fail(&format!("import: {error}")));
    imports.insert(key, imported);
    key
}

/// Convert into this target and read both planes back as one buffer.
fn convert_and_read(
    converter: &Converter,
    device: &Device,
    source: &Imported,
    target: &Nv12,
) -> Vec<u8> {
    converter
        .run(device, source, target, false)
        .unwrap_or_else(|error| fail(&format!("convert: {error}")));
    let (mut luma, chroma) = device
        .read_nv12(target)
        .unwrap_or_else(|error| fail(&format!("read planes: {error}")));
    luma.extend_from_slice(&chroma);
    luma
}

/// Where two reads differ, by row of the luma plane.
///
/// **Rows past the luma plane are the colour plane**, and are reported as such
/// rather than as impossible row numbers.
fn describe(before: &[u8], after: &[u8], width: usize) -> String {
    if before.len() != after.len() {
        return format!("lengths differ, {} against {}", before.len(), after.len());
    }
    let mut rows: Vec<usize> = Vec::new();
    let mut bytes = 0usize;
    for (index, (a, b)) in before.iter().zip(after).enumerate() {
        if a != b {
            bytes += 1;
            let row = index / width.max(1);
            if rows.last() != Some(&row) {
                rows.push(row);
            }
        }
    }
    if rows.is_empty() {
        return "no bytes differ".into();
    }
    let named: Vec<String> = rows
        .iter()
        .take(NAMED_ROWS)
        .map(|row| row.to_string())
        .collect();
    let more = rows.len().saturating_sub(NAMED_ROWS);
    format!(
        "{bytes} bytes over {} rows, first at {}{}",
        rows.len(),
        named.join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    )
}

/// What the counts mean together.
///
/// **A is only readable on a desktop nobody is drawing to.** The source is
/// imported once but the memory behind it stays live, so a compositor writing
/// into it between the two conversions makes them differ for a reason that has
/// nothing to do with the conversion. Reading A as determinism on a moving
/// desktop says the shader is broken when the picture merely moved.
fn verdict(
    a_differed: usize,
    still_differed: usize,
    still_compared: usize,
    moving: bool,
) -> String {
    if !moving && a_differed > 0 {
        return "VERDICT: the conversion is not a function of its input. One source converted \
                twice produced two answers on a desktop that was not being drawn to, so the \
                encoder is fed changing bytes whatever the display does."
            .into();
    }
    if still_compared == 0 {
        return "VERDICT: the display flipped on every frame, so nothing here says what an \
                unflipped buffer does. Run it again with the display in a different state."
            .into();
    }
    if still_differed > 0 {
        return format!(
            "VERDICT: the picture changed {still_differed} of {still_compared} times inside a \
             buffer that was never replaced. **Identity is not a sufficient duplicate key**: \
             suppressing on it would drop those frames. A duplicate check has to compare what \
             was drawn, not which buffer it was drawn into."
        );
    }
    "VERDICT: an unflipped buffer held still every time. Identity did not mislead over this \
     run -- but a run with nothing being drawn cannot show that it would not, because a still \
     picture and a sound key give the same answer."
        .into()
}

fn fail(message: &str) -> ! {
    eprintln!("duplicate-probe: {message}");
    std::process::exit(1)
}
