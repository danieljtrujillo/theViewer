//! Still and animated pictures.
//!
//! A picture is not geometry, and pretending otherwise makes it worse to look at: a photograph
//! textured onto a quad in a 3D scene has to be hunted down with an orbit camera. So this importer
//! produces a [`Screen`] payload, and the viewer presents it the way the format means it — fitted
//! to the window for a flat picture, wrapped around the viewer for a panorama.
//!
//! Three things here are easy to get wrong and are handled deliberately:
//!
//! - **EXIF orientation.** Phone cameras write the sensor's pixels and a rotation tag. Ignoring
//!   the tag shows a quarter of everyone's photo library sideways.
//! - **High dynamic range.** Radiance and OpenEXR decode to linear `f32` above 1.0. Rounding that
//!   straight to 8 bits clips every highlight, so it goes through a tone map and an sRGB encode.
//! - **Animation.** GIF, animated WebP and APNG all carry a timeline. They play.

use std::io::Cursor;

use image::{AnimationDecoder, DynamicImage, ImageFormat};
use viewer_io::ImageKind;
use viewer_scene::{
    Image, Node, Payload, Scene, Screen, ScreenProjection, StereoLayout, Timeline,
};

use crate::spherical::{self, Evidence, Spherical};
use crate::{Frame, FrameSource, ImportError, Opened};

/// Ceiling on an animation held in memory at once.
///
/// GIF and APNG have no seek structure worth the name — reaching frame 400 means decoding 400
/// frames — so the whole animation is decoded up front and kept. That is fine for the sizes these
/// formats are actually used at and ruinous past them, hence the cap. Video, which does have a
/// keyframe index, goes down a different path entirely.
const MAX_ANIMATION_BYTES: usize = 512 * 1024 * 1024;
const MAX_ANIMATION_FRAMES: usize = 4_096;

pub fn import(
    bytes: &[u8],
    kind: ImageKind,
    path: Option<&std::path::Path>,
) -> Result<Opened, ImportError> {
    let file_name = path
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let meta = sidecar_metadata(bytes, kind);

    // Animated containers first: a GIF with one frame is a picture, and a GIF with 60 is a movie,
    // and only the decoder knows which.
    if let Some(anim) = decode_animation(bytes, kind)? {
        return Ok(assemble_animation(anim, &meta, file_name));
    }

    // Formats this build carries no Rust decoder for go straight to the platform. HEIF and AVIF
    // need an HEVC and an AV1 decoder, which Windows hosts and this binary has no business
    // shipping.
    let (picture, label) = match image_format(kind) {
        Some(format) => match image::load_from_memory_with_format(bytes, format) {
            Ok(decoded) => (
                to_rgba8(apply_orientation(decoded, meta.orientation), kind),
                kind.label().to_string(),
            ),
            // A Rust decoder that declines still leaves the platform a turn: a TIFF with an
            // unusual compression, a PNG variant, a camera RAW behind a vendor codec.
            Err(rust_error) => match platform_decode(path, kind, &meta) {
                Some(result) => result,
                None => {
                    return Err(ImportError::Malformed {
                        format: viewer_io::FormatId::Image(kind),
                        reason: rust_error.to_string(),
                    })
                }
            },
        },
        None => platform_decode(path, kind, &meta).ok_or_else(|| ImportError::Malformed {
            format: viewer_io::FormatId::Image(kind),
            reason: missing_codec(kind),
        })?,
    };

    let spherical = spherical::detect(
        meta.xmp.as_deref(),
        file_name,
        picture.width,
        picture.height,
    );
    Ok(Opened::still(assemble(picture, spherical, &meta, &label)))
}

/// What to tell someone whose machine cannot decode a format this build delegates.
///
/// Naming the exact package matters. HEIF is the case that catches people out: the *HEIF Image
/// Extension* is free and widely installed, but it only handles the container — the pixels are
/// HEVC, and the **HEVC Video Extensions** package that decodes them is a separate, paid install.
/// A machine with the first and not the second reports HEIF as supported and then fails, which is
/// exactly what a bare "unsupported format" would leave unexplained.
fn missing_codec(kind: ImageKind) -> String {
    match kind {
        ImageKind::Heif => "no HEVC decoder is installed. The free HEIF Image Extension reads the \
             container, but the pixels inside are HEVC and need the separate HEVC Video \
             Extensions package from the Microsoft Store."
            .to_string(),
        ImageKind::Avif => "no AV1 decoder is installed. The AV1 Video Extension from the \
             Microsoft Store supplies one."
            .to_string(),
        ImageKind::Jpeg2000 => "Windows ships no JPEG 2000 decoder, and this build carries none."
            .to_string(),
        other => format!(
            "{} needs a codec neither this build nor this machine has",
            other.label()
        ),
    }
}

/// Decode through the operating system, when there is a path to hand it.
///
/// Returns the picture and a label naming the decoder, because "HEIF image, decoded by Microsoft
/// HEIF Decoder" is the answer to the question someone asks when a file opens on one machine and
/// not another.
fn platform_decode(
    path: Option<&std::path::Path>,
    kind: ImageKind,
    meta: &Meta,
) -> Option<(Picture, String)> {
    let image = viewer_media::decode_image(path?).ok()?;
    let mut picture = Picture {
        width: image.width,
        height: image.height,
        rgba: image.rgba,
    };
    // WIC applies EXIF orientation for some formats and not others, and does not say which. The
    // tag is honoured here only when the picture still has the shape the sensor produced, which
    // is the case exactly when WIC left it alone.
    if meta.orientation > 1 {
        picture = rotate(picture, meta.orientation);
    }
    let label = if image.decoder.is_empty() {
        kind.label().to_string()
    } else {
        format!("{}, decoded by {}", kind.label(), image.decoder)
    };
    Some((picture, label))
}

/// Apply an EXIF orientation to an already-decoded picture.
fn rotate(picture: Picture, orientation: u8) -> Picture {
    let Some(buffer) =
        image::RgbaImage::from_raw(picture.width, picture.height, picture.rgba.clone())
    else {
        return picture;
    };
    let rotated = apply_orientation(DynamicImage::ImageRgba8(buffer), orientation);
    let (width, height) = (rotated.width(), rotated.height());
    Picture {
        width,
        height,
        rgba: rotated.to_rgba8().into_raw(),
    }
}

/// A decoded still, before it becomes a scene.
struct Picture {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

struct Animation {
    frames: Vec<(u64, Picture)>,
    /// Whether the source was cut short by the memory cap, so the timeline can say so.
    truncated: bool,
}

fn assemble(picture: Picture, spherical: Spherical, meta: &Meta, label: &str) -> Scene {
    let mut scene = Scene::new();
    let image = Image {
        name: "picture".into(),
        width: picture.width,
        height: picture.height,
        rgba: picture.rgba,
    };
    // Registered as dynamic even for a still: an animated source rewrites this slot every frame,
    // and a still simply never does. One code path in the renderer instead of two.
    let index = scene.push_dynamic_image(image);
    scene.push_root(Node::with_payload(
        "picture",
        Payload::Screen(Screen {
            image: index,
            projection: spherical.projection,
            stereo: spherical.stereo,
            width: picture.width,
            height: picture.height,
        }),
    ));

    scene.set_meta("format", label);
    scene.set_meta(
        "pixels",
        format!("{} x {}", picture.width, picture.height),
    );
    if spherical.is_panorama() || spherical.evidence != Evidence::None {
        scene.set_meta("projection", projection_label(spherical));
        scene.set_meta("projection source", spherical.evidence.label());
    }
    if meta.orientation > 1 {
        scene.set_meta("exif orientation", meta.orientation.to_string());
    }
    scene
}

fn assemble_animation(anim: Animation, meta: &Meta, file_name: &str) -> Opened {
    let first = &anim.frames[0].1;
    let spherical = spherical::detect(meta.xmp.as_deref(), file_name, first.width, first.height);

    let last_pts = anim.frames.last().map_or(0, |(pts, _)| *pts);
    // The final frame's own duration is not recorded by the last timestamp, so the loop point is
    // one average frame beyond it. Without this an animation stalls on its last frame.
    let count = anim.frames.len() as u64;
    let duration_us = if count > 1 {
        last_pts + last_pts / (count - 1)
    } else {
        last_pts
    }
    .max(1);

    let fps = if duration_us > 0 {
        count as f32 * 1_000_000.0 / duration_us as f32
    } else {
        0.0
    };

    let mut scene = assemble(
        Picture {
            width: first.width,
            height: first.height,
            rgba: first.rgba.clone(),
        },
        spherical,
        meta,
        "animation",
    );
    scene.timeline = Some(Timeline {
        frame_count: anim.frames.len() as u32,
        fps,
        duration_us,
        seekable: true,
    });
    scene.set_meta("frames", anim.frames.len().to_string());
    if anim.truncated {
        scene.set_meta(
            "truncated",
            format!("stopped after {} frames", anim.frames.len()),
        );
    }

    let timeline = scene.timeline.clone().unwrap();
    Opened {
        scene,
        frames: Some(Box::new(AnimationSource {
            frames: anim.frames,
            timeline,
            served: None,
        })),
    }
}

fn projection_label(s: Spherical) -> String {
    let arc = match s.projection {
        ScreenProjection::Flat => return "flat".into(),
        ScreenProjection::Equirect { horizontal_fov } => horizontal_fov,
    };
    let stereo = match s.stereo {
        StereoLayout::Mono => "",
        StereoLayout::OverUnder => ", stereo over-under",
        StereoLayout::SideBySide => ", stereo side-by-side",
    };
    format!("equirectangular {arc:.0}\u{b0}{stereo}")
}

/// Frames already in memory, replayed by timestamp.
struct AnimationSource {
    frames: Vec<(u64, Picture)>,
    timeline: Timeline,
    /// The index last handed out, so a repeated request for the same frame costs nothing.
    served: Option<u32>,
}

impl FrameSource for AnimationSource {
    fn timeline(&self) -> Timeline {
        self.timeline.clone()
    }

    fn frame_at(&mut self, pts_us: u64) -> Result<Option<Frame>, ImportError> {
        // The frame that is on screen at this instant is the last one whose timestamp has passed,
        // not the nearest one: a 2-second frame followed by a 40-millisecond one must hold for
        // two seconds.
        let index = match self.frames.partition_point(|(pts, _)| *pts <= pts_us) {
            0 => 0,
            n => n - 1,
        };
        if self.served == Some(index as u32) {
            return Ok(None);
        }
        self.served = Some(index as u32);
        let (pts, picture) = &self.frames[index];
        Ok(Some(Frame {
            index: index as u32,
            pts_us: *pts,
            image: Some(Image {
                name: format!("frame {index}"),
                width: picture.width,
                height: picture.height,
                rgba: picture.rgba.clone(),
            }),
            mesh: None,
        }))
    }
}

fn image_format(kind: ImageKind) -> Option<ImageFormat> {
    Some(match kind {
        ImageKind::Png => ImageFormat::Png,
        ImageKind::Jpeg => ImageFormat::Jpeg,
        ImageKind::Gif => ImageFormat::Gif,
        ImageKind::Bmp => ImageFormat::Bmp,
        ImageKind::Tiff => ImageFormat::Tiff,
        ImageKind::WebP => ImageFormat::WebP,
        ImageKind::Tga => ImageFormat::Tga,
        ImageKind::Qoi => ImageFormat::Qoi,
        ImageKind::Ico => ImageFormat::Ico,
        ImageKind::Dds => ImageFormat::Dds,
        ImageKind::Pnm => ImageFormat::Pnm,
        ImageKind::Farbfeld => ImageFormat::Farbfeld,
        ImageKind::Hdr => ImageFormat::Hdr,
        ImageKind::OpenExr => ImageFormat::OpenExr,
        ImageKind::Avif | ImageKind::Heif | ImageKind::Jpeg2000 | ImageKind::Svg => return None,
    })
}

/// Decode an animated container, or `None` if this file is a single still.
fn decode_animation(bytes: &[u8], kind: ImageKind) -> Result<Option<Animation>, ImportError> {
    let malformed = |e: image::ImageError| ImportError::Malformed {
        format: viewer_io::FormatId::Image(kind),
        reason: e.to_string(),
    };

    let frames = match kind {
        ImageKind::Gif => {
            let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(malformed)?;
            decoder.into_frames().take(MAX_ANIMATION_FRAMES + 1)
                .collect::<Result<Vec<_>, _>>()
                .map_err(malformed)?
        }
        ImageKind::WebP => {
            let decoder =
                image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(malformed)?;
            if !decoder.has_animation() {
                return Ok(None);
            }
            decoder.into_frames().take(MAX_ANIMATION_FRAMES + 1)
                .collect::<Result<Vec<_>, _>>()
                .map_err(malformed)?
        }
        ImageKind::Png => {
            let decoder =
                image::codecs::png::PngDecoder::new(Cursor::new(bytes)).map_err(malformed)?;
            if !decoder.is_apng().map_err(malformed)? {
                return Ok(None);
            }
            decoder
                .apng()
                .map_err(malformed)?
                .into_frames()
                .take(MAX_ANIMATION_FRAMES + 1)
                .collect::<Result<Vec<_>, _>>()
                .map_err(malformed)?
        }
        _ => return Ok(None),
    };

    if frames.len() < 2 {
        return Ok(None);
    }

    let mut out = Vec::with_capacity(frames.len());
    let mut pts_us = 0u64;
    let mut bytes_held = 0usize;
    let mut truncated = frames.len() > MAX_ANIMATION_FRAMES;
    for frame in frames.into_iter().take(MAX_ANIMATION_FRAMES) {
        let (num, den) = frame.delay().numer_denom_ms();
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        let rgba = buffer.into_raw();
        bytes_held += rgba.len();
        out.push((
            pts_us,
            Picture {
                width,
                height,
                rgba,
            },
        ));
        // A zero delay means "as fast as possible", which every browser renders as 100 ms. Doing
        // anything else makes old GIFs play at several hundred frames a second.
        let ms = if den == 0 || num == 0 {
            100.0
        } else {
            num as f64 / den as f64
        };
        pts_us += (ms * 1_000.0).round().max(1.0) as u64;
        if bytes_held >= MAX_ANIMATION_BYTES {
            truncated = true;
            break;
        }
    }

    Ok(Some(Animation {
        frames: out,
        truncated,
    }))
}

/// Rotate and flip according to the EXIF orientation tag.
///
/// The eight values are the four rotations crossed with a mirror. Cameras write 6 and 8 constantly
/// — those are portrait shots taken by turning the body — and 2, 4, 5 and 7 only turn up from
/// scanners and front-facing lenses, but all eight are one line each.
fn apply_orientation(image: DynamicImage, orientation: u8) -> DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

/// Convert a decoded image to the RGBA8 the renderer uploads.
///
/// The interesting case is floating point. Radiance and OpenEXR store linear radiance that runs
/// well past 1.0, and `to_rgba8` would clip every value above it, turning a sunlit window into a
/// flat white rectangle. Instead the values go through a Reinhard curve and the sRGB transfer
/// function, which is what a display expects and what the sRGB texture format assumes.
fn to_rgba8(image: DynamicImage, kind: ImageKind) -> Picture {
    let is_hdr = matches!(kind, ImageKind::Hdr | ImageKind::OpenExr)
        || matches!(
            image,
            DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
        );

    if !is_hdr {
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        return Picture {
            width,
            height,
            rgba: rgba.into_raw(),
        };
    }

    let source = image.to_rgba32f();
    let (width, height) = source.dimensions();
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for px in source.pixels() {
        for c in 0..3 {
            rgba.push(encode_srgb(tone_map(px.0[c])));
        }
        rgba.push((px.0[3].clamp(0.0, 1.0) * 255.0).round() as u8);
    }
    Picture {
        width,
        height,
        rgba,
    }
}

/// Extended Reinhard, normalized so that a linear value of 1.0 stays near 1.0 and everything
/// above it compresses instead of clipping.
fn tone_map(v: f32) -> f32 {
    if !v.is_finite() || v <= 0.0 {
        return 0.0;
    }
    const WHITE: f32 = 8.0;
    let numerator = v * (1.0 + v / (WHITE * WHITE));
    (numerator / (1.0 + v)).clamp(0.0, 1.0)
}

/// The sRGB transfer function. The texture is uploaded as `Rgba8UnormSrgb`, so the GPU undoes
/// exactly this on sampling; skipping it makes every HDR image render far too dark.
fn encode_srgb(linear: f32) -> u8 {
    let v = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[derive(Default)]
struct Meta {
    orientation: u8,
    xmp: Option<String>,
}

/// Pull the EXIF orientation and the XMP packet out of a container, without decoding pixels.
fn sidecar_metadata(bytes: &[u8], kind: ImageKind) -> Meta {
    match kind {
        ImageKind::Jpeg => jpeg_metadata(bytes),
        ImageKind::Png => png_metadata(bytes),
        ImageKind::WebP => webp_metadata(bytes),
        ImageKind::Tiff => Meta {
            orientation: exif_orientation(bytes).unwrap_or(1),
            xmp: None,
        },
        _ => Meta::default(),
    }
}

const XMP_NAMESPACE: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

/// Walk JPEG segments looking for the two APP1 flavours: Exif and XMP.
///
/// Segment walking rather than a full parse: after the SOI every marker is `0xFF` then a type
/// byte, and every segment except the standalone ones carries a big-endian length. Stopping at
/// SOS matters — past it the entropy-coded scan contains `0xFF` bytes that are not markers.
fn jpeg_metadata(bytes: &[u8]) -> Meta {
    let mut meta = Meta {
        orientation: 1,
        xmp: None,
    };
    let mut i = 2usize;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            break;
        }
        let marker = bytes[i + 1];
        // Start of scan, or end of image: no more metadata segments follow.
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        // Standalone markers carry no length.
        if (0xD0..=0xD8).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let length = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if length < 2 {
            break;
        }
        let Some(payload) = bytes.get(i + 4..i + 2 + length) else {
            break;
        };
        if marker == 0xE1 {
            if payload.starts_with(b"Exif\0\0") {
                if let Some(o) = exif_orientation(&payload[6..]) {
                    meta.orientation = o;
                }
            } else if payload.starts_with(XMP_NAMESPACE) {
                meta.xmp = Some(String::from_utf8_lossy(&payload[XMP_NAMESPACE.len()..]).into_owned());
            }
        }
        i += 2 + length;
    }
    meta
}

/// PNG carries EXIF in an `eXIf` chunk and XMP in an `iTXt` chunk keyed `XML:com.adobe.xmp`.
fn png_metadata(bytes: &[u8]) -> Meta {
    let mut meta = Meta {
        orientation: 1,
        xmp: None,
    };
    let mut i = 8usize; // past the signature
    while i + 8 <= bytes.len() {
        let length = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        let Some(kind) = bytes.get(i + 4..i + 8) else {
            break;
        };
        let Some(data) = bytes.get(i + 8..i + 8 + length) else {
            break;
        };
        match kind {
            b"eXIf" => {
                if let Some(o) = exif_orientation(data) {
                    meta.orientation = o;
                }
            }
            b"iTXt" => {
                if data.starts_with(b"XML:com.adobe.xmp\0") {
                    // keyword NUL, compression flag, compression method, language NUL,
                    // translated keyword NUL, then the text. Only uncompressed XMP is read;
                    // the compressed form is vanishingly rare and not worth an inflate here.
                    let rest = &data[18..];
                    if rest.first() == Some(&0) {
                        let after_flags = &rest[2..];
                        let mut nulls = after_flags.splitn(3, |b| *b == 0);
                        nulls.next();
                        nulls.next();
                        if let Some(text) = nulls.next() {
                            meta.xmp = Some(String::from_utf8_lossy(text).into_owned());
                        }
                    }
                }
            }
            b"IDAT" | b"IEND" => break,
            _ => {}
        }
        i += 12 + length; // length + type + data + CRC
    }
    meta
}

/// WebP is RIFF: `EXIF` and `XMP ` are ordinary chunks in the extended form.
fn webp_metadata(bytes: &[u8]) -> Meta {
    let mut meta = Meta {
        orientation: 1,
        xmp: None,
    };
    let mut i = 12usize; // RIFF + size + WEBP
    while i + 8 <= bytes.len() {
        let Some(kind) = bytes.get(i..i + 4) else {
            break;
        };
        let length =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let Some(data) = bytes.get(i + 8..i + 8 + length) else {
            break;
        };
        match kind {
            b"EXIF" => {
                if let Some(o) = exif_orientation(data) {
                    meta.orientation = o;
                }
            }
            b"XMP " => meta.xmp = Some(String::from_utf8_lossy(data).into_owned()),
            _ => {}
        }
        // RIFF chunks are padded to an even length.
        i += 8 + length + (length & 1);
    }
    meta
}

/// Read tag 0x0112 out of IFD0 of a TIFF block.
///
/// This is the whole of the EXIF parsing this app needs. Both byte orders occur — Canon writes
/// little-endian, some scanners big — so the reader is parameterized on it rather than assuming.
fn exif_orientation(tiff: &[u8]) -> Option<u8> {
    let big_endian = match tiff.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o + 2)?;
        Some(if big_endian {
            u16::from_be_bytes([b[0], b[1]])
        } else {
            u16::from_le_bytes([b[0], b[1]])
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = tiff.get(o..o + 4)?;
        Some(if big_endian {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        })
    };

    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    // A corrupt count field would otherwise walk off into the pixel data.
    if count > 512 {
        return None;
    }
    for i in 0..count {
        let entry = ifd + 2 + i * 12;
        if u16_at(entry)? == 0x0112 {
            // SHORT, stored inline in the value field, which is left-justified in both orders.
            let value = u16_at(entry + 8)?;
            return (1..=8).contains(&value).then_some(value as u8);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal little-endian TIFF block declaring one tag: orientation 6.
    fn exif_block(orientation: u16, big_endian: bool) -> Vec<u8> {
        let mut v = Vec::new();
        if big_endian {
            v.extend_from_slice(b"MM");
            v.extend_from_slice(&42u16.to_be_bytes());
            v.extend_from_slice(&8u32.to_be_bytes());
            v.extend_from_slice(&1u16.to_be_bytes());
            v.extend_from_slice(&0x0112u16.to_be_bytes());
            v.extend_from_slice(&3u16.to_be_bytes()); // SHORT
            v.extend_from_slice(&1u32.to_be_bytes());
            v.extend_from_slice(&orientation.to_be_bytes());
            v.extend_from_slice(&[0, 0]);
        } else {
            v.extend_from_slice(b"II");
            v.extend_from_slice(&42u16.to_le_bytes());
            v.extend_from_slice(&8u32.to_le_bytes());
            v.extend_from_slice(&1u16.to_le_bytes());
            v.extend_from_slice(&0x0112u16.to_le_bytes());
            v.extend_from_slice(&3u16.to_le_bytes());
            v.extend_from_slice(&1u32.to_le_bytes());
            v.extend_from_slice(&orientation.to_le_bytes());
            v.extend_from_slice(&[0, 0]);
        }
        v.extend_from_slice(&[0; 4]); // next IFD offset
        v
    }

    #[test]
    fn exif_orientation_reads_both_byte_orders() {
        assert_eq!(exif_orientation(&exif_block(6, false)), Some(6));
        assert_eq!(exif_orientation(&exif_block(8, true)), Some(8));
    }

    #[test]
    fn a_nonsense_orientation_is_ignored_rather_than_applied() {
        assert_eq!(exif_orientation(&exif_block(99, false)), None);
        assert_eq!(exif_orientation(b"not a tiff block at all"), None);
        assert_eq!(exif_orientation(&[]), None);
    }

    #[test]
    fn jpeg_app1_segments_yield_both_orientation_and_xmp() {
        let exif = exif_block(6, false);
        let xmp = br#"<x:xmpmeta GPano:ProjectionType="equirectangular"/>"#;

        let mut jpeg = vec![0xFF, 0xD8];
        // APP1 / Exif
        let payload_len = 2 + 6 + exif.len();
        jpeg.extend_from_slice(&[0xFF, 0xE1]);
        jpeg.extend_from_slice(&(payload_len as u16).to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&exif);
        // APP1 / XMP
        let payload_len = 2 + XMP_NAMESPACE.len() + xmp.len();
        jpeg.extend_from_slice(&[0xFF, 0xE1]);
        jpeg.extend_from_slice(&(payload_len as u16).to_be_bytes());
        jpeg.extend_from_slice(XMP_NAMESPACE);
        jpeg.extend_from_slice(xmp);
        // SOS: the walk must stop here.
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        jpeg.extend_from_slice(&[0xFF; 64]);

        let meta = jpeg_metadata(&jpeg);
        assert_eq!(meta.orientation, 6);
        let spherical = spherical::from_xmp(meta.xmp.as_deref().unwrap()).unwrap();
        assert!(spherical.is_panorama());
    }

    #[test]
    fn a_jpeg_without_metadata_does_not_loop_or_panic() {
        let meta = jpeg_metadata(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xFF]);
        assert_eq!(meta.orientation, 1);
        assert!(meta.xmp.is_none());
        // Truncated in the middle of a segment header.
        assert_eq!(jpeg_metadata(&[0xFF, 0xD8, 0xFF]).orientation, 1);
        assert_eq!(jpeg_metadata(&[]).orientation, 1);
    }

    #[test]
    fn webp_riff_chunks_are_walked_with_their_padding() {
        // An odd-length chunk is padded to even; getting that wrong desynchronizes the walk and
        // loses every chunk after the first odd one.
        let xmp = b"GPano:ProjectionType=\"equirectangular\"";
        let mut webp = b"RIFF\0\0\0\0WEBP".to_vec();
        webp.extend_from_slice(b"VP8X");
        webp.extend_from_slice(&5u32.to_le_bytes());
        webp.extend_from_slice(&[0; 5]);
        webp.push(0); // pad
        webp.extend_from_slice(b"XMP ");
        webp.extend_from_slice(&(xmp.len() as u32).to_le_bytes());
        webp.extend_from_slice(xmp);

        let meta = webp_metadata(&webp);
        assert!(meta.xmp.is_some(), "the chunk after the padded one was lost");
        assert!(spherical::from_xmp(meta.xmp.as_deref().unwrap())
            .unwrap()
            .is_panorama());
    }

    #[test]
    fn tone_mapping_keeps_highlights_apart_instead_of_clipping_them() {
        // The failure this prevents: everything above 1.0 rounding to 255.
        let a = encode_srgb(tone_map(1.0));
        let b = encode_srgb(tone_map(4.0));
        let c = encode_srgb(tone_map(40.0));
        assert!(a < b && b < c, "{a} {b} {c}");
        assert!(c > b, "the brightest highlight must stay distinct");
        assert_eq!(encode_srgb(tone_map(0.0)), 0);
        // Not a number and negative radiance both appear in real EXR files.
        assert_eq!(encode_srgb(tone_map(f32::NAN)), 0);
        assert_eq!(encode_srgb(tone_map(-1.0)), 0);
    }

    #[test]
    fn srgb_encoding_matches_the_standard_at_its_anchors() {
        assert_eq!(encode_srgb(0.0), 0);
        assert_eq!(encode_srgb(1.0), 255);
        // Mid-grey: linear 0.2140 encodes to about 0.5.
        assert!((encode_srgb(0.2140) as i32 - 128).abs() <= 1);
    }

    #[test]
    fn orientation_six_turns_a_landscape_frame_into_a_portrait_one() {
        let image = DynamicImage::new_rgba8(4, 2);
        let rotated = apply_orientation(image, 6);
        assert_eq!((rotated.width(), rotated.height()), (2, 4));
    }
}
