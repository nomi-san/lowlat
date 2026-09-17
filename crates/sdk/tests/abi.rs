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

/// The profile directory this test was built into, which is also where cargo
/// puts the shared object.
///
/// **Found by name, not by depth.** Where a test executable sits under the
/// profile directory has changed between cargo versions -- `deps/` in one,
/// `build/<crate>/<hash>/out/` in another -- and the shared object is put in
/// the profile directory by both.
fn artifacts() -> PathBuf {
    let exe = std::env::current_exe().expect("a running test has a path");
    exe.ancestors()
        .find(|dir| {
            dir.file_name()
                .is_some_and(|name| name == "debug" || name == "release")
        })
        .expect("a test executable lives under a profile directory")
        .to_path_buf()
}

/// The target triple this test was built for, when one was named.
///
/// A build for a named target lands one directory deeper than the target
/// directory itself, so the triple is the directory between the two. The
/// target directory is the workspace's unless the environment moved it.
fn named_target(profile: &Path) -> Option<String> {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root().join("target"), PathBuf::from)
        .canonicalize()
        .ok()?;
    let parent = profile.parent()?.canonicalize().ok()?;
    if parent == target_dir {
        return None;
    }
    parent.file_name()?.to_str().map(str::to_owned)
}

/// The flags with any sanitizer request taken out, in either spelling.
fn without_sanitizer(flags: &str) -> String {
    let mut kept = Vec::new();
    let mut tokens = flags.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        if token.starts_with("-Zsanitizer=") {
            continue;
        }
        if token == "-Z"
            && tokens
                .peek()
                .is_some_and(|next| next.starts_with("sanitizer="))
        {
            tokens.next();
            continue;
        }
        kept.push(token);
    }
    kept.join(" ")
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
    build.args(["build", "--quiet", "-p", "lowlat-sdk"]);
    // The test profile decides which directory this is running from, and the
    // build has to land in the same one.
    if profile.file_name().is_some_and(|name| name == "release") {
        build.arg("--release");
    }
    // **Named again when it was named.** Naming the target is what keeps the
    // flags the outer build was given -- a sanitizer among them -- off the
    // proc macros, which cargo compiles for the host only when a target is
    // named. Without it a sanitized derive cannot be loaded by the compiler
    // and the build fails on a crate it cannot find.
    if let Some(triple) = named_target(&profile) {
        build.args(["--target", &triple]);
    }
    // **Unsanitized, whatever this test was.** The object is opened by a C
    // harness compiled with no sanitizer runtime, and an object built with one
    // fails to load on that runtime's own symbols. What these tests check of
    // it -- its exported names, a panic held at the boundary -- is the same
    // either way; the library's own tests are what a sanitizer sees.
    build.env(
        "RUSTFLAGS",
        without_sanitizer(&std::env::var("RUSTFLAGS").unwrap_or_default()),
    );
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
/// **From this crate, which holds nothing but the boundary.** Generating from
/// a crate publishes every `pub const` in it, which is why the orchestration
/// lives in `lowlat-host` and not here: a type crossing the boundary is
/// defined in this crate, because a type from anywhere else cannot be seen
/// from the generator. Parsing the crate rather than one file is what carries
/// the feature on a module down to every item in it, so the host half comes
/// out under `LOWLAT_HOST`.
fn generate() -> String {
    let crate_dir = env!("CARGO_MANIFEST_DIR");
    let config = cbindgen::Config::from_root_or_default(crate_dir);
    let bindings = cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("the definitions parse");
    let mut out = Vec::new();
    bindings.write(&mut out);
    let header = String::from_utf8(out).expect("the generated header is text");
    to_doxygen(&realign_wrapped_arguments(&use_typedef_names(
        &merge_feature_guards(&header),
    )))
}

/// One guard around a run of items, not one around each.
///
/// **The generator guards every item it derives a feature for, members
/// included**, so a half comes out as a hundred `#if`/`#endif` pairs, one per
/// constant, and each enumerator inside an already guarded enumeration wears
/// its own. This drops a guard opened inside the same guard, and closes and
/// reopens nothing between two neighbours under the same one, so the header
/// reads as the two halves it is. Only the feature guards are touched; the
/// `#pragma`, the C++ fences and the `noexcept` block go through untouched.
fn merge_feature_guards(header: &str) -> String {
    const GUARDS: [&str; 3] = [
        "#if defined(LOWLAT_HOST)",
        "#if defined(LOWLAT_CLIENT)",
        "#if (defined(LOWLAT_HOST) || defined(LOWLAT_CLIENT))",
    ];
    let lines: Vec<&str> = header.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    // Every open `#if`, feature or not; a feature guard remembers whether it
    // was emitted (the outermost of its kind) so its `#endif` follows suit.
    let mut open: Vec<(Option<&str>, bool)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(guard) = GUARDS.iter().find(|g| line == **g).copied() {
            let already = open.iter().any(|(g, _)| *g == Some(guard));
            open.push((Some(guard), !already));
            if !already {
                out.push(line);
            }
        } else if line == "#endif" {
            let (guard, emitted) = open.pop().unwrap_or((None, true));
            match guard {
                Some(guard) if emitted => {
                    // Reopened by the next item under the same guard: keep
                    // the run open instead of closing and reopening it.
                    let mut j = i + 1;
                    while j < lines.len() && lines[j].trim().is_empty() {
                        j += 1;
                    }
                    if j < lines.len() && lines[j] == guard {
                        open.push((Some(guard), true));
                        // Keep exactly one blank line between the neighbours.
                        out.push("");
                        i = j + 1;
                        continue;
                    }
                    out.push(line);
                }
                Some(_) => {}
                None => out.push(line),
            }
        } else if line.starts_with("#if") {
            open.push((None, true));
            out.push(line);
        } else {
            out.push(line);
        }
        i += 1;
    }
    let mut merged = out.join("\n");
    if header.ends_with('\n') {
        merged.push('\n');
    }
    merged
}

/// Spell a type by the name its `typedef` gave it, everywhere it is used.
///
/// **The generator ties the tag to the keyword and cannot be asked for one
/// without the other.** It writes `struct` or `enum` in front of every use of
/// a type whose tag it emitted, so keeping the tags -- which is what lets an
/// application forward-declare a handle -- also means `enum lowlat_status
/// lowlat_host_create(const struct lowlat_host_create_info *)` at every signature and
/// every field. Both spellings name the same type in C and neither is one in
/// C++, so the tags stay and the keyword goes wherever the typedef already
/// says it: everywhere except the `typedef` that introduces it.
fn use_typedef_names(header: &str) -> String {
    const REDUNDANT: [&str; 3] = ["struct ", "union ", "enum "];

    let mut out = String::with_capacity(header.len());
    let mut rest = header;
    'text: while !rest.is_empty() {
        for keyword in REDUNDANT {
            if rest.starts_with(keyword)
                && rest[keyword.len()..].starts_with("lowlat")
                && !out.ends_with("typedef ")
            {
                rest = &rest[keyword.len()..];
                continue 'text;
            }
        }
        let ch = rest
            .chars()
            .next()
            .expect("the loop stops when nothing is left");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Hang the wrapped arguments under the parenthesis they belong to, and keep
/// the trailing macro on the line that closes it.
///
/// **The generator wrapped them against the longer spelling.** Dropping
/// `struct ` from a return type shortens the first line without moving the
/// lines that were aligned to its open parenthesis, so every wrapped signature
/// drifts right by exactly what came out of it. The macro is the generator's
/// own doing: it puts a postfix on its own line whenever it wrapped, which
/// reads as a stray statement rather than as part of the declaration.
fn realign_wrapped_arguments(header: &str) -> String {
    let mut out = String::with_capacity(header.len());
    // Where arguments hang from, and how many parentheses are still open.
    let mut column: Option<usize> = None;
    let mut depth = 0usize;

    for line in header.lines() {
        let line = match column {
            Some(column) if !line.is_empty() => {
                format!("{}{}", " ".repeat(column), line.trim_start())
            }
            _ => line.to_owned(),
        };

        if line == "LOWLAT_NOEXCEPT;" && out.ends_with(")\n") {
            out.truncate(out.len() - 1);
            out.push(' ');
        }
        out.push_str(&line);
        out.push('\n');

        if line.trim_start().starts_with("//") {
            continue;
        }
        for (at, ch) in line.char_indices() {
            match ch {
                '(' => {
                    if depth == 0 {
                        column = Some(at + 1);
                    }
                    depth += 1;
                }
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if depth == 0 {
            column = None;
        }
    }
    out
}

/// Say the documentation in the C toolchain's dialect rather than in Rust's.
///
/// **Both are load-bearing and neither side can hold both.** `# Safety` is
/// what `clippy::missing_safety_doc` looks for on an unsafe function and
/// ``[`name`]`` is how rustdoc links one, so the definitions keep them and the
/// header is translated, which is what this file is for.
///
/// **What it translates to is decided by what renders, not by what is most
/// precise.** The editor tooling most applications read this header with knows
/// a fixed set of block commands and drops any other silently, taking the text
/// under it with it: `@pre` is the accurate word for a caller's obligation and
/// it disappears, so the safety section becomes `@attention`, which that set
/// has. A cross-reference is the same trade the other way -- `@ref` links in a
/// generated site and reduces to undistinguished prose in a tooltip, so a name
/// keeps the backticks it already had and is code in both.
fn to_doxygen(header: &str) -> String {
    let mut out = String::with_capacity(header.len());
    let mut rest = header;

    while let Some(at) = rest.find("[`") {
        let (before, tail) = rest.split_at(at);
        match tail[2..].find("`]") {
            Some(end) => {
                out.push_str(before);
                out.push('`');
                out.push_str(&tail[2..2 + end]);
                out.push('`');
                rest = &tail[2 + end + 2..];
            }
            // A bracket that opens nothing is text; keep it and move past it.
            None => {
                out.push_str(before);
                out.push_str("[`");
                rest = &tail[2..];
            }
        }
    }
    out.push_str(rest);

    // The heading and the blank line under it become the one command that says
    // what they meant. **`@pre` is the accurate one and it is not the one to
    // use**: the editor tooling most applications read this header with drops
    // any block command it does not know, silently and without rendering what
    // was under it, and its set has no `@pre`. `@attention` is in it, and in
    // every documentation generator going back twenty years.
    let mut folded = String::with_capacity(out.len());
    let mut lines = out.lines().peekable();
    while let Some(line) = lines.next() {
        let indent = &line[..line.len() - line.trim_start().len()];
        if line.trim_start() == "/// # Safety"
            && lines.peek().map(|next| next.trim_start()) == Some("///")
        {
            lines.next();
            folded.push_str(indent);
            folded.push_str("/// @attention ");
            let body = lines.next().expect("a heading is followed by its section");
            folded.push_str(body.trim_start().trim_start_matches("/// "));
        } else {
            folded.push_str(line);
        }
        folded.push('\n');
    }
    folded
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
/// Regenerate with `LOWLAT_BLESS_HEADER=1 cargo test -p lowlat-sdk --test abi`.
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
        "{} is stale. Regenerate it: LOWLAT_BLESS_HEADER=1 cargo test -p lowlat-sdk --test abi",
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
    let source = root().join("crates/sdk/tests/c/alone.c");
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

    // **And with either half hidden**, which is what an application built
    // against a library carrying one half does: the shared types must still
    // be there, and nothing of the hidden half may be.
    for (hidden, name) in [
        ("-DLOWLAT_NO_HOST", "no-host"),
        ("-DLOWLAT_NO_CLIENT", "no-client"),
    ] {
        let object = dir.join(format!("alone-{name}.o"));
        let object = object.to_string_lossy().to_string();
        let mut args: Vec<&str> = vec!["-std=c11", hidden];
        args.extend(warnings);
        args.extend(["-I", &include, "-c", &source, "-o", &object]);
        if let Err(why) = compile("cc", &args) {
            panic!("the header does not compile with {hidden}:\n{why}");
        }
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
    let source = root().join("crates/sdk/tests/c/harness.c");
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
        } else if line.split_once(" = ").is_some_and(|(name, _)| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        }) {
            // An enumerator. These carry the codes now, so they are exactly
            // the names an application collides with.
            line.split_once(" = ").map(|(name, _)| name)
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
        // A closing brace with no name after it is a block ending, not a
        // declaration: `};` closes an enum whose name was on the opening line.
        if let Some(name) =
            name.filter(|name| name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_'))
        {
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

/// **The C mirror's own arithmetic, so a field added on one side and not the
/// other is caught here rather than at a caller's size check.**
///
/// A caller stamps `size` from its own `sizeof` and this library refuses
/// anything smaller than its own, so a mirror that has drifted fails at every
/// call with one invalid-argument status and names nothing. The number below
/// is written out from the fields rather than taken from `size_of`, because
/// taking it from `size_of` would agree with any layout at all.
#[test]
fn the_status_struct_is_the_size_its_fields_come_to() {
    use lowlat::abi::{LOWLAT_OUTPUT_MAX, lowlat_host_status};
    let fields = 4 * 4        // size, guests, width, height
        + 1 + 1 + 1 + 1       // running, audio_active, ten_bit, reserved
        + 4 + 4               // codec, chroma
        + LOWLAT_OUTPUT_MAX; // audio_device
    assert_eq!(
        core::mem::size_of::<lowlat_host_status>(),
        fields,
        "the status struct is not the size its fields come to; a mirror stamping its own \
         sizeof will be refused"
    );
    assert_eq!(core::mem::align_of::<lowlat_host_status>(), 4);
}
