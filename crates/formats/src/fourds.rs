//! 4DViews HOLOSYS `.4ds`, read as far as it can honestly be read.
//!
//! The mesh and DXT1 texture blocks are decodable only through `BridgeCodec4DS.dll`, a licensed
//! native plugin that cannot be redistributed, so this file will not play here. That is not a
//! reason to say nothing about it. The **container skeleton**, magic, version, frame count, frame
//! rate, texture dimensions, and the trailing frame directory that reveals the keyframe structure
//! is plain little-endian data, and reading it turns "unsupported format" into a sentence that
//! tells someone what they have and what it would take to open it.
//!
//! The field offsets come from the structural probe in the ARES toolkit
//! (`tools/probe-4ds.cjs`), which derived them by analysis rather than from a published
//! specification. They are therefore **inferred, and treated as such**: every one of them is
//! sanity-checked against the file's own length before it is believed, and a field that fails its
//! check is reported as unknown rather than as a number.

use viewer_io::FormatId;

use crate::ImportError;

const MAGIC: &[u8; 4] = b"4DS0";

/// What the container says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Structure {
    pub version: (u16, u16),
    pub frame_count: Option<u32>,
    pub fps: Option<f32>,
    pub texture: Option<(u32, u32)>,
    /// Number of keyframes in the trailing directory, which is the GOP structure.
    pub keyframes: Option<u32>,
}

impl Structure {
    /// A one-line summary for the inspector and the error message.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("HOLOSYS v{}.{}", self.version.0, self.version.1)];
        if let Some(frames) = self.frame_count {
            match self.fps {
                Some(fps) if fps > 0.0 => parts.push(format!("{frames} frames at {fps:.0} fps")),
                _ => parts.push(format!("{frames} frames")),
            }
        }
        if let Some((w, h)) = self.texture {
            parts.push(format!("{w}x{h} DXT1 texture"));
        }
        if let Some(k) = self.keyframes {
            parts.push(format!("{k} keyframes"));
        }
        parts.join(", ")
    }
}

/// Read the container skeleton. Returns `None` if this is not a `.4ds` file at all.
pub fn probe(bytes: &[u8], file_len: u64) -> Option<Structure> {
    if bytes.len() < 128 || &bytes[..4] != MAGIC {
        return None;
    }
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u32_at =
        |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let u64_at = |o: usize| {
        u64::from_le_bytes([
            bytes[o],
            bytes[o + 1],
            bytes[o + 2],
            bytes[o + 3],
            bytes[o + 4],
            bytes[o + 5],
            bytes[o + 6],
            bytes[o + 7],
        ])
    };

    // Every field below is believed only if it is plausible. An inferred offset that happens to
    // land on something else produces a number, not an error, and a viewer that reports
    // "4,027,268,911 frames" is worse than one that reports nothing.
    let frame_count = Some(u32_at(39)).filter(|n| *n > 0 && *n < 1_000_000);
    let fps = Some(f32::from_bits(u32_at(47))).filter(|f| *f > 0.0 && *f <= 480.0);
    let texture = {
        let (w, h) = (u32_at(63), u32_at(67));
        // Block-compressed atlases are powers of two in practice, and never larger than 16K.
        let plausible = |v: u32| (64..=16_384).contains(&v);
        (plausible(w) && plausible(h)).then_some((w, h))
    };

    // The frame directory sits at the end: nine bytes, then one 16-byte record per keyframe.
    let index_offset = u64_at(14);
    let keyframes = if index_offset > 128 && index_offset < file_len {
        let span = file_len - index_offset;
        (span >= 9).then(|| ((span - 9) / 16) as u32)
    } else {
        None
    };

    Some(Structure {
        version: (u16_at(4), u16_at(6)),
        frame_count,
        fps,
        texture,
        keyframes,
    })
}

/// The refusal, with the file's own numbers in it.
pub fn unsupported(bytes: &[u8], file_len: u64) -> ImportError {
    let detail = match probe(bytes, file_len) {
        Some(s) => format!("{}. ", s.summary()),
        None => String::new(),
    };
    ImportError::Malformed {
        format: FormatId::FourDs,
        reason: format!(
            "{detail}Playing it needs BridgeCodec4DS.dll from the 4DViews SDK, which is licensed \
             and cannot be shipped with this viewer. Transcoding to .ares or to a mesh sequence \
             is the way through."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header carrying the values the ARES probe measured on a real capture.
    fn header(frames: u32, fps: f32, tex: (u32, u32), index_offset: u64) -> Vec<u8> {
        let mut v = vec![0u8; 128];
        v[..4].copy_from_slice(MAGIC);
        v[4..6].copy_from_slice(&1u16.to_le_bytes());
        v[6..8].copy_from_slice(&2u16.to_le_bytes());
        v[14..22].copy_from_slice(&index_offset.to_le_bytes());
        v[39..43].copy_from_slice(&frames.to_le_bytes());
        v[47..51].copy_from_slice(&fps.to_bits().to_le_bytes());
        v[63..67].copy_from_slice(&tex.0.to_le_bytes());
        v[67..71].copy_from_slice(&tex.1.to_le_bytes());
        v
    }

    #[test]
    fn the_container_skeleton_reads() {
        let bytes = header(500, 30.0, (2048, 2048), 40_000_000);
        let s = probe(&bytes, 40_420_433).expect("a 4DS0 header");
        assert_eq!(s.version, (1, 2));
        assert_eq!(s.frame_count, Some(500));
        assert_eq!(s.fps, Some(30.0));
        assert_eq!(s.texture, Some((2048, 2048)));
        assert_eq!(s.keyframes, Some((40_420_433 - 40_000_000 - 9) / 16));
        assert!(s.summary().contains("500 frames at 30 fps"));
        assert!(s.summary().contains("2048x2048"));
    }

    #[test]
    fn a_file_that_is_not_4ds_is_not_claimed() {
        assert_eq!(probe(&[0u8; 128], 128), None);
        assert_eq!(probe(b"4DS0", 4), None, "too short to read the header");
        assert_eq!(probe(&[], 0), None);
    }

    #[test]
    fn implausible_fields_are_reported_as_unknown_rather_than_as_numbers() {
        // The offsets are inferred, not specified. A file that does not match the inference must
        // produce silence, not four billion frames at 10^38 fps.
        let mut bytes = header(0, 0.0, (0, 0), 0);
        bytes[39..43].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[47..51].copy_from_slice(&f32::MAX.to_bits().to_le_bytes());
        let s = probe(&bytes, 1024).unwrap();
        assert_eq!(s.frame_count, None);
        assert_eq!(s.fps, None);
        assert_eq!(s.texture, None);
        assert_eq!(s.keyframes, None);
        // It still names the format rather than saying nothing at all.
        assert!(s.summary().starts_with("HOLOSYS"));
    }

    #[test]
    fn an_index_offset_past_the_end_of_the_file_is_ignored() {
        let bytes = header(500, 30.0, (2048, 2048), 999_999_999);
        assert_eq!(probe(&bytes, 40_420_433).unwrap().keyframes, None);
    }

    #[test]
    fn the_refusal_names_the_dll_and_a_way_out() {
        let bytes = header(500, 30.0, (2048, 2048), 40_000_000);
        let message = unsupported(&bytes, 40_420_433).to_string();
        assert!(message.contains("500 frames"), "{message}");
        assert!(message.contains("BridgeCodec4DS.dll"), "{message}");
        assert!(message.contains(".ares"), "{message}");
    }
}
