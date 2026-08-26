//! 3MF — the 3D Manufacturing Format.
//!
//! A ZIP with an XML model inside it, which is a pleasant change from most of this directory. The
//! parts that need care are the two that a naive reader gets wrong quietly:
//!
//! - **Units.** 3MF declares its own — micron through metre, and inches and feet. A printer file
//!   in millimetres imported as metres is a thousand times too big, which reads as "the model
//!   failed to load" because the camera is inside a wall of triangle.
//! - **The object graph.** An object is either a mesh *or* a list of components referencing other
//!   objects, each with its own transform, and the `<build>` section then places objects again
//!   with transforms of their own. Ignoring either level puts parts in a heap at the origin.
//!
//! The model part is located through `_rels/.rels` rather than assumed to be `3D/3dmodel.model`.
//! The conventional path is nearly universal, and the relationship file is the one the
//! specification actually points at.

use std::collections::HashMap;
use std::io::Read;

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{Aabb, Material, Node, Payload, Scene};

use crate::ImportError;

/// Refuse a model that declares more geometry than any printer file plausibly holds, before the
/// declaration becomes an allocation.
const MAX_VERTICES: usize = 50_000_000;

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::ThreeMf,
        reason: reason.into(),
    }
}

/// Metres per unit, for every unit 3MF allows.
fn scale_for(unit: &str) -> f32 {
    match unit {
        "micron" => 1e-6,
        "millimeter" => 1e-3,
        "centimeter" => 1e-2,
        "inch" => 0.0254,
        "foot" => 0.3048,
        "meter" => 1.0,
        // The specification's default, and what a file that omits the attribute means.
        _ => 1e-3,
    }
}

/// A 4x3 transform as 3MF writes it: three basis vectors then a translation, row-major.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Transform([f32; 12]);

impl Transform {
    const IDENTITY: Transform = Transform([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);

    fn parse(text: &str) -> Option<Transform> {
        let mut values = [0.0f32; 12];
        let mut n = 0;
        for token in text.split_whitespace() {
            if n == 12 {
                return None;
            }
            values[n] = token.parse().ok()?;
            n += 1;
        }
        (n == 12).then_some(Transform(values))
    }

    fn apply(&self, p: [f32; 3]) -> [f32; 3] {
        let m = &self.0;
        [
            p[0] * m[0] + p[1] * m[3] + p[2] * m[6] + m[9],
            p[0] * m[1] + p[1] * m[4] + p[2] * m[7] + m[10],
            p[0] * m[2] + p[1] * m[5] + p[2] * m[8] + m[11],
        ]
    }

    /// `self` applied after `outer` — the composition a nested component needs.
    fn compose(&self, outer: &Transform) -> Transform {
        let mut out = [0.0f32; 12];
        for row in 0..3 {
            for col in 0..3 {
                out[row * 3 + col] = (0..3)
                    .map(|k| self.0[row * 3 + k] * outer.0[k * 3 + col])
                    .sum();
            }
        }
        for col in 0..3 {
            out[9 + col] = (0..3)
                .map(|k| self.0[9 + k] * outer.0[k * 3 + col])
                .sum::<f32>()
                + outer.0[9 + col];
        }
        Transform(out)
    }
}

/// One `<object>`: either a mesh, or a list of references to other objects.
enum Object {
    Mesh {
        vertices: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
        /// Material index, resolved from `pid`/`pindex`.
        material: Option<u32>,
    },
    Components(Vec<(u32, Transform)>),
}

pub fn import(bytes: &[u8]) -> Result<Scene, ImportError> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| malformed(format!("not a readable ZIP: {e}")))?;

    let model_path = model_part(&mut archive)
        .unwrap_or_else(|| "3D/3dmodel.model".to_string());
    let xml = read_part(&mut archive, &model_path).ok_or_else(|| {
        malformed(format!("the archive has no model part at {model_path:?}"))
    })?;
    let document = roxmltree::Document::parse(&xml)
        .map_err(|e| malformed(format!("the model part is not valid XML: {e}")))?;

    let root = document.root_element();
    let scale = scale_for(root.attribute("unit").unwrap_or("millimeter"));

    let mut materials: Vec<Material> = Vec::new();
    // Maps a `pid` plus a `pindex` onto our own flat material table.
    let mut material_index: HashMap<(u32, u32), u32> = HashMap::new();
    let mut objects: HashMap<u32, Object> = HashMap::new();

    for resources in root.children().filter(|n| n.has_tag_name("resources")) {
        for node in resources.children().filter(|n| n.is_element()) {
            match node.tag_name().name() {
                "basematerials" | "colorgroup" => {
                    let Some(id) = attribute_u32(&node, "id") else {
                        continue;
                    };
                    for (i, entry) in node
                        .children()
                        .filter(|n| n.is_element())
                        .enumerate()
                    {
                        let colour = entry
                            .attribute("displaycolor")
                            .or_else(|| entry.attribute("color"))
                            .and_then(parse_colour)
                            .unwrap_or([0.82, 0.82, 0.85, 1.0]);
                        material_index.insert((id, i as u32), materials.len() as u32);
                        materials.push(Material {
                            name: entry.attribute("name").unwrap_or("3mf").to_string(),
                            base_color: colour,
                            ..Default::default()
                        });
                    }
                }
                "object" => {
                    let Some(id) = attribute_u32(&node, "id") else {
                        continue;
                    };
                    if let Some(object) = parse_object(&node, &material_index)? {
                        objects.insert(id, object);
                    }
                }
                _ => {}
            }
        }
    }

    if objects.is_empty() {
        return Err(malformed("the model declares no objects"));
    }

    let mut scene = Scene::new();
    for material in materials {
        scene.push_material(material);
    }

    // The build section says what is actually placed. A file whose build section is empty still
    // has objects worth showing, so that falls back to every mesh at the origin.
    let mut placed = 0usize;
    for build in root.children().filter(|n| n.has_tag_name("build")) {
        for item in build.children().filter(|n| n.has_tag_name("item")) {
            let Some(id) = attribute_u32(&item, "objectid") else {
                continue;
            };
            let transform = item
                .attribute("transform")
                .and_then(Transform::parse)
                .unwrap_or(Transform::IDENTITY);
            placed += emit(&objects, id, transform, scale, &mut scene, 0);
        }
    }
    if placed == 0 {
        let mut ids: Vec<&u32> = objects.keys().collect();
        // Sorted so that a file with no build section still loads the same way twice.
        ids.sort_unstable();
        for id in ids {
            placed += emit(&objects, *id, Transform::IDENTITY, scale, &mut scene, 0);
        }
    }
    if placed == 0 {
        return Err(malformed("the model declares no drawable geometry"));
    }

    scene.set_meta("format", "3MF");
    scene.set_meta("unit", root.attribute("unit").unwrap_or("millimeter"));
    scene.set_meta("objects", placed.to_string());
    if !scene.materials.is_empty() {
        scene.set_meta("materials", scene.materials.len().to_string());
    }
    Ok(scene)
}

/// Follow `_rels/.rels` to the model part.
fn model_part<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>) -> Option<String> {
    const MODEL_RELATIONSHIP: &str =
        "http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel";
    let xml = read_part(archive, "_rels/.rels")?;
    let document = roxmltree::Document::parse(&xml).ok()?;
    for node in document.descendants().filter(|n| n.has_tag_name("Relationship")) {
        if node.attribute("Type") == Some(MODEL_RELATIONSHIP) {
            let target = node.attribute("Target")?;
            // Relationship targets are written with a leading slash as often as without.
            return Some(target.trim_start_matches('/').to_string());
        }
    }
    None
}

fn read_part<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<String> {
    let mut file = archive.by_name(name).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    Some(text)
}

fn parse_object(
    node: &roxmltree::Node<'_, '_>,
    material_index: &HashMap<(u32, u32), u32>,
) -> Result<Option<Object>, ImportError> {
    if let Some(components) = node.children().find(|n| n.has_tag_name("components")) {
        let list = components
            .children()
            .filter(|n| n.has_tag_name("component"))
            .filter_map(|c| {
                let id = attribute_u32(&c, "objectid")?;
                let transform = c
                    .attribute("transform")
                    .and_then(Transform::parse)
                    .unwrap_or(Transform::IDENTITY);
                Some((id, transform))
            })
            .collect();
        return Ok(Some(Object::Components(list)));
    }

    let Some(mesh) = node.children().find(|n| n.has_tag_name("mesh")) else {
        return Ok(None);
    };

    let material = attribute_u32(node, "pid")
        .and_then(|pid| {
            let index = attribute_u32(node, "pindex").unwrap_or(0);
            material_index.get(&(pid, index)).copied()
        });

    let mut vertices = Vec::new();
    if let Some(list) = mesh.children().find(|n| n.has_tag_name("vertices")) {
        for vertex in list.children().filter(|n| n.has_tag_name("vertex")) {
            if vertices.len() >= MAX_VERTICES {
                return Err(malformed("the model declares an implausible vertex count"));
            }
            let coordinate = |name: &str| -> f32 {
                vertex
                    .attribute(name)
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(0.0)
            };
            vertices.push([coordinate("x"), coordinate("y"), coordinate("z")]);
        }
    }

    let mut triangles = Vec::new();
    if let Some(list) = mesh.children().find(|n| n.has_tag_name("triangles")) {
        for triangle in list.children().filter(|n| n.has_tag_name("triangle")) {
            let index = |name: &str| -> Option<u32> {
                let v = attribute_u32(&triangle, name)?;
                // An index out of range is a corrupt file, not a triangle.
                ((v as usize) < vertices.len()).then_some(v)
            };
            if let (Some(a), Some(b), Some(c)) = (index("v1"), index("v2"), index("v3")) {
                triangles.push([a, b, c]);
            }
        }
    }

    Ok(Some(Object::Mesh {
        vertices,
        triangles,
        material,
    }))
}

/// Walk one object into the scene, following components and composing their transforms.
fn emit(
    objects: &HashMap<u32, Object>,
    id: u32,
    transform: Transform,
    scale: f32,
    scene: &mut Scene,
    depth: u32,
) -> usize {
    // A file may reference itself. Real nesting is two or three deep.
    if depth > 16 {
        return 0;
    }
    let Some(object) = objects.get(&id) else {
        return 0;
    };
    match object {
        Object::Components(list) => list
            .iter()
            .map(|(child, child_transform)| {
                emit(
                    objects,
                    *child,
                    child_transform.compose(&transform),
                    scale,
                    scene,
                    depth + 1,
                )
            })
            .sum(),
        Object::Mesh {
            vertices,
            triangles,
            material,
        } => {
            if vertices.is_empty() || triangles.is_empty() {
                return 0;
            }
            let mut bounds = Aabb::EMPTY;
            let mut positions = Vec::with_capacity(vertices.len() * 12);
            for v in vertices {
                let world = transform.apply(*v);
                let scaled = [world[0] * scale, world[1] * scale, world[2] * scale];
                bounds.expand(scaled);
                for c in scaled {
                    positions.extend_from_slice(&c.to_le_bytes());
                }
            }
            let Ok(attribute) = Attribute::new(
                Semantic::Position,
                ComponentType::F32,
                3,
                vertices.len(),
                positions,
            ) else {
                return 0;
            };
            scene.push_root(Node::with_payload(
                format!("object {id}"),
                Payload::TriMesh(TriMesh {
                    attributes: vec![attribute],
                    indices: Indices::U32(triangles.iter().flatten().copied().collect()),
                    bounds,
                    material: *material,
                }),
            ));
            1
        }
    }
}

fn attribute_u32(node: &roxmltree::Node<'_, '_>, name: &str) -> Option<u32> {
    node.attribute(name)?.trim().parse().ok()
}

/// `#RRGGBB` or `#RRGGBBAA`, which is how 3MF writes both base materials and colour groups.
fn parse_colour(text: &str) -> Option<[f32; 4]> {
    let hex = text.trim().strip_prefix('#')?;
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok();
    let (r, g, b) = (channel(0)?, channel(1)?, channel(2)?);
    let a = if hex.len() == 8 { channel(3)? } else { 255 };
    // 3MF colours are sRGB, matching the base colour factor the renderer expects.
    Some([
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Wrap a model part in the minimum ZIP a 3MF needs.
    fn package(model: &str) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("_rels/.rels", options).unwrap();
            zip.write_all(
                br#"<?xml version="1.0" encoding="UTF-8"?>
                <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
                  <Relationship Id="rel0" Target="/3D/3dmodel.model"
                    Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
                </Relationships>"#,
            )
            .unwrap();
            zip.start_file("3D/3dmodel.model", options).unwrap();
            zip.write_all(model.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buffer
    }

    /// A 10-unit square, in whatever unit is asked for.
    fn cube_model(unit: &str, transform: Option<&str>) -> String {
        let item = match transform {
            Some(t) => format!(r#"<item objectid="1" transform="{t}"/>"#),
            None => r#"<item objectid="1"/>"#.to_string(),
        };
        format!(
            r##"<?xml version="1.0" encoding="UTF-8"?>
<model unit="{unit}" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources>
    <basematerials id="5">
      <base name="red" displaycolor="#FF0000FF"/>
      <base name="green" displaycolor="#00FF00"/>
    </basematerials>
    <object id="1" type="model" pid="5" pindex="1">
      <mesh>
        <vertices>
          <vertex x="0" y="0" z="0"/><vertex x="10" y="0" z="0"/>
          <vertex x="10" y="10" z="0"/><vertex x="0" y="10" z="0"/>
        </vertices>
        <triangles>
          <triangle v1="0" v2="1" v3="2"/><triangle v1="0" v2="2" v3="3"/>
        </triangles>
      </mesh>
    </object>
  </resources>
  <build>{item}</build>
</model>"##
        )
    }

    fn mesh(scene: &Scene) -> &TriMesh {
        scene
            .nodes
            .iter()
            .find_map(|n| match &n.payload {
                Some(Payload::TriMesh(m)) => Some(m),
                _ => None,
            })
            .expect("expected a mesh")
    }

    #[test]
    fn a_millimetre_model_arrives_in_metres() {
        // The whole point of reading the unit: a 10 mm square is 0.01 m across, not 10.
        let scene = import(&package(&cube_model("millimeter", None))).expect("a valid 3MF");
        let m = mesh(&scene);
        assert_eq!(m.triangle_count(), 2);
        assert!((m.bounds.extent()[0] - 0.01).abs() < 1e-6, "{:?}", m.bounds);
    }

    #[test]
    fn every_unit_the_specification_allows_is_converted() {
        for (unit, expected) in [
            ("micron", 1e-5),
            ("millimeter", 0.01),
            ("centimeter", 0.1),
            ("meter", 10.0),
            ("inch", 0.254),
            ("foot", 3.048),
        ] {
            let scene = import(&package(&cube_model(unit, None))).unwrap();
            let width = mesh(&scene).bounds.extent()[0];
            assert!(
                (width - expected).abs() < expected * 1e-4,
                "{unit}: {width} vs {expected}"
            );
        }
    }

    #[test]
    fn a_missing_unit_attribute_defaults_to_millimetres() {
        let model = cube_model("millimeter", None).replace(r#"unit="millimeter" "#, "");
        let scene = import(&package(&model)).unwrap();
        assert!((mesh(&scene).bounds.extent()[0] - 0.01).abs() < 1e-6);
    }

    #[test]
    fn a_build_transform_places_the_object() {
        // Identity basis, translated 100 mm along x.
        let scene = import(&package(&cube_model(
            "millimeter",
            Some("1 0 0 0 1 0 0 0 1 100 0 0"),
        )))
        .unwrap();
        let b = mesh(&scene).bounds;
        assert!((b.min[0] - 0.1).abs() < 1e-6, "{b:?}");
        assert!((b.max[0] - 0.11).abs() < 1e-6, "{b:?}");
    }

    #[test]
    fn the_material_comes_from_pid_and_pindex() {
        let scene = import(&package(&cube_model("millimeter", None))).unwrap();
        assert_eq!(scene.materials.len(), 2);
        // pindex 1 is the second base: green.
        let material = scene
            .material(mesh(&scene).material)
            .expect("the object named a material");
        assert_eq!(material.name, "green");
        assert!(material.base_color[1] > 0.9 && material.base_color[0] < 0.1);
    }

    #[test]
    fn colours_parse_with_and_without_an_alpha_channel() {
        assert_eq!(parse_colour("#FF0000"), Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(
            parse_colour("#00FF0080").map(|c| c[3] > 0.4 && c[3] < 0.6),
            Some(true)
        );
        assert_eq!(parse_colour("not a colour"), None);
        assert_eq!(parse_colour("#FFF"), None);
    }

    #[test]
    fn components_compose_their_transforms_with_the_build_items() {
        // An object made of one component, itself translated, placed by a translated build item.
        // Both offsets have to apply, or parts land in a heap at the origin.
        let model = r#"<?xml version="1.0"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources>
    <object id="1"><mesh>
      <vertices><vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x="0" y="1" z="0"/></vertices>
      <triangles><triangle v1="0" v2="1" v3="2"/></triangles>
    </mesh></object>
    <object id="2"><components>
      <component objectid="1" transform="1 0 0 0 1 0 0 0 1 10 0 0"/>
    </components></object>
  </resources>
  <build><item objectid="2" transform="1 0 0 0 1 0 0 0 1 100 0 0"/></build>
</model>"#;
        let scene = import(&package(model)).unwrap();
        let b = mesh(&scene).bounds;
        assert!((b.min[0] - 0.110).abs() < 1e-6, "expected 110 mm, got {b:?}");
    }

    #[test]
    fn a_self_referencing_component_terminates() {
        let model = r#"<?xml version="1.0"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources>
    <object id="1"><components><component objectid="1"/></components></object>
  </resources>
  <build><item objectid="1"/></build>
</model>"#;
        // No geometry to find, but it must return rather than recurse forever.
        assert!(import(&package(model)).is_err());
    }

    #[test]
    fn a_model_with_no_build_section_still_shows_its_objects() {
        let model =
            cube_model("millimeter", None).replace(r#"<build><item objectid="1"/></build>"#, "<build/>");
        let scene = import(&package(&model)).expect("objects are worth showing regardless");
        assert_eq!(mesh(&scene).triangle_count(), 2);
    }

    #[test]
    fn out_of_range_triangle_indices_are_dropped_not_drawn() {
        let model = r#"<?xml version="1.0"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
  <resources><object id="1"><mesh>
    <vertices><vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x="0" y="1" z="0"/></vertices>
    <triangles><triangle v1="0" v2="1" v3="2"/><triangle v1="0" v2="1" v3="99"/></triangles>
  </mesh></object></resources>
  <build><item objectid="1"/></build>
</model>"#;
        let scene = import(&package(model)).unwrap();
        assert_eq!(mesh(&scene).triangle_count(), 1);
    }

    #[test]
    fn a_file_that_is_not_a_zip_or_has_no_model_is_refused() {
        assert!(import(b"not a zip at all").is_err());
        let mut buffer = Vec::new();
        {
            let zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
            zip.finish().unwrap();
        }
        assert!(import(&buffer).is_err());
    }

    #[test]
    fn transform_composition_matches_applying_them_in_order() {
        let inner = Transform::parse("1 0 0 0 1 0 0 0 1 1 2 3").unwrap();
        let outer = Transform::parse("2 0 0 0 2 0 0 0 2 10 0 0").unwrap();
        let composed = inner.compose(&outer);
        let point = [1.0, 1.0, 1.0];
        assert_eq!(composed.apply(point), outer.apply(inner.apply(point)));
    }

    #[test]
    fn a_transform_with_the_wrong_number_of_values_is_ignored() {
        assert_eq!(Transform::parse("1 0 0"), None);
        assert_eq!(Transform::parse("1 0 0 0 1 0 0 0 1 0 0 0 0"), None);
        assert_eq!(Transform::parse("a b c d e f g h i j k l"), None);
    }
}
