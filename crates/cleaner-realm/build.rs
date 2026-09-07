//! Builds realm-core's static C API library and generates Rust bindings for it.
//!
//! osu!lazer stores its library in a Realm database (file format 24, schema 52). No Rust
//! Realm SDK exists, so we bind realm-core's official C API (`src/realm.h`) — the surface
//! Realm maintainers point non-SDK languages at.

use std::path::{Path, PathBuf};

/// Vendored realm-core checkout, pinned to the tag matching the Realm .NET version osu uses.
const REALM_CORE_DIR: &str = "vendor/realm-core";

fn main() {
    // Read at runtime, not through `env!`. Cargo fingerprints build scripts by content, so a
    // compile-time path can be baked in from a different checkout that shares this file.
    let manifest =
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo");
    let vendor = PathBuf::from(manifest).join(REALM_CORE_DIR);
    let header = vendor.join("src/realm.h");

    assert!(
        header.is_file(),
        "realm-core sources missing at {}; run `git submodule update --init --recursive`",
        vendor.display()
    );

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", header.display());

    patch_realm_core(&vendor);
    let build_dir = build_realm_core(&vendor);
    emit_link_flags(&build_dir);
    generate_bindings(&vendor, &header);
}

/// Makes realm-core compile under clang-cl, which the Windows cross-build uses.
///
/// `array.hpp` returns `-0x8000000000000000LL` for the 64-bit lower bound, but the literal
/// `0x8000000000000000` is 9223372036854775808 and does not fit in `long long`, so the
/// expression is ill-formed. MSVC and GCC accept it; clang rejects it inside a `constexpr`
/// function, which is where realm-core uses it. `-0x7fffffffffffffffLL - 1` is the same value
/// and is well-formed everywhere.
///
/// The equivalent diff is kept at `patches/0001-clang-cl-compat.patch` for review. It is
/// applied here in Rust rather than shelled out to `git apply` or `patch` so that the build
/// does not depend on either tool being installed. Rewriting is skipped when the fix is
/// already present, so this is idempotent and leaves the submodule clean on repeat builds.
fn patch_realm_core(vendor: &Path) {
    const BROKEN: &str = "return -0x8000000000000000LL;";
    const FIXED: &str = "return -0x7fffffffffffffffLL - 1;";

    let target = vendor.join("src/realm/array.hpp");
    let source = std::fs::read_to_string(&target).expect("failed to read realm-core array.hpp");

    if !source.contains(BROKEN) {
        assert!(
            source.contains(FIXED),
            "neither the original nor the patched literal found in {}; realm-core may have \
             changed upstream and this patch needs revisiting",
            target.display()
        );
        return;
    }

    std::fs::write(&target, source.replace(BROKEN, FIXED))
        .expect("failed to patch realm-core array.hpp");
}

/// Compiles `RealmFFIStatic` and its dependencies, returning the `CMake` build directory.
fn build_realm_core(vendor: &Path) -> PathBuf {
    let mut config = cmake::Config::new(vendor);

    config
        .build_target("RealmFFIStatic")
        // We never query geospatial types, and this drops the heavy `external/s2` dependency.
        .define("REALM_ENABLE_GEOSPATIAL", "OFF")
        // osu!lazer never encrypts `client.realm`, so we do not need to read encrypted files.
        // Leaving this on would drag in OpenSSL (and transitively zlib) on Linux only —
        // `REALM_NEEDS_OPENSSL` is set for `Linux|Android`, while Windows uses Win32 crypto.
        // Turning it off keeps the dependency surface identical on both targets.
        .define("REALM_ENABLE_ENCRYPTION", "OFF")
        // Skip the test suite, tools, and the bson external they pull in.
        .define("REALM_BUILD_LIB_ONLY", "ON")
        .define("REALM_NO_TESTS", "ON")
        // realm-core's vendored externals predate CMake 4's policy floor.
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        // Always build realm-core optimised, whichever profile cargo is using. We never debug
        // into it, and an unoptimised realm-core makes every scan slower.
        //
        // Naming the profile also keeps the output layout predictable. Left to itself, cmake-rs
        // configures with one build type and then builds `--config RelWithDebInfo`, which
        // multi-config generators such as Visual Studio honour by nesting artifacts in a
        // directory nobody went looking in. Ninja, used by the cross-build, ignores the whole
        // question, so the mismatch only appears on a native Windows build.
        .profile("Release");

    if is_clang_cl() {
        // realm-core defines `REALM_COMPILER_SSE` unconditionally for 64-bit x86
        // (`utilities.hpp:71-74`) and then calls SSE4.2 intrinsics such as `_mm_cmpeq_epi64`.
        // MSVC accepts those without any target-feature flag; clang refuses to inline an
        // `always_inline` intrinsic into a function compiled without the feature. Enabling
        // SSE4.2 explicitly is what the MSVC build effectively does anyway.
        //
        // The cost is that a cross-built binary requires an SSE4.2-capable CPU, which means
        // anything from 2008 onwards. The natively built CI artifact does not carry that
        // floor, so this only affects locally cross-compiled executables.
        config.cxxflag("/clang:-msse4.2");
    }

    config.build()
}

/// Reports whether the C++ compiler for this build is `clang-cl` rather than MSVC.
///
/// `cargo-xwin` points `CMake` at its own clang-cl toolchain file, which is the signal we key
/// on: the same build under a real MSVC toolchain must not receive clang-only flags.
fn is_clang_cl() -> bool {
    println!("cargo:rerun-if-env-changed=CMAKE_TOOLCHAIN_FILE");
    println!("cargo:rerun-if-env-changed=CXX_x86_64_pc_windows_msvc");

    let toolchain = std::env::var("CMAKE_TOOLCHAIN_FILE").unwrap_or_default();
    let cxx = std::env::var("CXX_x86_64_pc_windows_msvc").unwrap_or_default();

    toolchain.contains("clang-cl") || cxx.contains("clang")
}

/// The libraries `RealmFFIStatic` needs, in link order.
///
/// These are `CMake` `OUTPUT_NAME`s, which differ from the target names: `RealmFFIStatic`
/// becomes `realm-ffi-static`, `ObjectStore` becomes `realm-object-store`, `QueryParser`
/// becomes `realm-parser`, and `Storage` becomes `realm`. Dependents come before their
/// dependencies, as static linking requires. `Bid` is an object library that is already folded
/// into the `Storage` archive.
const REALM_LIBRARIES: &[&str] = &[
    "realm-ffi-static",
    "realm-object-store",
    "realm-parser",
    "realm",
];

/// Tells cargo where the static libraries are and which ones to link.
fn emit_link_flags(build_dir: &Path) {
    // `build_target` skips the install step, so artifacts stay in the CMake tree. Where in
    // that tree depends on the generator: Ninja writes one file per target directory, while
    // the Visual Studio generator nests each build configuration in its own subdirectory. So
    // each library is located by name rather than by guessing at the layout.
    let root = build_dir.join("build");

    for library in REALM_LIBRARIES {
        let Some(directory) = find_library(&root, library) else {
            panic!(
                "built realm-core but could not find {library} under {}; the CMake generator \
                 may have used a layout this build script does not understand",
                root.display()
            );
        };

        println!("cargo:rustc-link-search=native={}", directory.display());
        println!("cargo:rustc-link-lib=static={library}");
    }

    // realm-core is C++, so the C++ runtime has to come along. MSVC links its own runtime
    // automatically via `/MD`, so naming one there would fail.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
}

/// Finds the directory holding a static library, searching the whole build tree.
///
/// Accepts either naming convention, because the same source builds as `realm.lib` under MSVC
/// and `librealm.a` everywhere else.
fn find_library(root: &Path, name: &str) -> Option<PathBuf> {
    let candidates = [format!("{name}.lib"), format!("lib{name}.a")];
    let mut stack = vec![root.to_path_buf()];

    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if path.is_dir() {
                stack.push(path);
                continue;
            }

            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if candidates.iter().any(|candidate| candidate == file_name) {
                return Some(directory);
            }
        }
    }

    None
}

/// Generates Rust declarations for the `realm_*` C API.
fn generate_bindings(vendor: &Path, header: &Path) {
    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .clang_arg(format!("-I{}", vendor.join("src").display()))
        // The C API is the only surface we bind; everything else is C++ and unparseable here.
        .allowlist_function("realm_.*")
        .allowlist_type("realm_.*")
        .allowlist_var("RLM_.*")
        .derive_debug(true)
        .derive_default(true)
        .generate()
        .expect("failed to generate realm-core bindings");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is always set by cargo"));
    bindings
        .write_to_file(out.join("bindings.rs"))
        .expect("failed to write realm-core bindings");
}
