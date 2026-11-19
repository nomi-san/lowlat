//! Write the package's setup information with the brand's names, and link the
//! user-mode framework's stub and the virtual HID framework's library from the
//! driver kit that scripts/fetch-wdk.py unpacks, or LOWLAT_WDK_DIR names.
//!
//! The setup information and the names the scripts that package, stage and
//! install the driver read go to `package` under the build's output, which
//! scripts/package-hid.sh finds through cargo's messages (docs/11-drivers.md
//! section 1.10).
//!
//! The framework's stub carries the entry point the driver host calls -- it
//! binds to the framework, then calls the driver's own `DriverEntry` -- and
//! exports it itself; nothing in the driver refers to it, so the stub is linked
//! whole. The virtual HID framework's calls come from its library, which the
//! system's own user-mode half answers. That library sits among the kit's
//! system libraries, so it is copied out and linked from a folder of its own:
//! the kit's copies of the system's other libraries stay out of this link.
//! Without the kit the crate still checks, and the link says what is missing.

use std::path::{Path, PathBuf};

use lowlat_brand::input as brand;

const KIT: &str = "10.0.26100.6584";

fn main() {
    println!("cargo:rerun-if-env-changed=LOWLAT_WDK_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    package();
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

fn fail(path: &Path, why: impl std::fmt::Display) -> ! {
    eprintln!("error: {}: {why}", path.display());
    std::process::exit(1);
}

/// The setup information, every placeholder replaced by the brand's name and
/// in the system's own line endings, and `package.json`: the names the
/// package's files and the device take. A brand's earlier build's files go
/// first.
fn package() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("package");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap_or_else(|error| fail(&out, error));
    let mut inf = include_str!("hid.inf").replace("\r\n", "\n");
    for (placeholder, name) in [
        ("@CATALOG@", brand::CATALOG),
        ("@HARDWARE_ID@", brand::HARDWARE_ID),
        ("@LIBRARY@", brand::LIBRARY),
        ("@SERVICE@", brand::SERVICE),
        ("@PROVIDER@", brand::PROVIDER),
        ("@DEVICE@", brand::DEVICE),
    ] {
        inf = inf.replace(placeholder, name);
    }
    // A brand's names never carry the sign, so one left is a placeholder
    // nothing here replaces.
    if inf.contains('@') {
        fail(
            Path::new("hid.inf"),
            "a placeholder the build does not know",
        );
    }
    let path = out.join(brand::INF);
    std::fs::write(&path, inf.replace('\n', "\r\n")).unwrap_or_else(|error| fail(&path, error));
    // A string's debug form is a JSON string for the printable ASCII a brand
    // allows: quotes and backslashes escaped.
    let names = format!(
        "{{\n  \"id\": {:?},\n  \"library\": {:?},\n  \"inf\": {:?},\n  \"catalog\": {:?},\n  \
         \"certificate\": {:?},\n  \"hardware_id\": {:?},\n  \"companion_inf\": {:?}\n}}\n",
        brand::ID,
        brand::LIBRARY,
        brand::INF,
        brand::CATALOG,
        brand::CERTIFICATE,
        brand::HARDWARE_ID,
        brand::COMPANION_INF,
    );
    let path = out.join("package.json");
    std::fs::write(&path, names).unwrap_or_else(|error| fail(&path, error));
}
