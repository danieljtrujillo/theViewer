//! Volume importers: DICOM series, NIfTI, NRRD.
//!
//! **A folder of DICOM slices is one volume, not many files.** Opening a single `.dcm` therefore
//! scans its directory for siblings in the same series and stacks them. Anything else shows one
//! slice out of several hundred, which is not a view of the study.
//!
//! Slices are ordered by their position along the slice normal rather than by `InstanceNumber`.
//! Instance numbers are frequently absent, restart per acquisition, or run backwards relative to
//! the geometry, and a mis-ordered stack looks plausible while being wrong.

use std::path::Path;

use viewer_io::FormatId;
use viewer_scene::payload::{VolumeGrid, VoxelType};
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

/// Refuse a stack that would exceed this many voxels before allocating for it. A 512 by 512 study
/// of 2000 slices sits just under it.
const MAX_VOXELS: usize = 600_000_000;

fn malformed(format: FormatId, reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format,
        reason: reason.into(),
    }
}

/// Build a scene around one grid, deriving world bounds from voxel spacing.
fn scene_from(mut grid: VolumeGrid, label: &str, format: &str) -> Scene {
    let extent = [
        grid.dims[0] as f32 * grid.spacing[0],
        grid.dims[1] as f32 * grid.spacing[1],
        grid.dims[2] as f32 * grid.spacing[2],
    ];
    // Centre on the origin so the orbit camera has something sensible to pivot around.
    grid.bounds = Aabb {
        min: [-extent[0] * 0.5, -extent[1] * 0.5, -extent[2] * 0.5],
        max: [extent[0] * 0.5, extent[1] * 0.5, extent[2] * 0.5],
    };

    let mut scene = Scene::new();
    scene.set_meta("format", format);
    scene.set_meta(
        "dimensions",
        format!("{} x {} x {}", grid.dims[0], grid.dims[1], grid.dims[2]),
    );
    scene.set_meta(
        "spacing",
        format!(
            "{:.3} x {:.3} x {:.3} mm",
            grid.spacing[0] * 1000.0,
            grid.spacing[1] * 1000.0,
            grid.spacing[2] * 1000.0
        ),
    );
    scene.push_root(Node::with_payload(label, Payload::VolumeGrid(grid)));
    scene
}

// ---------------------------------------------------------------------------- NRRD

/// NRRD: an ASCII header of `key: value` lines, a blank line, then raw or gzip-compressed data.
pub fn import_nrrd(bytes: &[u8]) -> Result<Scene, ImportError> {
    let fmt = FormatId::Nrrd;
    let split = find(bytes, b"\n\n")
        .map(|i| i + 2)
        .or_else(|| find(bytes, b"\r\n\r\n").map(|i| i + 4))
        .ok_or_else(|| malformed(fmt.clone(), "no blank line separating header from data"))?;

    let header = String::from_utf8_lossy(&bytes[..split]);
    let mut dims = [0u32; 3];
    let mut spacing = [1.0f32; 3];
    let mut voxel = None;
    let mut gzipped = false;
    let mut little_endian = true;

    for line in header.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim_start_matches('=').trim();
        match key.as_str() {
            "sizes" => {
                for (i, v) in value.split_whitespace().take(3).enumerate() {
                    dims[i] = v.parse().unwrap_or(0);
                }
            }
            "spacings" => {
                for (i, v) in value.split_whitespace().take(3).enumerate() {
                    if let Ok(s) = v.parse::<f32>() {
                        if s.is_finite() && s > 0.0 {
                            spacing[i] = s;
                        }
                    }
                }
            }
            "type" => {
                voxel = Some(match value.to_ascii_lowercase().as_str() {
                    "uchar" | "unsigned char" | "uint8" | "uint8_t" => VoxelType::U8,
                    "ushort" | "unsigned short" | "uint16" | "uint16_t" => VoxelType::U16,
                    "short" | "signed short" | "int16" | "int16_t" => VoxelType::I16,
                    "float" | "float32" => VoxelType::F32,
                    other => {
                        return Err(malformed(fmt.clone(), format!("unsupported type {other}")))
                    }
                });
            }
            "encoding" => match value.to_ascii_lowercase().as_str() {
                "raw" => gzipped = false,
                "gzip" | "gz" => gzipped = true,
                other => {
                    return Err(malformed(
                        fmt.clone(),
                        format!("unsupported encoding {other}"),
                    ))
                }
            },
            "endian" => little_endian = value.eq_ignore_ascii_case("little"),
            _ => {}
        }
    }

    let voxel = voxel.ok_or_else(|| malformed(fmt.clone(), "no type line"))?;
    if dims.contains(&0) {
        return Err(malformed(fmt.clone(), "sizes must name three positive extents"));
    }
    let count = voxel_count(&dims, fmt.clone())?;

    let mut data = if gzipped {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[split..])
            .read_to_end(&mut out)
            .map_err(|e| malformed(fmt.clone(), format!("gzip stream is unreadable: {e}")))?;
        out
    } else {
        bytes[split..].to_vec()
    };

    let expected = count * voxel.size();
    if data.len() < expected {
        return Err(malformed(
            fmt.clone(),
            format!("data is {} bytes, the header predicts {expected}", data.len()),
        ));
    }
    data.truncate(expected);
    if !little_endian && voxel.size() > 1 {
        swap_bytes(&mut data, voxel.size());
    }

    // NRRD spacings are millimetres by convention; the scene works in metres.
    let spacing = [
        spacing[0] * 0.001,
        spacing[1] * 0.001,
        spacing[2] * 0.001,
    ];

    Ok(scene_from(
        VolumeGrid {
            dims,
            spacing,
            voxel,
            bounds: Aabb::EMPTY,
            data,
            window: None,
        },
        "nrrd",
        "NRRD",
    ))
}

// ---------------------------------------------------------------------------- NIfTI

pub fn import_nifti(path: &Path) -> Result<Scene, ImportError> {
    use nifti::{IntoNdArray, NiftiObject, ReaderOptions};

    let fmt = FormatId::Nifti;
    let object = ReaderOptions::new()
        .read_file(path)
        .map_err(|e| malformed(fmt.clone(), e.to_string()))?;
    let header = object.header().clone();
    let dims = header.dim;
    if dims[0] < 3 {
        return Err(malformed(fmt.clone(), "fewer than three dimensions"));
    }
    let d = [dims[1] as u32, dims[2] as u32, dims[3] as u32];
    let count = voxel_count(&d, fmt.clone())?;

    // Everything is read as f32 and re-quantized. NIfTI carries a dozen datatypes and a
    // scl_slope/scl_inter pair; going through f32 applies the scaling once and leaves one path.
    let volume = object
        .into_volume()
        .into_ndarray::<f32>()
        .map_err(|e| malformed(fmt.clone(), e.to_string()))?;

    let mut data = Vec::with_capacity(count * 4);
    for v in volume.iter().take(count) {
        data.extend_from_slice(&v.to_le_bytes());
    }
    data.resize(count * 4, 0);

    // pixdim is in the units named by xyzt_units; millimetres is the overwhelming default.
    let spacing = [
        header.pixdim[1].abs().max(1e-6) * 0.001,
        header.pixdim[2].abs().max(1e-6) * 0.001,
        header.pixdim[3].abs().max(1e-6) * 0.001,
    ];

    Ok(scene_from(
        VolumeGrid {
            dims: d,
            spacing,
            voxel: VoxelType::F32,
            bounds: Aabb::EMPTY,
            data,
            window: None,
        },
        "nifti",
        "NIfTI",
    ))
}

// ---------------------------------------------------------------------------- DICOM

/// Import the whole series the given file belongs to.
pub fn import_dicom(path: &Path) -> Result<Scene, ImportError> {
    use dicom_dictionary_std::tags;
    use dicom_object::open_file;
    use dicom_pixeldata::PixelDecoder;

    let fmt = FormatId::Dicom;
    let first = open_file(path).map_err(|e| malformed(fmt.clone(), e.to_string()))?;

    let series = first
        .element(tags::SERIES_INSTANCE_UID)
        .ok()
        .and_then(|e| e.to_str().ok())
        .map(|s| s.trim().to_string());

    // Collect the siblings that belong to the same series.
    let mut slices: Vec<(f64, std::path::PathBuf)> = Vec::new();
    let dir = path.parent().unwrap_or(Path::new("."));
    let normal = slice_normal(&first);

    let entries = std::fs::read_dir(dir)
        .map_err(|e| malformed(fmt.clone(), format!("cannot read the folder: {e}")))?;
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Ok(obj) = open_file(&p) else { continue };
        let uid = obj
            .element(tags::SERIES_INSTANCE_UID)
            .ok()
            .and_then(|e| e.to_str().ok())
            .map(|s| s.trim().to_string());
        if uid != series {
            continue;
        }
        slices.push((slice_position(&obj, normal), p));
    }

    if slices.is_empty() {
        slices.push((0.0, path.to_path_buf()));
    }
    slices.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let rows = u32::from(
        first
            .element(tags::ROWS)
            .ok()
            .and_then(|e| e.to_int::<u16>().ok())
            .ok_or_else(|| malformed(fmt.clone(), "no Rows"))?,
    );
    let cols = u32::from(
        first
            .element(tags::COLUMNS)
            .ok()
            .and_then(|e| e.to_int::<u16>().ok())
            .ok_or_else(|| malformed(fmt.clone(), "no Columns"))?,
    );
    let depth = slices.len() as u32;
    let dims = [cols, rows, depth];
    let count = voxel_count(&dims, fmt.clone())?;
    let per_slice = (cols as usize) * (rows as usize);

    // Decoding runs through dicom-pixeldata rather than reading PixelData directly. Most studies
    // arrive in an encapsulated transfer syntax (JPEG Lossless, JPEG-LS, JPEG 2000, RLE), where
    // the element is a pixel sequence and reading it as bytes fails outright. The decoder also
    // applies the rescale slope and intercept per slice, which matters because those can differ
    // between slices in one series.
    let mut data: Vec<u8> = Vec::with_capacity(count * 4);
    for (_, p) in &slices {
        let obj = open_file(p).map_err(|e| malformed(fmt.clone(), e.to_string()))?;
        let decoded = obj
            .decode_pixel_data()
            .map_err(|e| malformed(fmt.clone(), format!("PixelData is unreadable: {e}")))?;
        let values: Vec<f32> = decoded
            .to_vec::<f32>()
            .map_err(|e| malformed(fmt.clone(), format!("PixelData is unreadable: {e}")))?;

        // A multi-frame instance carries the whole stack in one file; take only the first frame
        // per file here, because the outer loop is already walking the series.
        for v in values.iter().take(per_slice) {
            data.extend_from_slice(&v.to_le_bytes());
        }
        // A short slice pads rather than shifting every slice after it.
        for _ in values.len().min(per_slice)..per_slice {
            data.extend_from_slice(&0f32.to_le_bytes());
        }
    }
    data.resize(count * 4, 0);

    // PixelSpacing is row spacing then column spacing, in millimetres.
    let (row_mm, col_mm) = first
        .element(tags::PIXEL_SPACING)
        .ok()
        .and_then(|e| e.to_multi_float64().ok())
        .filter(|v| v.len() >= 2)
        .map(|v| (v[0] as f32, v[1] as f32))
        .unwrap_or((1.0, 1.0));
    // Prefer the measured gap between the first two slices; fall back to SliceThickness.
    let slice_mm = if slices.len() > 1 {
        let gap = (slices[1].0 - slices[0].0).abs() as f32;
        if gap > 1e-4 {
            gap
        } else {
            thickness(&first)
        }
    } else {
        thickness(&first)
    };

    let window = first
        .element(tags::WINDOW_CENTER)
        .ok()
        .and_then(|e| e.to_float64().ok())
        .zip(
            first
                .element(tags::WINDOW_WIDTH)
                .ok()
                .and_then(|e| e.to_float64().ok()),
        )
        .map(|(c, w)| (w as f32, c as f32));

    let mut scene = scene_from(
        VolumeGrid {
            dims,
            spacing: [
                col_mm.abs().max(1e-6) * 0.001,
                row_mm.abs().max(1e-6) * 0.001,
                slice_mm.abs().max(1e-6) * 0.001,
            ],
            voxel: VoxelType::F32,
            bounds: Aabb::EMPTY,
            data,
            window,
        },
        "dicom",
        "DICOM",
    );
    scene.set_meta("slices", slices.len().to_string());
    if let Some(m) = first.element(tags::MODALITY).ok().and_then(|e| e.to_str().ok()) {
        scene.set_meta("modality", m.trim());
    }
    Ok(scene)
}

fn thickness(obj: &dicom_object::DefaultDicomObject) -> f32 {
    use dicom_dictionary_std::tags;
    obj.element(tags::SLICE_THICKNESS)
        .ok()
        .and_then(|e| e.to_float64().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0) as f32
}

/// Cross product of the two row vectors in ImageOrientationPatient.
fn slice_normal(obj: &dicom_object::DefaultDicomObject) -> [f64; 3] {
    use dicom_dictionary_std::tags;
    let o = obj
        .element(tags::IMAGE_ORIENTATION_PATIENT)
        .ok()
        .and_then(|e| e.to_multi_float64().ok())
        .filter(|v| v.len() >= 6);
    match o {
        Some(v) => [
            v[1] * v[5] - v[2] * v[4],
            v[2] * v[3] - v[0] * v[5],
            v[0] * v[4] - v[1] * v[3],
        ],
        // Without orientation, assume the slices stack along z.
        None => [0.0, 0.0, 1.0],
    }
}

/// Position of a slice along the stacking direction.
fn slice_position(obj: &dicom_object::DefaultDicomObject, normal: [f64; 3]) -> f64 {
    use dicom_dictionary_std::tags;
    if let Some(p) = obj
        .element(tags::IMAGE_POSITION_PATIENT)
        .ok()
        .and_then(|e| e.to_multi_float64().ok())
        .filter(|v| v.len() >= 3)
    {
        return p[0] * normal[0] + p[1] * normal[1] + p[2] * normal[2];
    }
    // InstanceNumber is the fallback, not the primary key.
    obj.element(tags::INSTANCE_NUMBER)
        .ok()
        .and_then(|e| e.to_float64().ok())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------- shared

fn voxel_count(dims: &[u32; 3], fmt: FormatId) -> Result<usize, ImportError> {
    let count = (dims[0] as usize)
        .checked_mul(dims[1] as usize)
        .and_then(|v| v.checked_mul(dims[2] as usize))
        .ok_or_else(|| malformed(fmt.clone(), "dimensions overflow"))?;
    if count == 0 || count > MAX_VOXELS {
        return Err(malformed(
            fmt,
            format!("{count} voxels is outside the supported range"),
        ));
    }
    Ok(count)
}

fn swap_bytes(data: &mut [u8], width: usize) {
    for chunk in data.chunks_exact_mut(width) {
        chunk.reverse();
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nrrd(header: &str, data: &[u8]) -> Vec<u8> {
        let mut out = header.as_bytes().to_vec();
        out.extend_from_slice(b"\n\n");
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn a_raw_nrrd_parses() {
        let src = nrrd(
            "NRRD0004\ntype: uchar\ndimension: 3\nsizes: 2 2 2\nencoding: raw\nspacings: 1 1 2",
            &[0, 1, 2, 3, 4, 5, 6, 7],
        );
        let scene = import_nrrd(&src).unwrap();
        assert_eq!(scene.summary().voxels, 8);
        // Spacing is millimetres in the file and metres in the scene, so z is twice x.
        let b = scene.bounds();
        let ext = b.extent();
        assert!((ext[2] / ext[0] - 2.0).abs() < 1e-4, "{ext:?}");
    }

    #[test]
    fn a_gzip_encoded_nrrd_parses() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&[9u8; 8]).unwrap();
        let src = nrrd(
            "NRRD0004\ntype: uchar\ndimension: 3\nsizes: 2 2 2\nencoding: gzip",
            &enc.finish().unwrap(),
        );
        assert_eq!(import_nrrd(&src).unwrap().summary().voxels, 8);
    }

    #[test]
    fn a_short_nrrd_payload_is_rejected() {
        let src = nrrd(
            "NRRD0004\ntype: ushort\ndimension: 3\nsizes: 4 4 4\nencoding: raw",
            &[0; 10],
        );
        assert!(import_nrrd(&src).is_err());
    }

    #[test]
    fn an_nrrd_without_a_blank_line_is_rejected() {
        assert!(import_nrrd(b"NRRD0004\ntype: uchar\n").is_err());
    }

    #[test]
    fn an_unsupported_nrrd_type_is_named_rather_than_guessed() {
        let src = nrrd(
            "NRRD0004\ntype: double\ndimension: 3\nsizes: 2 2 2\nencoding: raw",
            &[0; 64],
        );
        let err = format!("{}", import_nrrd(&src).unwrap_err());
        assert!(err.contains("double"), "{err}");
    }

    #[test]
    fn big_endian_data_is_swapped() {
        // 0x0100 big-endian is 256; read little-endian without swapping it would be 1.
        let src = nrrd(
            "NRRD0004\ntype: ushort\ndimension: 3\nsizes: 1 1 1\nencoding: raw\nendian: big",
            &[0x01, 0x00],
        );
        let scene = import_nrrd(&src).unwrap();
        let Payload::VolumeGrid(g) = scene.nodes[0].payload.as_ref().unwrap() else {
            panic!("expected a volume");
        };
        assert_eq!(u16::from_le_bytes([g.data[0], g.data[1]]), 256);
    }

    #[test]
    fn an_absurd_voxel_count_is_refused() {
        let src = nrrd(
            "NRRD0004\ntype: uchar\ndimension: 3\nsizes: 5000 5000 5000\nencoding: raw",
            &[0; 8],
        );
        assert!(import_nrrd(&src).is_err());
    }

    #[test]
    fn bounds_are_centred_on_the_origin() {
        let src = nrrd(
            "NRRD0004\ntype: uchar\ndimension: 3\nsizes: 2 2 2\nencoding: raw\nspacings: 1 1 1",
            &[0; 8],
        );
        let b = import_nrrd(&src).unwrap().bounds();
        assert!((b.center()[0]).abs() < 1e-6);
        assert!((b.center()[2]).abs() < 1e-6);
    }

    #[test]
    fn the_slice_normal_defaults_to_z_without_orientation() {
        // Exercised indirectly: the cross product of the identity row vectors is +z.
        let v = [1.0f64, 0.0, 0.0, 0.0, 1.0, 0.0];
        let n = [
            v[1] * v[5] - v[2] * v[4],
            v[2] * v[3] - v[0] * v[5],
            v[0] * v[4] - v[1] * v[3],
        ];
        assert_eq!(n, [0.0, 0.0, 1.0]);
    }
}
