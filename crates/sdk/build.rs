//! The shared library's install name on macOS, `@rpath/liblowlat.dylib`, so
//! an application finds it through its own run path wherever it ships it, as
//! it finds `liblowlat.so` on Linux; the linker would otherwise record the
//! path the library was built at, which no other machine has. Nothing is
//! compiled and nothing else is linked here.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "macos" {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/liblowlat.dylib");
    }
}
