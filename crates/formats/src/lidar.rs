//! LiDAR point clouds: ASPRS LAS, its compressed twin LAZ, and ASTM E57.
//!
//! Two formats, one shape, and the same two problems in both.
//!
//! **Coordinates are enormous.** A LAS tile is georeferenced: UTM eastings run to seven digits,
//! and a survey in state-plane feet to eight. Handing those to a renderer with `f32` positions
//! quantizes a centimetre-accurate scan to somewhere between a decimetre and a metre, and the
//! camera's near plane fights the far plane for what is left. So both importers **recentre**: the
//! cloud's own centre becomes the origin, and the offset that was removed is recorded in the
//! scene metadata so nobody has to guess later where it went.
//!
//! **Files are bigger than memory.** A county tile is 200 million points. Both readers here are
//! streaming, and both stop at a point ceiling rather than trying and failing, a viewer that
//! shows the first twenty million points of a tile and says so is far more useful than one that
//! exhausts the machine.

use std::path::Path;

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::PointCloud;
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::ImportError;

/// How many points to keep. Beyond this the cloud is subsampled and the scene says so.
///
/// Twenty million points is 320 MB of interleaved position and colour on the GPU, which is a
/// reasonable ceiling for a viewer that is also expected to start instantly.
pub const MAX_POINTS: usize = 20_000_000;

fn malformed(format: FormatId, reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format,
        reason: reason.into(),
    }
}

/// Points accumulated in double precision, then recentred on the way out.
///
/// Doubles all the way to the recentring is the point: subtracting the centre *after* narrowing
/// to `f32` has already lost the precision the subtraction was meant to preserve.
#[derive(Default)]
pub struct Accumulator {
    positions: Vec<[f64; 3]>,
    colours: Vec<u8>,
    has_colour: bool,
    /// Points the source held, including any this dropped.
    pub seen: usize,
}

impl Accumulator {
    pub fn push(&mut self, position: [f64; 3], colour: Option<[u8; 4]>) {
        self.seen += 1;
        if self.positions.len() >= MAX_POINTS {
            return;
        }
        if !position.iter().all(|v| v.is_finite()) {
            return;
        }
        self.positions.push(position);
        if let Some(c) = colour {
            self.has_colour = true;
            self.colours.extend_from_slice(&c);
        } else {
            self.colours.extend_from_slice(&[255, 255, 255, 255]);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    pub fn kept(&self) -> usize {
        self.positions.len()
    }

    /// The centre that will be subtracted, in source coordinates.
    pub fn centre(&self) -> [f64; 3] {
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for p in &self.positions {
            for a in 0..3 {
                min[a] = min[a].min(p[a]);
                max[a] = max[a].max(p[a]);
            }
        }
        [
            (min[0] + max[0]) * 0.5,
            (min[1] + max[1]) * 0.5,
            (min[2] + max[2]) * 0.5,
        ]
    }

    fn finish(self, format: FormatId) -> Result<(PointCloud, [f64; 3]), ImportError> {
        let centre = self.centre();
        let count = self.positions.len();
        let mut bounds = Aabb::EMPTY;
        let mut bytes = Vec::with_capacity(count * 12);
        for p in &self.positions {
            let local = [
                (p[0] - centre[0]) as f32,
                (p[1] - centre[1]) as f32,
                (p[2] - centre[2]) as f32,
            ];
            bounds.expand(local);
            for v in local {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }

        let mut attributes =
            vec![
                Attribute::new(Semantic::Position, ComponentType::F32, 3, count, bytes)
                    .map_err(|e| malformed(format.clone(), e.to_string()))?,
            ];
        if self.has_colour {
            attributes.push(
                Attribute::new(
                    Semantic::Color(0),
                    ComponentType::U8,
                    4,
                    count,
                    self.colours,
                )
                .map_err(|e| malformed(format, e.to_string()))?
                .normalized(),
            );
        }

        Ok((
            PointCloud {
                attributes,
                count,
                bounds,
            },
            centre,
        ))
    }
}

fn assemble(accumulator: Accumulator, format: FormatId, label: &str) -> Result<Scene, ImportError> {
    if accumulator.is_empty() {
        return Err(malformed(format, "the file holds no points"));
    }
    let seen = accumulator.seen;
    let kept = accumulator.kept();
    let (cloud, centre) = accumulator.finish(format)?;

    let mut scene = Scene::new();
    scene.push_root(Node::with_payload("points", Payload::PointCloud(cloud)));
    scene.set_meta("format", label);
    scene.set_meta("points", kept.to_string());
    if kept < seen {
        // Silent truncation reads as "this is the whole file", which it is not.
        scene.set_meta("truncated", format!("showing {kept} of {seen} points"));
    }
    // Georeferenced coordinates were removed to keep f32 positions accurate. Saying by how much
    // is what makes the displayed coordinates translatable back to the survey.
    if centre.iter().any(|v| v.abs() > 1.0) {
        scene.set_meta(
            "recentred by",
            format!("{:.3}, {:.3}, {:.3}", -centre[0], -centre[1], -centre[2]),
        );
    }
    Ok(scene)
}

/// ASPRS LAS, and LAZ, which is the same content behind an arithmetic coder.
///
/// The reader is given the path rather than the bytes because a 4 GB tile has no business being
/// resident, and because `las` handles the LAZ chunk table itself once it owns the file.
pub fn import_las(path: &Path) -> Result<Scene, ImportError> {
    /// Points per read. Bounded so that a 200 million point tile never becomes 200 million
    /// points of resident `las::PointData`, and large enough that the per-batch overhead
    /// disappears against the decode.
    const BATCH: u64 = 1_000_000;

    let mut reader =
        las::Reader::from_path(path).map_err(|e| malformed(FormatId::Las, e.to_string()))?;
    let header = reader.header().clone();
    let total = header.number_of_points();
    let compressed = header.point_format().is_compressed;
    let version = format!("{}.{}", header.version().major, header.version().minor);
    let date = header.date().map(|d| d.to_string());

    let mut accumulator = Accumulator::default();
    let mut remaining = total;
    while remaining > 0 && accumulator.seen < MAX_POINTS * 4 {
        let batch = reader
            .read_points(remaining.min(BATCH))
            .map_err(|e| malformed(FormatId::Las, e.to_string()))?;
        if batch.is_empty() {
            break;
        }
        remaining = remaining.saturating_sub(batch.len() as u64);

        // The reader is column-oriented, so colour is zipped in only when the point format has
        // any. Asking for it on a format without it would be an error rather than a `None`.
        let coordinates: Vec<[f64; 3]> = batch
            .x()
            .zip(batch.y())
            .zip(batch.z())
            .map(|((x, y), z)| [x, y, z])
            .collect();
        // Collected rather than zipped lazily: the colour iterator borrows the batch, and the
        // batch has to be dropped before the next read overwrites it.
        let colours: Option<Vec<[u8; 4]>> = batch
            .rgb()
            .map(|rgb| rgb.map(|(r, g, b)| to_eight_bit(r, g, b)).collect());
        match colours {
            Some(colours) => {
                for (position, colour) in coordinates.iter().zip(colours) {
                    accumulator.push(*position, Some(colour));
                }
            }
            None => {
                for position in &coordinates {
                    accumulator.push(*position, None);
                }
            }
        }
    }

    let mut scene = assemble(
        accumulator,
        FormatId::Las,
        if compressed {
            "LAZ point cloud"
        } else {
            "LAS point cloud"
        },
    )?;
    scene.set_meta("las version", version);
    if let Some(date) = date {
        scene.set_meta("captured", date);
    }
    Ok(scene)
}

/// LAS stores colour as 16 bits per channel, but a great many writers put 8-bit values in the
/// low byte and leave it at that. Shifting unconditionally renders those files almost black, so
/// the shift is decided from the values themselves.
fn to_eight_bit(r: u16, g: u16, b: u16) -> [u8; 4] {
    let shift = if r.max(g).max(b) > 255 { 8 } else { 0 };
    [
        (r >> shift) as u8,
        (g >> shift) as u8,
        (b >> shift) as u8,
        255,
    ]
}

/// ASTM E57: an XML index describing binary sections, holding one or more scans.
///
/// Multiple scans in one file are the normal case for a terrestrial survey, and each carries its
/// own pose. They are merged into a single cloud here rather than kept as separate nodes, because
/// what a viewer is asked for is the site, not the setups.
pub fn import_e57(path: &Path) -> Result<Scene, ImportError> {
    let mut file =
        e57::E57Reader::from_file(path).map_err(|e| malformed(FormatId::E57, e.to_string()))?;
    let clouds = file.pointclouds();
    if clouds.is_empty() {
        return Err(malformed(
            FormatId::E57,
            "the file declares no point clouds",
        ));
    }
    let scans = clouds.len();

    let mut accumulator = Accumulator::default();
    for cloud in &clouds {
        let iterator = file
            .pointcloud_simple(cloud)
            .map_err(|e| malformed(FormatId::E57, e.to_string()))?;
        for point in iterator {
            let point = point.map_err(|e| malformed(FormatId::E57, e.to_string()))?;
            // A point can be a direction with no range, or nothing at all, a laser return that
            // never came back. Only fully valid coordinates are points.
            let e57::CartesianCoordinate::Valid { x, y, z } = point.cartesian else {
                continue;
            };
            let colour = point.color.map(|c| {
                [
                    (c.red * 255.0).clamp(0.0, 255.0) as u8,
                    (c.green * 255.0).clamp(0.0, 255.0) as u8,
                    (c.blue * 255.0).clamp(0.0, 255.0) as u8,
                    255,
                ]
            });
            accumulator.push([x, y, z], colour);
            if accumulator.seen > MAX_POINTS * 4 {
                break;
            }
        }
        if accumulator.seen > MAX_POINTS * 4 {
            break;
        }
    }

    let mut scene = assemble(accumulator, FormatId::E57, "ASTM E57 point cloud")?;
    if scans > 1 {
        scene.set_meta("scans", format!("{scans}, merged"));
    }
    Ok(scene)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recentring_preserves_centimetres_at_survey_coordinates() {
        // A UTM easting with a centimetre step. Narrowing that to f32 without recentring loses
        // the step entirely: f32 has about seven significant digits and the number has nine.
        let mut a = Accumulator::default();
        a.push([500_000.00, 4_100_000.00, 100.0], None);
        a.push([500_000.01, 4_100_000.00, 100.0], None);
        a.push([500_001.00, 4_100_001.00, 101.0], None);

        let naive = 500_000.01f64 as f32 - 500_000.00f64 as f32;
        assert_eq!(naive, 0.0, "the failure this exists to prevent");

        let (cloud, centre) = a.finish(FormatId::Las).unwrap();
        assert!(centre[0] > 400_000.0);
        let mut first = [0.0f32; 4];
        let mut second = [0.0f32; 4];
        let position = cloud.attribute(&Semantic::Position).unwrap();
        position.get_f32(0, &mut first);
        position.get_f32(1, &mut second);
        assert!(
            (second[0] - first[0] - 0.01).abs() < 1e-4,
            "the centimetre step survived: {first:?} {second:?}"
        );
    }

    #[test]
    fn the_recorded_offset_translates_a_point_back_to_its_survey_coordinate() {
        let mut a = Accumulator::default();
        a.push([500_000.0, 4_100_000.0, 100.0], None);
        a.push([500_010.0, 4_100_020.0, 130.0], None);
        let centre = a.centre();
        assert_eq!(centre, [500_005.0, 4_100_010.0, 115.0]);

        let (cloud, centre) = a.finish(FormatId::Las).unwrap();
        let mut out = [0.0f32; 4];
        cloud
            .attribute(&Semantic::Position)
            .unwrap()
            .get_f32(0, &mut out);
        assert_eq!(out[0] as f64 + centre[0], 500_000.0);
    }

    #[test]
    fn the_point_ceiling_truncates_rather_than_exhausting_the_machine() {
        let mut a = Accumulator::default();
        for i in 0..(MAX_POINTS + 1000) {
            a.push([i as f64, 0.0, 0.0], None);
        }
        assert_eq!(a.kept(), MAX_POINTS);
        assert_eq!(a.seen, MAX_POINTS + 1000);

        let scene = assemble(a, FormatId::Las, "LAS point cloud").unwrap();
        let note = scene
            .meta
            .iter()
            .find(|(k, _)| k == "truncated")
            .expect("truncation must be reported, never silent");
        assert!(note.1.contains(&MAX_POINTS.to_string()));
    }

    #[test]
    fn non_finite_points_are_dropped_but_still_counted() {
        let mut a = Accumulator::default();
        a.push([0.0, 0.0, 0.0], None);
        a.push([f64::NAN, 0.0, 0.0], None);
        a.push([1.0, 1.0, 1.0], None);
        assert_eq!(a.kept(), 2);
        assert_eq!(a.seen, 3);
    }

    #[test]
    fn colour_is_carried_only_when_the_source_had_any() {
        let mut plain = Accumulator::default();
        plain.push([0.0; 3], None);
        let (cloud, _) = plain.finish(FormatId::Las).unwrap();
        assert!(cloud.attribute(&Semantic::Color(0)).is_none());

        let mut coloured = Accumulator::default();
        coloured.push([0.0; 3], Some([10, 20, 30, 255]));
        let (cloud, _) = coloured.finish(FormatId::Las).unwrap();
        let mut out = [0.0f32; 4];
        cloud
            .attribute(&Semantic::Color(0))
            .expect("colour was supplied")
            .get_f32(0, &mut out);
        assert!((out[0] - 10.0 / 255.0).abs() < 1e-3, "{out:?}");
    }

    #[test]
    fn an_empty_cloud_is_an_error_rather_than_an_empty_window() {
        assert!(assemble(Accumulator::default(), FormatId::Las, "LAS").is_err());
    }

    #[test]
    fn a_cloud_already_near_the_origin_is_not_annotated_as_recentred() {
        let mut a = Accumulator::default();
        a.push([-0.5, -0.5, -0.5], None);
        a.push([0.5, 0.5, 0.5], None);
        let scene = assemble(a, FormatId::E57, "E57").unwrap();
        assert!(!scene.meta.iter().any(|(k, _)| k == "recentred by"));
    }
}
