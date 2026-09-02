// Two jobs: the C++ runtime link arguments shared with `viewer-formats`, and the Windows
// resources that give `viewer.exe` its icon and its file-properties name.

include!("../formats/build_link.rs");

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../formats/build_link.rs");
    println!("cargo:rerun-if-changed=../../assets/icon.ico");

    emit_trailing_link_args();
    embed_resources();
}

/// Compile an icon and a version block into the executable.
///
/// Explorer, the taskbar and the Open with menu all read the icon out of the binary rather than
/// from any file beside it, so a self-contained executable has to carry its own. `DefaultIcon` in
/// the registry points at `viewer.exe,0`, which is this resource.
///
/// Skipped rather than failed when `windres` is missing: a missing icon is a cosmetic problem and
/// stopping the build over it would be worse than the blank default.
fn embed_resources() {
    if !targets_windows_gnu() {
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set for build scripts"));
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set for build scripts"),
    );
    let icon = manifest.join("../../assets/icon.ico");
    if !icon.exists() {
        println!("cargo:warning=assets/icon.ico is missing; run `node tools/make-icons.mjs`");
        return;
    }

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut parts = version.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );

    let rc_path = out.join("viewer.rc");
    let rc = resource_script(&icon, &version, major, minor, patch);
    if std::fs::write(&rc_path, rc).is_err() {
        println!("cargo:warning=could not write the resource script; the icon is not embedded");
        return;
    }

    let object = out.join("viewer-resources.o");
    let status = std::process::Command::new("windres")
        .arg(&rc_path)
        .args(["-O", "coff", "-o"])
        .arg(&object)
        .status();

    match status {
        Ok(status) if status.success() => {
            // Applied to binaries only. The test harness links the same crate and has no use for
            // an icon.
            println!("cargo:rustc-link-arg-bins={}", object.display());
        }
        Ok(status) => {
            println!("cargo:warning=windres exited with {status}; the icon is not embedded");
        }
        Err(e) => {
            println!("cargo:warning=windres could not be run ({e}); the icon is not embedded");
        }
    }
}

/// The `.rc` source, generated so the version never drifts from `Cargo.toml`.
///
/// Resource 1 is the icon Windows shows for the executable: the shell picks the lowest-numbered
/// `ICON` resource, so the number is not arbitrary. Backslashes are escaped because a resource
/// script takes C string escapes.
fn resource_script(icon: &Path, version: &str, major: u16, minor: u16, patch: u16) -> String {
    let icon = icon.display().to_string().replace('\\', "\\\\");
    format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "GANTASMO"
      VALUE "FileDescription", "theViewer"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "viewer"
      VALUE "LegalCopyright", "Copyright (c) 2026 Daniel Joaquin Trujillo"
      VALUE "OriginalFilename", "viewer.exe"
      VALUE "ProductName", "theViewer"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}
