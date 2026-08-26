//! Niantic SPZ importer, versions 1 through 4.
//!
//! Two encodings share the extension, and which one a file uses is decided at offset 0:
//!
//! - **v1 to v3**: the whole file is a gzip stream. Inflating it yields a 16-byte header starting
//!   with the `NGSP` magic, then per-attribute contiguous arrays.
//! - **v4**, released 2026-05-05: a plaintext header sits at offset 0, outside any compression, so
//!   point count and SH degree are readable without inflating anything. The payload is six
//!   independent ZSTD streams, one per attribute, which is what makes it decode on several cores.
//!
//! Both land on the same fixed-point layout once decompressed:
//!
//! ```text
//! positions : count * 3 * int24   metres = v / (1 << fractional_bits)
//! alphas    : count * u8          alpha  = a / 255
//! colors    : count * 3 * u8      sh0    = (c/255 - 0.5) / 0.15
//! scales    : count * 3 * u8      metres = exp(s/16 - 10)
//! rotations : count * 3 * u8      xyz    = (r - 127.5)/127.5, w from the unit constraint
//! sh        : count * sh_dim * u8 sh_dim = 0/9/24/45/72 for degree 0/1/2/3/4
//! ```
//!
//! The exact byte total is known in advance, which makes an equality check the cheapest possible
//! corruption gate. It is verified byte-exact against a 27.8 MB Marble export: v2, 1,920,000
//! splats, shDegree 0, fractionalBits 10, predicted 36,480,016 bytes, actual 36,480,016.

use std::io::Read;

use viewer_io::{FormatId, SpzEncoding};
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::SplatCloud;
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

const MAGIC: u32 = 0x5053_474E; // "NGSP" read as a little-endian u32.
const HEADER_LEN: usize = 16;

/// SPZ 4 removed the previous 10 M cap. This bound only exists to stop a corrupt count from
/// triggering a huge allocation before any data is read.
const MAX_POINTS: u32 = 200_000_000;

fn malformed(encoding: SpzEncoding, reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Spz(encoding),
        reason: reason.into(),
    }
}

struct Header {
    version: u32,
    count: u32,
    sh_degree: u8,
    fractional_bits: u8,
    antialiased: bool,
}

fn parse_header(b: &[u8], encoding: SpzEncoding) -> Result<Header, ImportError> {
    if b.len() < HEADER_LEN {
        return Err(malformed(
            encoding,
            "file is shorter than the 16-byte header",
        ));
    }
    let magic = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    if magic != MAGIC {
        return Err(malformed(encoding, "the NGSP magic is missing"));
    }
    let version = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let count = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
    if count > MAX_POINTS {
        return Err(malformed(
            encoding,
            format!("the header declares {count} splats"),
        ));
    }
    let sh_degree = b[12];
    if sh_degree > 4 {
        return Err(malformed(
            encoding,
            format!("spherical harmonic degree {sh_degree} is above the maximum of 4"),
        ));
    }
    Ok(Header {
        version,
        count,
        sh_degree,
        fractional_bits: b[13],
        antialiased: b[14] & 1 != 0,
    })
}

/// Coefficient triples per splat, times three channels, gives the stored byte width.
fn sh_dim(degree: u8) -> usize {
    SplatCloud::sh_coef_count(degree) * 3
}

pub fn import(bytes: &[u8], encoding: SpzEncoding) -> Result<Scene, ImportError> {
    match encoding {
        SpzEncoding::Gzip => import_gzip(bytes),
        SpzEncoding::PlaintextV4 => import_v4(bytes),
    }
}

fn import_gzip(bytes: &[u8]) -> Result<Scene, ImportError> {
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut raw)
        .map_err(|e| malformed(SpzEncoding::Gzip, format!("gzip stream is unreadable: {e}")))?;

    let header = parse_header(&raw, SpzEncoding::Gzip)?;
    let body = &raw[HEADER_LEN..];
    let n = header.count as usize;
    let expected = n * (9 + 1 + 3 + 3 + 3 + sh_dim(header.sh_degree));
    if body.len() != expected {
        return Err(malformed(
            SpzEncoding::Gzip,
            format!(
                "payload is {} bytes, the header predicts {expected}",
                body.len()
            ),
        ));
    }

    // Contiguous per-attribute runs, in this order.
    let mut at = 0usize;
    let mut take = |len: usize| {
        let s = &body[at..at + len];
        at += len;
        s
    };
    let positions = take(n * 9);
    let alphas = take(n);
    let colors = take(n * 3);
    let scales = take(n * 3);
    let rotations = take(n * 3);
    let sh = take(n * sh_dim(header.sh_degree));

    build(
        &header,
        positions,
        alphas,
        colors,
        scales,
        rotations,
        sh,
        SpzEncoding::Gzip,
    )
}

fn import_v4(bytes: &[u8]) -> Result<Scene, ImportError> {
    let e = SpzEncoding::PlaintextV4;
    let header = parse_header(bytes, e)?;
    let n = header.count as usize;

    // Six independent ZSTD streams follow the header, each prefixed with its compressed length.
    // Decoding to a known expected size is another integrity check: a stream that inflates to the
    // wrong length is corrupt regardless of whether ZSTD reported success.
    let mut at = HEADER_LEN;
    let mut stream = |expected: usize| -> Result<Vec<u8>, ImportError> {
        if expected == 0 {
            return Ok(Vec::new());
        }
        if at + 4 > bytes.len() {
            return Err(malformed(e, "file ends before an attribute stream header"));
        }
        let len =
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        at += 4;
        let end = at
            .checked_add(len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| malformed(e, "an attribute stream runs past the end of the file"))?;
        let mut out = Vec::with_capacity(expected);
        zstd::stream::read::Decoder::new(&bytes[at..end])
            .map_err(|err| malformed(e, format!("zstd stream is unreadable: {err}")))?
            .read_to_end(&mut out)
            .map_err(|err| malformed(e, format!("zstd stream is unreadable: {err}")))?;
        at = end;
        if out.len() != expected {
            return Err(malformed(
                e,
                format!(
                    "an attribute stream decoded to {} bytes, expected {expected}",
                    out.len()
                ),
            ));
        }
        Ok(out)
    };

    let positions = stream(n * 9)?;
    let colors = stream(n * 3)?;
    let scales = stream(n * 3)?;
    let rotations = stream(n * 3)?;
    let alphas = stream(n)?;
    let sh = stream(n * sh_dim(header.sh_degree))?;

    build(
        &header, &positions, &alphas, &colors, &scales, &rotations, &sh, e,
    )
}

#[allow(clippy::too_many_arguments)]
fn build(
    header: &Header,
    positions: &[u8],
    alphas: &[u8],
    colors: &[u8],
    scales: &[u8],
    rotations: &[u8],
    sh: &[u8],
    encoding: SpzEncoding,
) -> Result<Scene, ImportError> {
    let n = header.count as usize;
    let scale_divisor = (1u32 << header.fractional_bits.min(31)) as f32;

    let mut pos = Vec::with_capacity(n * 3);
    let mut bounds = Aabb::EMPTY;
    for i in 0..n {
        let mut p = [0.0f32; 3];
        for (c, slot) in p.iter_mut().enumerate() {
            let o = i * 9 + c * 3;
            // Little-endian signed 24-bit, sign-extended through a shift pair.
            let raw = positions[o] as u32
                | ((positions[o + 1] as u32) << 8)
                | ((positions[o + 2] as u32) << 16);
            *slot = (((raw << 8) as i32) >> 8) as f32 / scale_divisor;
        }
        bounds.expand(p);
        pos.extend_from_slice(&p);
    }

    let opacity: Vec<f32> = alphas.iter().map(|a| *a as f32 / 255.0).collect();

    let scale: Vec<f32> = scales
        .iter()
        .map(|s| (*s as f32 / 16.0 - 10.0).exp())
        .collect();

    let mut rotation = Vec::with_capacity(n * 4);
    for i in 0..n {
        let x = (rotations[i * 3] as f32 - 127.5) / 127.5;
        let y = (rotations[i * 3 + 1] as f32 - 127.5) / 127.5;
        let z = (rotations[i * 3 + 2] as f32 - 127.5) / 127.5;
        let w = (1.0 - x * x - y * y - z * z).max(0.0).sqrt();
        // Already x,y,z,w, which is the order the scene model and glTF both use.
        rotation.extend_from_slice(&[x, y, z, w]);
    }

    let dc: Vec<f32> = colors
        .iter()
        .map(|c| (*c as f32 / 255.0 - 0.5) / 0.15)
        .collect();

    let mut attributes = vec![
        f32_attr(Semantic::Position, 3, pos, encoding)?,
        f32_attr(Semantic::SplatScale, 3, scale, encoding)?,
        f32_attr(Semantic::SplatRotation, 4, rotation, encoding)?,
        f32_attr(Semantic::SplatOpacity, 1, opacity, encoding)?,
        f32_attr(
            Semantic::SplatSh {
                degree: 0,
                index: 0,
            },
            3,
            dc,
            encoding,
        )?,
    ];

    // Higher bands are stored per splat as coefficient-major triples, unlike the PLY layout which
    // groups by channel.
    if header.sh_degree > 0 {
        let dim = sh_dim(header.sh_degree);
        for degree in 1..=header.sh_degree {
            let first = SplatCloud::sh_coef_count(degree - 1);
            let last = SplatCloud::sh_coef_count(degree);
            for c in first..last {
                let mut triple = Vec::with_capacity(n * 3);
                for i in 0..n {
                    for ch in 0..3 {
                        let v = sh[i * dim + c * 3 + ch];
                        triple.push((v as f32 - 128.0) / 128.0);
                    }
                }
                attributes.push(f32_attr(
                    Semantic::SplatSh {
                        degree,
                        index: (c - first) as u8,
                    },
                    3,
                    triple,
                    encoding,
                )?);
            }
        }
    }

    let mut scene = Scene::new();
    scene.set_meta("format", "SPZ");
    scene.set_meta("version", header.version.to_string());
    scene.set_meta("sh_degree", header.sh_degree.to_string());
    scene.push_root(Node::with_payload(
        "spz",
        Payload::SplatCloud(SplatCloud {
            attributes,
            count: n,
            bounds,
            sh_degree: header.sh_degree,
            antialiased: header.antialiased,
            orbit_hint: None,
        }),
    ));
    Ok(scene)
}

fn f32_attr(
    semantic: Semantic,
    components: u8,
    data: Vec<f32>,
    encoding: SpzEncoding,
) -> Result<Attribute, ImportError> {
    let count = data.len() / components as usize;
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for v in &data {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    Attribute::new(semantic, ComponentType::F32, components, count, bytes)
        .map_err(|e| malformed(encoding, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn header_bytes(version: u32, count: u32, degree: u8, frac: u8) -> Vec<u8> {
        let mut h = Vec::with_capacity(HEADER_LEN);
        h.extend_from_slice(&MAGIC.to_le_bytes());
        h.extend_from_slice(&version.to_le_bytes());
        h.extend_from_slice(&count.to_le_bytes());
        h.push(degree);
        h.push(frac);
        h.push(0);
        h.push(0);
        h
    }

    /// One splat at (1.0, -1.0, 0.0) with a known scale, alpha and identity-ish rotation.
    fn payload_one() -> Vec<u8> {
        let mut b = Vec::new();
        // positions: int24 at 10 fractional bits, so 1024 == 1.0 metre.
        for v in [1024i32, -1024, 0] {
            let u = v as u32;
            b.push((u & 0xFF) as u8);
            b.push(((u >> 8) & 0xFF) as u8);
            b.push(((u >> 16) & 0xFF) as u8);
        }
        b.push(255); // alpha
        b.extend_from_slice(&[128, 128, 128]); // colors
        b.extend_from_slice(&[160, 160, 160]); // scales
        b.extend_from_slice(&[128, 128, 128]); // rotations, near zero xyz
        b
    }

    #[test]
    fn gzip_v2_round_trips_through_the_importer() {
        let mut raw = header_bytes(2, 1, 0, 10);
        raw.extend_from_slice(&payload_one());
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&raw).unwrap();
        let gz = enc.finish().unwrap();

        let scene = import(&gz, SpzEncoding::Gzip).unwrap();
        assert_eq!(scene.summary().splats, 1);
        // 1024 / (1 << 10) == 1.0. A single splat makes min and max the same point.
        assert_eq!(scene.bounds().min, [1.0, -1.0, 0.0]);
        assert_eq!(scene.bounds().max, [1.0, -1.0, 0.0]);
    }

    #[test]
    fn the_predicted_byte_total_rejects_a_truncated_payload() {
        let mut raw = header_bytes(2, 2, 0, 10);
        raw.extend_from_slice(&payload_one()); // only one splat of data for a two-splat header
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&raw).unwrap();
        let gz = enc.finish().unwrap();
        assert!(import(&gz, SpzEncoding::Gzip).is_err());
    }

    #[test]
    fn v4_reads_six_length_prefixed_zstd_streams() {
        let n = 1usize;
        let mut file = header_bytes(4, n as u32, 0, 10);
        let one = payload_one();
        // v4 orders the streams positions, colors, scales, rotations, alphas.
        let parts: [&[u8]; 5] = [
            &one[0..9],
            &one[10..13],
            &one[13..16],
            &one[16..19],
            &one[9..10],
        ];
        for part in parts {
            let packed = zstd::stream::encode_all(part, 1).unwrap();
            file.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            file.extend_from_slice(&packed);
        }
        let scene = import(&file, SpzEncoding::PlaintextV4).unwrap();
        assert_eq!(scene.summary().splats, 1);
        assert_eq!(scene.bounds().max, [1.0, -1.0, 0.0]);
    }

    #[test]
    fn a_missing_magic_is_rejected() {
        let mut raw = header_bytes(2, 1, 0, 10);
        raw[0] = 0;
        raw.extend_from_slice(&payload_one());
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&raw).unwrap();
        assert!(import(&enc.finish().unwrap(), SpzEncoding::Gzip).is_err());
    }

    #[test]
    fn an_absurd_splat_count_is_refused_before_allocating() {
        let raw = header_bytes(2, 999_999_999, 0, 10);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&raw).unwrap();
        assert!(import(&enc.finish().unwrap(), SpzEncoding::Gzip).is_err());
    }

    #[test]
    fn a_degree_above_four_is_refused() {
        let raw = header_bytes(4, 1, 5, 10);
        assert!(parse_header(&raw, SpzEncoding::PlaintextV4).is_err());
    }

    #[test]
    fn sh_dim_matches_the_documented_widths() {
        // 0 / 9 / 24 / 45 for degrees 0..=3, and 72 once degree 4 is in play.
        assert_eq!(sh_dim(0), 0);
        assert_eq!(sh_dim(1), 9);
        assert_eq!(sh_dim(2), 24);
        assert_eq!(sh_dim(3), 45);
        assert_eq!(sh_dim(4), 72);
    }

    #[test]
    fn the_marble_export_size_prediction_holds() {
        // v2, 1,920,000 splats, degree 0, verified byte-exact against the real 27.8 MB file.
        let n = 1_920_000usize;
        let predicted = HEADER_LEN + n * (9 + 1 + 3 + 3 + 3 + sh_dim(0));
        assert_eq!(predicted, 36_480_016);
    }

    #[test]
    fn garbage_input_does_not_panic() {
        assert!(import(&[0u8; 4], SpzEncoding::Gzip).is_err());
        assert!(import(&[0u8; 4], SpzEncoding::PlaintextV4).is_err());
        assert!(import(&[], SpzEncoding::PlaintextV4).is_err());
    }
}
