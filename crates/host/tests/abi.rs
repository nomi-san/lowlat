//! The C ABI's three mechanical gates ([`docs/06-api.md`]).
//!
//! Each one guards something that fails silently: a header that drifted from
//! the definitions it describes, a header that only compiles in one language,
//! a panic that crosses the boundary as undefined behaviour, and a symbol
//! exported without the prefix that makes a mismatch a link error instead of
//! memory corruption.
//!
//! **They run against the built shared object, not against this crate.** That
//! is the whole point of them: the library form linked into this test answers
//! for this test's build settings, and what ships is the other one.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the repository is, from where this crate is.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root is two levels above this crate")
}

/// The directory cargo put this test's own executable in, which is also where
/// it put the shared object.
fn artifacts() -> PathBuf {
    let exe = std::env::current_exe().expect("a running test has a path");
    exe.parent()
        .and_then(Path::parent)
        .expect("a test executable lives in <profile>/deps")
        .to_path_buf()
}

/// The shared object, which is what every check here is really about.
///
/// **Built here, because `cargo test` does not build it.** A test binary
/// depends on the library form and nothing asks for the shared one, so the
/// file sitting in the profile directory is whatever some earlier command left
/// there. That was not a theory: removing the containment from
/// `lowlat_debug_panic` and running this suite passed, against an object eight
/// hours old. A gate that tests yesterday's artifact reports on yesterday.
fn shared_object() -> PathBuf {
    let profile = artifacts();
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "-p", "lowlat-host"]);
    // The test profile decides which directory this is running from, and the
    // build has to land in the same one.
    if profile.file_name().is_some_and(|name| name == "release") {
        build.arg("--release");
    }
    let built = build.output().expect("cargo builds the shared object");
    assert!(
        built.status.success(),
        "the shared object could not be built:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );

    let object = profile.join("liblowlat.so");
    assert!(object.is_file(), "{} was not produced", object.display());
    object
}

/// A scratch directory under the profile, so nothing lands in the source tree
/// and a second run starts clean.
fn scratch(name: &str) -> PathBuf {
    let dir = artifacts().join("abi-gate").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory under the profile");
    dir
}

/// Generate the header from the definitions, exactly as the committed one was.
///
/// **From the ABI module alone, not from the crate.** Generating from the
/// crate publishes every `pub const` in it -- `MAX_GUESTS`, `HOLD_MS` and the
/// rest arrived in the header on the first run -- and an application that
/// includes this header would collide with names it never asked for. Naming
/// the file makes publishing a decision rather than a default, and it forces
/// the other half of the rule: a type crossing the boundary is defined in the
/// ABI layer, because a type from anywhere else cannot be seen from here.
fn generate() -> String {
    let crate_dir = env!("CARGO_MANIFEST_DIR");
    let config = cbindgen::Config::from_root_or_default(crate_dir);
    let bindings = cbindgen::Builder::new()
        .with_src(Path::new(crate_dir).join("src/abi.rs"))
        .with_config(config)
        .generate()
        .expect("the definitions parse");
    let mut out = Vec::new();
    bindings.write(&mut out);
    String::from_utf8(out).expect("the generated header is text")
}

/// Run a compiler and give back what it said, so a failure reports the
/// diagnostic rather than an exit code.
fn compile(compiler: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(compiler)
        .args(args)
        .output()
        .map_err(|why| format!("{compiler} could not be run: {why}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{compiler} {}\n{}{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

/// **The header is generated, so it cannot describe something the library does
/// not do** -- but only if a stale one fails the build, which is this.
///
/// Regenerate with `LOWLAT_BLESS_HEADER=1 cargo test -p lowlat-host --test abi`.
#[test]
fn the_header_matches_the_definitions() {
    let committed = root().join("include/lowlat.h");
    let generated = generate();

    if std::env::var_os("LOWLAT_BLESS_HEADER").is_some() {
        std::fs::create_dir_all(committed.parent().expect("include/ has a parent"))
            .expect("the include directory");
        std::fs::write(&committed, &generated).expect("writing the header");
        return;
    }

    let found = std::fs::read_to_string(&committed).unwrap_or_default();
    assert_eq!(
        found,
        generated,
        "{} is stale. Regenerate it: LOWLAT_BLESS_HEADER=1 cargo test -p lowlat-host --test abi",
        committed.display()
    );
}

/// **One header, both languages, warnings as errors.**
///
/// The translation unit declares nothing of its own, so the only thing that
/// can produce a diagnostic is the header.
#[test]
fn the_header_compiles_alone_as_c_and_as_c_plus_plus() {
    let include = root().join("include");
    let source = root().join("crates/host/tests/c/alone.c");
    let dir = scratch("alone");
    let warnings = ["-Wall", "-Wextra", "-Werror"];

    let object = dir.join("alone-c.o");
    let mut args: Vec<&str> = vec!["-std=c11"];
    args.extend(warnings);
    let (include, source, object) = (
        include.to_string_lossy().to_string(),
        source.to_string_lossy().to_string(),
        object.to_string_lossy().to_string(),
    );
    args.extend(["-I", &include, "-c", &source, "-o", &object]);
    if let Err(why) = compile("cc", &args) {
        panic!("the header does not compile as C:\n{why}");
    }

    let object = dir.join("alone-cpp.o");
    let object = object.to_string_lossy().to_string();
    let mut args: Vec<&str> = vec!["-std=c++17", "-x", "c++"];
    args.extend(warnings);
    args.extend(["-I", &include, "-c", &source, "-o", &object]);
    if let Err(why) = compile("c++", &args) {
        panic!("the header does not compile as C++:\n{why}");
    }
}

/// **A deliberate panic comes back as a status**, from the object that ships.
///
/// Undefined behaviour if it regresses, which is why the check loads the
/// shared object rather than calling the same code from Rust: building the
/// library to abort on panic would disable containment everywhere and this
/// test would still pass if it linked the library form.
#[test]
fn a_deliberate_panic_returns_a_status_from_the_shared_object() {
    let include = root().join("include");
    let source = root().join("crates/host/tests/c/harness.c");
    let dir = scratch("harness");
    let program = dir.join("harness");

    let (include, source, program) = (
        include.to_string_lossy().to_string(),
        source.to_string_lossy().to_string(),
        program.to_string_lossy().to_string(),
    );
    let args = vec![
        "-std=c11", "-Wall", "-Wextra", "-Werror", "-I", &include, &source, "-o", &program,
    ];
    if let Err(why) = compile("cc", &args) {
        panic!("the harness does not compile:\n{why}");
    }

    let object = shared_object();
    let run = Command::new(&program)
        .arg(&object)
        .output()
        .expect("the harness runs");
    assert!(
        run.status.success(),
        "the harness failed against {}:\n{}{}",
        object.display(),
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}

/// **Every exported symbol carries the prefix**, so an application built
/// against a different version of this library fails to link rather than
/// finding a function whose arguments have quietly moved.
#[test]
fn every_exported_symbol_carries_the_prefix() {
    let object = shared_object();
    let listed = Command::new("nm")
        .args(["-D", "--defined-only", "--format=posix"])
        .arg(&object)
        .output()
        .expect("nm runs; it ships with the linker this toolchain already needs");
    assert!(
        listed.status.success(),
        "nm could not read {}:\n{}",
        object.display(),
        String::from_utf8_lossy(&listed.stderr)
    );

    let listed = String::from_utf8_lossy(&listed.stdout);
    let exported: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();

    assert!(
        !exported.is_empty(),
        "{} exports nothing, so this check cannot have proven anything",
        object.display()
    );
    let stray: Vec<&&str> = exported
        .iter()
        .filter(|name| !name.starts_with("lowlat_"))
        .collect();
    assert!(
        stray.is_empty(),
        "{} exports {:?} without the project prefix",
        object.display(),
        stray
    );
}

/// **Nothing reaches the header without the prefix either**, which is the same
/// rule as the symbol table's and a different mechanism.
///
/// The first generated header carried `MAX_GUESTS`, `HOLD_MS` and four more
/// constants from modules that have nothing to do with the boundary, because
/// generating from the crate publishes every `pub const` in it. An application
/// that includes a header like that gets its own `MAX_GUESTS` redefined by
/// one it never asked for. Generating from the ABI module alone is the fix;
/// this is what says so when something works around it.
#[test]
fn the_header_declares_no_name_without_the_prefix() {
    let header = std::fs::read_to_string(root().join("include/lowlat.h")).expect("the header");

    let mut declared = Vec::new();
    for line in header.lines().map(str::trim) {
        let name = if let Some(rest) = line.strip_prefix("#define ") {
            rest.split_whitespace().next()
        } else if line.ends_with(';') && (line.starts_with("typedef ") || line.starts_with('}')) {
            // `typedef int32_t lowlat_status;` and the closing line of a
            // struct or enum, which is where its name is.
            line.trim_end_matches(';').split_whitespace().last()
        } else if line.ends_with(");") {
            // A function declaration. The name is what sits against the
            // opening parenthesis.
            line.split('(').next().and_then(|before| {
                before
                    .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
            })
        } else {
            None
        };
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            declared.push(name);
        }
    }

    assert!(
        declared.len() >= 6,
        "only found {declared:?} in the header, so this check cannot have proven anything"
    );
    // The handle's own type is the bare project name, which owns the
    // namespace just as surely as the prefixed names do.
    let stray: Vec<&&str> = declared
        .iter()
        .filter(|name| !name.starts_with("lowlat") && !name.starts_with("LOWLAT"))
        .collect();
    assert!(
        stray.is_empty(),
        "the header declares {stray:?} without the project prefix"
    );
}
