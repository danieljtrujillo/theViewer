//! Whitespace-separated ASCII point clouds: `.xyz`, `.pts`, `.asc`.
//!
//! There is no specification. Every scanner, every photogrammetry tool and every survey package
//! writes rows of numbers and stops there, so the column layout has to be inferred from the row
//! width and from the values themselves:
//!
//! | Columns | Read as |
//! |---|---|
//! | 3 | x y z |
//! | 4 | x y z intensity |
//! | 6 | x y z followed by either colour or a normal, decided by the values |
//! | 7 | x y z intensity r g b, which is Leica's `.pts` layout |
//! | 9 | x y z r g b nx ny nz |
//!
//! Six columns is the ambiguous case and the one that matters. Integers in 0 to 255 are colour;
//! anything fractional, negative, or outside that range is a normal. Reading a normal as colour
//! renders the cloud in near-black, and reading colour as a normal breaks nothing visible but
//! throws the colour away, so the guess is worth making carefully.
//!
//! Coordinates are recentred in `f64` before they narrow to `f32`, exactly as the LiDAR readers
//! do. A survey `.xyz` carries the same nine-significant-digit eastings as a LAS tile, and `f32`
//! holds seven, so without recentring a centimetre step disappears.

use std::path::Path;

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::PointCloud;
use viewer_scene::{Aabb, Node, Payload, Scene};

use crate::lidar::MAX_POINTS;
use crate::ImportError;

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Xyz,
        reason: reason.into(),
    }
}

/// What the trailing columns turned out to mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `x y z`
    Positions,
    /// `x y z i`
    Intensity,
    /// `x y z r g b`
    Colour,
    /// `x y z nx ny nz`
    Normals,
    /// `x y z i r g b`
    IntensityColour,
    /// `x y z r g b nx ny nz`
    ColourNormals,
}

impl Layout {
    pub fn label(self) -> &'static str {
        match self {
            Layout::Positions => "x y z",
            Layout::Intensity => "x y z intensity",
            Layout::Colour => "x y z r g b",
            Layout::Normals => "x y z nx ny nz",
            Layout::IntensityColour => "x y z intensity r g b",
            Layout::ColourNormals => "x y z r g b nx ny nz",
        }
    }

    fn has_colour(self) -> bool {
        matches!(
            self,
            Layout::Colour | Layout::IntensityColour | Layout::ColourNormals
        )
    }

    fn colour_at(self) -> usize {
        match self {
            Layout::Colour | Layout::ColourNormals => 3,
            Layout::IntensityColour => 4,
            _ => 0,
        }
    }

    fn has_normals(self) -> bool {
        matches!(self, Layout::Normals | Layout::ColourNormals)
    }

    fn normal_at(self) -> usize {
        match self {
            Layout::Normals => 3,
            Layout::ColourNormals => 6,
            _ => 0,
        }
    }
}

/// Split a line into numeric fields. Commas are separators too, because plenty of exporters write
/// CSV and call it `.xyz`.
fn fields(line: &str) -> Vec<&str> {
    line.split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|f| !f.is_empty())
        .collect()
}

fn is_comment(line: &str) -> bool {
    line.is_empty() || line.starts_with('#') || line.starts_with("//") || line.starts_with(';')
}

/// Decide the column layout from the first rows that carry data.
///
/// `samples` is a handful of parsed rows, not the whole file: the decision only needs enough
/// values to tell a byte from a unit vector, and reading the file twice to be sure would cost more
/// than the ambiguity is worth.
pub fn layout_for(columns: usize, samples: &[Vec<f64>]) -> Option<Layout> {
    let byte_like = |at: usize| {
        samples.iter().all(|row| {
            row.get(at..at + 3).is_some_and(|c| {
                c.iter()
                    .all(|v| *v >= 0.0 && *v <= 255.0 && v.fract() == 0.0)
            })
        })
    };
    // A normal is a unit vector, so no component leaves -1 to 1 and at least one row has to carry
    // something that is not a whole number. An all-integer triple of 0 and 1 is colour.
    let normal_like = |at: usize| {
        samples.iter().all(|row| {
            row.get(at..at + 3)
                .is_some_and(|c| c.iter().all(|v| (-1.001..=1.001).contains(v)))
        }) && samples.iter().any(|row| {
            row.get(at..at + 3)
                .is_some_and(|c| c.iter().any(|v| v.fract() != 0.0))
        })
    };

    Some(match columns {
        3 => Layout::Positions,
        4 => Layout::Intensity,
        6 if normal_like(3) => Layout::Normals,
        6 if byte_like(3) => Layout::Colour,
        // Neither test fits: floats outside the unit range are more plausible as colour scaled
        // some other way than as a normal, and treating them as an unknown trailing field loses
        // nothing.
        6 => Layout::Positions,
        7 => Layout::IntensityColour,
        9 => Layout::ColourNormals,
        // 5, 8 and anything above 9 have no convention worth guessing at. The positions are still
        // good, so the extra columns are dropped rather than the file refused.
        n if n > 3 => Layout::Positions,
        _ => return None,
    })
}

pub fn import(bytes: &[u8], path: Option<&Path>) -> Result<Scene, ImportError> {
    let text = std::str::from_utf8(bytes).map_err(|_| malformed("file is not UTF-8"))?;

    // Accumulate in f64. A survey easting is nine significant digits and the subtraction that
    // makes it renderable has to happen before anything narrows.
    let mut xs: Vec<f64> = Vec::new();
    let mut ys: Vec<f64> = Vec::new();
    let mut zs: Vec<f64> = Vec::new();
    let mut extra: Vec<f32> = Vec::new();
    let mut samples: Vec<Vec<f64>> = Vec::new();
    let mut layout: Option<Layout> = None;
    let mut columns = 0usize;
    let mut skipped = 0usize;
    let mut truncated = false;

    for (number, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if is_comment(line) {
            continue;
        }
        let cols = fields(line);

        // A bare count on the first data line is how Leica `.pts` starts. Only the first one is
        // allowed to be a header; a later single number is a malformed row.
        if cols.len() == 1 && xs.is_empty() && cols[0].parse::<u64>().is_ok() {
            continue;
        }

        let mut values = Vec::with_capacity(cols.len());
        let mut ok = cols.len() >= 3;
        for f in &cols {
            match f.parse::<f64>() {
                Ok(v) if v.is_finite() => values.push(v),
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            // Tolerated rather than fatal: trailing junk, a stray label column and a truncated
            // last line are all common, and refusing the file over one row would be worse than
            // dropping it. A file that is mostly unparseable still fails, below.
            skipped += 1;
            continue;
        }

        // The first data row fixes the width, and every later row has to match it. Deciding this
        // before the layout is known matters: a short file resolves its layout after the loop, and
        // without this a row of a different width would leave `extra` ragged while `stride` said
        // otherwise, which misaligns every colour and normal after it.
        if columns == 0 {
            columns = values.len();
        } else if values.len() != columns {
            skipped += 1;
            continue;
        }

        if layout.is_none() {
            samples.push(values.clone());
            if samples.len() >= 8 {
                layout = layout_for(columns, &samples);
                if layout.is_none() {
                    return Err(malformed(format!(
                        "line {} has {columns} columns, which is not a point",
                        number + 1
                    )));
                }
            }
        }

        if xs.len() >= MAX_POINTS {
            truncated = true;
            break;
        }
        xs.push(values[0]);
        ys.push(values[1]);
        zs.push(values[2]);
        // Held as f32 in source order; the layout decides what it means once it is known.
        extra.extend(values[3..].iter().map(|v| *v as f32));
    }

    // Files shorter than the sampling window never resolved a layout above.
    let layout = match layout {
        Some(l) => l,
        None if !samples.is_empty() => layout_for(columns, &samples)
            .ok_or_else(|| malformed(format!("{columns} columns is not a point")))?,
        None => return Err(malformed("no numeric rows")),
    };

    let count = xs.len();
    if count == 0 {
        return Err(malformed("no points"));
    }
    if skipped > count {
        return Err(malformed(format!(
            "{skipped} unreadable rows against {count} points; this is not a point list"
        )));
    }

    let centre = [mean(&xs), mean(&ys), mean(&zs)];
    let mut bounds = Aabb::EMPTY;
    let mut positions = Vec::with_capacity(count * 12);
    for i in 0..count {
        let p = [
            (xs[i] - centre[0]) as f32,
            (ys[i] - centre[1]) as f32,
            (zs[i] - centre[2]) as f32,
        ];
        bounds.expand(p);
        for c in p {
            positions.extend_from_slice(&c.to_le_bytes());
        }
    }

    let mut attributes =
        vec![
            Attribute::new(Semantic::Position, ComponentType::F32, 3, count, positions)
                .map_err(|e| malformed(e.to_string()))?,
        ];

    let stride = columns - 3;
    if layout.has_colour() && stride > 0 {
        let at = layout.colour_at() - 3;
        let mut rgb = Vec::with_capacity(count * 3);
        for i in 0..count {
            for c in 0..3 {
                let v = extra.get(i * stride + at + c).copied().unwrap_or(0.0);
                rgb.push(v.clamp(0.0, 255.0) as u8);
            }
        }
        attributes.push(
            Attribute::new(Semantic::Color(0), ComponentType::U8, 3, count, rgb)
                .map_err(|e| malformed(e.to_string()))?
                .normalized(),
        );
    }

    if layout.has_normals() && stride > 0 {
        let at = layout.normal_at() - 3;
        let mut normals = Vec::with_capacity(count * 12);
        for i in 0..count {
            for c in 0..3 {
                let v = extra.get(i * stride + at + c).copied().unwrap_or(0.0);
                normals.extend_from_slice(&v.to_le_bytes());
            }
        }
        attributes.push(
            Attribute::new(Semantic::Normal, ComponentType::F32, 3, count, normals)
                .map_err(|e| malformed(e.to_string()))?,
        );
    }

    let mut scene = Scene::new();
    let name = path
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("points");
    scene.push_root(Node::with_payload(
        name,
        Payload::PointCloud(PointCloud {
            attributes,
            count,
            bounds,
        }),
    ));

    scene.set_meta("format", "ASCII point cloud");
    scene.set_meta("columns", layout.label());
    if skipped > 0 {
        scene.set_meta("skipped rows", skipped.to_string());
    }
    if truncated {
        scene.set_meta("truncated at", MAX_POINTS.to_string());
    }
    // The subtraction that made the cloud renderable, so the original coordinates are recoverable
    // from what is on screen.
    if centre.iter().any(|c| c.abs() > 1.0) {
        scene.set_meta(
            "recentred by",
            format!("{:.3} {:.3} {:.3}", -centre[0], -centre[1], -centre[2]),
        );
    }
    Ok(scene)
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use viewer_scene::payload::Payload;

    fn cloud(scene: &Scene) -> &PointCloud {
        scene
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Some(Payload::PointCloud(p)) => Some(p),
                _ => None,
            })
            .expect("a point cloud")
    }

    #[test]
    fn three_columns_are_positions() {
        let text = "1 2 3\n4 5 6\n7 8 9\n";
        let scene = import(text.as_bytes(), None).unwrap();
        assert_eq!(cloud(&scene).count, 3);
        assert_eq!(
            scene.meta.iter().find(|(k, _)| k == "columns").unwrap().1,
            "x y z"
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let text = "# a comment\n\n1 2 3\n// another\n4 5 6\n; and another\n7 8 9\n";
        assert_eq!(cloud(&import(text.as_bytes(), None).unwrap()).count, 3);
    }

    #[test]
    fn a_leading_count_is_a_header_not_a_point() {
        let text = "3\n1 2 3\n4 5 6\n7 8 9\n";
        assert_eq!(cloud(&import(text.as_bytes(), None).unwrap()).count, 3);
    }

    #[test]
    fn commas_separate_as_well_as_spaces() {
        let text = "1,2,3\n4,5,6\n7,8,9\n";
        assert_eq!(cloud(&import(text.as_bytes(), None).unwrap()).count, 3);
    }

    #[test]
    fn six_integer_columns_in_byte_range_are_colour() {
        let rows: String = (0..10)
            .map(|i| format!("{i} {i} {i} 255 128 0\n"))
            .collect();
        let scene = import(rows.as_bytes(), None).unwrap();
        let c = cloud(&scene);
        assert!(c.attribute(&Semantic::Color(0)).is_some());
        assert!(c.attribute(&Semantic::Normal).is_none());
    }

    #[test]
    fn six_fractional_columns_in_unit_range_are_normals() {
        let rows: String = (0..10)
            .map(|i| format!("{i} {i} {i} 0.0 0.7071 -0.7071\n"))
            .collect();
        let scene = import(rows.as_bytes(), None).unwrap();
        let c = cloud(&scene);
        assert!(c.attribute(&Semantic::Normal).is_some());
        assert!(c.attribute(&Semantic::Color(0)).is_none());
    }

    #[test]
    fn seven_columns_are_the_leica_layout() {
        let rows: String = (0..10)
            .map(|i| format!("{i} {i} {i} -1500 12 34 56\n"))
            .collect();
        let scene = import(rows.as_bytes(), None).unwrap();
        assert!(cloud(&scene).attribute(&Semantic::Color(0)).is_some());
    }

    #[test]
    fn huge_coordinates_are_recentred_before_they_narrow() {
        // A UTM easting with a centimetre step. Without recentring in f64 the step is lost.
        let text = "500000.00 4600000.00 100.00\n500000.01 4600000.00 100.00\n\
                    500000.02 4600000.00 100.00\n";
        let scene = import(text.as_bytes(), None).unwrap();
        let c = cloud(&scene);
        let mut a = [0.0f32; 3];
        let mut b = [0.0f32; 3];
        c.attribute(&Semantic::Position).unwrap().get_f32(0, &mut a);
        c.attribute(&Semantic::Position).unwrap().get_f32(1, &mut b);
        assert!((b[0] - a[0]).abs() > 0.005, "{a:?} against {b:?}");
        assert!(scene.meta.iter().any(|(k, _)| k == "recentred by"));
    }

    #[test]
    fn a_file_of_prose_is_refused() {
        let text = "the quick brown fox\njumped over\nthe lazy dog\n";
        assert!(import(text.as_bytes(), None).is_err());
    }

    #[test]
    fn a_few_bad_rows_are_dropped_rather_than_fatal() {
        let text = "1 2 3\nnot a point\n4 5 6\n7 8 9\n";
        let scene = import(text.as_bytes(), None).unwrap();
        assert_eq!(cloud(&scene).count, 3);
        assert_eq!(
            scene
                .meta
                .iter()
                .find(|(k, _)| k == "skipped rows")
                .unwrap()
                .1,
            "1"
        );
    }

    #[test]
    fn a_row_of_a_different_width_is_dropped_even_before_the_layout_is_known() {
        // Short enough that the layout is decided after the loop rather than during it. The wide
        // row must not widen the stride, or every colour after it reads from the wrong offset.
        let text = "1 2 3 10 20 30\n4 5 6 40 50 60\n7 8 9 70 80 90 99\n";
        let scene = import(text.as_bytes(), None).unwrap();
        let c = cloud(&scene);
        assert_eq!(c.count, 2);
        assert_eq!(
            scene
                .meta
                .iter()
                .find(|(k, _)| k == "skipped rows")
                .unwrap()
                .1,
            "1"
        );
        let colour = c.attribute(&Semantic::Color(0)).unwrap();
        assert_eq!(&colour.data[..6], &[10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn colour_lands_at_the_right_offset_in_the_seven_column_layout() {
        // stride is 4 and the colour starts one past the intensity, so an off-by-one here shows up
        // as the intensity bleeding into the red channel.
        let rows: String = (0..10)
            .map(|i| format!("{i} {i} {i} -1500 11 22 33\n"))
            .collect();
        let scene = import(rows.as_bytes(), None).unwrap();
        let colour = cloud(&scene).attribute(&Semantic::Color(0)).unwrap();
        assert_eq!(&colour.data[..3], &[11, 22, 33]);
    }

    #[test]
    fn colour_and_normals_together_land_at_the_right_offsets() {
        let rows: String = (0..10)
            .map(|i| format!("{i} {i} {i} 11 22 33 0.0 0.5 -0.5\n"))
            .collect();
        let scene = import(rows.as_bytes(), None).unwrap();
        let c = cloud(&scene);
        assert_eq!(
            &c.attribute(&Semantic::Color(0)).unwrap().data[..3],
            &[11, 22, 33]
        );
        let mut n = [0.0f32; 3];
        c.attribute(&Semantic::Normal).unwrap().get_f32(0, &mut n);
        assert_eq!(n, [0.0, 0.5, -0.5]);
    }

    #[test]
    fn an_empty_file_is_refused() {
        assert!(import(b"", None).is_err());
        assert!(import(b"# only a comment\n", None).is_err());
    }
}
