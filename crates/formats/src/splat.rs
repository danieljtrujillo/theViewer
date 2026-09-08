//! The `.splat` format: antimatter15's, and the one with no header at all.
//!
//! Thirty-two bytes per splat and nothing else: no magic, no count, no version. Everything the
//! viewer needs has to come from the arithmetic, which is why [`viewer_io::sniff()`] never calls
//! this format better than a guess.
//!
//! The layout, which is the whole specification:
//!
//! | Bytes | Field | Encoding |
//! |---|---|---|
//! | 0 to 11 | position | `f32` x, y, z |
//! | 12 to 23 | scale | `f32`, already exponentiated |
//! | 24 to 27 | colour | `u8` RGBA, already through the SH and sigmoid |
//! | 28 to 31 | rotation | `u8` quaternion w, x, y, z, as `(q * 128 + 128)` |
//!
//! Two of those "already" notes are the interesting part. A `.splat` file has had the 3DGS
//! training parameters *baked*: `exp()` applied to the log-scales, `sigmoid()` to the opacity
//! logit, and the degree-0 spherical harmonic evaluated into an 8-bit colour. That makes the file
//! small and the loader trivial, and it makes higher-degree view-dependent colour unrecoverable,
//! the format simply threw it away.

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::SplatCloud;
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

/// Bytes per splat. The only structure the format has.
pub const STRIDE: usize = 32;

/// The degree-0 spherical harmonic basis constant, `1 / (2 * sqrt(pi))`.
///
/// The renderer reconstructs colour as `0.5 + C0 * dc`, so a file that stored the *result* has to
/// be run backwards through the same constant to reach the coefficient the pipeline expects.
const C0: f32 = 0.282_094_79;

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Splat,
        reason: reason.into(),
    }
}

pub fn import(bytes: &[u8]) -> Result<Scene, ImportError> {
    if bytes.is_empty() || bytes.len() % STRIDE != 0 {
        return Err(malformed(format!(
            "{} bytes is not a whole number of {STRIDE}-byte splats",
            bytes.len()
        )));
    }
    let count = bytes.len() / STRIDE;

    let mut positions = Vec::with_capacity(count * 3);
    let mut scales = Vec::with_capacity(count * 3);
    let mut rotations = Vec::with_capacity(count * 4);
    let mut opacity = Vec::with_capacity(count);
    let mut sh = Vec::with_capacity(count * 3);
    let mut bounds = Aabb::EMPTY;

    for record in bytes.chunks_exact(STRIDE) {
        let f =
            |o: usize| f32::from_le_bytes([record[o], record[o + 1], record[o + 2], record[o + 3]]);
        let position = [f(0), f(4), f(8)];
        // A file with no header cannot declare that it is corrupt, so the values have to be
        // checked. One NaN position collapses the bounding box and takes the camera with it.
        if !position.iter().all(|v| v.is_finite()) {
            return Err(malformed("a splat position is not a finite number"));
        }
        bounds.expand(position);
        positions.extend_from_slice(&position);

        for o in [12usize, 16, 20] {
            let s = f(o);
            scales.push(if s.is_finite() { s.max(0.0) } else { 0.0 });
        }

        // Colour arrives as the rendered result. Running it back through the basis constant is
        // what lets one shader path serve every splat format.
        for c in 0..3 {
            sh.push((record[24 + c] as f32 / 255.0 - 0.5) / C0);
        }
        opacity.push(record[27] as f32 / 255.0);

        // Quaternions are stored w, x, y, z, each byte mapping to `(q * 128 + 128)`. The scene
        // model uses glTF's x, y, z, w order, so this reorders as well as rescales.
        let q = |o: usize| (record[o] as f32 - 128.0) / 128.0;
        let (w, x, y, z) = (q(28), q(29), q(30), q(31));
        let length = (w * w + x * x + y * y + z * z).sqrt();
        let length = if length > 1e-6 { length } else { 1.0 };
        rotations.extend_from_slice(&[x / length, y / length, z / length, w / length]);
    }

    let attributes = vec![
        attribute(Semantic::Position, 3, positions)?,
        attribute(Semantic::SplatScale, 3, scales)?,
        attribute(Semantic::SplatRotation, 4, rotations)?,
        attribute(Semantic::SplatOpacity, 1, opacity)?,
        attribute(
            Semantic::SplatSh {
                degree: 0,
                index: 0,
            },
            3,
            sh,
        )?,
    ];

    let mut scene = Scene::new();
    scene.push_root(Node::with_payload(
        "splats",
        Payload::SplatCloud(SplatCloud {
            attributes,
            count,
            bounds,
            // The format has nowhere to put higher bands, so the colour is view-independent by
            // construction rather than by our omission.
            sh_degree: 0,
            antialiased: false,
            orbit_hint: None,
        }),
    ));
    scene.set_meta("format", "splat (antimatter15)");
    scene.set_meta("spherical harmonics", "degree 0 only, baked by the format");
    Ok(scene)
}

fn attribute(semantic: Semantic, components: u8, data: Vec<f32>) -> Result<Attribute, ImportError> {
    let count = data.len() / components as usize;
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
    Attribute::new(semantic, ComponentType::F32, components, count, bytes)
        .map_err(|e| malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        position: [f32; 3],
        scale: [f32; 3],
        rgba: [u8; 4],
        quaternion_wxyz: [f32; 4],
    ) -> Vec<u8> {
        let mut v = Vec::with_capacity(STRIDE);
        for c in position.iter().chain(scale.iter()) {
            v.extend_from_slice(&c.to_le_bytes());
        }
        v.extend_from_slice(&rgba);
        for q in quaternion_wxyz {
            v.push((q * 128.0 + 128.0).clamp(0.0, 255.0) as u8);
        }
        v
    }

    #[test]
    fn a_single_splat_round_trips_through_every_encoding() {
        let bytes = record(
            [1.0, 2.0, 3.0],
            [0.1, 0.2, 0.3],
            [255, 128, 0, 255],
            [1.0, 0.0, 0.0, 0.0],
        );
        let scene = import(&bytes).expect("one well-formed splat");
        let Some(Payload::SplatCloud(cloud)) = &scene.nodes[0].payload else {
            panic!("expected a splat cloud");
        };
        assert_eq!(cloud.count, 1);
        assert_eq!(cloud.bounds.min, [1.0, 2.0, 3.0]);

        let mut out = [0.0f32; 4];
        cloud
            .attribute(&Semantic::SplatOpacity)
            .unwrap()
            .get_f32(0, &mut out);
        assert_eq!(out[0], 1.0, "alpha 255 is fully opaque");

        // The identity quaternion is w = 1, and it must come out in x, y, z, w order.
        cloud
            .attribute(&Semantic::SplatRotation)
            .unwrap()
            .get_f32(0, &mut out);
        assert_eq!(&out[..4], &[0.0, 0.0, 0.0, 1.0]);

        // Colour runs backwards through the basis constant, so that the renderer's own
        // `0.5 + C0 * dc` reproduces the byte that was in the file.
        cloud
            .attribute(&Semantic::SplatSh {
                degree: 0,
                index: 0,
            })
            .unwrap()
            .get_f32(0, &mut out);
        let reconstructed = 0.5 + C0 * out[0];
        assert!((reconstructed - 1.0).abs() < 1e-4, "{reconstructed}");
    }

    #[test]
    fn scales_are_taken_as_linear_because_the_format_already_exponentiated_them() {
        let bytes = record([0.0; 3], [0.5, 0.25, 0.125], [0; 4], [1.0, 0.0, 0.0, 0.0]);
        let scene = import(&bytes).unwrap();
        let Some(Payload::SplatCloud(cloud)) = &scene.nodes[0].payload else {
            panic!()
        };
        let mut out = [0.0f32; 4];
        cloud
            .attribute(&Semantic::SplatScale)
            .unwrap()
            .get_f32(0, &mut out);
        assert_eq!(&out[..3], &[0.5, 0.25, 0.125]);
    }

    #[test]
    fn a_length_that_is_not_a_multiple_of_the_stride_is_refused() {
        assert!(import(&[0u8; 31]).is_err());
        assert!(import(&[0u8; 33]).is_err());
        assert!(import(&[]).is_err());
    }

    #[test]
    fn a_non_finite_position_is_refused_rather_than_collapsing_the_bounds() {
        // Nothing in the file can flag corruption, so the arithmetic has to.
        let bytes = record(
            [f32::NAN, 0.0, 0.0],
            [1.0; 3],
            [255; 4],
            [1.0, 0.0, 0.0, 0.0],
        );
        assert!(import(&bytes).is_err());
    }

    #[test]
    fn many_splats_all_arrive() {
        let mut bytes = Vec::new();
        for i in 0..1000 {
            bytes.extend(record(
                [i as f32, 0.0, 0.0],
                [1.0; 3],
                [255, 255, 255, 255],
                [1.0, 0.0, 0.0, 0.0],
            ));
        }
        let scene = import(&bytes).unwrap();
        let Some(Payload::SplatCloud(cloud)) = &scene.nodes[0].payload else {
            panic!()
        };
        assert_eq!(cloud.count, 1000);
        assert_eq!(cloud.bounds.min[0], 0.0);
        assert_eq!(cloud.bounds.max[0], 999.0);
    }

    #[test]
    fn a_degenerate_quaternion_does_not_produce_nan() {
        let bytes = record([0.0; 3], [1.0; 3], [255; 4], [0.0, 0.0, 0.0, 0.0]);
        let scene = import(&bytes).unwrap();
        let Some(Payload::SplatCloud(cloud)) = &scene.nodes[0].payload else {
            panic!()
        };
        let mut out = [0.0f32; 4];
        cloud
            .attribute(&Semantic::SplatRotation)
            .unwrap()
            .get_f32(0, &mut out);
        assert!(out.iter().all(|v| v.is_finite()), "{out:?}");
    }
}
