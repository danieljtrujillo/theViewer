//! ITK MetaImage: `.mha` and `.mhd`.
//!
//! A `key = value` text header followed by raw voxels. The two extensions are the same format
//! wearing different clothes: `.mha` ends its header with `ElementDataFile = LOCAL` and the
//! voxels follow in the same file, while `.mhd` names a separate raw file that sits beside it.
//! The split form is why this importer needs a path.
//!
//! It is the working format of ITK, 3D Slicer and most of the segmentation literature, so a great
//! many public medical datasets ship as a `.mhd` and a `.raw`. The header is small enough to read
//! in full and specific enough that nothing else looks like it.
//!
//! `CompressedData = True` wraps the voxels in a zlib stream, which is common for anything
//! distributed. The declared `CompressedDataSize` is a length field out of an untrusted file, so
//! the decompressed result is checked against the dimensions rather than trusted.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use viewer_io::FormatId;
use viewer_scene::payload::{VolumeGrid, VoxelType};
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

/// Ceiling on a decoded volume, in bytes.
///
/// The dimensions come out of an untrusted file and drive both the allocation and the
/// decompression bound. Two gigabytes is far past any volume this renderer can usefully sample and
/// far short of what a hostile header can ask for.
const MAX_VOLUME_BYTES: usize = 2 * 1024 * 1024 * 1024;

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::MetaImage,
        reason: reason.into(),
    }
}

/// The header fields this importer acts on. Everything else is kept as metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub dims: [u32; 3],
    pub spacing: [f32; 3],
    pub element: Element,
    pub big_endian: bool,
    pub compressed: bool,
    /// `None` for `LOCAL`, meaning the voxels follow the header in the same file.
    pub data_file: Option<String>,
    /// Byte offset of the voxels within this file, for the `LOCAL` case.
    pub data_offset: usize,
}

/// The declared voxel type, before it is narrowed to something the renderer samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element {
    UChar,
    Char,
    UShort,
    Short,
    UInt,
    Int,
    Float,
    Double,
}

impl Element {
    fn parse(name: &str) -> Option<Element> {
        Some(match name.trim().to_ascii_uppercase().as_str() {
            "MET_UCHAR" => Element::UChar,
            "MET_CHAR" => Element::Char,
            "MET_USHORT" => Element::UShort,
            "MET_SHORT" => Element::Short,
            "MET_UINT" | "MET_ULONG" => Element::UInt,
            "MET_INT" | "MET_LONG" => Element::Int,
            "MET_FLOAT" => Element::Float,
            "MET_DOUBLE" => Element::Double,
            _ => return None,
        })
    }

    pub const fn size(self) -> usize {
        match self {
            Element::UChar | Element::Char => 1,
            Element::UShort | Element::Short => 2,
            Element::UInt | Element::Int | Element::Float => 4,
            Element::Double => 8,
        }
    }

    /// What the grid becomes once it is stored.
    ///
    /// `VolumeGrid` carries four voxel types and MetaImage declares eight. The four that do not
    /// map directly widen: a signed byte to `i16` because there is no `i8` grid, and the 32-bit
    /// integers and `f64` to `f32`, which is what the raymarcher samples anyway.
    pub const fn stored_as(self) -> VoxelType {
        match self {
            Element::UChar => VoxelType::U8,
            Element::UShort => VoxelType::U16,
            Element::Char | Element::Short => VoxelType::I16,
            Element::UInt | Element::Int | Element::Float | Element::Double => VoxelType::F32,
        }
    }
}

/// Parse the header, and report where it ended.
///
/// `ElementDataFile` is the last field by definition, so the header ends at the newline after it.
pub fn parse_header(bytes: &[u8]) -> Result<Header, ImportError> {
    // Only the header is text. Reading the whole file as UTF-8 would fail on the voxels that
    // follow it in a `.mha`, so the scan is bounded and stays on the raw bytes.
    //
    // Decoding the window as one lossy string and measuring lines in it would be wrong: lossy
    // decoding replaces each invalid byte with U+FFFD, which is three bytes, so the offset it
    // yields is not an offset into the file. `.mha` voxels start immediately after the header and
    // that offset has to be exact.
    let scan = &bytes[..bytes.len().min(64 * 1024)];

    let mut dims: Option<Vec<u32>> = None;
    let mut spacing: Option<Vec<f32>> = None;
    let mut element: Option<Element> = None;
    let mut ndims: Option<usize> = None;
    let mut big_endian = false;
    let mut compressed = false;
    let mut data_file: Option<String> = None;
    let mut consumed = 0usize;

    for raw_line in scan.split_inclusive(|b| *b == b'\n') {
        consumed += raw_line.len();
        // Each line is decoded on its own, so a replacement character in one field's value cannot
        // move the offset of the voxels that follow.
        let line = String::from_utf8_lossy(raw_line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "ndims" => ndims = value.parse().ok(),
            "dimsize" => {
                dims = Some(
                    value
                        .split_ascii_whitespace()
                        .filter_map(|v| v.parse().ok())
                        .collect(),
                )
            }
            "elementspacing" | "elementsize" => {
                if spacing.is_none() {
                    spacing = Some(
                        value
                            .split_ascii_whitespace()
                            .filter_map(|v| v.parse().ok())
                            .collect(),
                    );
                }
            }
            "elementtype" => {
                element = Element::parse(value);
                if element.is_none() {
                    return Err(malformed(format!("{value} is not an element type")));
                }
            }
            "elementbyteordermsb" | "binarydatabyteordermsb" => {
                big_endian = value.eq_ignore_ascii_case("true")
            }
            "compresseddata" => compressed = value.eq_ignore_ascii_case("true"),
            "elementdatafile" => {
                data_file = if value.eq_ignore_ascii_case("LOCAL") {
                    None
                } else {
                    Some(value.to_string())
                };
                // Defined to be the last field. Everything after it is voxels.
                break;
            }
            _ => {}
        }
    }

    let ndims = ndims.ok_or_else(|| malformed("no NDims field"))?;
    if !(2..=3).contains(&ndims) {
        return Err(malformed(format!(
            "{ndims}-dimensional images are not volumes"
        )));
    }
    let declared = dims.ok_or_else(|| malformed("no DimSize field"))?;
    if declared.len() < ndims {
        return Err(malformed(format!(
            "NDims is {ndims} but DimSize gives {} sizes",
            declared.len()
        )));
    }
    let dims = [
        declared[0].max(1),
        declared[1].max(1),
        if ndims == 3 { declared[2].max(1) } else { 1 },
    ];

    let s = spacing.unwrap_or_default();
    let spacing = [
        s.first().copied().filter(|v| *v > 0.0).unwrap_or(1.0),
        s.get(1).copied().filter(|v| *v > 0.0).unwrap_or(1.0),
        s.get(2).copied().filter(|v| *v > 0.0).unwrap_or(1.0),
    ];

    Ok(Header {
        dims,
        spacing,
        element: element.ok_or_else(|| malformed("no ElementType field"))?,
        big_endian,
        compressed,
        data_file,
        data_offset: consumed,
    })
}

pub fn import(path: &Path) -> Result<Scene, ImportError> {
    let bytes = std::fs::read(path)?;
    let header = parse_header(&bytes)?;

    // `product()` wraps rather than reporting, so three `u32` dimensions can multiply past `usize`
    // and come back small. The `checked_mul` below would then be checking a number that had
    // already lost the overflow, which is a guard that cannot fire.
    let voxels = header
        .dims
        .iter()
        .try_fold(1usize, |acc, d| acc.checked_mul(*d as usize))
        .ok_or_else(|| malformed("declared dimensions overflow the addressable range"))?;
    let declared_bytes = voxels
        .checked_mul(header.element.size())
        .ok_or_else(|| malformed("declared dimensions overflow the addressable range"))?;
    if declared_bytes > MAX_VOLUME_BYTES {
        return Err(malformed(format!(
            "{}x{}x{} of {:?} declares {declared_bytes} bytes, past the {MAX_VOLUME_BYTES} ceiling",
            header.dims[0], header.dims[1], header.dims[2], header.element
        )));
    }

    // A raw file named by the header, or the tail of this one.
    let (raw, source): (Vec<u8>, String) = match &header.data_file {
        Some(name) => {
            let sibling = resolve(path, name)
                .ok_or_else(|| malformed(format!("{name} is not beside the header")))?;
            let file = std::fs::File::open(&sibling)
                .map_err(|e| malformed(format!("{} could not be read: {e}", sibling.display())))?;
            let raw = if header.compressed {
                let mut out = Vec::with_capacity(declared_bytes.min(16 * 1024 * 1024));
                flate2::read::ZlibDecoder::new(file)
                    .take(declared_bytes as u64 + 1)
                    .read_to_end(&mut out)
                    .map_err(|e| malformed(format!("the zlib stream did not decompress: {e}")))?;
                out
            } else {
                let mut raw = Vec::new();
                file.take(declared_bytes as u64 + 1)
                    .read_to_end(&mut raw)
                    .map_err(|e| {
                        malformed(format!("{} could not be read: {e}", sibling.display()))
                    })?;
                raw
            };
            (
                raw,
                sibling
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            )
        }
        None => (
            bytes[header.data_offset.min(bytes.len())..].to_vec(),
            "LOCAL".into(),
        ),
    };

    let raw = if header.compressed && header.data_file.is_none() {
        // Bounded by what the dimensions say the volume needs, plus one byte so a stream that
        // carries more than it should is detectable rather than silently truncated. An unbounded
        // `read_to_end` here would let a few kilobytes of zlib expand until the process dies,
        // and the compression ratio is chosen by whoever wrote the file.
        let mut out = Vec::with_capacity(declared_bytes.min(16 * 1024 * 1024));
        flate2::read::ZlibDecoder::new(&raw[..])
            .take(declared_bytes as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| malformed(format!("the zlib stream did not decompress: {e}")))?;
        out
    } else {
        raw
    };

    if raw.len() < declared_bytes {
        return Err(malformed(format!(
            "{}x{}x{} of {:?} needs {declared_bytes} bytes, the data has {}",
            header.dims[0],
            header.dims[1],
            header.dims[2],
            header.element,
            raw.len()
        )));
    }

    let data = convert(&raw[..declared_bytes], header.element, header.big_endian);
    let voxel = header.element.stored_as();

    let extent = [
        header.dims[0] as f32 * header.spacing[0],
        header.dims[1] as f32 * header.spacing[1],
        header.dims[2] as f32 * header.spacing[2],
    ];
    let bounds = Aabb {
        min: [-extent[0] * 0.5, -extent[1] * 0.5, -extent[2] * 0.5],
        max: [extent[0] * 0.5, extent[1] * 0.5, extent[2] * 0.5],
    };

    let grid = VolumeGrid {
        dims: header.dims,
        spacing: header.spacing,
        voxel,
        bounds,
        data,
        window: None,
    };
    if !grid.is_consistent() {
        return Err(malformed(
            "the converted voxels do not match the dimensions",
        ));
    }

    let mut scene = Scene::new();
    scene.push_root(Node::with_payload("metaimage", Payload::VolumeGrid(grid)));
    scene.set_meta("format", "MetaImage");
    scene.set_meta(
        "dimensions",
        format!(
            "{} x {} x {}",
            header.dims[0], header.dims[1], header.dims[2]
        ),
    );
    scene.set_meta(
        "spacing",
        format!(
            "{:.4} x {:.4} x {:.4}",
            header.spacing[0], header.spacing[1], header.spacing[2]
        ),
    );
    scene.set_meta("element", format!("{:?}", header.element));
    scene.set_meta("data", source);
    if header.compressed {
        scene.set_meta("compression", "zlib");
    }
    if header.big_endian {
        scene.set_meta("byte order", "big endian");
    }
    Ok(scene)
}

/// Find the raw file the header names.
///
/// The name is relative to the header, and a header written on one platform and read on another
/// carries the other one's separator, so both are accepted. Anything that climbs out of the
/// header's own directory is refused: the path comes from the file, and a viewer has no business
/// following it somewhere else.
fn resolve(header: &Path, name: &str) -> Option<PathBuf> {
    let base = header.parent()?;
    let relative = name.replace('\\', "/");

    // Every component has to be an ordinary name. `Component::Normal` is the whole check: it
    // excludes `Prefix` (a drive letter or a UNC share), `RootDir`, `ParentDir` and `CurDir` in
    // one go.
    //
    // Rejecting only `..` is not enough, and not because of a clever encoding: `Path::join`
    // *replaces* the base when the argument is absolute, so `ElementDataFile = C:\Windows\...`
    // does not escape the directory so much as never enter it. A colon anywhere is refused too,
    // which catches the drive-relative `C:file` form that is neither absolute nor normal.
    let candidate = Path::new(&relative);
    if relative.contains(':') || candidate.is_absolute() || relative.starts_with('/') {
        return None;
    }
    if !candidate
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return None;
    }

    let joined = base.join(candidate);
    joined.is_file().then_some(joined)
}

/// Narrow the declared element type into what `VolumeGrid` stores.
fn convert(raw: &[u8], element: Element, big_endian: bool) -> Vec<u8> {
    let size = element.size();
    let count = raw.len() / size;
    let read = |i: usize| -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[..size].copy_from_slice(&raw[i * size..i * size + size]);
        if big_endian {
            buf[..size].reverse();
        }
        buf
    };

    match element {
        // Already the stored width, and single bytes have no order to swap.
        Element::UChar => raw.to_vec(),
        Element::UShort | Element::Short => {
            let mut out = Vec::with_capacity(count * 2);
            for i in 0..count {
                let b = read(i);
                out.extend_from_slice(&b[..2]);
            }
            out
        }
        Element::Char => {
            // Widened to i16, because there is no signed byte grid.
            let mut out = Vec::with_capacity(count * 2);
            for byte in raw {
                out.extend_from_slice(&(*byte as i8 as i16).to_le_bytes());
            }
            out
        }
        Element::UInt | Element::Int | Element::Float | Element::Double => {
            let mut out = Vec::with_capacity(count * 4);
            for i in 0..count {
                let b = read(i);
                let v = match element {
                    Element::UInt => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32,
                    Element::Int => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32,
                    Element::Float => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                    _ => f64::from_le_bytes(b) as f32,
                };
                out.extend_from_slice(&v.to_le_bytes());
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "ObjectType = Image\nNDims = 3\nDimSize = 2 2 2\n\
                          ElementType = MET_UCHAR\nElementSpacing = 1 1 2\nElementDataFile = LOCAL\n";

    #[test]
    fn the_voxel_offset_is_a_byte_offset_not_a_character_offset() {
        // A header field carrying a byte that is not valid UTF-8. Decoding the whole window as one
        // lossy string would widen it to a three-byte replacement character and push every later
        // offset along with it, so the voxels would be read from the wrong place.
        let mut src = b"ObjectType = Image\nNDims = 3\nDimSize = 2 2 2\n".to_vec();
        src.extend_from_slice(b"AnatomicalOrientation = ");
        src.push(0xFF);
        src.extend_from_slice(b"AI\nElementType = MET_UCHAR\nElementDataFile = LOCAL\n");
        let header_len = src.len();
        src.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);

        let h = parse_header(&src).unwrap();
        assert_eq!(h.data_offset, header_len);
        assert_eq!(&src[h.data_offset..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn a_local_header_parses_and_reports_where_the_voxels_start() {
        let h = parse_header(HEADER.as_bytes()).unwrap();
        assert_eq!(h.dims, [2, 2, 2]);
        assert_eq!(h.spacing, [1.0, 1.0, 2.0]);
        assert_eq!(h.element, Element::UChar);
        assert_eq!(h.data_file, None);
        assert_eq!(h.data_offset, HEADER.len());
    }

    #[test]
    fn a_separate_data_file_is_named_rather_than_inlined() {
        let text = HEADER.replace("LOCAL", "volume.raw");
        let h = parse_header(text.as_bytes()).unwrap();
        assert_eq!(h.data_file.as_deref(), Some("volume.raw"));
    }

    #[test]
    fn every_element_type_has_a_width_and_a_stored_form() {
        for (name, element, size) in [
            ("MET_UCHAR", Element::UChar, 1),
            ("MET_CHAR", Element::Char, 1),
            ("MET_USHORT", Element::UShort, 2),
            ("MET_SHORT", Element::Short, 2),
            ("MET_INT", Element::Int, 4),
            ("MET_FLOAT", Element::Float, 4),
            ("MET_DOUBLE", Element::Double, 8),
        ] {
            assert_eq!(Element::parse(name), Some(element), "{name}");
            assert_eq!(element.size(), size, "{name}");
        }
        assert_eq!(Element::parse("MET_BANANA"), None);
        assert_eq!(Element::Char.stored_as(), VoxelType::I16);
        assert_eq!(Element::Double.stored_as(), VoxelType::F32);
    }

    #[test]
    fn a_two_dimensional_image_is_read_as_one_slice() {
        let text = "ObjectType = Image\nNDims = 2\nDimSize = 4 4\n\
                    ElementType = MET_UCHAR\nElementDataFile = LOCAL\n";
        assert_eq!(parse_header(text.as_bytes()).unwrap().dims, [4, 4, 1]);
    }

    #[test]
    fn a_missing_required_field_is_refused() {
        let text = "ObjectType = Image\nDimSize = 2 2 2\nElementDataFile = LOCAL\n";
        assert!(parse_header(text.as_bytes()).is_err());
    }

    #[test]
    fn big_endian_shorts_are_swapped_on_the_way_in() {
        let raw = [0x01u8, 0x00, 0x02, 0x00];
        let little = convert(&raw, Element::UShort, false);
        let big = convert(&raw, Element::UShort, true);
        assert_eq!(u16::from_le_bytes([little[0], little[1]]), 1);
        assert_eq!(u16::from_le_bytes([big[0], big[1]]), 256);
    }

    #[test]
    fn a_signed_byte_widens_and_keeps_its_sign() {
        let out = convert(&[0xFFu8, 0x01], Element::Char, false);
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), -1);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), 1);
    }

    #[test]
    fn doubles_narrow_to_f32() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&2.5f64.to_le_bytes());
        let out = convert(&raw, Element::Double, false);
        assert_eq!(f32::from_le_bytes([out[0], out[1], out[2], out[3]]), 2.5);
    }

    #[test]
    fn a_data_path_that_leaves_the_header_directory_is_refused() {
        let header = Path::new("C:/scan/head.mhd");
        // `Path::join` replaces the base outright when the argument is absolute, so an absolute
        // ElementDataFile does not climb out of the directory, it never enters it. Rejecting only
        // `..` leaves every one of these open.
        for hostile in [
            "../../secrets.raw",
            "..\\..\\secrets.raw",
            "C:/Windows/System32/config/SAM",
            "C:\\Windows\\System32\\config\\SAM",
            "//server/share/secret.raw",
            "\\\\server\\share\\secret.raw",
            "/etc/shadow",
            "C:relative-to-drive.raw",
            "sub/../../out.raw",
        ] {
            assert_eq!(resolve(header, hostile), None, "accepted {hostile:?}");
        }
    }

    #[test]
    fn dimensions_that_overflow_usize_are_caught_rather_than_wrapping() {
        // Three u32 dimensions multiply past usize. `product()` wraps and comes back with a small
        // number, so a `checked_mul` after it would be checking a value that had already lost the
        // overflow.
        let text = "ObjectType = Image\nNDims = 3\nDimSize = 4294967295 4294967295 4294967295\n\
                    ElementType = MET_SHORT\nElementDataFile = LOCAL\n";
        let h = parse_header(text.as_bytes()).unwrap();
        assert!(h
            .dims
            .iter()
            .try_fold(1usize, |acc, d| acc.checked_mul(*d as usize))
            .is_none());
    }

    #[test]
    fn a_volume_past_the_ceiling_is_refused_before_anything_allocates() {
        let dir = std::env::temp_dir().join("theviewer-metaimage-ceiling");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.mha");
        // 2048 cubed shorts is 16 GB, past the ceiling and three lines to declare.
        let text = "ObjectType = Image\nNDims = 3\nDimSize = 2048 2048 2048\n\
                    ElementType = MET_SHORT\nElementDataFile = LOCAL\n";
        std::fs::write(&path, text).unwrap();
        let err = import(&path).unwrap_err();
        assert!(
            format!("{err}").contains("ceiling"),
            "expected the ceiling in the message, got: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
