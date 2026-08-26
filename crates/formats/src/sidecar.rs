//! Companion-file resolution and image decoding.
//!
//! Most mesh formats reference their textures by a path written on another machine. Those paths
//! are absolute more often than not, use the wrong separator, and name a directory layout that no
//! longer exists. What survives reliably is the file name, so resolution walks outward from the
//! model's own directory and matches on the base name.

use std::path::{Path, PathBuf};

use viewer_scene::Image;

/// Subdirectories worth checking. Exporters scatter textures into these often enough that the
/// alternative, failing to find them, is the common case without this list.
const TEXTURE_DIRS: [&str; 8] = [
    "",
    "textures",
    "Textures",
    "maps",
    "Maps",
    "images",
    "Images",
    "tex",
];

/// Extensions to try when the referenced one does not exist. Exporters routinely write `.tga` into
/// the material and ship `.png` beside the model.
const TEXTURE_EXTS: [&str; 7] = ["png", "jpg", "jpeg", "tga", "bmp", "webp", "gif"];

/// Locate a texture referenced by a material.
///
/// `reference` is taken verbatim from the model file and may be absolute, Windows-style, or
/// relative. `base` is the directory holding the model.
pub fn resolve_texture(base: &Path, reference: &str) -> Option<PathBuf> {
    let reference = reference.trim().trim_matches('"');
    if reference.is_empty() {
        return None;
    }

    // Normalize separators, then keep only the trailing file name. An absolute path from another
    // machine is worse than useless: it may exist here and point at something unrelated.
    let normalized = reference.replace('\\', "/");
    let name = normalized.rsplit('/').next()?;
    if name.is_empty() {
        return None;
    }

    // The path as written, in case it really is relative and correct.
    let direct = base.join(&normalized);
    if direct.is_file() {
        return Some(direct);
    }

    let stem = Path::new(name).file_stem()?.to_str()?;
    for dir in TEXTURE_DIRS {
        let folder = if dir.is_empty() {
            base.to_path_buf()
        } else {
            base.join(dir)
        };
        if !folder.is_dir() {
            continue;
        }
        let exact = folder.join(name);
        if exact.is_file() {
            return Some(exact);
        }
        for ext in TEXTURE_EXTS {
            let swapped = folder.join(format!("{stem}.{ext}"));
            if swapped.is_file() {
                return Some(swapped);
            }
        }
        // Last resort: a case-insensitive scan of the folder.
        if let Some(found) = scan_case_insensitive(&folder, stem) {
            return Some(found);
        }
    }
    None
}

fn scan_case_insensitive(folder: &Path, stem: &str) -> Option<PathBuf> {
    let target = stem.to_ascii_lowercase();
    let entries = std::fs::read_dir(folder).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(s) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if s.to_ascii_lowercase() != target {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if ext.is_some_and(|e| TEXTURE_EXTS.contains(&e.as_str())) {
            return Some(path);
        }
    }
    None
}

/// Decode an image file to RGBA8. Returns `None` rather than failing the whole import, because a
/// missing or corrupt texture should still leave a viewable mesh.
pub fn load_image(path: &Path) -> Option<Image> {
    let bytes = std::fs::read(path).ok()?;
    decode_image(&bytes, path.file_name()?.to_str()?)
}

pub fn decode_image(bytes: &[u8], name: &str) -> Option<Image> {
    let decoded = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    Some(Image {
        name: name.to_string(),
        width,
        height,
        rgba: decoded.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("viewer-sidecar-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_windows_absolute_path_falls_back_to_the_file_name() {
        let dir = temp_dir("abs");
        fs::write(dir.join("skin.png"), b"x").unwrap();
        let found = resolve_texture(&dir, r"D:\Work\Old Project\maps\skin.png").unwrap();
        assert_eq!(found.file_name().unwrap(), "skin.png");
    }

    #[test]
    fn a_texture_subdirectory_is_searched() {
        let dir = temp_dir("subdir");
        fs::create_dir_all(dir.join("textures")).unwrap();
        fs::write(dir.join("textures/atlas.png"), b"x").unwrap();
        assert!(resolve_texture(&dir, "atlas.png").is_some());
    }

    #[test]
    fn a_different_extension_is_accepted_when_the_named_one_is_absent() {
        let dir = temp_dir("ext");
        fs::write(dir.join("body.png"), b"x").unwrap();
        // The material names a TGA; only the PNG exists.
        let found = resolve_texture(&dir, "body.tga").unwrap();
        assert_eq!(found.extension().unwrap(), "png");
    }

    #[test]
    fn case_differences_are_tolerated() {
        let dir = temp_dir("case");
        fs::write(dir.join("Atlas_01.PNG"), b"x").unwrap();
        assert!(resolve_texture(&dir, "atlas_01.png").is_some());
    }

    #[test]
    fn a_missing_texture_resolves_to_nothing() {
        let dir = temp_dir("missing");
        assert!(resolve_texture(&dir, "nope.png").is_none());
    }

    #[test]
    fn an_empty_reference_is_rejected() {
        let dir = temp_dir("empty");
        assert!(resolve_texture(&dir, "").is_none());
        assert!(resolve_texture(&dir, "   ").is_none());
    }

    #[test]
    fn a_corrupt_image_returns_none_rather_than_failing() {
        assert!(decode_image(&[0xDE, 0xAD, 0xBE, 0xEF], "bad.png").is_none());
    }

    #[test]
    fn a_real_png_decodes_to_rgba() {
        // 1x1 opaque red PNG.
        let png = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let img = decode_image(&png, "red.png").unwrap();
        assert_eq!((img.width, img.height), (1, 1));
        assert!(img.is_consistent());
        assert_eq!(img.rgba[0], 255);
    }
}
