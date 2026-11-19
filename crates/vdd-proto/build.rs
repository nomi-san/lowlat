//! The display driver's identity, from the brand file the build names
//! (docs/11-drivers.md section 1.10): the file `LOWLAT_VDD_BRAND` names --
//! a path from the workspace's root, or an absolute one -- or lowlat's own,
//! `lowlat.brand` beside this script.
//!
//! Read once, here, and written as the constants of the crate's `brand`
//! module, which the driver, its package, its controller and the library all
//! take their names from. A brand file that does not read, an EDID that is
//! not valid, or a brand sharing an identifier with lowlat's own fails the
//! build: two drivers on one machine would otherwise take each other's
//! devices, files or settings.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[path = "src/brand_file.rs"]
mod brand_file;
#[path = "src/edid_block.rs"]
mod edid_block;

use brand_file::{Brand, Guid, Monitor};

fn main() {
    println!("cargo::rerun-if-env-changed=LOWLAT_VDD_BRAND");
    println!("cargo::rustc-check-cfg=cfg(lowlat_vdd_brand)");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let own_path = manifest.join("lowlat.brand");
    let own_text = text(&own_path);
    let own = brand(&own_path, &own_text);
    let own_edid = edid(&own_path, &own);

    let Some(named) = std::env::var_os("LOWLAT_VDD_BRAND").filter(|path| !path.is_empty()) else {
        write(&own, &own_edid);
        return;
    };
    // An absolute path replaces the root it is joined to.
    let path = manifest.join("../..").join(named);
    let text = text(&path);
    let named = brand(&path, &text);
    let edid = edid(&path, &named);
    if let Some(what) = named.shared_with(&own) {
        fail(
            &path,
            format_args!("its {what} is lowlat's own; a brand needs its own"),
        );
    }
    if edid_block::same_monitor(&edid, &own_edid) {
        fail(
            &path,
            "its EDID's maker and product are lowlat's own; a brand needs its own",
        );
    }
    println!("cargo::rustc-cfg=lowlat_vdd_brand");
    write(&named, &edid);
}

fn fail(path: &Path, why: impl std::fmt::Display) -> ! {
    eprintln!(
        "error: the display driver's brand file {}: {why}",
        path.display()
    );
    std::process::exit(1);
}

fn text(path: &Path) -> String {
    println!("cargo::rerun-if-changed={}", path.display());
    std::fs::read_to_string(path).unwrap_or_else(|error| fail(path, error))
}

fn brand<'a>(path: &Path, text: &'a str) -> Brand<'a> {
    brand_file::read(text).unwrap_or_else(|error| fail(path, error))
}

/// The EDID every monitor starts from: the brand's dumped one, checked, or
/// the block generated for it.
fn edid(path: &Path, brand: &Brand<'_>) -> Vec<u8> {
    match brand.monitor {
        Monitor::Dumped(file) => {
            let dumped = path.parent().unwrap_or(Path::new(".")).join(file);
            println!("cargo::rerun-if-changed={}", dumped.display());
            let bytes = std::fs::read(&dumped)
                .unwrap_or_else(|error| fail(path, format_args!("{}: {error}", dumped.display())));
            if !edid_block::valid(&bytes) {
                fail(
                    path,
                    format_args!(
                        "{} is not an EDID of one or two valid blocks",
                        dumped.display()
                    ),
                );
            }
            if edid_block::maker(&bytes).is_none() {
                fail(
                    path,
                    format_args!("{}: its maker is not three letters", dumped.display()),
                );
            }
            bytes
        }
        Monitor::Generated {
            maker,
            product,
            name,
        } => edid_block::generate(maker.as_bytes(), product, name.as_bytes())
            .map(|block| block.to_vec())
            .unwrap_or_else(|| fail(path, "its EDID's maker or name is out of its form")),
    }
}

/// The crate's `brand` module's constants, and every name derived from the
/// short one -- here alone, so the driver, its package and its controller
/// cannot derive two.
fn write(brand: &Brand<'_>, edid: &[u8]) {
    let id = brand.id;
    let mut capital = id.to_owned();
    capital[..1].make_ascii_uppercase();
    // The serial's upper half: the maker's first two letters, `LW` for
    // lowlat's own, as its monitors have always carried.
    let maker = edid_block::maker(edid).unwrap_or(*b"???");
    let serial_base = (u32::from(maker[0]) << 24) | (u32::from(maker[1]) << 16);
    // The frame page's magic: the short name's first six bytes and `fc`,
    // `lowlatfc` for lowlat's own.
    let mut magic = [0u8; 8];
    let head = &id.as_bytes()[..id.len().min(6)];
    magic[..head.len()].copy_from_slice(head);
    magic[6..].copy_from_slice(b"fc");

    let mut out = String::new();
    let mut text = |doc: &str, name: &str, value: &str| {
        // A string's debug form is a Rust literal for the printable ASCII a
        // brand file allows.
        let _ = writeln!(out, "/// {doc}\npub const {name}: &str = {value:?};");
    };
    text("The short name every name below derives from.", "ID", id);
    text(
        "The driver's provider, and the adapter's maker.",
        "PROVIDER",
        brand.provider,
    );
    text(
        "The adapter's name in the device list.",
        "DEVICE",
        brand.device,
    );
    text(
        "The settings folder, under the system's program-data directory.",
        "FOLDER",
        &format!("{id}\\display"),
    );
    text(
        "What each of the driver's log lines starts with.",
        "LOG",
        &format!("{id}-vdd"),
    );
    text(
        "The package's library.",
        "LIBRARY",
        &format!("{id}_idd.dll"),
    );
    text(
        "The package's setup information.",
        "INF",
        &format!("{id}-idd.inf"),
    );
    text(
        "The package's catalogue.",
        "CATALOG",
        &format!("{id}-idd.cat"),
    );
    text(
        "The certificate the package is signed with, as a staged folder carries it.",
        "CERTIFICATE",
        &format!("{id}-idd.cer"),
    );
    text(
        "The device's hardware ID.",
        "HARDWARE_ID",
        &format!("Root\\{id}-vdd"),
    );
    text("The driver's service.", "SERVICE", &format!("{capital}Vdd"));
    text(
        "The driver's host process group.",
        "GROUP",
        &format!("{capital}VddGroup"),
    );
    let guid = |guid: Guid| {
        let bytes: Vec<String> = guid.data4.iter().map(|b| format!("{b:#04x}")).collect();
        format!(
            "Guid {{ data1: {:#010x}, data2: {:#06x}, data3: {:#06x}, data4: [{}] }}",
            guid.data1,
            guid.data2,
            guid.data3,
            bytes.join(", ")
        )
    };
    let _ = writeln!(
        out,
        "/// The control interface.\npub const CONTROL_INTERFACE: Guid = {};",
        guid(brand.control)
    );
    let _ = writeln!(
        out,
        "/// The monitors' container; the slot replaces its last byte.\n\
         pub const CONTAINER: Guid = {};",
        guid(brand.container)
    );
    let bytes: Vec<String> = edid.iter().map(|b| format!("{b:#04x}")).collect();
    let _ = writeln!(
        out,
        "/// The EDID every monitor starts from, before its serial is patched.\n\
         pub const EDID: &[u8] = &[{}];",
        bytes.join(", ")
    );
    let _ = writeln!(
        out,
        "/// The upper half of every monitor's serial.\npub const SERIAL_BASE: u32 = {serial_base:#010x};"
    );
    let _ = writeln!(
        out,
        "/// What a frame page's first word holds once the driver has prepared it.\n\
         pub const PAGE_MAGIC: u64 = {:#018x};",
        u64::from_le_bytes(magic)
    );
    let path = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("brand.rs");
    std::fs::write(&path, out).unwrap_or_else(|error| fail(&path, error));
}
