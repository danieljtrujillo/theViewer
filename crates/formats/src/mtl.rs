//! Wavefront MTL parser.
//!
//! MTL predates PBR, so its values map onto the metallic-roughness set rather than transferring
//! directly. `Kd` becomes base colour, `Ns` becomes roughness through the Blinn-Phong inverse, and
//! `d` or `Tr` becomes alpha. `Ka` is discarded: an ambient term shading nothing is meaningless
//! under any modern lighting model, and honouring it washes models out.
//!
//! Map lines carry options before the file name (`-s 1 1 1`, `-bm 0.5`, `-o 0 0 0`). Every token
//! starting with `-` and its arguments are skipped, and the remainder is the path, which may
//! itself contain spaces.

use std::collections::HashMap;
use std::path::Path;

use viewer_scene::{AlphaMode, Material, Scene};

use crate::sidecar;

/// Options that take a known number of arguments. Anything else beginning with `-` is assumed to
/// take one.
fn option_arity(token: &str) -> usize {
    match token {
        "-s" | "-o" | "-t" => 3,
        "-mm" => 2,
        "-blendu" | "-blendv" | "-cc" | "-clamp" | "-texres" | "-bm" | "-imfchan" => 1,
        _ => 1,
    }
}

/// Strip leading options from a map line and return the file reference.
fn map_reference(rest: &str) -> Option<String> {
    let tokens: Vec<&str> = rest.split_ascii_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].starts_with('-') {
            i += 1 + option_arity(tokens[i]);
        } else {
            break;
        }
    }
    if i >= tokens.len() {
        return None;
    }
    // The remainder may contain spaces, so rejoin rather than taking one token.
    Some(tokens[i..].join(" "))
}

fn parse_f32(parts: &mut std::str::SplitAsciiWhitespace) -> Option<f32> {
    parts.next()?.parse().ok()
}

fn parse_rgb(parts: &mut std::str::SplitAsciiWhitespace) -> Option<[f32; 3]> {
    let r = parse_f32(parts)?;
    // `Kd 0.8` is legal and means grey.
    let g = parse_f32(parts).unwrap_or(r);
    let b = parse_f32(parts).unwrap_or(r);
    Some([r, g, b])
}

/// Parse an MTL file, appending materials and images to `scene`.
///
/// Returns a map from material name to index. Unreadable or missing files yield an empty map
/// rather than an error: a model with no materials is still worth showing.
pub fn parse(text: &str, base: &Path, scene: &mut Scene) -> HashMap<String, u32> {
    let mut names = HashMap::new();
    let mut current: Option<Material> = None;
    // One image per path, so a texture shared by several materials uploads once.
    let mut image_cache: HashMap<String, u32> = HashMap::new();

    let commit =
        |material: Option<Material>, scene: &mut Scene, names: &mut HashMap<String, u32>| {
            if let Some(m) = material {
                let name = m.name.clone();
                let index = scene.push_material(m);
                names.insert(name, index);
            }
        };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_ascii_whitespace();
        let Some(tag) = parts.next() else { continue };
        let rest = line[tag.len()..].trim();

        match tag {
            "newmtl" => {
                commit(current.take(), scene, &mut names);
                current = Some(Material::named(rest));
            }
            _ => {
                let Some(m) = current.as_mut() else { continue };
                match tag {
                    "Kd" => {
                        if let Some(c) = parse_rgb(&mut parts) {
                            m.base_color = [c[0], c[1], c[2], m.base_color[3]];
                        }
                    }
                    "Ke" => {
                        if let Some(c) = parse_rgb(&mut parts) {
                            m.emissive = c;
                        }
                    }
                    "Ns" => {
                        if let Some(ns) = parse_f32(&mut parts) {
                            m.roughness = Material::roughness_from_shininess(ns);
                        }
                    }
                    "Pm" => {
                        // PBR extension, written by newer exporters.
                        if let Some(v) = parse_f32(&mut parts) {
                            m.metallic = v.clamp(0.0, 1.0);
                        }
                    }
                    "Pr" => {
                        if let Some(v) = parse_f32(&mut parts) {
                            m.roughness = v.clamp(0.0, 1.0);
                        }
                    }
                    "d" | "Tr" => {
                        if let Some(v) = parse_f32(&mut parts) {
                            // `d` is opacity, `Tr` is transparency: the same number inverted.
                            let alpha = if tag == "Tr" { 1.0 - v } else { v };
                            m.base_color[3] = alpha.clamp(0.0, 1.0);
                            if alpha < 0.999 {
                                m.alpha_mode = AlphaMode::Blend;
                            }
                        }
                    }
                    "map_Kd" | "map_Ka" => {
                        if let Some(reference) = map_reference(rest) {
                            if let Some(index) =
                                load_cached(&reference, base, scene, &mut image_cache)
                            {
                                m.base_color_texture = Some(index);
                                // An exporter that writes both a white Kd and a texture is common;
                                // an exporter that writes black Kd and a texture is not, and
                                // multiplying would black the surface out.
                                if m.base_color[..3] == [0.0, 0.0, 0.0] {
                                    m.base_color = [1.0, 1.0, 1.0, m.base_color[3]];
                                }
                            }
                        }
                    }
                    // An alpha map implies cutout rather than fully opaque.
                    "map_d" if m.alpha_mode == AlphaMode::Opaque => {
                        m.alpha_mode = AlphaMode::Mask;
                    }
                    _ => {}
                }
            }
        }
    }
    commit(current.take(), scene, &mut names);
    names
}

fn load_cached(
    reference: &str,
    base: &Path,
    scene: &mut Scene,
    cache: &mut HashMap<String, u32>,
) -> Option<u32> {
    if let Some(index) = cache.get(reference) {
        return Some(*index);
    }
    let path = sidecar::resolve_texture(base, reference)?;
    let image = sidecar::load_image(&path)?;
    let index = scene.push_image(image);
    cache.insert(reference.to_string(), index);
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materials_are_named_and_indexed() {
        let mut scene = Scene::new();
        let names = parse(
            "newmtl red\nKd 1 0 0\nnewmtl blue\nKd 0 0 1\n",
            Path::new("."),
            &mut scene,
        );
        assert_eq!(scene.materials.len(), 2);
        assert_eq!(names["red"], 0);
        assert_eq!(names["blue"], 1);
        assert_eq!(scene.materials[1].base_color[2], 1.0);
    }

    #[test]
    fn a_single_kd_value_means_grey() {
        let mut scene = Scene::new();
        parse("newmtl g\nKd 0.5\n", Path::new("."), &mut scene);
        assert_eq!(scene.materials[0].base_color[..3], [0.5, 0.5, 0.5]);
    }

    #[test]
    fn d_and_tr_are_inverses_of_each_other() {
        let mut scene = Scene::new();
        parse(
            "newmtl a\nd 0.25\nnewmtl b\nTr 0.25\n",
            Path::new("."),
            &mut scene,
        );
        assert_eq!(scene.materials[0].base_color[3], 0.25);
        assert_eq!(scene.materials[1].base_color[3], 0.75);
        assert_eq!(scene.materials[0].alpha_mode, AlphaMode::Blend);
    }

    #[test]
    fn map_options_are_skipped_to_reach_the_file_name() {
        assert_eq!(map_reference("-s 1 1 1 atlas.png").unwrap(), "atlas.png");
        assert_eq!(map_reference("-bm 0.5 bump.tga").unwrap(), "bump.tga");
        assert_eq!(map_reference("-o 0 0 0 -s 2 2 2 t.png").unwrap(), "t.png");
        assert_eq!(map_reference("plain.png").unwrap(), "plain.png");
    }

    #[test]
    fn a_map_reference_may_contain_spaces() {
        assert_eq!(
            map_reference("-s 1 1 1 my texture.png").unwrap(),
            "my texture.png"
        );
    }

    #[test]
    fn a_map_line_with_only_options_yields_nothing() {
        assert!(map_reference("-s 1 1 1").is_none());
        assert!(map_reference("").is_none());
    }

    #[test]
    fn shininess_becomes_roughness() {
        let mut scene = Scene::new();
        parse("newmtl shiny\nNs 900\n", Path::new("."), &mut scene);
        assert!(scene.materials[0].roughness < 0.1);
    }

    #[test]
    fn a_directive_before_any_newmtl_is_ignored_rather_than_panicking() {
        let mut scene = Scene::new();
        let names = parse("Kd 1 0 0\nNs 50\n", Path::new("."), &mut scene);
        assert!(names.is_empty());
        assert!(scene.materials.is_empty());
    }
}
