//! The drivers' identity, from the brand file the build names
//! (docs/11-drivers.md section 1.10): the file `LOWLAT_BRAND` names -- a path
//! from the workspace's root, or an absolute one -- or lowlat's own,
//! `lowlat.brand` beside this script.
//!
//! Read once, here, and written as the crate's two modules of constants,
//! `display` and `input`, which the drivers, their packages, their controllers
//! and the library all take their names from. A brand file that does not read,
//! an EDID that is not valid, or a brand sharing an identifier with lowlat's
//! own fails the build: two drivers on one machine would otherwise take each
//! other's devices, files or settings.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[allow(dead_code, unreachable_pub)]
#[path = "src/edid.rs"]
mod edid;
#[path = "src/file.rs"]
mod file;

use file::{Brand, Guid, Monitor};

fn main() {
    println!("cargo::rerun-if-env-changed=LOWLAT_BRAND");
    println!("cargo::rustc-check-cfg=cfg(lowlat_brand)");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let own_path = manifest.join("lowlat.brand");
    let own_text = text(&own_path);
    let own = brand(&own_path, &own_text);
    let own_edid = edid(&own_path, &own);

    let Some(named) = std::env::var_os("LOWLAT_BRAND").filter(|path| !path.is_empty()) else {
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
    if edid::same_monitor(&edid, &own_edid) {
        fail(
            &path,
            "its EDID's maker and product are lowlat's own; a brand needs its own",
        );
    }
    println!("cargo::rustc-cfg=lowlat_brand");
    write(&named, &edid);
}

fn fail(path: &Path, why: impl std::fmt::Display) -> ! {
    eprintln!("error: the drivers' brand file {}: {why}", path.display());
    std::process::exit(1);
}

fn text(path: &Path) -> String {
    println!("cargo::rerun-if-changed={}", path.display());
    std::fs::read_to_string(path).unwrap_or_else(|error| fail(path, error))
}

fn brand<'a>(path: &Path, text: &'a str) -> Brand<'a> {
    file::read(text).unwrap_or_else(|error| fail(path, error))
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
            if !edid::valid(&bytes) {
                fail(
                    path,
                    format_args!(
                        "{} is not an EDID of one or two valid blocks",
                        dumped.display()
                    ),
                );
            }
            if edid::maker(&bytes).is_none() {
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
        } => edid::generate(maker.as_bytes(), product, name.as_bytes())
            .map(|block| block.to_vec())
            .unwrap_or_else(|| fail(path, "its EDID's maker or name is out of its form")),
    }
}

/// One module's constants, as Rust source.
#[derive(Default)]
struct Module(String);

impl Module {
    /// A string's debug form is a Rust literal for the printable ASCII a
    /// brand file allows, and a C string's literal with a `c` before it.
    fn text(&mut self, doc: &str, name: &str, value: &str) {
        let _ = writeln!(
            self.0,
            "    /// {doc}\n    pub const {name}: &str = {value:?};"
        );
    }

    fn guid(&mut self, doc: &str, name: &str, guid: Guid) {
        let bytes: Vec<String> = guid.data4.iter().map(|b| format!("{b:#04x}")).collect();
        let _ = writeln!(
            self.0,
            "    /// {doc}\n    pub const {name}: Guid = Guid {{ data1: {:#010x}, data2: {:#06x}, \
             data3: {:#06x}, data4: [{}] }};",
            guid.data1,
            guid.data2,
            guid.data3,
            bytes.join(", ")
        );
    }

    /// The names both drivers' packages take: the library, the setup
    /// information, the catalogue, the certificate a staged folder carries,
    /// the hardware ID and the service, each from the short name and the
    /// driver's own stem -- `idd`/`vdd` for the display, `hid` for the input.
    fn package(&mut self, id: &str, file: &str, device: &str) {
        let mut capital = id.to_owned();
        capital[..1].make_ascii_uppercase();
        let mut service = device.to_owned();
        service[..1].make_ascii_uppercase();
        self.text(
            "The package's library.",
            "LIBRARY",
            &format!("{id}_{file}.dll"),
        );
        self.text(
            "The package's setup information.",
            "INF",
            &format!("{id}-{file}.inf"),
        );
        self.text(
            "The package's catalogue.",
            "CATALOG",
            &format!("{id}-{file}.cat"),
        );
        self.text(
            "The certificate the package is signed with, as a staged folder carries it.",
            "CERTIFICATE",
            &format!("{id}-{file}.cer"),
        );
        self.text(
            "The device's hardware ID.",
            "HARDWARE_ID",
            &format!("Root\\{id}-{device}"),
        );
        self.text(
            "The driver's service.",
            "SERVICE",
            &format!("{capital}{service}"),
        );
        self.text(
            "What each of the driver's log lines starts with.",
            "LOG",
            &format!("{id}-{device}"),
        );
    }
}

/// The crate's constants, and every name derived from the short one -- here
/// alone, so the drivers, their packages and their controllers cannot derive
/// two.
fn write(brand: &Brand<'_>, edid: &[u8]) {
    let id = brand.id;
    let mut display = Module::default();
    display.text("The short name every name derives from.", "ID", id);
    display.text(
        "The drivers' provider, and the adapter's maker.",
        "PROVIDER",
        brand.provider,
    );
    display.text(
        "The adapter's name in the device list.",
        "DEVICE",
        brand.device,
    );
    display.package(id, "idd", "vdd");
    display.text(
        "The other driver's setup information, signed with the same certificate.",
        "COMPANION_INF",
        &format!("{id}-hid.inf"),
    );
    let mut capital = id.to_owned();
    capital[..1].make_ascii_uppercase();
    display.text(
        "The driver's host process group.",
        "GROUP",
        &format!("{capital}VddGroup"),
    );
    display.text(
        "The settings folder, under the system's program-data directory.",
        "FOLDER",
        &format!("{id}\\display"),
    );
    display.guid("The control interface.", "CONTROL_INTERFACE", brand.control);
    display.guid(
        "The monitors' container; the slot replaces its last byte.",
        "CONTAINER",
        brand.container,
    );
    let bytes: Vec<String> = edid.iter().map(|b| format!("{b:#04x}")).collect();
    let _ = writeln!(
        display.0,
        "    /// The EDID every monitor starts from, before its serial is patched.\n    \
         pub const EDID: &[u8] = &[{}];",
        bytes.join(", ")
    );
    // The serial's upper half: the maker's first two letters, `LW` for
    // lowlat's own, as its monitors have always carried.
    let maker = edid::maker(edid).unwrap_or(*b"???");
    let serial_base = (u32::from(maker[0]) << 24) | (u32::from(maker[1]) << 16);
    let _ = writeln!(
        display.0,
        "    /// The upper half of every monitor's serial.\n    \
         pub const SERIAL_BASE: u32 = {serial_base:#010x};"
    );
    // The frame page's magic: the short name's first six bytes and `fc`,
    // `lowlatfc` for lowlat's own.
    let mut magic = [0u8; 8];
    let head = &id.as_bytes()[..id.len().min(6)];
    magic[..head.len()].copy_from_slice(head);
    magic[6..].copy_from_slice(b"fc");
    let _ = writeln!(
        display.0,
        "    /// What a frame page's first word holds once the driver has prepared it.\n    \
         pub const PAGE_MAGIC: u64 = {:#018x};",
        u64::from_le_bytes(magic)
    );

    let mut input = Module::default();
    input.text("The short name every name derives from.", "ID", id);
    input.text("The drivers' provider.", "PROVIDER", brand.provider);
    input.text(
        "The input driver's device in the device list.",
        "DEVICE",
        brand.input,
    );
    input.package(id, "hid", "hid");
    input.text(
        "The other driver's setup information, signed with the same certificate.",
        "COMPANION_INF",
        &format!("{id}-idd.inf"),
    );
    let _ = writeln!(
        input.0,
        "    /// The name the framework knows a handle's context type by.\n    \
         pub const CONTEXT: &core::ffi::CStr = c{:?};",
        format!("{id}-hid handle")
    );
    input.guid(
        "The control interface.",
        "CONTROL_INTERFACE",
        brand.input_control,
    );
    input.guid(
        "The devices' container; a device's kind and slot replace its last two bytes.",
        "CONTAINER",
        brand.input_container,
    );

    let out = format!(
        "/// The display driver's names and identifiers.\npub mod display {{\n    use crate::Guid;\n\n{}}}\n\n\
         /// The input driver's names and identifiers.\npub mod input {{\n    use crate::Guid;\n\n{}}}\n",
        display.0, input.0
    );
    let path = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("brand.rs");
    std::fs::write(&path, out).unwrap_or_else(|error| fail(&path, error));
}
