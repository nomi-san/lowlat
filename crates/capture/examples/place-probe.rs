//! Where the captured output sits in the desktop, and what says so.
//!
//! Run it on the machine under test:
//!
//!   sudo -E place-probe [/dev/dri/card0]
//!
//! The display half needs the elevated capability, for the same reason the
//! scanout probe does. The layout half does not, and prints on its own.
//!
//! What to do with it:
//!
//!   - one display: expect no placement, which is correct
//!   - a second to the right: expect the desktop to grow and the rectangle to
//!     stay at 0,0
//!   - the second on the LEFT: expect a nonzero x. **This is the case worth
//!     running.** With the captured output at the desktop's corner the offset
//!     is zero, so an implementation that fixes only the scale looks right.
//!   - a scaled display: expect the rectangle to be smaller than the picture
//!
//! It also prints where a peer's coordinates land, which is the number the
//! whole exercise is about: the far edge of the picture must reach the far
//! edge of the captured output and no further.

use std::path::PathBuf;

use lowlat_capture::desktop;
use lowlat_capture::scanout::Card;

/// The absolute axis an injected coordinate is expressed on.
const AXIS: u64 = 65535;

fn main() {
    let node = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);

    let card = match Card::open(&node) {
        Ok(card) => card,
        Err(error) => {
            println!("cannot open {}: {error}", node.display());
            return;
        }
    };
    let layout = match card.scan() {
        Ok(layout) => layout,
        Err(error) => {
            println!("nothing to scan on {}: {error}", node.display());
            return;
        }
    };

    let picture = (layout.primary.width, layout.primary.height);
    println!(
        "picture   {}x{} on {}",
        picture.0,
        picture.1,
        node.display()
    );

    let Some(connector) = layout.connector.as_deref() else {
        println!("connector unknown, so nothing can be matched to a layout");
        return;
    };
    println!("connector {connector}");

    let Some(place) = desktop::placement_of(connector) else {
        println!("placement none, the picture is taken to be the whole desktop");
        return;
    };
    println!(
        "rectangle {}x{} at {},{}",
        place.width, place.height, place.x, place.y
    );
    println!("desktop   {}x{}", place.desktop_width, place.desktop_height);

    // The two ends a guest actually reaches, in desktop units. The far one is
    // the whole point: it must be the captured output's own far edge.
    for (axis, from, origin, within, desktop) in [
        ("x", picture.0, place.x, place.width, place.desktop_width),
        ("y", picture.1, place.y, place.height, place.desktop_height),
    ] {
        let landed = |value: u64| {
            let placed = u64::from(origin) + value * u64::from(within) / u64::from(from);
            placed * AXIS / u64::from(desktop) * u64::from(desktop) / AXIS
        };
        println!(
            "{axis}: a peer's 0 lands at {}, its {from} lands at {} of {desktop}",
            landed(0),
            landed(u64::from(from))
        );
    }
}
