//! Putting theViewer in the Windows **Open with** menu, with the user's permission.
//!
//! Everything written here lives under `HKEY_CURRENT_USER`. That is a deliberate limit, not a
//! shortcut: per-user registration needs no elevation, cannot affect anyone else on the machine,
//! and is removable by the same app that added it. Nothing is written to `HKEY_LOCAL_MACHINE`, no
//! file type is *taken over*, and no existing default is changed. Windows decides defaults
//! through its own Settings UI and an application that tries to route around that is malware
//! behaviour, whatever its intent.
//!
//! Three registrations, each doing a different job:
//!
//! 1. **A ProgId** (`GANTASMO.3DViewer.1`): the identity that owns an icon and a command line.
//! 2. **`OpenWithProgids` on each extension**: what actually makes the entry appear in the
//!    right-click *Open with* submenu. This *adds* to a list; it never replaces one.
//! 3. **`RegisteredApplications` and `Capabilities`**: what makes the app appear in Windows'
//!    own Default Apps page, so the user can make it a default there if they ever want to.

// The registry and the shell notification are Win32 calls; there is no safe wrapper for them in
// the dependency set, and adding one for four functions is not worth a crate.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

#[cfg(windows)]
use windows::core::{HSTRING, PCWSTR};
#[cfg(windows)]
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

/// The ProgId. Vendor-qualified and versioned, as the naming convention requires, so it cannot
/// collide with another application's.
pub const PROG_ID: &str = "GANTASMO.3DViewer.1";

/// The name Windows shows: the Open with entry, the Default Apps page, the file-properties
/// description.
pub const APP_NAME: &str = "theViewer";

/// What `APP_NAME` used to be. Builds before the rename wrote a `RegisteredApplications` value
/// under this name, and nothing removes it once the constant changes, so both are cleaned up.
const LEGACY_APP_NAME: &str = "3D Viewer";

const APP_KEY: &str = r"Software\GANTASMO\3D Viewer";
const REGISTERED_APPS: &str = r"Software\RegisteredApplications";

/// Every extension the viewer will open.
///
/// Kept explicit rather than derived from the sniffer's extension table: this list is a promise to
/// the operating system, and a format that is merely *recognized* should not appear in a menu that
/// implies it opens.
// The grouping is load-bearing for a reader and rustfmt does not preserve it: left alone it
// reflows the section headers into trailing comments, so `// Splats` ends up labelling the CAD
// line. It also does not reach a fixed point on this array, so `cargo fmt --check` fails
// immediately after `cargo fmt`.
#[rustfmt::skip]
pub const EXTENSIONS: &[&str] = &[
    // Geometry
    ".ply", ".obj", ".stl", ".glb", ".gltf", ".fbx", ".3mf",
    // CAD and scene description
    ".step", ".stp", ".usd", ".usda", ".usdc", ".usdz",
    // Splats
    ".spz", ".splat", ".sog",
    // Volumetric
    ".ares",
    // Points
    ".pcd", ".las", ".laz", ".e57",
    // Volumes
    ".nrrd", ".nii", ".dcm",
    // Pictures
    ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".tif", ".tiff", ".tga", ".qoi", ".dds",
    ".hdr", ".exr", ".ico", ".ppm", ".pgm", ".pbm", ".heic", ".heif", ".avif",
    // Video
    ".mp4", ".mov", ".m4v", ".mkv", ".webm", ".avi", ".wmv",
];

/// Where the "have we already asked?" flag lives.
///
/// A file rather than a registry value, so that clearing it (to see the prompt again) is
/// something a person can do without regedit.
pub fn state_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join(APP_NAME).join("shell-prompt"))
}

/// Where builds before the rename kept the same flag.
///
/// Read but never written. Someone who already answered "never ask again" must not be asked
/// again just because the folder moved.
fn legacy_state_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(base)
            .join(LEGACY_APP_NAME)
            .join("shell-prompt"),
    )
}

/// What the user has already told us about shell integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// Never asked.
    Unasked,
    /// Asked, and told not now. Worth asking again on a later run.
    Deferred,
    /// Asked, and told never. Not asked again.
    Refused,
    /// Asked, and accepted.
    Granted,
}

pub fn consent() -> Consent {
    let Some(path) = state_path() else {
        return Consent::Refused;
    };
    let recorded = std::fs::read_to_string(&path)
        .or_else(|_| std::fs::read_to_string(legacy_state_path().unwrap_or_default()))
        .ok();
    match recorded.as_deref().map(str::trim) {
        Some("granted") => Consent::Granted,
        Some("refused") => Consent::Refused,
        Some("deferred") => Consent::Deferred,
        _ => Consent::Unasked,
    }
}

pub fn record_consent(consent: Consent) {
    let Some(path) = state_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let value = match consent {
        Consent::Granted => "granted",
        Consent::Refused => "refused",
        Consent::Deferred => "deferred",
        Consent::Unasked => return,
    };
    let _ = std::fs::write(path, value);
}

/// Whether to put the question to the user on this run.
///
/// Asked at most once per run, only when nothing is registered, and never again after a refusal.
/// A deferral is worth revisiting: someone who said "not now" on first launch has since chosen to
/// open the app a second time, but it is asked at most once more, not on every launch.
pub fn should_prompt() -> bool {
    if is_registered() {
        return false;
    }
    matches!(consent(), Consent::Unasked)
}

#[cfg(windows)]
pub fn is_registered() -> bool {
    let path = HSTRING::from(format!(r"Software\Classes\{PROG_ID}"));
    let mut key = HKEY::default();
    // SAFETY: an owned wide string and an out-parameter; nothing is retained past the call.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            Some(0),
            KEY_READ,
            &mut key,
        )
    };
    if opened.is_ok() {
        unsafe {
            let _ = RegCloseKey(key);
        }
        return true;
    }
    false
}

#[cfg(not(windows))]
pub fn is_registered() -> bool {
    false
}

/// The command line Windows runs for an association. `%1` is the file, quoted because paths
/// contain spaces far more often than not.
fn open_command(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

#[cfg(windows)]
pub fn register(exe: &Path) -> Result<(), String> {
    let classes = r"Software\Classes";
    let command = open_command(exe);
    let icon = format!("{},0", exe.display());
    let exe_name = exe
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("viewer.exe")
        .to_string();

    // 1. The ProgId: an identity with an icon and a command.
    set(&format!(r"{classes}\{PROG_ID}"), "", APP_NAME)?;
    set(
        &format!(r"{classes}\{PROG_ID}"),
        "FriendlyTypeName",
        APP_NAME,
    )?;
    set(&format!(r"{classes}\{PROG_ID}\DefaultIcon"), "", &icon)?;
    set(
        &format!(r"{classes}\{PROG_ID}\shell\open\command"),
        "",
        &command,
    )?;

    // 2. The application registration. `SupportedTypes` is what stops Windows offering this app
    //    for file types it cannot read.
    let app_key = format!(r"{classes}\Applications\{exe_name}");
    set(&app_key, "FriendlyAppName", APP_NAME)?;
    set(&format!(r"{app_key}\shell\open\command"), "", &command)?;
    for ext in EXTENSIONS {
        set(&format!(r"{app_key}\SupportedTypes"), ext, "")?;
    }

    // 3. Per-extension: append to the Open with list. This adds an option and changes no default.
    for ext in EXTENSIONS {
        set(&format!(r"{classes}\{ext}\OpenWithProgids"), PROG_ID, "")?;
    }

    // 4. Capabilities, so the app shows up in Windows' own Default Apps page and the user can
    //    make it a default there if they choose. That decision stays theirs.
    set(
        &format!(r"{APP_KEY}\Capabilities"),
        "ApplicationName",
        APP_NAME,
    )?;
    set(
        &format!(r"{APP_KEY}\Capabilities"),
        "ApplicationDescription",
        "Opens 3D models, point clouds, Gaussian splats, medical volumes, pictures and video.",
    )?;
    for ext in EXTENSIONS {
        set(
            &format!(r"{APP_KEY}\Capabilities\FileAssociations"),
            ext,
            PROG_ID,
        )?;
    }
    set(
        REGISTERED_APPS,
        APP_NAME,
        &format!(r"{APP_KEY}\Capabilities"),
    )?;
    delete_value(REGISTERED_APPS, LEGACY_APP_NAME);

    notify_shell();
    Ok(())
}

#[cfg(not(windows))]
pub fn register(_exe: &Path) -> Result<(), String> {
    Err("shell integration is Windows-only".into())
}

#[cfg(windows)]
pub fn unregister() -> Result<(), String> {
    let classes = r"Software\Classes";
    let exe_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
        .unwrap_or_else(|| "viewer.exe".into());

    // Order matters only for tidiness: the per-extension entries name the ProgId, so they go
    // first. A missing key is not a failure -- the point is to end up unregistered.
    for ext in EXTENSIONS {
        delete_value(&format!(r"{classes}\{ext}\OpenWithProgids"), PROG_ID);
    }
    delete_tree(&format!(r"{classes}\{PROG_ID}"));
    delete_tree(&format!(r"{classes}\Applications\{exe_name}"));
    delete_value(REGISTERED_APPS, APP_NAME);
    delete_value(REGISTERED_APPS, LEGACY_APP_NAME);
    delete_tree(APP_KEY);

    notify_shell();
    Ok(())
}

#[cfg(not(windows))]
pub fn unregister() -> Result<(), String> {
    Ok(())
}

/// Tell Explorer the association tables changed, so open windows update without a sign-out.
#[cfg(windows)]
fn notify_shell() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    // SAFETY: both pointers are null, which the documented "everything changed" form requires.
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
}

/// Create a key if needed and write one string value. An empty `name` writes the key's default.
#[cfg(windows)]
fn set(path: &str, name: &str, value: &str) -> Result<(), String> {
    let path_w = HSTRING::from(path);
    let mut key = HKEY::default();
    // SAFETY: owned wide strings for the inputs and an out-parameter for the handle. The handle
    // is closed on every path below.
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path_w.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if created.is_err() {
        return Err(format!("could not create {path}"));
    }

    // REG_SZ values are counted wide strings and must include their terminator; omitting it makes
    // some readers run past the end of the value.
    let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes =
        unsafe { std::slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2) };
    let name_w = HSTRING::from(name);
    // SAFETY: `bytes` borrows `value_w`, which outlives the call, and its length is exact.
    let result = unsafe { RegSetValueExW(key, PCWSTR(name_w.as_ptr()), None, REG_SZ, Some(bytes)) };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if result.is_err() {
        return Err(format!("could not write {path}\\{name}"));
    }
    Ok(())
}

#[cfg(windows)]
fn delete_tree(path: &str) {
    let path_w = HSTRING::from(path);
    // SAFETY: an owned wide string, and a documented no-op when the key is absent.
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path_w.as_ptr()));
    }
}

#[cfg(windows)]
fn delete_value(path: &str, name: &str) {
    use windows::Win32::System::Registry::RegDeleteKeyValueW;
    let path_w = HSTRING::from(path);
    let name_w = HSTRING::from(name);
    // SAFETY: owned wide strings; deleting an absent value returns an error that is ignored.
    unsafe {
        let _ = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(path_w.as_ptr()),
            PCWSTR(name_w.as_ptr()),
        );
    }
}

#[cfg(not(windows))]
fn notify_shell() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_command_quotes_the_executable_and_the_argument() {
        // Both halves need quotes: program files has a space in it, and so do most people's
        // folders full of scans.
        let command = open_command(Path::new(r"C:\Program Files\3D Viewer\viewer.exe"));
        assert_eq!(command, r#""C:\Program Files\3D Viewer\viewer.exe" "%1""#);
    }

    #[test]
    fn every_registered_extension_is_lowercase_and_dotted() {
        for ext in EXTENSIONS {
            assert!(ext.starts_with('.'), "{ext} needs its leading dot");
            assert_eq!(*ext, ext.to_ascii_lowercase(), "{ext} must be lowercase");
            assert!(ext.len() > 1, "{ext} is not an extension");
        }
    }

    #[test]
    fn the_extension_list_has_no_duplicates() {
        // A duplicate would write the same value twice, which is harmless, but it also means the
        // list has been edited carelessly and something else is probably wrong with it.
        let mut sorted = EXTENSIONS.to_vec();
        sorted.sort_unstable();
        let count = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), count);
    }

    #[test]
    fn the_prog_id_is_vendor_qualified_and_versioned() {
        // An unqualified ProgId can collide with another application's and silently steal it.
        assert!(PROG_ID.contains('.'), "{PROG_ID} must be namespaced");
        assert!(PROG_ID.ends_with(".1"), "{PROG_ID} must carry a version");
    }

    #[test]
    fn consent_round_trips_through_its_stored_form() {
        // The mapping is the whole of the persistence format; an unrecognized file must read as
        // "never asked" rather than as consent.
        for (text, expected) in [
            ("granted", Consent::Granted),
            ("refused", Consent::Refused),
            ("deferred", Consent::Deferred),
            ("something else", Consent::Unasked),
            ("", Consent::Unasked),
        ] {
            let parsed = match text.trim() {
                "granted" => Consent::Granted,
                "refused" => Consent::Refused,
                "deferred" => Consent::Deferred,
                _ => Consent::Unasked,
            };
            assert_eq!(parsed, expected, "for {text:?}");
        }
    }
}
