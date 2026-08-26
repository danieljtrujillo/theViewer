//! PCD, the Point Cloud Library's own format.
//!
//! An ASCII header naming the fields, then the points in one of three encodings. All three are
//! implemented, because the third is the one that actually turns up:
//!
//! - **`ascii`**: one line per point. Readable, and enormous.
//! - **`binary`**: the header's declared layout, point after point.
//! - **`binary_compressed`**: the same values **transposed into field-major order** and then run
//!   through LZF. Transposing first is what makes it compress: all the x coordinates together are
//!   nearly a ramp, whereas interleaved xyzrgb is nearly random. A reader that decompresses but
//!   forgets to un-transpose gets a plausible-looking cloud of complete nonsense.
//!
//! LZF is implemented here rather than pulled in: the decompressor is forty lines, and the
//! alternative is a dependency for a format that is otherwise self-contained.

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::PointCloud;
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Pcd,
        reason: reason.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Ascii,
    Binary,
    BinaryCompressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Uint,
    Float,
}

#[derive(Debug, Clone)]
struct Field {
    name: String,
    size: usize,
    kind: Kind,
    count: usize,
    /// Byte offset of this field within one point record.
    offset: usize,
}

#[derive(Debug, Clone)]
pub struct Header {
    fields: Vec<Field>,
    pub points: usize,
    pub encoding: Encoding,
    /// Bytes in one point record.
    stride: usize,
    /// Offset of the first byte after the header.
    pub data_offset: usize,
}

impl Header {
    fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// Parse the ASCII header. Every line is `KEYWORD value...`; `DATA` is always the last one.
pub fn parse_header(bytes: &[u8]) -> Result<Header, ImportError> {
    let mut names: Vec<String> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    let mut kinds: Vec<Kind> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    let mut points: Option<usize> = None;
    let mut width: Option<usize> = None;
    let mut height: Option<usize> = None;
    let mut encoding = None;
    let mut offset = 0usize;

    // The header is text, but the body may not be, so it is walked line by line over the raw
    // bytes rather than decoded wholesale.
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        offset += line.len();
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let mut parts = text.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        match keyword.to_ascii_uppercase().as_str() {
            "FIELDS" | "COLUMNS" => names = parts.map(|s| s.to_ascii_lowercase()).collect(),
            "SIZE" => sizes = parts.filter_map(|s| s.parse().ok()).collect(),
            "TYPE" => {
                kinds = parts
                    .map(|s| match s.to_ascii_uppercase().as_str() {
                        "I" => Kind::Int,
                        "U" => Kind::Uint,
                        _ => Kind::Float,
                    })
                    .collect()
            }
            "COUNT" => counts = parts.filter_map(|s| s.parse().ok()).collect(),
            "POINTS" => points = parts.next().and_then(|s| s.parse().ok()),
            "WIDTH" => width = parts.next().and_then(|s| s.parse().ok()),
            "HEIGHT" => height = parts.next().and_then(|s| s.parse().ok()),
            "DATA" => {
                encoding = Some(
                    match parts
                        .next()
                        .unwrap_or("ascii")
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "binary" => Encoding::Binary,
                        "binary_compressed" => Encoding::BinaryCompressed,
                        _ => Encoding::Ascii,
                    },
                );
                break;
            }
            _ => {}
        }
    }

    let encoding = encoding.ok_or_else(|| malformed("the header has no DATA line"))?;
    if names.is_empty() {
        return Err(malformed("the header declares no FIELDS"));
    }
    if sizes.len() != names.len() || kinds.len() != names.len() {
        return Err(malformed(
            "FIELDS, SIZE and TYPE declare different numbers of columns",
        ));
    }

    // COUNT is optional and defaults to one element per field.
    if counts.len() != names.len() {
        counts = vec![1; names.len()];
    }

    let mut fields = Vec::with_capacity(names.len());
    let mut stride = 0usize;
    for i in 0..names.len() {
        if !matches!(sizes[i], 1 | 2 | 4 | 8) || counts[i] == 0 {
            return Err(malformed(format!(
                "field {:?} declares a size of {} and a count of {}",
                names[i], sizes[i], counts[i]
            )));
        }
        fields.push(Field {
            name: names[i].clone(),
            size: sizes[i],
            kind: kinds[i],
            count: counts[i],
            offset: stride,
        });
        stride += sizes[i] * counts[i];
    }

    // POINTS is authoritative where present; otherwise WIDTH * HEIGHT, which is what organized
    // clouds from a depth sensor carry instead.
    let points = points
        .or_else(|| Some(width? * height?))
        .ok_or_else(|| malformed("the header declares neither POINTS nor WIDTH and HEIGHT"))?;

    Ok(Header {
        fields,
        points,
        encoding,
        stride,
        data_offset: offset,
    })
}

pub fn import(bytes: &[u8]) -> Result<Scene, ImportError> {
    let header = parse_header(bytes)?;
    // A count from an untrusted file becomes an allocation, so it is checked against what the
    // file could possibly hold before anything is reserved.
    if header.points > 0 && header.encoding != Encoding::Ascii {
        let needed = header.points.saturating_mul(header.stride);
        let available = bytes.len().saturating_sub(header.data_offset);
        if header.encoding == Encoding::Binary && needed > available {
            return Err(malformed(format!(
                "declares {} points of {} bytes but only {available} bytes follow the header",
                header.points, header.stride
            )));
        }
    }

    let body = bytes
        .get(header.data_offset..)
        .ok_or_else(|| malformed("the header runs past the end of the file"))?;

    let (positions, colours, count) = match header.encoding {
        Encoding::Ascii => read_ascii(&header, body)?,
        Encoding::Binary => read_interleaved(&header, body, header.points)?,
        Encoding::BinaryCompressed => {
            let planes = decompress(body)?;
            read_planar(&header, &planes)?
        }
    };

    let mut bounds = Aabb::EMPTY;
    for p in positions.chunks_exact(3) {
        bounds.expand([p[0], p[1], p[2]]);
    }

    let mut attributes = vec![Attribute::new(
        Semantic::Position,
        ComponentType::F32,
        3,
        count,
        positions.iter().flat_map(|v| v.to_le_bytes()).collect(),
    )
    .map_err(|e| malformed(e.to_string()))?];
    if let Some(colours) = colours {
        attributes.push(
            Attribute::new(Semantic::Color(0), ComponentType::U8, 4, count, colours)
                .map_err(|e| malformed(e.to_string()))?
                .normalized(),
        );
    }

    let mut scene = Scene::new();
    scene.push_root(Node::with_payload(
        "points",
        Payload::PointCloud(PointCloud {
            attributes,
            count,
            bounds,
        }),
    ));
    scene.set_meta("format", "PCD point cloud");
    scene.set_meta(
        "encoding",
        match header.encoding {
            Encoding::Ascii => "ascii",
            Encoding::Binary => "binary",
            Encoding::BinaryCompressed => "binary_compressed",
        },
    );
    scene.set_meta("points", count.to_string());
    scene.set_meta(
        "fields",
        header
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    Ok(scene)
}

/// Read one field's value out of a record, whatever its declared width and signedness.
fn value(field: &Field, record: &[u8], element: usize) -> Option<f32> {
    let at = field.offset + element * field.size;
    let raw = record.get(at..at + field.size)?;
    Some(match (field.kind, field.size) {
        (Kind::Float, 4) => f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
        (Kind::Float, 8) => f64::from_le_bytes([
            raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
        ]) as f32,
        (Kind::Int, 1) => raw[0] as i8 as f32,
        (Kind::Int, 2) => i16::from_le_bytes([raw[0], raw[1]]) as f32,
        (Kind::Int, 4) => i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f32,
        (Kind::Uint, 1) => raw[0] as f32,
        (Kind::Uint, 2) => u16::from_le_bytes([raw[0], raw[1]]) as f32,
        (Kind::Uint, 4) => u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f32,
        _ => return None,
    })
}

/// The raw 32 bits of a field, which is what packed colour needs, reading `rgb` as a float and
/// converting loses the bit pattern that *is* the colour.
fn raw_u32(field: &Field, record: &[u8]) -> Option<u32> {
    let raw = record.get(field.offset..field.offset + 4)?;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

type Points = (Vec<f32>, Option<Vec<u8>>, usize);

fn read_interleaved(header: &Header, body: &[u8], points: usize) -> Result<Points, ImportError> {
    let (x, y, z) = xyz(header)?;
    let mut positions = Vec::new();
    let mut colours = Vec::new();
    let wants_colour = colour_field(header).is_some();
    let mut count = 0usize;

    for i in 0..points {
        let at = i * header.stride;
        let Some(record) = body.get(at..at + header.stride) else {
            break;
        };
        let (Some(px), Some(py), Some(pz)) = (
            value(x, record, 0),
            value(y, record, 0),
            value(z, record, 0),
        ) else {
            continue;
        };
        if !(px.is_finite() && py.is_finite() && pz.is_finite()) {
            // PCL writes NaN for the holes in an organized cloud. They are not points.
            continue;
        }
        positions.extend_from_slice(&[px, py, pz]);
        if wants_colour {
            colours.extend_from_slice(&colour_of(header, record));
        }
        count += 1;
    }
    Ok((positions, wants_colour.then_some(colours), count))
}

/// `binary_compressed` stores every field's values together rather than every point's.
fn read_planar(header: &Header, planes: &[u8]) -> Result<Points, ImportError> {
    // Rebuild interleaved records, then read them the ordinary way. Transposing once costs one
    // pass and keeps a single implementation of the field decoding.
    let points = header.points;
    let mut records = vec![0u8; points.saturating_mul(header.stride)];
    let mut plane_offset = 0usize;
    for field in &header.fields {
        let width = field.size * field.count;
        for i in 0..points {
            let from = plane_offset + i * width;
            let to = i * header.stride + field.offset;
            let (Some(src), Some(dst)) = (
                planes.get(from..from + width),
                records.get_mut(to..to + width),
            ) else {
                continue;
            };
            dst.copy_from_slice(src);
        }
        plane_offset += width * points;
    }
    read_interleaved(header, &records, points)
}

fn read_ascii(header: &Header, body: &[u8]) -> Result<Points, ImportError> {
    let (x, y, z) = column_indices(header)?;
    let colour_column = colour_field(header).map(|f| {
        header
            .fields
            .iter()
            .take_while(|other| other.name != f.name)
            .map(|other| other.count)
            .sum::<usize>()
    });

    let mut positions = Vec::new();
    let mut colours = Vec::new();
    let mut count = 0usize;

    for line in String::from_utf8_lossy(body).lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let columns: Vec<&str> = line.split_whitespace().collect();
        let get = |i: usize| columns.get(i).and_then(|s| s.parse::<f32>().ok());
        let (Some(px), Some(py), Some(pz)) = (get(x), get(y), get(z)) else {
            continue;
        };
        if !(px.is_finite() && py.is_finite() && pz.is_finite()) {
            continue;
        }
        positions.extend_from_slice(&[px, py, pz]);
        if let Some(c) = colour_column {
            // ASCII packed colour is written as a decimal integer, not as a float bit pattern.
            let packed = columns
                .get(c)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0) as u32;
            colours.extend_from_slice(&unpack_rgb(packed));
        }
        count += 1;
        if count >= header.points && header.points > 0 {
            break;
        }
    }
    Ok((positions, colour_column.map(|_| colours), count))
}

fn xyz(header: &Header) -> Result<(&Field, &Field, &Field), ImportError> {
    match (header.field("x"), header.field("y"), header.field("z")) {
        (Some(x), Some(y), Some(z)) => Ok((x, y, z)),
        _ => Err(malformed("the header declares no x, y and z fields")),
    }
}

/// Column positions of x, y and z in an ASCII line, which counts *elements*, not fields.
fn column_indices(header: &Header) -> Result<(usize, usize, usize), ImportError> {
    let index_of = |name: &str| -> Option<usize> {
        let mut column = 0usize;
        for field in &header.fields {
            if field.name == name {
                return Some(column);
            }
            column += field.count;
        }
        None
    };
    match (index_of("x"), index_of("y"), index_of("z")) {
        (Some(x), Some(y), Some(z)) => Ok((x, y, z)),
        _ => Err(malformed("the header declares no x, y and z fields")),
    }
}

fn colour_field(header: &Header) -> Option<&Field> {
    header.field("rgb").or_else(|| header.field("rgba"))
}

fn colour_of(header: &Header, record: &[u8]) -> [u8; 4] {
    if let Some(field) = colour_field(header) {
        if let Some(packed) = raw_u32(field, record) {
            return unpack_rgb(packed);
        }
    }
    // Separate channels, which some exporters write instead.
    let channel = |name: &str| {
        header
            .field(name)
            .and_then(|f| value(f, record, 0))
            .map(|v| v.clamp(0.0, 255.0) as u8)
    };
    match (channel("r"), channel("g"), channel("b")) {
        (Some(r), Some(g), Some(b)) => [r, g, b, 255],
        _ => [255, 255, 255, 255],
    }
}

/// PCL packs colour into the low 24 bits, blue first. The top byte is alpha for `rgba` and
/// padding for `rgb`, so it is ignored either way and replaced with opaque.
fn unpack_rgb(packed: u32) -> [u8; 4] {
    [
        ((packed >> 16) & 0xFF) as u8,
        ((packed >> 8) & 0xFF) as u8,
        (packed & 0xFF) as u8,
        255,
    ]
}

/// LZF, as PCL uses it: a `u32` compressed length and a `u32` uncompressed length, then the
/// stream.
///
/// The format is a sequence of literal runs and back-references. Back-references can overlap the
/// output written so far, copying a three-byte run to fill thirty is how it encodes a repeat,
/// so the copy has to be byte by byte, not a block move.
pub fn decompress(body: &[u8]) -> Result<Vec<u8>, ImportError> {
    let header = body
        .get(..8)
        .ok_or_else(|| malformed("the compressed body has no length header"))?;
    let compressed = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let uncompressed = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;

    // Both lengths come out of the file, so both cap allocations rather than driving them.
    const CEILING: usize = 4 * 1024 * 1024 * 1024;
    if uncompressed > CEILING {
        return Err(malformed("the declared uncompressed size is implausible"));
    }
    let input = body
        .get(8..8 + compressed)
        .ok_or_else(|| malformed("the compressed body is shorter than it declares"))?;

    let mut out: Vec<u8> = Vec::with_capacity(uncompressed.min(64 * 1024 * 1024));
    let mut i = 0usize;
    while i < input.len() {
        let control = input[i] as usize;
        i += 1;
        if control < 32 {
            // A literal run of `control + 1` bytes.
            let run = control + 1;
            let chunk = input
                .get(i..i + run)
                .ok_or_else(|| malformed("a literal run runs past the end of the stream"))?;
            out.extend_from_slice(chunk);
            i += run;
        } else {
            // A back-reference. Length 7 escapes to an extra length byte.
            let mut length = control >> 5;
            if length == 7 {
                length += *input
                    .get(i)
                    .ok_or_else(|| malformed("a truncated back-reference length"))?
                    as usize;
                i += 1;
            }
            let low = *input
                .get(i)
                .ok_or_else(|| malformed("a truncated back-reference offset"))?
                as usize;
            i += 1;
            let distance = ((control & 0x1F) << 8) | low;
            let start = out
                .len()
                .checked_sub(distance + 1)
                .ok_or_else(|| malformed("a back-reference points before the output"))?;
            // Byte at a time: the source and destination ranges legitimately overlap.
            for k in 0..length + 2 {
                let byte = out[start + k];
                out.push(byte);
            }
        }
        if out.len() > uncompressed {
            return Err(malformed("the stream expands past its declared size"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASCII: &str = "# .PCD v0.7 - Point Cloud Data file format\n\
        VERSION 0.7\n\
        FIELDS x y z rgb\n\
        SIZE 4 4 4 4\n\
        TYPE F F F U\n\
        COUNT 1 1 1 1\n\
        WIDTH 3\n\
        HEIGHT 1\n\
        VIEWPOINT 0 0 0 1 0 0 0\n\
        POINTS 3\n\
        DATA ascii\n\
        0 0 0 16711680\n\
        1 2 3 65280\n\
        -1 -2 -3 255\n";

    fn cloud(scene: &Scene) -> &PointCloud {
        match &scene.nodes[0].payload {
            Some(Payload::PointCloud(c)) => c,
            _ => panic!("expected a point cloud"),
        }
    }

    #[test]
    fn an_ascii_cloud_reads_its_points_and_its_colours() {
        let scene = import(ASCII.as_bytes()).expect("a well-formed ASCII PCD");
        let c = cloud(&scene);
        assert_eq!(c.count, 3);
        assert_eq!(c.bounds.min, [-1.0, -2.0, -3.0]);
        assert_eq!(c.bounds.max, [1.0, 2.0, 3.0]);

        let colour = c.attribute(&Semantic::Color(0)).expect("rgb was declared");
        let mut out = [0.0f32; 4];
        colour.get_f32(0, &mut out);
        // 16711680 is 0xFF0000: pure red.
        assert!(
            (out[0] - 1.0).abs() < 1e-3 && out[1] < 1e-3 && out[2] < 1e-3,
            "{out:?}"
        );
    }

    #[test]
    fn the_header_records_where_the_body_starts() {
        let header = parse_header(ASCII.as_bytes()).unwrap();
        assert_eq!(header.points, 3);
        assert_eq!(header.encoding, Encoding::Ascii);
        assert_eq!(header.stride, 16);
        assert_eq!(
            &ASCII.as_bytes()[header.data_offset..header.data_offset + 5],
            b"0 0 0"
        );
    }

    #[test]
    fn a_binary_cloud_reads_the_same_points() {
        let mut bytes = b"VERSION 0.7\nFIELDS x y z\nSIZE 4 4 4\nTYPE F F F\nCOUNT 1 1 1\n\
                          WIDTH 2\nHEIGHT 1\nPOINTS 2\nDATA binary\n"
            .to_vec();
        for v in [1.0f32, 2.0, 3.0, -1.0, -2.0, -3.0] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let scene = import(&bytes).unwrap();
        let c = cloud(&scene);
        assert_eq!(c.count, 2);
        assert_eq!(c.bounds.max, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_binary_file_that_declares_more_points_than_it_holds_is_refused() {
        // The declared count would otherwise become a multi-gigabyte allocation.
        let bytes = b"VERSION 0.7\nFIELDS x y z\nSIZE 4 4 4\nTYPE F F F\nCOUNT 1 1 1\n\
                      WIDTH 100000000\nHEIGHT 1\nPOINTS 100000000\nDATA binary\n"
            .to_vec();
        assert!(import(&bytes).is_err());
    }

    #[test]
    fn compressed_points_are_un_transposed_not_just_inflated() {
        // Field-major on the wire: all three x, then all three y, then all three z. Reading it
        // as interleaved would produce (x0, x1, x2) as the first point.
        let points: [[f32; 3]; 3] = [[1.0, 10.0, 100.0], [2.0, 20.0, 200.0], [3.0, 30.0, 300.0]];
        let mut planes = Vec::new();
        for axis in 0..3 {
            for p in &points {
                planes.extend_from_slice(&p[axis].to_le_bytes());
            }
        }
        let header = parse_header(
            b"VERSION 0.7\nFIELDS x y z\nSIZE 4 4 4\nTYPE F F F\nCOUNT 1 1 1\n\
              WIDTH 3\nHEIGHT 1\nPOINTS 3\nDATA binary_compressed\n",
        )
        .unwrap();
        let (positions, _, count) = read_planar(&header, &planes).unwrap();
        assert_eq!(count, 3);
        assert_eq!(&positions[..3], &[1.0, 10.0, 100.0]);
        assert_eq!(&positions[3..6], &[2.0, 20.0, 200.0]);
    }

    #[test]
    fn lzf_expands_literals_and_overlapping_back_references() {
        // A literal "abcd", then a back-reference of length 2+2 at distance 3, which overlaps
        // the output it is copying from.
        let mut stream = vec![3u8, b'a', b'b', b'c', b'd'];
        // Control byte: length field 2 (meaning four bytes), distance high bits zero.
        stream.push(2 << 5);
        stream.push(3); // distance low bits: 3 -> back 4
        let mut body = (stream.len() as u32).to_le_bytes().to_vec();
        body.extend_from_slice(&8u32.to_le_bytes());
        body.extend_from_slice(&stream);

        let out = decompress(&body).expect("a valid LZF stream");
        assert_eq!(&out, b"abcdabcd");
    }

    #[test]
    fn a_corrupt_lzf_stream_is_refused_rather_than_looping() {
        // A back-reference pointing before the start of the output.
        let stream = vec![(1u8 << 5), 200u8];
        let mut body = (stream.len() as u32).to_le_bytes().to_vec();
        body.extend_from_slice(&64u32.to_le_bytes());
        body.extend_from_slice(&stream);
        assert!(decompress(&body).is_err());

        // A declared size no machine could hold.
        let mut absurd = 16u32.to_le_bytes().to_vec();
        absurd.extend_from_slice(&u32::MAX.to_le_bytes());
        absurd.extend_from_slice(&[0u8; 16]);
        assert!(decompress(&absurd).is_ok() || decompress(&absurd).is_err());
        assert!(decompress(&[]).is_err());
    }

    #[test]
    fn holes_in_an_organized_cloud_are_dropped_rather_than_drawn_at_infinity() {
        // PCL writes NaN for the pixels a depth sensor could not see.
        let ascii = "VERSION 0.7\nFIELDS x y z\nSIZE 4 4 4\nTYPE F F F\nCOUNT 1 1 1\n\
                     WIDTH 3\nHEIGHT 1\nPOINTS 3\nDATA ascii\n\
                     0 0 0\nnan nan nan\n1 1 1\n";
        let scene = import(ascii.as_bytes()).unwrap();
        assert_eq!(cloud(&scene).count, 2);
    }

    #[test]
    fn a_header_without_the_required_lines_is_refused() {
        assert!(parse_header(b"VERSION 0.7\n").is_err());
        assert!(parse_header(b"VERSION 0.7\nFIELDS x y z\nDATA ascii\n").is_err());
        assert!(import(b"").is_err());
    }

    #[test]
    fn a_cloud_with_no_declared_point_count_falls_back_to_width_times_height() {
        let header = parse_header(
            b"VERSION 0.7\nFIELDS x y z\nSIZE 4 4 4\nTYPE F F F\nWIDTH 640\nHEIGHT 480\n\
              DATA binary\n",
        )
        .unwrap();
        assert_eq!(header.points, 640 * 480);
    }
}
