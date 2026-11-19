//! Write the package's setup information with the brand's names, and link
//! the frameworks' stub libraries from the driver kit that
//! scripts/fetch-wdk.py unpacks, or LOWLAT_WDK_DIR names.
//!
//! The setup information and the names the scripts that package, stage and
//! install the driver read go to `package` under the build's output, which
//! scripts/package-vdd.sh finds through cargo's messages (docs/11-drivers.md
//! section 1.10).
//!
//! The user-mode framework's stub carries the entry point the driver host
//! calls -- it binds to the framework, then calls the driver's own
//! `DriverEntry` -- and exports it itself; nothing in the driver refers to it,
//! so the stub is linked whole. The indirect display class's stub carries that
//! class's function table. Without the kit the crate still checks, and the
//! link says what is missing.

use std::path::{Path, PathBuf};

use lowlat_brand::display as brand;

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
    let idd = lib.join("10.0.26100.0/um/x64/iddcx/1.4");
    if !umdf.join("WdfDriverStubUm.lib").is_file() || !idd.join("iddcxstub.lib").is_file() {
        println!(
            "cargo:warning=no driver kit at {}: run scripts/fetch-wdk.py, or the driver will not link",
            kit.display()
        );
        return;
    }
    println!("cargo:rustc-link-search=native={}", umdf.display());
    println!("cargo:rustc-link-search=native={}", idd.display());
    println!("cargo:rustc-link-lib=static:+whole-archive=WdfDriverStubUm");
    println!("cargo:rustc-link-lib=static=iddcxstub");
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
    let mut inf = include_str!("idd.inf").replace("\r\n", "\n");
    for (placeholder, name) in [
        ("@CATALOG@", brand::CATALOG),
        ("@HARDWARE_ID@", brand::HARDWARE_ID),
        ("@LIBRARY@", brand::LIBRARY),
        ("@GROUP@", brand::GROUP),
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
            Path::new("idd.inf"),
            "a placeholder the build does not know",
        );
    }
    let path = out.join(brand::INF);
    std::fs::write(&path, inf.replace('\n', "\r\n")).unwrap_or_else(|error| fail(&path, error));
    // A string's debug form is a JSON string for the printable ASCII a brand
    // allows: quotes and backslashes escaped.
    let names = format!(
        "{{\n  \"id\": {:?},\n  \"library\": {:?},\n  \"inf\": {:?},\n  \"catalog\": {:?},\n  \
         \"certificate\": {:?},\n  \"hardware_id\": {:?},\n  \"folder\": {:?},\n  \
         \"companion_inf\": {:?}\n}}\n",
        brand::ID,
        brand::LIBRARY,
        brand::INF,
        brand::CATALOG,
        brand::CERTIFICATE,
        brand::HARDWARE_ID,
        brand::FOLDER,
        brand::COMPANION_INF,
    );
    let path = out.join("package.json");
    std::fs::write(&path, names).unwrap_or_else(|error| fail(&path, error));
}
