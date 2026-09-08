//! Geomview OFF, and the prefixed variants.
//!
//! The keyword on the first line says what each vertex carries: `C` adds colour, `N` a normal, `4`
//! a fourth position component, `ST` texture coordinates. They stack, and both `CNOFF` and `NCOFF`
//! are written in practice, so the prefix is read as a set of letters rather than matched against
//! a list of whole words.
//!
//! Faces are arbitrary polygons, each written as a vertex count followed by that many indices, and
//! optionally its own colour. They are fanned into triangles from the first vertex. A fan is only
//! correct for a convex polygon, which is what the format is used for: OFF is the output of
//! geometry-processing tools and mesh datasets, where faces are triangles or quads almost without
//! exception.
//!
//! Colour is written either as three or four floats in 0 to 1, or as three or four integers in 0
//! to 255, with nothing in the file to say which. The values decide: anything fractional is
//! floats, and an all-integer triple that stays within 0 and 1 is floats too, because a colour
//! that is pure black or pure white is far more likely than one that is 1/255 of grey.

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Off,
        reason: reason.into(),
    }
}

/// What the keyword's prefix letters declare.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Header {
    pub colour: bool,
    pub normals: bool,
    pub texcoords: bool,
    /// `4OFF`: positions carry a homogeneous `w` that is read and divided out.
    pub homogeneous: bool,
    /// `nOFF`: a leading dimension count on its own line. Only 3 is renderable.
    pub dimensioned: bool,
}

pub fn parse_keyword(word: &str) -> Option<Header> {
    let prefix = word.strip_suffix("OFF")?;
    let mut header = Header::default();
    let mut rest = prefix;
    // ST is two letters and has to be taken before S or T could be mistaken for anything else.
    if let Some(r) = rest.strip_prefix("ST") {
        header.texcoords = true;
        rest = r;
    }
    for c in rest.chars() {
        match c {
            'C' => header.colour = true,
            'N' => header.normals = true,
            '4' => header.homogeneous = true,
            'n' => header.dimensioned = true,
            _ => return None,
        }
    }
    Some(header)
}

/// Take the next token, or say what was expected where the file ran out.
fn take<'a>(tokens: &[&'a str], at: &mut usize, what: &str) -> Result<&'a str, ImportError> {
    let t = tokens
        .get(*at)
        .copied()
        .ok_or_else(|| malformed(format!("file ends where {what} was expected")))?;
    *at += 1;
    Ok(t)
}

/// How many values of per-face colour follow the first face's indices.
///
/// Zero, three or four. The answer is whichever width leaves a token stream that still reads as
/// the number of faces the header promised, tried narrowest first so a file without colour is
/// never charged for one.
///
/// `remaining` is the number of faces after this one. When it is zero there is no following face
/// to test against, so anything left over is colour by elimination.
fn detect_face_colour(tokens: &[&str], at: usize, vertex_count: usize, remaining: usize) -> usize {
    if remaining == 0 {
        let left = tokens.len() - at.min(tokens.len());
        return if (3..=4).contains(&left) { left } else { 0 };
    }
    for width in [0usize, 3, 4] {
        if plausible_faces(tokens, at + width, vertex_count, remaining, width) {
            return width;
        }
    }
    // Nothing fits. Zero keeps the error that follows pointing at the real problem rather than at
    // a guess this function made.
    0
}

/// Whether the token stream from `at` reads as `count` more faces, each optionally trailed by
/// `width` colour values.
///
/// Bounded rather than exhaustive: checking a handful of faces separates the three candidates,
/// and walking a million of them three times to prove it would cost more than the ambiguity does.
fn plausible_faces(
    tokens: &[&str],
    mut at: usize,
    vertex_count: usize,
    count: usize,
    width: usize,
) -> bool {
    for _ in 0..count.min(4) {
        let Some(n) = tokens.get(at).and_then(|t| t.parse::<usize>().ok()) else {
            return false;
        };
        if n < 3 || n > vertex_count {
            return false;
        }
        at += 1;
        for _ in 0..n {
            match tokens.get(at).and_then(|t| t.parse::<usize>().ok()) {
                Some(i) if i < vertex_count => at += 1,
                _ => return false,
            }
        }
        at += width;
    }
    true
}

/// Tokens of the file, with comments and blank lines removed.
///
/// OFF puts one record per line by convention and not by rule, and exporters do wrap long faces,
/// so the parser reads a flat token stream rather than trusting line breaks.
fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = match line.find('#') {
            Some(at) => &line[..at],
            None => line,
        };
        out.extend(line.split_ascii_whitespace());
    }
    out
}

pub fn import(bytes: &[u8]) -> Result<Scene, ImportError> {
    let text = std::str::from_utf8(bytes).map_err(|_| malformed("file is not UTF-8"))?;
    let tokens = tokens(text);
    let mut at = 0usize;

    let keyword = tokens.first().ok_or_else(|| malformed("file is empty"))?;
    let header = parse_keyword(keyword)
        .ok_or_else(|| malformed(format!("{keyword} is not an OFF keyword")))?;
    at += 1;

    // A free function rather than a closure: the face loop below reads `at` directly to skip an
    // optional per-face colour, and a closure holding `&mut at` would keep that borrow alive.
    macro_rules! next {
        ($what:expr) => {
            take(&tokens, &mut at, $what)?
        };
    }

    if header.dimensioned {
        let dims: usize = next!("the dimension count")
            .parse()
            .map_err(|_| malformed("the dimension count is not a number"))?;
        if dims != 3 {
            return Err(malformed(format!(
                "{dims}-dimensional OFF is not renderable"
            )));
        }
    }

    let vertex_count: usize = next!("the vertex count")
        .parse()
        .map_err(|_| malformed("the vertex count is not a number"))?;
    let face_count: usize = next!("the face count")
        .parse()
        .map_err(|_| malformed("the face count is not a number"))?;
    // The edge count is declared and, by the format's own admission, usually wrong. Read past it.
    let _ = next!("the edge count");

    // Counts come out of the file, so they are checked against what the file can actually hold
    // before anything allocates on them.
    let per_vertex = 3
        + usize::from(header.homogeneous)
        + if header.normals { 3 } else { 0 }
        + if header.texcoords { 2 } else { 0 };
    let least = vertex_count
        .checked_mul(per_vertex)
        .and_then(|n| n.checked_add(face_count.saturating_mul(4)))
        .ok_or_else(|| malformed("declared counts overflow the addressable range"))?;
    if least > tokens.len() {
        return Err(malformed(format!(
            "{vertex_count} vertices and {face_count} faces need at least {least} values, the file has {}",
            tokens.len()
        )));
    }

    let mut positions = Vec::with_capacity(vertex_count * 12);
    let mut normals = Vec::with_capacity(if header.normals { vertex_count * 12 } else { 0 });
    let mut raw_colour: Vec<[f32; 4]> =
        Vec::with_capacity(if header.colour { vertex_count } else { 0 });
    let mut bounds = Aabb::EMPTY;

    for v in 0..vertex_count {
        let mut p = [0.0f32; 3];
        for slot in &mut p {
            *slot = next!("a vertex coordinate").parse().map_err(|_| {
                malformed(format!("vertex {v} has a coordinate that is not a number"))
            })?;
        }
        if header.homogeneous {
            let w: f32 = next!("a vertex w")
                .parse()
                .map_err(|_| malformed(format!("vertex {v} has a w that is not a number")))?;
            if w != 0.0 && w != 1.0 {
                for c in &mut p {
                    *c /= w;
                }
            }
        }
        bounds.expand(p);
        for c in p {
            positions.extend_from_slice(&c.to_le_bytes());
        }

        if header.normals {
            for _ in 0..3 {
                let n: f32 = next!("a vertex normal").parse().map_err(|_| {
                    malformed(format!("vertex {v} has a normal that is not a number"))
                })?;
                normals.extend_from_slice(&n.to_le_bytes());
            }
        }
        if header.colour {
            let mut c = [0.0f32, 0.0, 0.0, 1.0];
            for slot in c.iter_mut().take(3) {
                *slot = next!("a vertex colour").parse().map_err(|_| {
                    malformed(format!("vertex {v} has a colour that is not a number"))
                })?;
            }
            raw_colour.push(c);
        }
        if header.texcoords {
            // Read and dropped: the mesh has no material to sample, so a texture coordinate has
            // nothing to address.
            let _ = next!("a texture coordinate");
            let _ = next!("a texture coordinate");
        }
    }

    let mut indices: Vec<u32> = Vec::with_capacity(face_count.min(1 << 20) * 3);
    // How many trailing values each face carries as its own colour. Settled once, from the first
    // face, because the format writes colour for every face or for none.
    let mut face_colour: Option<usize> = None;

    for f in 0..face_count {
        let n: usize = next!("a face vertex count")
            .parse()
            .map_err(|_| malformed(format!("face {f} has a vertex count that is not a number")))?;
        // Bounded on both sides before it reaches an allocator. A face cannot name more vertices
        // than the file declares, and `n` arrives straight out of an untrusted file: without the
        // upper bound, `99999999999 0 1 2` reserves 400 GB and the process aborts inside the
        // allocator rather than reporting a malformed file. Release builds set `panic = "abort"`,
        // so neither that nor the capacity-overflow path can be caught.
        if n < 3 || n > vertex_count {
            return Err(malformed(format!(
                "face {f} declares {n} vertices, and the file has {vertex_count}"
            )));
        }
        let mut face = Vec::with_capacity(n);
        for _ in 0..n {
            let i: usize = next!("a face index")
                .parse()
                .map_err(|_| malformed(format!("face {f} has an index that is not a number")))?;
            if i >= vertex_count {
                return Err(malformed(format!(
                    "face {f} names vertex {i}, past the {vertex_count} declared"
                )));
            }
            face.push(i as u32);
        }
        for k in 1..n - 1 {
            indices.extend_from_slice(&[face[0], face[k], face[k + 1]]);
        }

        // A face may carry its own colour: three or four values, integers or floats. It is
        // per-face and the renderer is per-vertex, so it is read past rather than kept.
        //
        // Nothing in the file says whether it is there. Testing for a decimal point is not enough,
        // because `255 0 0` is a legal face colour and reading it as the next face's vertex count
        // loses the rest of the file. So the width is decided once, on the first face, by asking
        // which of the three possibilities leaves a stream that still parses as faces.
        let width = match face_colour {
            Some(w) => w,
            None => {
                let w = detect_face_colour(&tokens, at, vertex_count, face_count - f - 1);
                face_colour = Some(w);
                w
            }
        };
        at = (at + width).min(tokens.len());
    }

    if indices.is_empty() && vertex_count == 0 {
        return Err(malformed("no vertices and no faces"));
    }

    let mut attributes = vec![Attribute::new(
        Semantic::Position,
        ComponentType::F32,
        3,
        vertex_count,
        positions,
    )
    .map_err(|e| malformed(e.to_string()))?];

    if header.normals {
        attributes.push(
            Attribute::new(
                Semantic::Normal,
                ComponentType::F32,
                3,
                vertex_count,
                normals,
            )
            .map_err(|e| malformed(e.to_string()))?,
        );
    }

    if header.colour && !raw_colour.is_empty() {
        // Floats in 0 to 1, or bytes in 0 to 255. Nothing in the file says which, so the values
        // do, and the only question that separates them is whether anything exceeds 1.
        //
        // Asking about fractional parts as well inverts the answer: `0.5` has a fractional part,
        // which read as evidence of bytes scales it by one and collapses every mid-tone to black.
        // A fixture of `0.0` and `1.0` hides that, because neither has a fractional part, and that
        // is exactly why the first test written for this passed while it was broken.
        let bytes_already = raw_colour.iter().any(|c| c[..3].iter().any(|v| *v > 1.0));
        let scale = if bytes_already { 1.0 } else { 255.0 };
        let mut data = Vec::with_capacity(vertex_count * 3);
        for c in &raw_colour {
            for v in &c[..3] {
                // Rounded, not truncated. A float colour of 0.5 is 127.5, and truncation would
                // darken every value in the file by up to one step.
                data.push((v * scale).round().clamp(0.0, 255.0) as u8);
            }
        }
        attributes.push(
            Attribute::new(Semantic::Color(0), ComponentType::U8, 3, vertex_count, data)
                .map_err(|e| malformed(e.to_string()))?
                .normalized(),
        );
    }

    let index_buffer = if indices.is_empty() {
        Indices::None
    } else if vertex_count <= u16::MAX as usize {
        Indices::U16(indices.iter().map(|i| *i as u16).collect())
    } else {
        Indices::U32(indices.clone())
    };

    let mut scene = Scene::new();
    scene.push_root(Node::with_payload(
        "off",
        Payload::TriMesh(TriMesh {
            attributes,
            indices: index_buffer,
            bounds,
            material: None,
        }),
    ));
    scene.set_meta("format", "OFF");
    scene.set_meta("keyword", *keyword);
    Ok(scene)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh(scene: &Scene) -> &TriMesh {
        scene
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Some(Payload::TriMesh(m)) => Some(m),
                _ => None,
            })
            .expect("a mesh")
    }

    const SQUARE: &str = "OFF\n4 2 4\n0 0 0\n1 0 0\n1 1 0\n0 1 0\n3 0 1 2\n3 0 2 3\n";

    #[test]
    fn keyword_prefixes_stack_in_either_order() {
        assert_eq!(parse_keyword("OFF"), Some(Header::default()));
        assert!(parse_keyword("COFF").unwrap().colour);
        assert!(parse_keyword("NOFF").unwrap().normals);
        let both = parse_keyword("CNOFF").unwrap();
        assert!(both.colour && both.normals);
        let reversed = parse_keyword("NCOFF").unwrap();
        assert_eq!(both, reversed);
        assert!(parse_keyword("STOFF").unwrap().texcoords);
        assert!(parse_keyword("4OFF").unwrap().homogeneous);
        assert_eq!(parse_keyword("PLY"), None);
    }

    #[test]
    fn two_triangles_make_a_square() {
        let scene = import(SQUARE.as_bytes()).unwrap();
        let m = mesh(&scene);
        assert_eq!(m.vertex_count(), 4);
        assert_eq!(m.triangle_count(), 2);
    }

    #[test]
    fn a_faceless_mesh_has_no_index_buffer() {
        let scene = import(b"OFF\n3 0 0\n0 0 0\n1 0 0\n0 1 0\n").unwrap();
        let m = mesh(&scene);
        assert!(matches!(m.indices, Indices::None));
    }

    #[test]
    fn a_quad_face_is_fanned_into_two_triangles() {
        let scene = import(b"OFF\n4 1 4\n0 0 0\n1 0 0\n1 1 0\n0 1 0\n4 0 1 2 3\n").unwrap();
        assert_eq!(mesh(&scene).triangle_count(), 2);
    }

    #[test]
    fn comments_are_stripped_anywhere_on_a_line() {
        let text = "OFF # the keyword\n# a whole line\n4 2 4\n0 0 0\n1 0 0\n1 1 0\n0 1 0\n\
                    3 0 1 2 # a face\n3 0 2 3\n";
        assert_eq!(mesh(&import(text.as_bytes()).unwrap()).triangle_count(), 2);
    }

    #[test]
    fn byte_colour_and_float_colour_are_told_apart_by_their_values() {
        let bytes = "COFF\n3 1 3\n0 0 0 255 0 0\n1 0 0 0 255 0\n0 1 0 0 0 255\n3 0 1 2\n";
        let scene = import(bytes.as_bytes()).unwrap();
        let colour = mesh(&scene).attribute(&Semantic::Color(0)).unwrap();
        assert_eq!(colour.data[0], 255);

        let floats =
            "COFF\n3 1 3\n0 0 0 1.0 0.0 0.0\n1 0 0 0.0 1.0 0.0\n0 1 0 0.0 0.0 1.0\n3 0 1 2\n";
        let scene = import(floats.as_bytes()).unwrap();
        let colour = mesh(&scene).attribute(&Semantic::Color(0)).unwrap();
        assert_eq!(colour.data[0], 255);
    }

    #[test]
    fn normals_survive_the_round_trip() {
        let text = "NOFF\n3 1 3\n0 0 0 0 0 1\n1 0 0 0 0 1\n0 1 0 0 0 1\n3 0 1 2\n";
        let scene = import(text.as_bytes()).unwrap();
        let m = mesh(&scene);
        assert!(!m.needs_derived_normals());
        let mut n = [0.0f32; 3];
        m.attribute(&Semantic::Normal).unwrap().get_f32(0, &mut n);
        assert_eq!(n, [0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_face_index_past_the_vertex_count_is_refused() {
        let text = "OFF\n3 1 3\n0 0 0\n1 0 0\n0 1 0\n3 0 1 9\n";
        assert!(import(text.as_bytes()).is_err());
    }

    #[test]
    fn a_hostile_per_face_vertex_count_is_refused_before_allocating() {
        // The header counts pass their own check: 17 tokens against a required 13. The per-face
        // count is the one that reaches an allocator, and unbounded it reserves 400 GB, which
        // aborts inside the allocator rather than returning an error.
        let text = "OFF\n3 1 3\n0 0 0\n1 0 0\n0 1 0\n99999999999 0 1 2\n";
        let err = import(text.as_bytes()).unwrap_err();
        assert!(
            format!("{err}").contains("99999999999"),
            "expected the count in the message, got: {err}"
        );
    }

    #[test]
    fn a_face_naming_more_vertices_than_the_file_holds_is_refused() {
        let text = "OFF\n3 1 3\n0 0 0\n1 0 0\n0 1 0\n4 0 1 2 0\n";
        assert!(import(text.as_bytes()).is_err());
    }

    #[test]
    fn fractional_float_colour_is_not_collapsed_to_black() {
        // 0.5 has a fractional part. Reading that as evidence of byte colour scales it by one and
        // every mid-tone becomes 0, which is the bug a fixture of 0.0 and 1.0 cannot see.
        let text = "COFF\n3 1 3\n0 0 0 0.5 0.25 0.75\n1 0 0 0.5 0.25 0.75\n\
                    0 1 0 0.5 0.25 0.75\n3 0 1 2\n";
        let scene = import(text.as_bytes()).unwrap();
        let colour = mesh(&scene).attribute(&Semantic::Color(0)).unwrap();
        assert_eq!(&colour.data[..3], &[128, 64, 191]);
    }

    #[test]
    fn a_declared_count_larger_than_the_file_is_refused_before_allocating() {
        let text = "OFF\n4000000000 4000000000 0\n0 0 0\n";
        assert!(import(text.as_bytes()).is_err());
    }

    #[test]
    fn integer_face_colour_is_not_mistaken_for_the_next_face() {
        // `255 0 0` after a face is a legal colour. Read as a vertex count it swallows the file.
        let text = "OFF\n4 2 4\n0 0 0\n1 0 0\n1 1 0\n0 1 0\n\
                    3 0 1 2 255 0 0\n3 0 2 3 0 255 0\n";
        let scene = import(text.as_bytes()).unwrap();
        assert_eq!(mesh(&scene).triangle_count(), 2);
    }

    #[test]
    fn float_face_colour_with_alpha_is_skipped() {
        let text = "OFF\n4 2 4\n0 0 0\n1 0 0\n1 1 0\n0 1 0\n\
                    3 0 1 2 1.0 0.0 0.0 1.0\n3 0 2 3 0.0 1.0 0.0 1.0\n";
        assert_eq!(mesh(&import(text.as_bytes()).unwrap()).triangle_count(), 2);
    }

    #[test]
    fn a_single_face_carrying_colour_still_parses() {
        // With no following face to test against, whatever is left over has to be the colour.
        let text = "OFF\n3 1 3\n0 0 0\n1 0 0\n0 1 0\n3 0 1 2 255 128 64\n";
        assert_eq!(mesh(&import(text.as_bytes()).unwrap()).triangle_count(), 1);
    }

    #[test]
    fn faces_without_colour_are_not_charged_for_one() {
        let text = "OFF\n5 3 0\n0 0 0\n1 0 0\n1 1 0\n0 1 0\n2 2 0\n\
                    3 0 1 2\n3 0 2 3\n3 2 4 3\n";
        assert_eq!(mesh(&import(text.as_bytes()).unwrap()).triangle_count(), 3);
    }

    #[test]
    fn a_wrapped_face_line_still_parses() {
        // The format puts one record per line by convention, not by rule.
        let text = "OFF\n4 2 4\n0 0 0 1 0 0\n1 1 0 0 1 0\n3 0 1 2 3 0\n2 3\n";
        assert_eq!(mesh(&import(text.as_bytes()).unwrap()).triangle_count(), 2);
    }
}
