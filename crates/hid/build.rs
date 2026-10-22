//! Link the user-mode framework's stub and the virtual HID framework's library
//! from the driver kit that scripts/fetch-wdk.py unpacks, or LOWLAT_WDK_DIR
//! names.
//!
//! The framework's stub carries the entry point the driver host calls -- it
//! binds to the framework, then calls the driver's own `DriverEntry` -- and
//! exports it itself; nothing in the driver refers to it, so the stub is linked
//! whole. The virtual HID framework's calls come from its library, which the
//! system's own user-mode half answers. That library sits among the kit's
//! system libraries, so it is copied out and linked from a folder of its own:
//! the kit's copies of the system's other libraries stay out of this link.
//! Without the kit the crate still checks, and the link says what is missing.

use std::path::PathBuf;

const KIT: &str = "10.0.26100.6584";

fn main() {
    println!("cargo:rerun-if-env-changed=LOWLAT_WDK_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let kit = match std::env::var_os("LOWLAT_WDK_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let manifest =
                PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
            manifest.join("../../target/wdk").join(KIT)
        }
    };
    let lib = kit.join("c").join("Lib");
    let umdf = lib.join("wdf/umdf/x64/2.25");
    let vhf = lib.join("10.0.26100.0/um/x64/VhfUm.lib");
    if !umdf.join("WdfDriverStubUm.lib").is_file() || !vhf.is_file() {
        println!(
            "cargo:warning=no driver kit at {}: run scripts/fetch-wdk.py, or the driver will not link",
            kit.display()
        );
        return;
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("vhf");
    if std::fs::create_dir_all(&out)
        .and_then(|()| std::fs::copy(&vhf, out.join("VhfUm.lib")))
        .is_err()
    {
        println!("cargo:warning=the virtual HID framework's library was not copied out of the kit");
        return;
    }
    println!("cargo:rerun-if-changed={}", vhf.display());
    println!("cargo:rustc-link-search=native={}", umdf.display());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static:+whole-archive=WdfDriverStubUm");
    println!("cargo:rustc-link-lib=dylib=VhfUm");
}
