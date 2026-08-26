//! PLY importer: mesh, point cloud, 3DGS, and SuperSplat-compressed 3DGS.
//!
//! One parser covers four of the sniffer's verdicts. The header declares elements and properties;
//! which of the four a file is depends entirely on what those properties are named.
//!
//! The 3DGS path applies four transforms that the INRIA reference format requires and that no
//! amount of "keep source precision" can avoid, because the stored values are not the rendered
//! values:
//!
//! | Stored | Rendered |
//! |---|---|
//! | `scale_0..2`, natural log | `exp(v)` |
//! | `opacity`, logit | `sigmoid(v)` |
//! | `rot_0..3`, quaternion in w,x,y,z order | normalized, reordered to x,y,z,w |
//! | `f_rest_0..44`, grouped by channel | interleaved per coefficient |
//!
//! The `f_rest` layout is the one that silently produces wrong colour if missed. The reference
//! implementation stores `(N, 15, 3)` transposed to `(N, 3, 15)` before flattening, so indices
//! 0..14 are red, 15..29 green, 30..44 blue. Coefficient `c` therefore reads
//! `(f_rest[c], f_rest[c + 15], f_rest[c + 30])`.

use viewer_io::{FormatId, PlyVariant};
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, PointCloud, SplatCloud, TriMesh};
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

/// Refuse counts that would allocate absurd amounts before a single byte is read. The number is
/// deliberately generous: SPZ 4 targets 34 M splats and LiDAR tiles run to 200 M points.
const MAX_ELEMENTS: u64 = 500_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Ascii,
    BinaryLe,
    BinaryBe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scalar {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Scalar {
    fn parse(s: &str) -> Option<Scalar> {
        Some(match s {
            "char" | "int8" => Scalar::I8,
            "uchar" | "uint8" => Scalar::U8,
            "short" | "int16" => Scalar::I16,
            "ushort" | "uint16" => Scalar::U16,
            "int" | "int32" => Scalar::I32,
            "uint" | "uint32" => Scalar::U32,
            "float" | "float32" => Scalar::F32,
            "double" | "float64" => Scalar::F64,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Scalar::I8 | Scalar::U8 => 1,
            Scalar::I16 | Scalar::U16 => 2,
            Scalar::I32 | Scalar::U32 | Scalar::F32 => 4,
            Scalar::F64 => 8,
        }
    }
}

#[derive(Debug, Clone)]
enum Property {
    Scalar {
        name: String,
        ty: Scalar,
    },
    List {
        name: String,
        count: Scalar,
        item: Scalar,
    },
}

impl Property {
    fn name(&self) -> &str {
        match self {
            Property::Scalar { name, .. } | Property::List { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone)]
struct Element {
    name: String,
    count: u64,
    props: Vec<Property>,
}

impl Element {
    fn index_of(&self, name: &str) -> Option<usize> {
        self.props.iter().position(|p| p.name() == name)
    }

    fn has(&self, name: &str) -> bool {
        self.index_of(name).is_some()
    }

    /// Byte width of one record. `None` when any property is a list, whose width varies per record.
    fn fixed_stride(&self) -> Option<usize> {
        let mut n = 0;
        for p in &self.props {
            match p {
                Property::Scalar { ty, .. } => n += ty.size(),
                Property::List { .. } => return None,
            }
        }
        Some(n)
    }
}

struct Header {
    encoding: Encoding,
    elements: Vec<Element>,
    body: usize,
}

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Ply(PlyVariant::Mesh),
        reason: reason.into(),
    }
}

fn parse_header(bytes: &[u8]) -> Result<Header, ImportError> {
    let scan = &bytes[..bytes.len().min(1 << 20)];
    let end = find(scan, b"end_header").ok_or_else(|| malformed("end_header not found"))?;

    // The body starts after the newline that terminates the end_header line.
    let mut body = end + b"end_header".len();
    while body < bytes.len() && bytes[body] != b'\n' {
        body += 1;
    }
    body += 1;

    let text =
        std::str::from_utf8(&scan[..end]).map_err(|_| malformed("header is not valid UTF-8"))?;

    let mut encoding = None;
    let mut elements: Vec<Element> = Vec::new();

    for line in text.lines() {
        let mut t = line.split_whitespace();
        match t.next() {
            Some("format") => {
                encoding = Some(match t.next() {
                    Some("ascii") => Encoding::Ascii,
                    Some("binary_little_endian") => Encoding::BinaryLe,
                    Some("binary_big_endian") => Encoding::BinaryBe,
                    other => return Err(malformed(format!("unsupported format {other:?}"))),
                });
            }
            Some("element") => {
                let name = t
                    .next()
                    .ok_or_else(|| malformed("element without a name"))?;
                let count: u64 = t
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| malformed("element without a count"))?;
                if count > MAX_ELEMENTS {
                    return Err(malformed(format!(
                        "element {name} declares {count} records"
                    )));
                }
                elements.push(Element {
                    name: name.to_string(),
                    count,
                    props: Vec::new(),
                });
            }
            Some("property") => {
                let el = elements
                    .last_mut()
                    .ok_or_else(|| malformed("property outside any element"))?;
                let kind = t
                    .next()
                    .ok_or_else(|| malformed("property without a type"))?;
                if kind == "list" {
                    let count = t.next().and_then(Scalar::parse);
                    let item = t.next().and_then(Scalar::parse);
                    let name = t.next();
                    match (count, item, name) {
                        (Some(count), Some(item), Some(name)) => el.props.push(Property::List {
                            name: name.to_string(),
                            count,
                            item,
                        }),
                        _ => return Err(malformed(format!("malformed list property: {line}"))),
                    }
                } else {
                    let ty = Scalar::parse(kind)
                        .ok_or_else(|| malformed(format!("unknown property type {kind}")))?;
                    let name = t
                        .next()
                        .ok_or_else(|| malformed("property without a name"))?;
                    el.props.push(Property::Scalar {
                        name: name.to_string(),
                        ty,
                    });
                }
            }
            _ => {}
        }
    }

    let encoding = encoding.ok_or_else(|| malformed("no format line"))?;
    if elements.is_empty() {
        return Err(malformed("no elements declared"));
    }
    Ok(Header {
        encoding,
        elements,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// One element's records, decoded to `f32` in property order.
///
/// Reading everything as `f32` costs memory on huge clouds, and a later revision should stream
/// per-attribute instead. It is correct and simple now, which matters more while the shape of the
/// renderer is still settling.
struct Records {
    values: Vec<f32>,
    stride: usize,
    count: usize,
}

impl Records {
    fn get(&self, record: usize, prop: usize) -> f32 {
        self.values[record * self.stride + prop]
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ImportError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| malformed("offset overflow"))?;
        let slice = self
            .data
            .get(self.at..end)
            .ok_or_else(|| malformed("file ends inside an element"))?;
        self.at = end;
        Ok(slice)
    }

    fn scalar(&mut self, ty: Scalar, big_endian: bool) -> Result<f64, ImportError> {
        let b = self.take(ty.size())?;
        let v = match (ty, big_endian) {
            (Scalar::I8, _) => b[0] as i8 as f64,
            (Scalar::U8, _) => b[0] as f64,
            (Scalar::I16, false) => i16::from_le_bytes([b[0], b[1]]) as f64,
            (Scalar::I16, true) => i16::from_be_bytes([b[0], b[1]]) as f64,
            (Scalar::U16, false) => u16::from_le_bytes([b[0], b[1]]) as f64,
            (Scalar::U16, true) => u16::from_be_bytes([b[0], b[1]]) as f64,
            (Scalar::I32, false) => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            (Scalar::I32, true) => i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
            (Scalar::U32, false) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            (Scalar::U32, true) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
            (Scalar::F32, false) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            (Scalar::F32, true) => f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
            (Scalar::F64, false) => {
                f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
            }
            (Scalar::F64, true) => {
                f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
            }
        };
        Ok(v)
    }
}

/// Read every element in order, returning the ones named in `wanted` plus any face indices.
struct Body {
    records: Vec<(String, Records)>,
    indices: Vec<u32>,
}

fn read_body(bytes: &[u8], header: &Header) -> Result<Body, ImportError> {
    match header.encoding {
        Encoding::Ascii => read_ascii(bytes, header),
        Encoding::BinaryLe => read_binary(bytes, header, false),
        Encoding::BinaryBe => read_binary(bytes, header, true),
    }
}

fn read_binary(bytes: &[u8], header: &Header, be: bool) -> Result<Body, ImportError> {
    let mut cur = Cursor {
        data: bytes,
        at: header.body,
    };
    let mut records = Vec::new();
    let mut indices = Vec::new();

    for el in &header.elements {
        let count = el.count as usize;
        let scalar_props = el.props.len();

        // Fast path: no list properties, so the record layout is fixed and can be bounds-checked
        // once for the whole element rather than per field.
        if let Some(stride) = el.fixed_stride() {
            let need = stride
                .checked_mul(count)
                .ok_or_else(|| malformed("element size overflow"))?;
            if cur.at + need > bytes.len() {
                return Err(malformed(format!(
                    "element {} needs {need} bytes, {} remain",
                    el.name,
                    bytes.len().saturating_sub(cur.at)
                )));
            }
        }

        if el.name == "face" {
            for _ in 0..count {
                for p in &el.props {
                    match p {
                        Property::List { count: c, item, .. } => {
                            let n = cur.scalar(*c, be)? as usize;
                            if n > 255 {
                                return Err(malformed("face with more than 255 vertices"));
                            }
                            let mut poly = Vec::with_capacity(n);
                            for _ in 0..n {
                                poly.push(cur.scalar(*item, be)? as u32);
                            }
                            triangulate(&poly, &mut indices);
                        }
                        Property::Scalar { ty, .. } => {
                            cur.scalar(*ty, be)?;
                        }
                    }
                }
            }
            continue;
        }

        let mut values = Vec::with_capacity(count * scalar_props);
        for _ in 0..count {
            for p in &el.props {
                match p {
                    Property::Scalar { ty, .. } => values.push(cur.scalar(*ty, be)? as f32),
                    Property::List { count: c, item, .. } => {
                        let n = cur.scalar(*c, be)? as usize;
                        for _ in 0..n {
                            cur.scalar(*item, be)?;
                        }
                        values.push(0.0);
                    }
                }
            }
        }
        records.push((
            el.name.clone(),
            Records {
                values,
                stride: scalar_props,
                count,
            },
        ));
    }
    Ok(Body { records, indices })
}

fn read_ascii(bytes: &[u8], header: &Header) -> Result<Body, ImportError> {
    let text = std::str::from_utf8(&bytes[header.body..])
        .map_err(|_| malformed("ASCII body is not valid UTF-8"))?;
    let mut tokens = text.split_ascii_whitespace();
    let mut records = Vec::new();
    let mut indices = Vec::new();

    let mut next = |what: &str| -> Result<f64, ImportError> {
        tokens
            .next()
            .ok_or_else(|| malformed(format!("file ends while reading {what}")))?
            .parse::<f64>()
            .map_err(|_| malformed(format!("non-numeric value in {what}")))
    };

    for el in &header.elements {
        let count = el.count as usize;
        if el.name == "face" {
            for _ in 0..count {
                for p in &el.props {
                    match p {
                        Property::List { .. } => {
                            let n = next("face")? as usize;
                            if n > 255 {
                                return Err(malformed("face with more than 255 vertices"));
                            }
                            let mut poly = Vec::with_capacity(n);
                            for _ in 0..n {
                                poly.push(next("face")? as u32);
                            }
                            triangulate(&poly, &mut indices);
                        }
                        Property::Scalar { .. } => {
                            next("face")?;
                        }
                    }
                }
            }
            continue;
        }

        let mut values = Vec::with_capacity(count * el.props.len());
        for _ in 0..count {
            for p in &el.props {
                match p {
                    Property::Scalar { .. } => values.push(next(&el.name)? as f32),
                    Property::List { .. } => {
                        let n = next(&el.name)? as usize;
                        for _ in 0..n {
                            next(&el.name)?;
                        }
                        values.push(0.0);
                    }
                }
            }
        }
        records.push((
            el.name.clone(),
            Records {
                values,
                stride: el.props.len(),
                count,
            },
        ));
    }
    Ok(Body { records, indices })
}

/// Fan triangulation. Correct for convex polygons, which is what PLY files contain in practice.
fn triangulate(poly: &[u32], out: &mut Vec<u32>) {
    if poly.len() < 3 {
        return;
    }
    for i in 1..poly.len() - 1 {
        out.push(poly[0]);
        out.push(poly[i]);
        out.push(poly[i + 1]);
    }
}

pub fn import(bytes: &[u8], variant: PlyVariant) -> Result<Scene, ImportError> {
    let header = parse_header(bytes)?;
    let body = read_body(bytes, &header)?;

    let vertex_el = header
        .elements
        .iter()
        .find(|e| e.name == "vertex")
        .ok_or_else(|| malformed("no vertex element"))?;
    let vertex = body
        .records
        .iter()
        .find(|(n, _)| n == "vertex")
        .map(|(_, r)| r)
        .ok_or_else(|| malformed("vertex element produced no records"))?;

    let mut scene = Scene::new();
    scene.set_meta("format", "PLY");
    scene.set_meta(
        "encoding",
        match header.encoding {
            Encoding::Ascii => "ascii",
            Encoding::BinaryLe => "binary_little_endian",
            Encoding::BinaryBe => "binary_big_endian",
        },
    );

    let payload = match variant {
        PlyVariant::GaussianSplat => splats(vertex_el, vertex)?,
        PlyVariant::CompressedGaussianSplat => {
            let chunk_el = header
                .elements
                .iter()
                .find(|e| e.name == "chunk")
                .ok_or_else(|| malformed("compressed PLY without a chunk element"))?;
            let chunk = body
                .records
                .iter()
                .find(|(n, _)| n == "chunk")
                .map(|(_, r)| r)
                .ok_or_else(|| malformed("chunk element produced no records"))?;
            compressed_splats(chunk_el, chunk, vertex_el, vertex)?
        }
        PlyVariant::Mesh => mesh(vertex_el, vertex, body.indices)?,
        PlyVariant::PointCloud => points(vertex_el, vertex)?,
    };

    scene.push_root(Node::with_payload("ply", payload));
    Ok(scene)
}

fn column(el: &Element, rec: &Records, name: &str) -> Option<Vec<f32>> {
    let idx = el.index_of(name)?;
    Some((0..rec.count).map(|i| rec.get(i, idx)).collect())
}

fn positions_of(el: &Element, rec: &Records) -> Result<(Vec<f32>, Aabb), ImportError> {
    let (xi, yi, zi) = match (el.index_of("x"), el.index_of("y"), el.index_of("z")) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => return Err(malformed("vertex element has no x/y/z")),
    };
    let mut out = Vec::with_capacity(rec.count * 3);
    let mut bounds = Aabb::EMPTY;
    for i in 0..rec.count {
        let p = [rec.get(i, xi), rec.get(i, yi), rec.get(i, zi)];
        bounds.expand(p);
        out.extend_from_slice(&p);
    }
    Ok((out, bounds))
}

fn f32_attr(semantic: Semantic, components: u8, data: Vec<f32>) -> Result<Attribute, ImportError> {
    let count = data.len() / components as usize;
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for v in &data {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    Attribute::new(semantic, ComponentType::F32, components, count, bytes)
        .map_err(|e| malformed(e.to_string()))
}

fn mesh(el: &Element, rec: &Records, indices: Vec<u32>) -> Result<Payload, ImportError> {
    let (pos, bounds) = positions_of(el, rec)?;
    let vertex_count = rec.count;
    let mut attributes = vec![f32_attr(Semantic::Position, 3, pos)?];

    if let (Some(nx), Some(ny), Some(nz)) = (
        column(el, rec, "nx"),
        column(el, rec, "ny"),
        column(el, rec, "nz"),
    ) {
        attributes.push(f32_attr(Semantic::Normal, 3, interleave3(&nx, &ny, &nz))?);
    }
    if let Some(colors) = vertex_colors(el, rec) {
        attributes.push(f32_attr(Semantic::Color(0), 4, colors)?);
    }
    if let (Some(u), Some(v)) = (uv_column(el, rec, 0), uv_column(el, rec, 1)) {
        attributes.push(f32_attr(Semantic::TexCoord(0), 2, interleave2(&u, &v))?);
    }

    for &i in &indices {
        if i as usize >= vertex_count {
            return Err(malformed(format!(
                "face index {i} exceeds the {vertex_count} declared vertices"
            )));
        }
    }
    let indices = if indices.is_empty() {
        Indices::None
    } else if vertex_count <= u16::MAX as usize {
        Indices::U16(indices.into_iter().map(|i| i as u16).collect())
    } else {
        Indices::U32(indices)
    };

    Ok(Payload::TriMesh(TriMesh {
        attributes,
        indices,
        bounds,
        material: None,
    }))
}

fn points(el: &Element, rec: &Records) -> Result<Payload, ImportError> {
    let (pos, bounds) = positions_of(el, rec)?;
    let mut attributes = vec![f32_attr(Semantic::Position, 3, pos)?];
    if let Some(colors) = vertex_colors(el, rec) {
        attributes.push(f32_attr(Semantic::Color(0), 4, colors)?);
    }
    if let (Some(nx), Some(ny), Some(nz)) = (
        column(el, rec, "nx"),
        column(el, rec, "ny"),
        column(el, rec, "nz"),
    ) {
        attributes.push(f32_attr(Semantic::Normal, 3, interleave3(&nx, &ny, &nz))?);
    }
    Ok(Payload::PointCloud(PointCloud {
        attributes,
        count: rec.count,
        bounds,
    }))
}

fn vertex_colors(el: &Element, rec: &Records) -> Option<Vec<f32>> {
    let names = [
        ("red", "green", "blue", "alpha"),
        ("r", "g", "b", "a"),
        (
            "diffuse_red",
            "diffuse_green",
            "diffuse_blue",
            "diffuse_alpha",
        ),
    ];
    for (rn, gn, bn, an) in names {
        if let (Some(r), Some(g), Some(b)) = (
            column(el, rec, rn),
            column(el, rec, gn),
            column(el, rec, bn),
        ) {
            // uchar colours arrive as 0..255 and floats as 0..1.
            let scale = if matches!(
                el.props[el.index_of(rn).unwrap()],
                Property::Scalar { ty: Scalar::U8, .. }
            ) {
                1.0 / 255.0
            } else {
                1.0
            };
            let a = column(el, rec, an);
            let mut out = Vec::with_capacity(r.len() * 4);
            for i in 0..r.len() {
                out.push(r[i] * scale);
                out.push(g[i] * scale);
                out.push(b[i] * scale);
                out.push(a.as_ref().map_or(1.0, |a| a[i] * scale));
            }
            return Some(out);
        }
    }
    None
}

fn uv_column(el: &Element, rec: &Records, axis: usize) -> Option<Vec<f32>> {
    let names: [&str; 3] = if axis == 0 {
        ["u", "s", "texture_u"]
    } else {
        ["v", "t", "texture_v"]
    };
    names.iter().find_map(|n| column(el, rec, n))
}

fn interleave2(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter().zip(b).flat_map(|(x, y)| [*x, *y]).collect()
}

fn interleave3(a: &[f32], b: &[f32], c: &[f32]) -> Vec<f32> {
    a.iter()
        .zip(b)
        .zip(c)
        .flat_map(|((x, y), z)| [*x, *y, *z])
        .collect()
}

fn sigmoid(v: f32) -> f32 {
    1.0 / (1.0 + (-v).exp())
}

fn splats(el: &Element, rec: &Records) -> Result<Payload, ImportError> {
    let (pos, bounds) = positions_of(el, rec)?;
    let count = rec.count;

    let scale_idx = [
        el.index_of("scale_0"),
        el.index_of("scale_1"),
        el.index_of("scale_2"),
    ];
    let rot_idx = [
        el.index_of("rot_0"),
        el.index_of("rot_1"),
        el.index_of("rot_2"),
        el.index_of("rot_3"),
    ];
    if scale_idx.iter().any(Option::is_none) || rot_idx.iter().any(Option::is_none) {
        return Err(malformed("3DGS vertex element missing scale_* or rot_*"));
    }
    let opacity_idx = el
        .index_of("opacity")
        .ok_or_else(|| malformed("3DGS vertex element missing opacity"))?;

    let mut scale = Vec::with_capacity(count * 3);
    let mut rotation = Vec::with_capacity(count * 4);
    let mut opacity = Vec::with_capacity(count);

    for i in 0..count {
        for s in scale_idx.iter().flatten() {
            scale.push(rec.get(i, *s).exp());
        }
        // Stored w,x,y,z. glTF quaternions are x,y,z,w.
        let w = rec.get(i, rot_idx[0].unwrap());
        let x = rec.get(i, rot_idx[1].unwrap());
        let y = rec.get(i, rot_idx[2].unwrap());
        let z = rec.get(i, rot_idx[3].unwrap());
        let n = (w * w + x * x + y * y + z * z).sqrt();
        let n = if n > 0.0 { n } else { 1.0 };
        rotation.extend_from_slice(&[x / n, y / n, z / n, w / n]);

        opacity.push(sigmoid(rec.get(i, opacity_idx)));
    }

    let mut attributes = vec![
        f32_attr(Semantic::Position, 3, pos)?,
        f32_attr(Semantic::SplatScale, 3, scale)?,
        f32_attr(Semantic::SplatRotation, 4, rotation)?,
        f32_attr(Semantic::SplatOpacity, 1, opacity)?,
    ];

    // Degree 0: the DC term, stored directly as f_dc_0..2.
    if let (Some(r), Some(g), Some(b)) = (
        column(el, rec, "f_dc_0"),
        column(el, rec, "f_dc_1"),
        column(el, rec, "f_dc_2"),
    ) {
        attributes.push(f32_attr(
            Semantic::SplatSh {
                degree: 0,
                index: 0,
            },
            3,
            interleave3(&r, &g, &b),
        )?);
    }

    // Higher degrees. f_rest is grouped by channel, not by coefficient.
    let rest: Vec<usize> = (0..45)
        .filter_map(|i| el.index_of(&format!("f_rest_{i}")))
        .collect();
    let per_channel = rest.len() / 3;
    let sh_degree = match per_channel {
        0 => 0u8,
        3 => 1,
        8 => 2,
        15 => 3,
        n => {
            return Err(malformed(format!(
                "f_rest declares {n} coefficients per channel, expected 0, 3, 8 or 15"
            )))
        }
    };

    if sh_degree > 0 {
        for degree in 1..=sh_degree {
            let first = SplatCloud::sh_coef_count(degree - 1);
            let last = SplatCloud::sh_coef_count(degree);
            for c in first..last {
                let mut triple = Vec::with_capacity(count * 3);
                for i in 0..count {
                    triple.push(rec.get(i, rest[c]));
                    triple.push(rec.get(i, rest[c + per_channel]));
                    triple.push(rec.get(i, rest[c + per_channel * 2]));
                }
                attributes.push(f32_attr(
                    Semantic::SplatSh {
                        degree,
                        index: (c - first) as u8,
                    },
                    3,
                    triple,
                )?);
            }
        }
    }

    Ok(Payload::SplatCloud(SplatCloud {
        attributes,
        count,
        bounds,
        sh_degree,
        antialiased: false,
        orbit_hint: None,
    }))
}

/// SuperSplat / PlayCanvas compressed PLY.
///
/// Each chunk covers 256 vertices and carries the dequantization ranges for that group. Vertex
/// records are four packed `uint32` fields:
///
/// - `packed_position`: 11, 10, 11 bits, normalized into the chunk position range.
/// - `packed_rotation`: 2 bits naming the omitted largest component, then 3 x 10 bits scaled by
///   `sqrt(2)` about zero. The omitted component is reconstructed from the unit-length constraint.
/// - `packed_scale`: 11, 10, 11 bits, normalized into the chunk scale range, then exponentiated.
/// - `packed_color`: four 8-bit channels, RGB plus opacity.
const CHUNK_SIZE: usize = 256;

fn compressed_splats(
    chunk_el: &Element,
    chunk: &Records,
    el: &Element,
    rec: &Records,
) -> Result<Payload, ImportError> {
    for name in [
        "packed_position",
        "packed_rotation",
        "packed_scale",
        "packed_color",
    ] {
        if !el.has(name) {
            return Err(malformed(format!("compressed PLY missing {name}")));
        }
    }
    let ranges = [
        "min_x",
        "min_y",
        "min_z",
        "max_x",
        "max_y",
        "max_z", //
        "min_scale_x",
        "min_scale_y",
        "min_scale_z", //
        "max_scale_x",
        "max_scale_y",
        "max_scale_z",
    ];
    let mut idx = Vec::with_capacity(ranges.len());
    for name in ranges {
        idx.push(
            chunk_el
                .index_of(name)
                .ok_or_else(|| malformed(format!("chunk element missing {name}")))?,
        );
    }

    let pos_i = el.index_of("packed_position").unwrap();
    let rot_i = el.index_of("packed_rotation").unwrap();
    let scl_i = el.index_of("packed_scale").unwrap();
    let col_i = el.index_of("packed_color").unwrap();

    let count = rec.count;
    let needed_chunks = count.div_ceil(CHUNK_SIZE);
    if chunk.count < needed_chunks {
        return Err(malformed(format!(
            "{count} vertices need {needed_chunks} chunks, {} present",
            chunk.count
        )));
    }

    let mut pos = Vec::with_capacity(count * 3);
    let mut scale = Vec::with_capacity(count * 3);
    let mut rotation = Vec::with_capacity(count * 4);
    let mut opacity = Vec::with_capacity(count);
    let mut sh0 = Vec::with_capacity(count * 3);
    let mut bounds = Aabb::EMPTY;

    // f_dc is recovered from stored colour by inverting rgb = 0.5 + C0 * sh0.
    const C0: f32 = 0.282_094_79;

    for i in 0..count {
        let c = i / CHUNK_SIZE;
        let r = |k: usize| chunk.get(c, idx[k]);

        let p = rec.get(i, pos_i) as u32;
        let px = lerp(r(0), r(3), unpack(p, 21, 11));
        let py = lerp(r(1), r(4), unpack(p, 11, 10));
        let pz = lerp(r(2), r(5), unpack(p, 0, 11));
        bounds.expand([px, py, pz]);
        pos.extend_from_slice(&[px, py, pz]);

        let s = rec.get(i, scl_i) as u32;
        scale.push(lerp(r(6), r(9), unpack(s, 21, 11)).exp());
        scale.push(lerp(r(7), r(10), unpack(s, 11, 10)).exp());
        scale.push(lerp(r(8), r(11), unpack(s, 0, 11)).exp());

        rotation.extend_from_slice(&unpack_quat(rec.get(i, rot_i) as u32));

        let col = rec.get(i, col_i) as u32;
        let ch = |shift: u32| ((col >> shift) & 0xFF) as f32 / 255.0;
        sh0.push((ch(24) - 0.5) / C0);
        sh0.push((ch(16) - 0.5) / C0);
        sh0.push((ch(8) - 0.5) / C0);
        opacity.push(ch(0));
    }

    let attributes = vec![
        f32_attr(Semantic::Position, 3, pos)?,
        f32_attr(Semantic::SplatScale, 3, scale)?,
        f32_attr(Semantic::SplatRotation, 4, rotation)?,
        f32_attr(Semantic::SplatOpacity, 1, opacity)?,
        f32_attr(
            Semantic::SplatSh {
                degree: 0,
                index: 0,
            },
            3,
            sh0,
        )?,
    ];

    Ok(Payload::SplatCloud(SplatCloud {
        attributes,
        count,
        bounds,
        sh_degree: 0,
        antialiased: false,
        orbit_hint: None,
    }))
}

fn unpack(v: u32, shift: u32, bits: u32) -> f32 {
    let mask = (1u32 << bits) - 1;
    ((v >> shift) & mask) as f32 / mask as f32
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Smallest-three quaternion decode, returning x,y,z,w.
fn unpack_quat(v: u32) -> [f32; 4] {
    const NORM: f32 = std::f32::consts::SQRT_2;
    let largest = (v >> 30) & 0x3;
    let a = (unpack(v, 20, 10) - 0.5) * NORM;
    let b = (unpack(v, 10, 10) - 0.5) * NORM;
    let c = (unpack(v, 0, 10) - 0.5) * NORM;
    let d = (1.0 - a * a - b * b - c * c).max(0.0).sqrt();
    // Components are stored in ascending index order, skipping the largest.
    let q = match largest {
        0 => [d, a, b, c],
        1 => [a, d, b, c],
        2 => [a, b, d, c],
        _ => [a, b, c, d],
    };
    // A corrupt word can encode three components whose squares already exceed 1, which drives the
    // reconstructed fourth to zero and leaves a non-unit quaternion. Normalize rather than trusting
    // the file, since the rasterizer builds a rotation matrix from this directly.
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if n <= f32::EPSILON {
        return [0.0, 0.0, 0.0, 1.0];
    }
    // q is w,x,y,z; return x,y,z,w.
    [q[1] / n, q[2] / n, q[3] / n, q[0] / n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use viewer_scene::payload::Payload;

    fn ascii_mesh() -> &'static [u8] {
        b"ply\nformat ascii 1.0\nelement vertex 4\n\
          property float x\nproperty float y\nproperty float z\n\
          element face 2\nproperty list uchar int vertex_indices\nend_header\n\
          0 0 0\n1 0 0\n1 1 0\n0 1 0\n3 0 1 2\n3 0 2 3\n"
    }

    #[test]
    fn ascii_mesh_parses_with_correct_topology() {
        let scene = import(ascii_mesh(), PlyVariant::Mesh).unwrap();
        let s = scene.summary();
        assert_eq!(s.vertices, 4);
        assert_eq!(s.triangles, 2);
        assert_eq!(scene.bounds().max, [1.0, 1.0, 0.0]);
    }

    #[test]
    fn quad_faces_are_fan_triangulated() {
        let src = b"ply\nformat ascii 1.0\nelement vertex 4\n\
                    property float x\nproperty float y\nproperty float z\n\
                    element face 1\nproperty list uchar int vertex_indices\nend_header\n\
                    0 0 0\n1 0 0\n1 1 0\n0 1 0\n4 0 1 2 3\n";
        let scene = import(src, PlyVariant::Mesh).unwrap();
        assert_eq!(scene.summary().triangles, 2);
    }

    #[test]
    fn out_of_range_face_index_is_rejected() {
        let src = b"ply\nformat ascii 1.0\nelement vertex 3\n\
                    property float x\nproperty float y\nproperty float z\n\
                    element face 1\nproperty list uchar int vertex_indices\nend_header\n\
                    0 0 0\n1 0 0\n1 1 0\n3 0 1 99\n";
        assert!(import(src, PlyVariant::Mesh).is_err());
    }

    #[test]
    fn point_cloud_reads_uchar_colours_into_unit_range() {
        let src = b"ply\nformat ascii 1.0\nelement vertex 2\n\
                    property float x\nproperty float y\nproperty float z\n\
                    property uchar red\nproperty uchar green\nproperty uchar blue\nend_header\n\
                    0 0 0 255 0 0\n1 1 1 0 255 0\n";
        let scene = import(src, PlyVariant::PointCloud).unwrap();
        assert_eq!(scene.summary().points, 2);
        let Payload::PointCloud(pc) = scene.nodes[0].payload.as_ref().unwrap() else {
            panic!("expected a point cloud");
        };
        let c = pc.attribute(&Semantic::Color(0)).unwrap();
        let mut out = [0.0f32; 4];
        c.get_f32(0, &mut out);
        assert_eq!(out, [1.0, 0.0, 0.0, 1.0]);
    }

    fn splat_ply(extra_props: &str, extra_values: &str) -> Vec<u8> {
        format!(
            "ply\nformat ascii 1.0\nelement vertex 1\n\
             property float x\nproperty float y\nproperty float z\n\
             property float f_dc_0\nproperty float f_dc_1\nproperty float f_dc_2\n\
             property float opacity\n\
             property float scale_0\nproperty float scale_1\nproperty float scale_2\n\
             property float rot_0\nproperty float rot_1\nproperty float rot_2\nproperty float rot_3\n\
             {extra_props}end_header\n\
             1 2 3 0.5 0.5 0.5 0 0 0 0 1 0 0 0 {extra_values}\n"
        )
        .into_bytes()
    }

    #[test]
    fn splat_scale_is_exponentiated_and_opacity_passed_through_sigmoid() {
        let scene = import(&splat_ply("", ""), PlyVariant::GaussianSplat).unwrap();
        let Payload::SplatCloud(sc) = scene.nodes[0].payload.as_ref().unwrap() else {
            panic!("expected splats");
        };
        assert_eq!(sc.count, 1);

        let mut scale = [0.0f32; 3];
        sc.attribute(&Semantic::SplatScale)
            .unwrap()
            .get_f32(0, &mut scale);
        // exp(0) == 1
        assert!((scale[0] - 1.0).abs() < 1e-6, "{scale:?}");

        let mut op = [0.0f32; 1];
        sc.attribute(&Semantic::SplatOpacity)
            .unwrap()
            .get_f32(0, &mut op);
        // sigmoid(0) == 0.5
        assert!((op[0] - 0.5).abs() < 1e-6, "{op:?}");
    }

    #[test]
    fn splat_rotation_is_reordered_from_wxyz_to_xyzw() {
        // rot_0..3 == 1 0 0 0 is the identity quaternion with w first.
        let scene = import(&splat_ply("", ""), PlyVariant::GaussianSplat).unwrap();
        let Payload::SplatCloud(sc) = scene.nodes[0].payload.as_ref().unwrap() else {
            panic!("expected splats");
        };
        let mut q = [0.0f32; 4];
        sc.attribute(&Semantic::SplatRotation)
            .unwrap()
            .get_f32(0, &mut q);
        assert_eq!(q, [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn f_rest_is_deinterleaved_by_channel_not_by_coefficient() {
        // Degree 1: 9 values, three coefficients per channel.
        // R = 1,2,3   G = 10,20,30   B = 100,200,300
        let props: String = (0..9)
            .map(|i| format!("property float f_rest_{i}\n"))
            .collect();
        let values = "1 2 3 10 20 30 100 200 300";
        let scene = import(&splat_ply(&props, values), PlyVariant::GaussianSplat).unwrap();
        let Payload::SplatCloud(sc) = scene.nodes[0].payload.as_ref().unwrap() else {
            panic!("expected splats");
        };
        assert_eq!(sc.sh_degree, 1);

        // Coefficient 0 must be (R0, G0, B0) == (1, 10, 100), not (1, 2, 3).
        let mut c0 = [0.0f32; 3];
        sc.attribute(&Semantic::SplatSh {
            degree: 1,
            index: 0,
        })
        .unwrap()
        .get_f32(0, &mut c0);
        assert_eq!(c0, [1.0, 10.0, 100.0]);

        let mut c2 = [0.0f32; 3];
        sc.attribute(&Semantic::SplatSh {
            degree: 1,
            index: 2,
        })
        .unwrap()
        .get_f32(0, &mut c2);
        assert_eq!(c2, [3.0, 30.0, 300.0]);
    }

    #[test]
    fn an_unexpected_f_rest_width_is_rejected_rather_than_guessed() {
        let props: String = (0..6)
            .map(|i| format!("property float f_rest_{i}\n"))
            .collect();
        let err = import(&splat_ply(&props, "1 2 3 4 5 6"), PlyVariant::GaussianSplat);
        assert!(err.is_err());
    }

    #[test]
    fn binary_little_endian_body_reads_correctly() {
        let mut src = b"ply\nformat binary_little_endian 1.0\nelement vertex 2\n\
                        property float x\nproperty float y\nproperty float z\nend_header\n"
            .to_vec();
        for v in [0.0f32, 0.0, 0.0, 3.0, 4.0, 5.0] {
            src.extend_from_slice(&v.to_le_bytes());
        }
        let scene = import(&src, PlyVariant::PointCloud).unwrap();
        assert_eq!(scene.summary().points, 2);
        assert_eq!(scene.bounds().max, [3.0, 4.0, 5.0]);
    }

    #[test]
    fn a_truncated_binary_body_fails_before_allocating() {
        let mut src = b"ply\nformat binary_little_endian 1.0\nelement vertex 1000000\n\
                        property float x\nproperty float y\nproperty float z\nend_header\n"
            .to_vec();
        src.extend_from_slice(&[0u8; 12]);
        assert!(import(&src, PlyVariant::PointCloud).is_err());
    }

    #[test]
    fn an_absurd_element_count_is_refused_at_header_time() {
        let src = b"ply\nformat binary_little_endian 1.0\nelement vertex 999999999999\n\
                    property float x\nend_header\n";
        assert!(import(src, PlyVariant::PointCloud).is_err());
    }

    #[test]
    fn quaternion_unpack_produces_unit_length_results() {
        for raw in [0u32, 0x4000_0000, 0xFFFF_FFFF, 0x1234_5678] {
            let q = unpack_quat(raw);
            let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
            assert!((n - 1.0).abs() < 1e-3, "raw {raw:#x} produced length {n}");
        }
    }

    #[test]
    fn bit_unpack_spans_the_full_zero_to_one_range() {
        assert_eq!(unpack(0, 0, 11), 0.0);
        assert_eq!(unpack(0x7FF, 0, 11), 1.0);
        assert_eq!(unpack(0x3FF << 11, 11, 10), 1.0);
    }
}
