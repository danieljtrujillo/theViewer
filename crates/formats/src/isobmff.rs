//! Just enough MP4/MOV box walking to answer three questions.
//!
//! Is this a 360 video, how are its two eyes packed, and is it really a video at all? Everything
//! else about the file is the platform decoder's business, which is why there is no demuxer here.
//!
//! Both spherical conventions are read, because both are in the wild:
//!
//! - **V1 (2015)**: a `uuid` box holding an XML blob. Written by every 360 camera of that era and
//!   still by ffmpeg's `-metadata` path.
//! - **V2 (2017)**: real boxes: `st3d` for the stereo mode, `sv3d`/`proj`/`equi` for the
//!   projection and its bounds. What YouTube ingests and what modern tools write.
//!
//! A file that has been through both pipelines carries both, and they can disagree. V2 wins: it is
//! the current spec, and a V1 blob is usually a stale copy carried along by a transcode.

use viewer_scene::StereoLayout;

use crate::spherical::{Evidence, Spherical};

/// How deep to follow nested boxes. The real nesting depth to `equi` is seven; twelve leaves room
/// for oddities without letting a crafted file walk forever.
const MAX_DEPTH: u32 = 12;

/// Total bytes of box headers to walk before giving up.
///
/// A `mdat` is skipped by its size field rather than scanned, so this budget is spent purely on
/// structure. A file that burns it is malformed or hostile.
const MAX_BOXES: u32 = 20_000;

/// The Spherical Video V1 `uuid`: ffcc8263-f855-4a93-8814-587a02521fdd.
const SPHERICAL_V1_UUID: [u8; 16] = [
    0xFF, 0xCC, 0x82, 0x63, 0xF8, 0x55, 0x4A, 0x93, 0x88, 0x14, 0x58, 0x7A, 0x02, 0x52, 0x1F, 0xDD,
];

/// Boxes whose payload is more boxes. Anything not on this list is skipped by its size.
const CONTAINERS: [&[u8; 4]; 9] = [
    b"moov", b"trak", b"mdia", b"minf", b"stbl", b"udta", b"sv3d", b"proj", b"edts",
];

/// Visual sample entries: a 78-byte fixed prologue, then child boxes. `st3d` and `sv3d` live
/// inside these, which is the part of the walk that is easy to get wrong.
const SAMPLE_ENTRIES: [&[u8; 4]; 10] = [
    b"avc1", b"avc3", b"hev1", b"hvc1", b"av01", b"vp08", b"vp09", b"mp4v", b"encv", b"dvh1",
];

#[derive(Debug, Default, Clone, PartialEq)]
struct Found {
    /// V2 `st3d` stereo mode, if present.
    stereo: Option<StereoLayout>,
    /// V2 `equi` seen: the track declares equirectangular projection.
    equirect_v2: bool,
    /// V2 `equi` bounds, as fractions of the full sphere: left and right insets.
    equi_bounds: Option<(f32, f32)>,
    /// V1 XML blob, if present.
    xml: Option<String>,
}

/// Read whatever the container says about projection and stereo layout.
pub fn spherical(bytes: &[u8]) -> Option<Spherical> {
    let mut found = Found::default();
    let mut budget = MAX_BOXES;
    walk(bytes, 0, &mut found, &mut budget);

    if found.equirect_v2 {
        // `equi` bounds are insets into the full sphere; a full panorama declares zero for all
        // four. A cropped one covers only the arc between them.
        let arc = match found.equi_bounds {
            Some((left, right)) => ((1.0 - left - right).clamp(0.0, 1.0) * 360.0).max(1.0),
            None => 360.0,
        };
        return Some(Spherical::equirect(
            arc,
            found.stereo.unwrap_or(StereoLayout::Mono),
            Evidence::Metadata,
        ));
    }

    if let Some(xml) = &found.xml {
        if let Some(mut s) = spherical_v1(xml) {
            // A V2 `st3d` alongside a V1 blob is still the better authority on packing.
            if let Some(stereo) = found.stereo {
                s.stereo = stereo;
            }
            return Some(s);
        }
    }

    // `st3d` without a projection box means stereo flat video -- a 3D movie, not a panorama.
    // Worth reporting so the viewer shows one eye rather than a squashed pair.
    if let Some(stereo) = found.stereo {
        if stereo != StereoLayout::Mono {
            return Some(Spherical {
                projection: viewer_scene::ScreenProjection::Flat,
                stereo,
                evidence: Evidence::Metadata,
            });
        }
    }
    None
}

/// The V1 XML blob uses the `GSpherical` namespace rather than `GPano`, but the same vocabulary.
fn spherical_v1(xml: &str) -> Option<Spherical> {
    let enabled = element(xml, "GSpherical:Spherical")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !enabled {
        return None;
    }
    let projection = element(xml, "GSpherical:ProjectionType").unwrap_or("equirectangular");
    if !projection.eq_ignore_ascii_case("equirectangular") {
        return None;
    }
    let stereo = match element(xml, "GSpherical:StereoMode") {
        Some(m) if m.eq_ignore_ascii_case("top-bottom") => StereoLayout::OverUnder,
        Some(m) if m.eq_ignore_ascii_case("left-right") => StereoLayout::SideBySide,
        _ => StereoLayout::Mono,
    };
    Some(Spherical::equirect(360.0, stereo, Evidence::Metadata))
}

fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let rest = &xml[start..];
    let end = rest.find('<')?;
    Some(rest[..end].trim())
}

/// Depth-first walk over one level of boxes.
fn walk(bytes: &[u8], depth: u32, found: &mut Found, budget: &mut u32) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut offset = 0usize;
    while offset + 8 <= bytes.len() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;

        let size32 = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as u64;
        let kind: [u8; 4] = [
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ];

        // Size 1 means the real size is a 64-bit field after the type; size 0 means "to the end
        // of the file", which only the last box may use.
        let (header, size) = match size32 {
            1 => {
                let Some(b) = bytes.get(offset + 8..offset + 16) else {
                    return;
                };
                (
                    16usize,
                    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
                )
            }
            0 => (8usize, (bytes.len() - offset) as u64),
            n => (8usize, n),
        };
        if size < header as u64 {
            return;
        }
        let end = offset.saturating_add(size as usize).min(bytes.len());
        let Some(payload) = bytes.get(offset + header..end) else {
            return;
        };

        match &kind {
            b"uuid" => {
                if payload.len() > 16 && payload[..16] == SPHERICAL_V1_UUID {
                    found.xml = Some(String::from_utf8_lossy(&payload[16..]).into_owned());
                }
            }
            b"st3d" => {
                // FullBox: version and flags, then one byte of stereo mode.
                if let Some(&mode) = payload.get(4) {
                    found.stereo = Some(match mode {
                        1 => StereoLayout::OverUnder,
                        2 => StereoLayout::SideBySide,
                        _ => StereoLayout::Mono,
                    });
                }
            }
            b"equi" => {
                found.equirect_v2 = true;
                // FullBox, then four 0.32 fixed-point bounds: top, bottom, left, right.
                if payload.len() >= 20 {
                    let read = |i: usize| -> f32 {
                        let o = 4 + i * 4;
                        u32::from_be_bytes([
                            payload[o],
                            payload[o + 1],
                            payload[o + 2],
                            payload[o + 3],
                        ]) as f32
                            / u32::MAX as f32
                    };
                    found.equi_bounds = Some((read(2), read(3)));
                }
            }
            b"stsd" => {
                // FullBox header plus a four-byte entry count, then the sample entries.
                if payload.len() > 8 {
                    walk(&payload[8..], depth + 1, found, budget);
                }
            }
            k if SAMPLE_ENTRIES.contains(&k) => {
                // 8 bytes of SampleEntry plus 70 of VisualSampleEntry before the child boxes.
                if payload.len() > 78 {
                    walk(&payload[78..], depth + 1, found, budget);
                }
            }
            k if CONTAINERS.contains(&k) => walk(payload, depth + 1, found, budget),
            _ => {}
        }

        // A box that does not advance would loop forever.
        let next = offset.saturating_add(size as usize);
        if next <= offset {
            return;
        }
        offset = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use viewer_scene::ScreenProjection;

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    /// A file with the V2 boxes at their real nesting depth, sample entry prologue and all.
    fn v2_file(stereo_mode: u8, equi_left_right: Option<(u32, u32)>) -> Vec<u8> {
        let mut st3d_payload = vec![0u8; 4];
        st3d_payload.push(stereo_mode);
        let st3d = boxed(b"st3d", &st3d_payload);

        let mut equi_payload = vec![0u8; 4];
        let (left, right) = equi_left_right.unwrap_or((0, 0));
        for v in [0u32, 0, left, right] {
            equi_payload.extend_from_slice(&v.to_be_bytes());
        }
        let equi = boxed(b"equi", &equi_payload);
        let proj = boxed(b"proj", &equi);
        let sv3d = boxed(b"sv3d", &proj);

        let mut avc1_payload = vec![0u8; 78];
        avc1_payload.extend_from_slice(&st3d);
        avc1_payload.extend_from_slice(&sv3d);
        let avc1 = boxed(b"avc1", &avc1_payload);

        let mut stsd_payload = vec![0u8; 4];
        stsd_payload.extend_from_slice(&1u32.to_be_bytes());
        stsd_payload.extend_from_slice(&avc1);
        let stsd = boxed(b"stsd", &stsd_payload);

        let stbl = boxed(b"stbl", &stsd);
        let minf = boxed(b"minf", &stbl);
        let mdia = boxed(b"mdia", &minf);
        let trak = boxed(b"trak", &mdia);
        let moov = boxed(b"moov", &trak);

        let mut file = boxed(b"ftyp", b"isom\0\0\x02\0isomiso2");
        file.extend_from_slice(&moov);
        file
    }

    #[test]
    fn v2_boxes_are_found_through_the_sample_entry_prologue() {
        // The 78-byte VisualSampleEntry prologue is the step most walkers miss; skipping it wrong
        // means st3d and sv3d are never seen and every 360 video reads as flat.
        let s = spherical(&v2_file(1, None)).expect("st3d and equi are both present");
        assert_eq!(
            s.projection,
            ScreenProjection::Equirect {
                horizontal_fov: 360.0
            }
        );
        assert_eq!(s.stereo, StereoLayout::OverUnder);
        assert_eq!(s.evidence, Evidence::Metadata);
    }

    #[test]
    fn equi_bounds_narrow_the_arc() {
        // A quarter inset on each side leaves half the sphere.
        let quarter = u32::MAX / 4;
        let s = spherical(&v2_file(0, Some((quarter, quarter)))).unwrap();
        match s.projection {
            ScreenProjection::Equirect { horizontal_fov } => {
                assert!((horizontal_fov - 180.0).abs() < 1.0, "{horizontal_fov}");
            }
            _ => panic!("expected equirect"),
        }
    }

    #[test]
    fn the_v1_uuid_blob_is_read_when_there_are_no_v2_boxes() {
        let xml = "<?xml version=\"1.0\"?><rdf:SphericalVideo>\
             <GSpherical:Spherical>true</GSpherical:Spherical>\
             <GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType>\
             <GSpherical:StereoMode>left-right</GSpherical:StereoMode>\
             </rdf:SphericalVideo>";
        let mut payload = SPHERICAL_V1_UUID.to_vec();
        payload.extend_from_slice(xml.as_bytes());
        let uuid = boxed(b"uuid", &payload);
        let trak = boxed(b"trak", &uuid);
        let moov = boxed(b"moov", &trak);

        let s = spherical(&moov).expect("the V1 blob declares a spherical video");
        assert!(matches!(s.projection, ScreenProjection::Equirect { .. }));
        assert_eq!(s.stereo, StereoLayout::SideBySide);
    }

    #[test]
    fn a_v1_blob_that_says_false_is_not_a_panorama() {
        let xml = "<GSpherical:Spherical>false</GSpherical:Spherical>";
        let mut payload = SPHERICAL_V1_UUID.to_vec();
        payload.extend_from_slice(xml.as_bytes());
        let moov = boxed(b"moov", &boxed(b"uuid", &payload));
        assert_eq!(spherical(&moov), None);
    }

    #[test]
    fn stereo_without_a_projection_is_a_3d_movie_not_a_panorama() {
        let mut st3d_payload = vec![0u8; 4];
        st3d_payload.push(2); // left-right
        let avc1 = {
            let mut p = vec![0u8; 78];
            p.extend_from_slice(&boxed(b"st3d", &st3d_payload));
            boxed(b"avc1", &p)
        };
        let mut stsd_payload = vec![0u8; 8];
        stsd_payload.extend_from_slice(&avc1);
        let moov = boxed(
            b"moov",
            &boxed(
                b"trak",
                &boxed(
                    b"mdia",
                    &boxed(b"minf", &boxed(b"stbl", &boxed(b"stsd", &stsd_payload))),
                ),
            ),
        );
        let s = spherical(&moov).expect("a stereo mode is worth reporting on its own");
        assert_eq!(s.projection, ScreenProjection::Flat);
        assert_eq!(s.stereo, StereoLayout::SideBySide);
    }

    #[test]
    fn an_ordinary_video_reports_nothing() {
        let moov = boxed(b"moov", &boxed(b"trak", &boxed(b"mdia", b"")));
        assert_eq!(spherical(&moov), None);
    }

    #[test]
    fn a_truncated_or_looping_box_tree_terminates() {
        // Size zero means "to the end of the file"; a box claiming size 8 with no payload, and a
        // header cut in half, both have to stop the walk rather than spin.
        assert_eq!(spherical(&[0, 0, 0, 0, b'm', b'o', b'o', b'v']), None);
        assert_eq!(spherical(&[0, 0, 0, 8, b'm', b'o', b'o', b'v']), None);
        assert_eq!(spherical(&[0, 0, 0]), None);
        assert_eq!(spherical(&[]), None);
        // A size smaller than its own header must not walk backwards.
        assert_eq!(spherical(&[0, 0, 0, 4, b'm', b'o', b'o', b'v', 0, 0]), None);
    }

    #[test]
    fn deep_nesting_is_bounded() {
        // A thousand nested containers: the depth limit has to stop this well before the stack
        // does.
        let mut inner = boxed(b"stbl", b"");
        for _ in 0..1000 {
            inner = boxed(b"minf", &inner);
        }
        assert_eq!(spherical(&boxed(b"moov", &inner)), None);
    }
}
