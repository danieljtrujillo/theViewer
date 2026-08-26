// Link the MinGW C++ runtime into the executable instead of depending on its DLL.
//
// `charls-sys`, behind DICOM's JPEG-LS support, compiles C++ and needs `libstdc++`. The
// `windows-gnu` toolchain links it dynamically by default, which leaves `viewer.exe` needing
// `libstdc++-6.dll` on PATH. That is true when the build script launches the app, because it puts MinGW
// on PATH first, and false the moment Explorer launches it from the Open with menu. The failure
// is a system error box before `main` ever runs. The dynamic request is turned off by the
// `nothing` feature on `link-cplusplus` in `Cargo.toml`, so what follows is the only source of
// C++ runtime symbols.
//
// **Position on the link line is the whole trick, and it is not the position anyone expects.**
// `rustc` lays out the command as: crate objects, rlibs, native libraries from build scripts,
// its own runtime libraries, and finally everything from `rustc-link-arg`. A `rustc-link-lib`
// therefore lands *ahead* of the rlibs that need it, and GNU `ld` resolves an archive only
// against undefined symbols it has already seen, so the C++ runtime has to go at the very end,
// as link arguments, together with everything it in turn depends on.
//
// This file is `include!`d by `crates/app/build.rs`, because link arguments apply only to the
// package that emits them and both this crate's test harness and the binary need them.
//
// None of it applies to the `msvc` toolchain, which is why it is guarded on the target
// environment rather than simply on Windows.

/// The C++ runtime and everything it needs, as one group.
///
/// A plain list would have to be in exact dependency order, and the order is not a chain but a
/// cycle: libstdc++ calls into libgcc for thread-local storage, libgcc calls into winpthread for
/// its mutexes, and winpthread calls back into the C runtime. `--start-group` makes the linker
/// re-scan until nothing new resolves, which is the answer to a cycle.
///
/// `libmsvcrt.a` and `libkernel32.a` are import libraries, so listing them under `-Bstatic` still
/// produces ordinary DLL imports, it selects the archive, not the linkage.
const TRAILING_LINK_ARGS: &[&str] = &[
    "-Wl,-Bstatic",
    "-Wl,--start-group",
    "-lstdc++",
    "-lgcc",
    // `__emutls_get_address`, thread-local storage support, which on this toolchain lives in
    // `libgcc_eh.a` rather than `libgcc.a`.
    "-lgcc_eh",
    // A POSIX-threads MinGW build calls straight into winpthread.
    "-lwinpthread",
    // `__mingw_sprintf` and the rest of the C99 shims MinGW layers over msvcrt.
    "-lmingwex",
    "-lmsvcrt",
    // `FormatMessageA` and `LocalFree`, used by the standard error category.
    "-lkernel32",
    "-Wl,--end-group",
    "-Wl,-Bdynamic",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // The *target* environment, not the host: a build script's own `cfg!` describes the machine
    // it is running on, which is not the question being asked here.
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let family = std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    if family != "windows" || env != "gnu" {
        return;
    }

    for arg in TRAILING_LINK_ARGS {
        println!("cargo:rustc-link-arg={arg}");
    }
}
