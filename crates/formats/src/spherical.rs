//! Deciding whether a picture or a video is a panorama, and how its two eyes are packed.
//!
//! Three sources of evidence, and they are not equally good:
//!
//! 1. **Metadata.** Google's XMP `GPano` schema for stills, and the Spherical Video V1 XML / V2
//!    `st3d`+`sv3d` boxes for MP4. When present this is a statement of fact by the capture rig.
//! 2. **The file name.** `_360`, `VR180`, `_TB`, `_LR`. Conventions rather than standards, but the
//!    only thing many exported files carry.
//! 3. **The aspect ratio.** A 2:1 image *might* be equirectangular. On its own that is a guess,
//!    and it is wrong often enough — book scans, film stills, ultrawide screenshots — that it is
//!    gated behind a resolution floor and always reversible from the toolbar.
//!
//! The rule the rest of the app relies on: a wrong guess must be one click from being corrected,
//! and the viewer must say which of the three it used.

use viewer_scene::{ScreenProjection, StereoLayout};

/// What was decided, and on what evidence. The evidence string is shown in the inspector, because
/// "this is a 360 photo" and "this is 2:1 so it might be a 360 photo" are different claims.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spherical {
    pub projection: ScreenProjection,
    pub stereo: StereoLayout,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// The file said so.
    Metadata,
    /// The file name said so.
    Name,
    /// Nothing said so; the shape merely allows it.
    AspectRatio,
    /// No reason to think it is anything but a flat picture.
    None,
}

impl Evidence {
    pub fn label(self) -> &'static str {
        match self {
            Evidence::Metadata => "declared by the file",
            Evidence::Name => "inferred from the file name",
            Evidence::AspectRatio => "guessed from the 2:1 aspect ratio",
            Evidence::None => "flat",
        }
    }

    /// Whether the viewer should offer the projection toggle prominently. A guess wants
    /// correcting; a declaration does not.
    pub fn is_guess(self) -> bool {
        matches!(self, Evidence::Name | Evidence::AspectRatio)
    }
}

impl Spherical {
    pub const FLAT: Spherical = Spherical {
        projection: ScreenProjection::Flat,
        stereo: StereoLayout::Mono,
        evidence: Evidence::None,
    };

    pub fn equirect(horizontal_fov: f32, stereo: StereoLayout, evidence: Evidence) -> Spherical {
        Spherical {
            projection: ScreenProjection::Equirect { horizontal_fov },
            stereo,
            evidence,
        }
    }

    pub fn is_panorama(&self) -> bool {
        matches!(self.projection, ScreenProjection::Equirect { .. })
    }
}

/// Read Google's `GPano` XMP schema, which is what every 360 camera and every phone panorama mode
/// writes. Also understands the `GImage`/`GAudio` siblings well enough to ignore them.
///
/// Parsed by substring rather than by an XML parser on purpose: XMP arrives as a packet embedded
/// in a JPEG APP1 segment or a PNG `iTXt` chunk, is frequently truncated by the writing tool, and
/// nothing here needs the tree — only four attribute values, any of which may be absent.
pub fn from_xmp(xmp: &str) -> Option<Spherical> {
    let projection = attribute(xmp, "GPano:ProjectionType")?;
    if !projection.eq_ignore_ascii_case("equirectangular") {
        // `cylindrical` exists in the schema and is not something this renderer draws.
        return None;
    }

    // A cropped panorama declares how much of the full sphere it covers. A phone panorama that
    // swept 200 degrees writes CroppedAreaImageWidthPixels < FullPanoWidthPixels, and treating it
    // as a full 360 stretches the scene by nearly a factor of two.
    let full_w = number(xmp, "GPano:FullPanoWidthPixels");
    let cropped_w = number(xmp, "GPano:CroppedAreaImageWidthPixels");
    let horizontal_fov = match (full_w, cropped_w) {
        (Some(full), Some(cropped)) if full > 0.0 && cropped > 0.0 => {
            (cropped / full * 360.0).clamp(1.0, 360.0)
        }
        _ => 360.0,
    };

    // The stereo schema is GImage/GPano's companion; VR180 files most often declare it as
    // `GPano:StereoMode` or through the Spherical Video tags copied into the still.
    let stereo = match attribute(xmp, "GPano:StereoMode")
        .or_else(|| attribute(xmp, "GSpherical:StereoMode"))
    {
        Some(m) if m.eq_ignore_ascii_case("top-bottom") => StereoLayout::OverUnder,
        Some(m) if m.eq_ignore_ascii_case("left-right") => StereoLayout::SideBySide,
        _ => StereoLayout::Mono,
    };

    Some(Spherical::equirect(
        horizontal_fov,
        stereo,
        Evidence::Metadata,
    ))
}

/// Pull `Name="value"` or `<Name>value</Name>` out of an XMP packet.
///
/// Both spellings occur in the wild for the same field: Adobe writes attributes, Google's own
/// spatial-media tool writes elements, and a file that has been through both carries a mix.
fn attribute<'a>(xmp: &'a str, name: &str) -> Option<&'a str> {
    if let Some(v) = find_after(xmp, &format!("{name}=\""), '"') {
        return Some(v);
    }
    if let Some(v) = find_after(xmp, &format!("{name}='"), '\'') {
        return Some(v);
    }
    find_after(xmp, &format!("<{name}>"), '<')
}

fn find_after<'a>(haystack: &'a str, prefix: &str, terminator: char) -> Option<&'a str> {
    let start = haystack.find(prefix)? + prefix.len();
    let rest = &haystack[start..];
    let end = rest.find(terminator)?;
    Some(rest[..end].trim())
}

fn number(xmp: &str, name: &str) -> Option<f32> {
    attribute(xmp, name)?.parse().ok()
}

/// Conventions used by exporters and by everyone who has ever renamed a file by hand.
///
/// Deliberately conservative: the token has to be delimited, so `photo_360.jpg` matches and
/// `bridge3600.jpg` does not.
pub fn from_name(name: &str) -> Option<Spherical> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.rsplit_once('.').map_or(lower.as_str(), |(s, _)| s);
    let tokens: Vec<&str> = stem
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();

    let stereo = if tokens.iter().any(|t| matches!(*t, "tb" | "ou" | "topbottom" | "overunder")) {
        StereoLayout::OverUnder
    } else if tokens.iter().any(|t| matches!(*t, "lr" | "sbs" | "leftright")) {
        StereoLayout::SideBySide
    } else {
        StereoLayout::Mono
    };

    // VR180 is half the sphere, and it is almost always stereo even when the name omits it.
    if tokens
        .iter()
        .any(|t| matches!(*t, "vr180" | "180" | "180vr"))
    {
        let stereo = if stereo == StereoLayout::Mono {
            StereoLayout::SideBySide
        } else {
            stereo
        };
        return Some(Spherical::equirect(180.0, stereo, Evidence::Name));
    }
    if tokens.iter().any(|t| {
        matches!(
            *t,
            "360" | "equirect" | "equirectangular" | "panorama" | "pano" | "spherical" | "vr360"
        )
    }) {
        return Some(Spherical::equirect(360.0, stereo, Evidence::Name));
    }
    None
}

/// Panoramas are 2:1 because that is what equirectangular means. Plenty of things that are not
/// panoramas are also 2:1, so this only fires when the picture is large enough that no ordinary
/// photograph would land on the ratio by accident.
///
/// The floor is 2,000 pixels wide. Below it, 2:1 images are usually banners, film stills and UI
/// screenshots; above it the common wide formats are not 2:1 at all — an ultrawide monitor is
/// 2.39:1, a dual-monitor screenshot 3.56:1 — so what is left is overwhelmingly stitched
/// panoramas. The smallest 360 cameras still in use write 2048x1024, which is what sets the exact
/// number.
pub fn from_aspect(width: u32, height: u32) -> Option<Spherical> {
    const MIN_WIDTH: u32 = 2_000;
    if width < MIN_WIDTH || height == 0 {
        return None;
    }
    let ratio = width as f32 / height as f32;
    if (ratio - 2.0).abs() < 0.01 {
        return Some(Spherical::equirect(360.0, StereoLayout::Mono, Evidence::AspectRatio));
    }
    // A 1:1 frame of that size is the over-under packing of two 2:1 eyes -- the standard VR180
    // and 360-stereo layout.
    if (ratio - 1.0).abs() < 0.01 {
        return Some(Spherical::equirect(
            360.0,
            StereoLayout::OverUnder,
            Evidence::AspectRatio,
        ));
    }
    None
}

/// Combine the three sources, best evidence first.
pub fn detect(xmp: Option<&str>, file_name: &str, width: u32, height: u32) -> Spherical {
    if let Some(s) = xmp.and_then(from_xmp) {
        return s;
    }
    if let Some(s) = from_name(file_name) {
        return s;
    }
    from_aspect(width, height).unwrap_or(Spherical::FLAT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_PANO: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
        <rdf:Description rdf:about=""
          xmlns:GPano="http://ns.google.com/photos/1.0/panorama/"
          GPano:UsePanoramaViewer="True"
          GPano:ProjectionType="equirectangular"
          GPano:FullPanoWidthPixels="8192"
          GPano:CroppedAreaImageWidthPixels="8192"/>
        </x:xmpmeta>"#;

    #[test]
    fn gpano_equirectangular_is_taken_at_its_word() {
        let s = from_xmp(FULL_PANO).expect("declared equirectangular");
        assert_eq!(
            s.projection,
            ScreenProjection::Equirect {
                horizontal_fov: 360.0
            }
        );
        assert_eq!(s.evidence, Evidence::Metadata);
        assert!(!s.evidence.is_guess());
    }

    #[test]
    fn a_cropped_panorama_reports_the_arc_it_actually_covers() {
        // A phone sweep that stopped at 200 degrees. Treating it as a full sphere stretches it.
        let xmp = r#"GPano:ProjectionType="equirectangular"
                     GPano:FullPanoWidthPixels="7200"
                     GPano:CroppedAreaImageWidthPixels="4000""#;
        let s = from_xmp(xmp).unwrap();
        match s.projection {
            ScreenProjection::Equirect { horizontal_fov } => {
                assert!((horizontal_fov - 200.0).abs() < 0.5, "{horizontal_fov}");
            }
            _ => panic!("expected equirect"),
        }
    }

    #[test]
    fn a_cylindrical_projection_is_not_claimed_as_a_sphere() {
        let xmp = r#"GPano:ProjectionType="cylindrical" GPano:FullPanoWidthPixels="8192""#;
        assert_eq!(from_xmp(xmp), None);
    }

    #[test]
    fn xmp_written_as_elements_parses_the_same_as_attributes() {
        let xmp = "<GPano:ProjectionType>equirectangular</GPano:ProjectionType>";
        assert!(from_xmp(xmp).unwrap().is_panorama());
    }

    #[test]
    fn stereo_mode_selects_the_packing() {
        let xmp = r#"GPano:ProjectionType="equirectangular" GPano:StereoMode="top-bottom""#;
        assert_eq!(from_xmp(xmp).unwrap().stereo, StereoLayout::OverUnder);
    }

    #[test]
    fn name_tokens_must_be_delimited_to_count() {
        assert!(from_name("terrace_360.jpg").unwrap().is_panorama());
        assert!(from_name("VR180_LR.mp4").unwrap().is_panorama());
        assert_eq!(from_name("bridge3600.jpg"), None);
        assert_eq!(from_name("holiday.jpg"), None);
    }

    #[test]
    fn vr180_is_half_a_sphere_and_stereo_by_default() {
        let s = from_name("garden_vr180.mp4").unwrap();
        assert_eq!(
            s.projection,
            ScreenProjection::Equirect {
                horizontal_fov: 180.0
            }
        );
        assert_eq!(s.stereo, StereoLayout::SideBySide);
    }

    #[test]
    fn the_aspect_guess_needs_the_picture_to_be_large() {
        // A 2:1 banner is not a panorama, and neither is a 1920x960 crop.
        assert_eq!(from_aspect(1200, 600), None);
        assert_eq!(from_aspect(1920, 960), None);
        let s = from_aspect(8192, 4096).expect("a large 2:1 image is worth guessing about");
        assert_eq!(s.evidence, Evidence::AspectRatio);
        assert!(s.evidence.is_guess());
        // The smallest 360 camera output still in circulation.
        assert!(from_aspect(2048, 1024).is_some());
    }

    #[test]
    fn the_wide_formats_that_are_not_panoramas_are_not_claimed() {
        // Each of these is a real screen or sensor shape that turns up at panorama-like sizes.
        for (w, h) in [
            (3440u32, 1440u32), // ultrawide monitor, 2.39:1
            (3840, 1080),       // two 1080p monitors side by side
            (5120, 1440),       // two 1440p monitors
            (4032, 3024),       // a phone camera
            (7680, 4320),       // 8K, 16:9
        ] {
            assert_eq!(from_aspect(w, h), None, "{w}x{h} is not a panorama");
        }
    }

    #[test]
    fn metadata_outranks_the_name_and_the_name_outranks_the_shape() {
        // The name says 180 and the shape says 360; the file's own metadata says 360, and wins.
        let s = detect(Some(FULL_PANO), "clip_vr180.jpg", 8192, 4096);
        assert_eq!(s.evidence, Evidence::Metadata);
        match s.projection {
            ScreenProjection::Equirect { horizontal_fov } => assert_eq!(horizontal_fov, 360.0),
            _ => panic!(),
        }

        // Without metadata the name wins over the 2:1 shape.
        let s = detect(None, "clip_vr180.jpg", 8192, 4096);
        assert_eq!(s.evidence, Evidence::Name);

        // With neither, an ordinary photograph stays flat.
        assert_eq!(detect(None, "cat.jpg", 4032, 3024), Spherical::FLAT);
    }
}
