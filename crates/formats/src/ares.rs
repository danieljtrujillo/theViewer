//! ARES volumetric video.
//!
//! The container is a 64-byte header, a superblock, a GOP seek table, a track directory, and then
//! time-ordered `CNK0` chunks each holding one closed GOP: a meshopt-compressed intra frame
//! followed by predicted frames that move the same vertices, alongside one closed video GOP of
//! texture. `docs/FORMATS.md` §E carries the field-by-field layout; this file implements it.
//!
//! Written against the reference implementation in
//! `G:\StableAudio\4 - Volumetric Interop\ares\packages\core\src`, `format.ts`, `container.ts`,
//! `demuxer.ts` and `geometry.ts`, and against its real output. Three things from that reading
//! are load-bearing and easy to get wrong:
//!
//! 1. **Positions are quantized against the chunk's own AABB.** `world = min + (q / levels) *
//!    size`, where `levels = (1 << quant_bits_pos) - 1`, `q` is `u16` at stride 4, and `min` and
//!    `size` come from the box in each chunk's header. The superblock's box spans the whole
//!    sequence: it is what the camera should frame and the wrong thing to dequantize against,
//!    because rescaling a frame from its own box up to the clip's stretches each axis by a
//!    different ratio. Measured on a real capture, that is a subject 2.07 times too wide.
//! 2. **Predicted frames are deltas on the quantized integers**, wrapping at 16 bits, not on the
//!    dequantized floats. Adding them in world space accumulates error across a GOP.
//! 3. **A predicted frame inherits the intra frame's topology.** Its index buffer is absent, not
//!    empty, and re-deriving one would throw away the whole point of the encoding.

use std::path::{Path, PathBuf};

use viewer_io::FormatId;
use viewer_scene::attr::{Attribute, ComponentType, Semantic};
use viewer_scene::payload::{Indices, TriMesh};
use viewer_scene::{
    Aabb, AlphaMode, Image, Material, MeshFrame, Node, NodeId, Payload, Scene, Timeline,
};

use crate::{Frame, FrameSource, ImportError, Opened};

const HEADER_SIZE: usize = 64;
const CHUNK_MAGIC: &[u8; 4] = b"CNK0";

/// Byte offsets in the fixed header (spec Appendix A.1).
mod h {
    pub const VERSION_MAJOR: usize = 4;
    pub const VERSION_MINOR: usize = 5;
    pub const HEADER_FLAGS: usize = 6;
    pub const GEOMETRY_PROFILE: usize = 8;
    pub const TEXTURE_CODEC: usize = 9;
    pub const INTRA_CODEC: usize = 10;
    pub const FPS: usize = 12;
    pub const FRAME_COUNT: usize = 16;
    pub const DURATION_US: usize = 20;
    pub const SUPERBLOCK_OFFSET: usize = 28;
    pub const GOP_INDEX_OFFSET: usize = 36;
    pub const TRACK_DIR_OFFSET: usize = 44;
}

mod attr_mask {
    pub const NORMAL: u16 = 1 << 1;
    pub const UV: u16 = 1 << 2;
}

/// Block types in a chunk's block directory.
mod block {
    pub const GEOMETRY_I: u8 = 0;
    pub const GEOMETRY_PB: u8 = 1;
    pub const TEXTURE_COLOR: u8 = 2;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureCodec {
    None,
    Av1,
    Vp9,
    Hevc,
    Avc,
}

impl TextureCodec {
    fn from_byte(b: u8) -> TextureCodec {
        match b {
            1 => TextureCodec::Av1,
            2 => TextureCodec::Vp9,
            3 => TextureCodec::Hevc,
            4 => TextureCodec::Avc,
            _ => TextureCodec::None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TextureCodec::None => "none",
            TextureCodec::Av1 => "AV1",
            TextureCodec::Vp9 => "VP9",
            TextureCodec::Hevc => "HEVC",
            TextureCodec::Avc => "H.264",
        }
    }
}

/// The static texture blob's format, which also says whether the texture is a video track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureBlob {
    None,
    Png,
    VideoVp9,
    VideoAv1,
}

#[derive(Debug, Clone)]
pub struct Header {
    pub version: (u8, u8),
    pub flags: u16,
    pub geometry_profile: u8,
    pub texture_codec: TextureCodec,
    pub intra_codec: u8,
    pub fps: f32,
    pub frame_count: u32,
    pub duration_us: u64,
    superblock_offset: u64,
    gop_index_offset: u64,
    track_dir_offset: u64,
}

#[derive(Debug, Clone)]
pub struct Superblock {
    pub bounds: Aabb,
    pub quant_bits_pos: u8,
    pub quant_bits_uv: u8,
    pub normal_encoding: u8,
    pub gop_length: u16,
    pub texture_offset: u64,
    pub texture_length: u32,
    pub texture_blob: TextureBlob,
    pub texture_width: u16,
    pub texture_height: u16,
    pub meta: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy)]
pub struct Gop {
    pub start_pts_us: u64,
    pub frame_start: u32,
    pub frame_count: u16,
    pub byte_offset: u64,
    pub byte_length: u32,
}

#[derive(Debug, Clone, Copy)]
struct BlockRef {
    kind: u8,
    /// Absolute file offset.
    offset: usize,
    length: usize,
}

/// A little-endian reader that refuses to walk off the end. Every offset here came out of a file
/// we did not write.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn at(data: &'a [u8], pos: usize) -> Reader<'a> {
        Reader { data, pos }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        let b = self.take(8)?;
        Some(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_bits(self.u32()?))
    }
    fn f32x3(&mut self) -> Option<[f32; 3]> {
        Some([self.f32()?, self.f32()?, self.f32()?])
    }
    /// A UTF-8 string with a `u16` length prefix.
    fn string(&mut self) -> Option<String> {
        let n = self.u16()? as usize;
        Some(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
}

fn malformed(reason: impl Into<String>) -> ImportError {
    ImportError::Malformed {
        format: FormatId::Ares,
        reason: reason.into(),
    }
}

pub fn parse_header(bytes: &[u8]) -> Result<Header, ImportError> {
    if bytes.len() < HEADER_SIZE {
        return Err(malformed("shorter than the 64-byte header"));
    }
    if &bytes[..4] != b"ARES" {
        return Err(malformed("missing the ARES magic"));
    }
    let major = bytes[h::VERSION_MAJOR];
    if major > 0 {
        return Err(malformed(format!("unsupported container version {major}")));
    }

    let mut r = Reader::at(bytes, 0);
    let get_u16 = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let get_u32 =
        |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    r.pos = h::DURATION_US;
    let duration_us = r.u64().unwrap_or(0);
    r.pos = h::SUPERBLOCK_OFFSET;
    let superblock_offset = r.u64().unwrap_or(0);
    r.pos = h::GOP_INDEX_OFFSET;
    let gop_index_offset = r.u64().unwrap_or(0);
    r.pos = h::TRACK_DIR_OFFSET;
    let track_dir_offset = r.u64().unwrap_or(0);

    Ok(Header {
        version: (major, bytes[h::VERSION_MINOR]),
        flags: get_u16(h::HEADER_FLAGS),
        geometry_profile: bytes[h::GEOMETRY_PROFILE],
        texture_codec: TextureCodec::from_byte(bytes[h::TEXTURE_CODEC]),
        intra_codec: bytes[h::INTRA_CODEC],
        fps: f32::from_bits(get_u32(h::FPS)),
        frame_count: get_u32(h::FRAME_COUNT),
        duration_us,
        superblock_offset,
        gop_index_offset,
        track_dir_offset,
    })
}

fn parse_superblock(bytes: &[u8], offset: u64) -> Result<Superblock, ImportError> {
    let mut r = Reader::at(
        bytes,
        usize::try_from(offset).map_err(|_| malformed("superblock offset out of range"))?,
    );
    let mut read = || -> Option<Superblock> {
        let min = r.f32x3()?;
        let max = r.f32x3()?;
        let quant_bits_pos = r.u8()?;
        let quant_bits_uv = r.u8()?;
        let normal_encoding = r.u8()?;
        r.u8()?; // reserved
        let gop_length = r.u16()?;
        let texture_offset = r.u64()?;
        let texture_length = r.u32()?;
        let texture_blob = match r.u8()? {
            1 => TextureBlob::Png,
            2 => TextureBlob::VideoVp9,
            3 => TextureBlob::VideoAv1,
            _ => TextureBlob::None,
        };
        r.u8()?; // reserved
        let texture_width = r.u16()?;
        let texture_height = r.u16()?;
        let kv_count = r.u16()?;
        let mut meta = Vec::new();
        for _ in 0..kv_count {
            let key = r.string()?;
            let value = r.string()?;
            meta.push((key, value));
        }
        Some(Superblock {
            bounds: Aabb { min, max },
            quant_bits_pos,
            quant_bits_uv,
            normal_encoding,
            gop_length,
            texture_offset,
            texture_length,
            texture_blob,
            texture_width,
            texture_height,
            meta,
        })
    };
    read().ok_or_else(|| malformed("truncated superblock"))
}

fn parse_gop_index(bytes: &[u8], offset: u64) -> Vec<Gop> {
    let Ok(pos) = usize::try_from(offset) else {
        return Vec::new();
    };
    let mut r = Reader::at(bytes, pos);
    let Some(count) = r.u32() else {
        return Vec::new();
    };
    // The count comes from the file, so it caps the allocation rather than driving it.
    let mut out = Vec::new();
    for _ in 0..count {
        let Some(entry) = (|| {
            let start_pts_us = r.u64()?;
            let frame_start = r.u32()?;
            let frame_count = r.u16()?;
            r.u16()?; // flags
            let byte_offset = r.u64()?;
            let byte_length = r.u32()?;
            Some(Gop {
                start_pts_us,
                frame_start,
                frame_count,
                byte_offset,
                byte_length,
            })
        })() else {
            break;
        };
        out.push(entry);
    }
    out
}

/// FourCC of the colour texture track, when the file declares one.
fn texture_track_fourcc(bytes: &[u8], offset: u64) -> Option<String> {
    let mut r = Reader::at(bytes, usize::try_from(offset).ok()?);
    let count = r.u16()?;
    for _ in 0..count {
        r.u16()?; // track id
        let track_type = r.u8()?;
        let fourcc = String::from_utf8_lossy(r.take(4)?).into_owned();
        r.u8()?; // tier
        let config_len = r.u16()? as usize;
        r.take(config_len)?;
        if track_type == 1 {
            return Some(fourcc);
        }
    }
    None
}

/// A chunk's own header: the box its geometry was quantized against, and its block directory.
///
/// The box is the important part and it is easy to miss. Positions are quantized *per chunk*,
/// against the box stored here, not against the superblock's box for the whole sequence. Reading
/// past this field and dequantizing against the superblock stretches every frame to fill the
/// extent of the entire clip: a subject that moves three metres across the shot comes out three
/// metres wide in every single frame, squashed in whichever axis moved least.
struct Chunk {
    bounds: Aabb,
    blocks: Vec<BlockRef>,
}

/// A chunk's block directory, with every offset resolved to an absolute, bounds-checked one.
fn parse_chunk(bytes: &[u8], gop: &Gop) -> Result<Chunk, ImportError> {
    let base =
        usize::try_from(gop.byte_offset).map_err(|_| malformed("chunk offset out of range"))?;
    let mut r = Reader::at(bytes, base);
    let magic = r.take(4).ok_or_else(|| malformed("chunk out of bounds"))?;
    if magic != CHUNK_MAGIC {
        return Err(malformed(format!(
            "chunk at {base} does not start with CNK0"
        )));
    }
    let mut read = || -> Option<Chunk> {
        r.u64()?; // pts
        r.u16()?; // frame count
        r.u16()?; // flags
        let min = r.f32x3()?;
        let max = r.f32x3()?;
        let block_count = r.u16()?;
        let mut out = Vec::new();
        for _ in 0..block_count {
            let kind = r.u8()?;
            r.u16()?; // track id
            let offset = r.u32()? as usize;
            let length = r.u32()? as usize;
            let start = base.checked_add(offset)?;
            // A length field from the file must never become a slice bound unchecked.
            if start.checked_add(length)? > bytes.len() {
                return None;
            }
            out.push(BlockRef {
                kind,
                offset: start,
                length,
            });
        }
        Some(Chunk {
            bounds: Aabb { min, max },
            blocks: out,
        })
    };
    read().ok_or_else(|| malformed("truncated chunk directory"))
}

/// Quantized geometry for one frame, exactly as it comes out of meshopt.
#[derive(Default, Clone)]
struct QuantGeometry {
    vertex_count: usize,
    /// `u16` at stride 4: x, y, z, pad.
    positions: Vec<u16>,
    /// `u16` at stride 2: u, v.
    uvs: Vec<u16>,
    /// 4 bytes per vertex, interpreted according to the superblock's normal encoding.
    normals: Vec<u8>,
    indices: Vec<u32>,
}

fn decode_intra(data: &[u8]) -> Result<QuantGeometry, ImportError> {
    let mut r = Reader::at(data, 0);
    let (vertex_count, index_count, mask) = (|| {
        let v = r.u32()? as usize;
        let i = r.u32()? as usize;
        let m = r.u16()?;
        Some((v, i, m))
    })()
    .ok_or_else(|| malformed("truncated geometry block header"))?;

    let positions = decode_vertex_u16(&mut r, vertex_count, 4)?;
    let uvs = if mask & attr_mask::UV != 0 {
        decode_vertex_u16(&mut r, vertex_count, 2)?
    } else {
        Vec::new()
    };
    let normals = if mask & attr_mask::NORMAL != 0 {
        decode_vertex_bytes(&mut r, vertex_count)?
    } else {
        Vec::new()
    };

    let index_len = r.u32().ok_or_else(|| malformed("truncated index length"))? as usize;
    let index_bytes = r
        .take(index_len)
        .ok_or_else(|| malformed("truncated index buffer"))?;
    // The decoder asserts on a count that is not a whole number of triangles, so the check has to
    // happen here rather than inside it.
    if index_count % 3 != 0 {
        return Err(malformed(format!(
            "index count {index_count} is not a whole number of triangles"
        )));
    }
    let mut indices = vec![0u32; index_count];
    meshopt_rs::index::buffer::decode_index_buffer(&mut indices, index_bytes)
        .map_err(|e| malformed(format!("meshopt index decode failed: {e:?}")))?;

    Ok(QuantGeometry {
        vertex_count,
        positions,
        uvs,
        normals,
        indices,
    })
}

/// A predicted frame: quantized position deltas, plus optionally fresh UVs and normals.
///
/// The addition wraps at 16 bits by construction, that is what the encoder's subtraction did,
/// so `wrapping_add` here is the correct operation and not a way of ignoring overflow.
fn decode_predicted(data: &[u8], previous: &QuantGeometry) -> Result<QuantGeometry, ImportError> {
    let mut r = Reader::at(data, 0);
    let vertex_count = r
        .u32()
        .ok_or_else(|| malformed("truncated predicted frame"))? as usize;
    if vertex_count != previous.vertex_count {
        return Err(malformed(format!(
            "predicted frame declares {vertex_count} vertices but the keyframe had {}",
            previous.vertex_count
        )));
    }

    let deltas = decode_vertex_u16(&mut r, vertex_count, 4)?;
    let mut positions = previous.positions.clone();
    for (p, d) in positions.iter_mut().zip(deltas.iter()) {
        *p = p.wrapping_add(*d);
    }

    let uv_len = r.u32().unwrap_or(0) as usize;
    let uvs = if uv_len > 0 {
        let bytes = r
            .take(uv_len)
            .ok_or_else(|| malformed("truncated predicted UVs"))?;
        decode_meshopt_u16(bytes, vertex_count, 2)?
    } else {
        previous.uvs.clone()
    };

    let normal_len = r.u32().unwrap_or(0) as usize;
    let normals = if normal_len > 0 {
        let bytes = r
            .take(normal_len)
            .ok_or_else(|| malformed("truncated predicted normals"))?;
        decode_meshopt_bytes(bytes, vertex_count)?
    } else {
        previous.normals.clone()
    };

    Ok(QuantGeometry {
        vertex_count,
        positions,
        uvs,
        normals,
        // Topology is inherited, not re-sent.
        indices: previous.indices.clone(),
    })
}

fn decode_vertex_u16(
    r: &mut Reader<'_>,
    vertex_count: usize,
    components: usize,
) -> Result<Vec<u16>, ImportError> {
    let len = r
        .u32()
        .ok_or_else(|| malformed("truncated attribute length"))? as usize;
    let bytes = r
        .take(len)
        .ok_or_else(|| malformed("truncated attribute buffer"))?;
    decode_meshopt_u16(bytes, vertex_count, components)
}

fn decode_meshopt_u16(
    bytes: &[u8],
    vertex_count: usize,
    components: usize,
) -> Result<Vec<u16>, ImportError> {
    // meshopt's stride is the size of one vertex, so the element type carries it.
    let stride = components * 2;
    let mut raw = vec![0u8; vertex_count * stride];
    crate::meshopt::decode_vertex_buffer(&mut raw, vertex_count, stride, bytes)
        .map_err(|e| malformed(format!("meshopt vertex decode failed: {e}")))?;
    Ok(raw
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect())
}

fn decode_vertex_bytes(r: &mut Reader<'_>, vertex_count: usize) -> Result<Vec<u8>, ImportError> {
    let len = r
        .u32()
        .ok_or_else(|| malformed("truncated normal length"))? as usize;
    let bytes = r
        .take(len)
        .ok_or_else(|| malformed("truncated normal buffer"))?;
    decode_meshopt_bytes(bytes, vertex_count)
}

fn decode_meshopt_bytes(bytes: &[u8], vertex_count: usize) -> Result<Vec<u8>, ImportError> {
    let mut out = vec![0u8; vertex_count * 4];
    crate::meshopt::decode_vertex_buffer(&mut out, vertex_count, 4, bytes)
        .map_err(|e| malformed(format!("meshopt normal decode failed: {e}")))?;
    Ok(out)
}

/// Turn quantized geometry into the world-space frame the renderer takes.
///
/// `chunk_bounds` is the box the chunk header declares. See the note inside on why it is not the
/// dequantization basis.
fn to_mesh_frame(
    q: &QuantGeometry,
    superblock: &Superblock,
    chunk_bounds: &Aabb,
    include_indices: bool,
) -> MeshFrame {
    let levels = ((1u32 << superblock.quant_bits_pos.clamp(1, 16)) - 1) as f32;

    // The chunk's box, not the superblock's, and the difference is the whole subject's shape.
    //
    // Measured on `daniel-coherent-v4.ares`, one frame of a standing adult:
    //   against the chunk box       0.68 x 1.70 x 0.50 m   correct proportions
    //   against the superblock box  1.47 x 1.77 x 0.94 m   2.07x too wide
    //
    // The superblock's box spans the whole nine-second clip, across which the subject travels.
    // Rescaling each frame from its own box up to that one stretches every axis by a different
    // ratio, most in whichever direction the subject moved furthest and least in height, which
    // reads as horizontally stretched and vertically squished.
    //
    // The superblock box remains the right thing to frame the camera on, and that is where it is
    // still used.
    let basis = if chunk_bounds.is_empty() || chunk_bounds.extent().iter().any(|e| *e <= 0.0) {
        &superblock.bounds
    } else {
        chunk_bounds
    };
    let min = basis.min;
    let size = basis.extent();

    let mut positions = Vec::with_capacity(q.vertex_count);
    let mut bounds = Aabb::EMPTY;
    for i in 0..q.vertex_count {
        let base = i * 4;
        let x = *q.positions.get(base).unwrap_or(&0) as f32;
        let y = *q.positions.get(base + 1).unwrap_or(&0) as f32;
        let z = *q.positions.get(base + 2).unwrap_or(&0) as f32;
        let p = [
            min[0] + (x / levels) * size[0],
            min[1] + (y / levels) * size[1],
            min[2] + (z / levels) * size[2],
        ];
        bounds.expand(p);
        positions.push(p);
    }

    let mut uvs = Vec::new();
    if q.uvs.len() >= q.vertex_count * 2 {
        uvs.reserve(q.vertex_count);
        for i in 0..q.vertex_count {
            uvs.push([
                q.uvs[i * 2] as f32 / 65535.0,
                q.uvs[i * 2 + 1] as f32 / 65535.0,
            ]);
        }
    }

    let normals = decode_normals(&q.normals, q.vertex_count, superblock.normal_encoding);

    MeshFrame {
        positions,
        normals,
        uvs,
        indices: if include_indices {
            q.indices.clone()
        } else {
            Vec::new()
        },
        // This frame's own box, measured from the vertices that are actually in it.
        //
        // It used to be the superblock's, on the reasoning that the camera should frame the whole
        // sequence rather than pump in and out as a figure walks. That reasoning was sound and the
        // place was wrong: the app frames on the scene's bounds, never on these, so all this did
        // was make every frame report the extent of the entire clip. `viewer info` printed that
        // number as the decoded extent, which is how a nine-second capture came to claim each of
        // its frames was 1,473 metres wide.
        bounds,
    }
}

/// Unpack per-vertex normals.
///
/// Encoding 0 is three signed bytes over 127; encoding 1 is octahedral, two signed 16-bit values
/// packed into the same four bytes. The reference viewer chooses the same way, from the
/// superblock's `normal_encoding`, and reading a degree-1 file as degree 0 produces normals that
/// look plausible from one angle and wrong from every other.
fn decode_normals(data: &[u8], vertex_count: usize, encoding: u8) -> Vec<[f32; 3]> {
    if data.len() < vertex_count * 4 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(vertex_count);
    for i in 0..vertex_count {
        let b = &data[i * 4..i * 4 + 4];
        out.push(match encoding {
            1 => {
                let x = i16::from_le_bytes([b[0], b[1]]) as f32 / 32767.0;
                let y = i16::from_le_bytes([b[2], b[3]]) as f32 / 32767.0;
                octahedral_to_normal(x, y)
            }
            _ => {
                let v = [
                    b[0] as i8 as f32 / 127.0,
                    b[1] as i8 as f32 / 127.0,
                    b[2] as i8 as f32 / 127.0,
                ];
                normalize(v)
            }
        });
    }
    out
}

fn octahedral_to_normal(x: f32, y: f32) -> [f32; 3] {
    let z = 1.0 - x.abs() - y.abs();
    let t = (-z).clamp(0.0, 1.0);
    let nx = x + if x >= 0.0 { -t } else { t };
    let ny = y + if y >= 0.0 { -t } else { t };
    normalize([nx, ny, z])
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < 1e-8 {
        [0.0, 1.0, 0.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

pub fn import(path: &Path, bytes: &[u8]) -> Result<Opened, ImportError> {
    let header = parse_header(bytes)?;
    let superblock = parse_superblock(bytes, header.superblock_offset)?;
    let gops = parse_gop_index(bytes, header.gop_index_offset);
    if gops.is_empty() {
        return Err(malformed("the GOP index is empty; nothing to play"));
    }

    let mut decoder = GeometryDecoder {
        superblock: superblock.clone(),
        gops: gops.clone(),
        state: None,
    };
    let first = decoder.frame(bytes, 0)?;

    let texture = TextureTrack::open(bytes, &header, &superblock);

    let mut scene = Scene::new();

    // The material's texture slot is registered as dynamic whether or not a decoder was found: an
    // untextured capture then renders as clay rather than as a hole in the pipeline.
    let image_index = scene.push_dynamic_image(Image {
        name: "texture".into(),
        width: superblock.texture_width.max(1) as u32,
        height: superblock.texture_height.max(1) as u32,
        rgba: vec![
            200;
            superblock.texture_width.max(1) as usize
                * superblock.texture_height.max(1) as usize
                * 4
        ],
    });
    let textured = texture.as_ref().is_some_and(|t| t.is_decoding());
    let material = scene.push_material(Material {
        name: "ares".into(),
        base_color: [1.0, 1.0, 1.0, 1.0],
        base_color_texture: textured.then_some(image_index),
        alpha_mode: AlphaMode::Opaque,
        double_sided: true,
        ..Default::default()
    });

    let node = scene.push_root(Node::with_payload(
        "capture",
        Payload::TriMesh(mesh_from_frame(&first, Some(material))),
    ));
    scene.dynamic_meshes.push(node);

    let timeline = Timeline {
        frame_count: header.frame_count,
        fps: header.fps,
        duration_us: header.duration_us,
        seekable: true,
    };
    scene.timeline = Some(timeline.clone());

    scene.set_meta("format", "ARES volumetric");
    scene.set_meta(
        "container",
        format!("v{}.{}", header.version.0, header.version.1),
    );
    scene.set_meta("frames", header.frame_count.to_string());
    scene.set_meta("frame rate", format!("{:.3} fps", header.fps));
    scene.set_meta(
        "duration",
        crate::movie::format_duration(header.duration_us),
    );
    scene.set_meta("gop length", superblock.gop_length.to_string());
    scene.set_meta(
        "position bits",
        format!("{} per axis", superblock.quant_bits_pos),
    );
    scene.set_meta(
        "texture",
        match &texture {
            Some(t) => t.description(),
            None => format!(
                "{} {}x{} (no decoder on this machine)",
                header.texture_codec.label(),
                superblock.texture_width,
                superblock.texture_height
            ),
        },
    );
    for (key, value) in &superblock.meta {
        scene.set_meta(key.clone(), value.clone());
    }

    Ok(Opened {
        scene,
        frames: Some(Box::new(AresSource {
            path: path.to_path_buf(),
            source: None,
            decoder,
            texture,
            timeline,
            served: None,
            node,
        })),
    })
}

fn mesh_from_frame(frame: &MeshFrame, material: Option<u32>) -> TriMesh {
    let n = frame.positions.len();
    let flat: Vec<u8> = frame
        .positions
        .iter()
        .flat_map(|p| p.iter().flat_map(|v| v.to_le_bytes()))
        .collect();
    let mut attributes =
        vec![Attribute::new(Semantic::Position, ComponentType::F32, 3, n, flat).unwrap()];
    if frame.uvs.len() == n {
        let uv: Vec<u8> = frame
            .uvs
            .iter()
            .flat_map(|p| p.iter().flat_map(|v| v.to_le_bytes()))
            .collect();
        attributes
            .push(Attribute::new(Semantic::TexCoord(0), ComponentType::F32, 2, n, uv).unwrap());
    }
    if frame.normals.len() == n {
        let nrm: Vec<u8> = frame
            .normals
            .iter()
            .flat_map(|p| p.iter().flat_map(|v| v.to_le_bytes()))
            .collect();
        attributes.push(Attribute::new(Semantic::Normal, ComponentType::F32, 3, n, nrm).unwrap());
    }
    TriMesh {
        attributes,
        indices: Indices::U32(frame.indices.clone()),
        bounds: frame.bounds,
        material,
    }
}

/// Walks the GOP index and decodes geometry, keeping the last decoded frame so that ordinary
/// forward playback costs one predicted frame per step rather than a whole GOP.
struct GeometryDecoder {
    superblock: Superblock,
    gops: Vec<Gop>,
    state: Option<DecodedState>,
}

struct DecodedState {
    gop: usize,
    /// Frame index within the GOP.
    position: usize,
    geometry: QuantGeometry,
}

impl GeometryDecoder {
    fn gop_for(&self, frame: u32) -> usize {
        // The index is ordered, so a binary search finds the last GOP starting at or before the
        // frame. Linear scanning a 9,000-frame capture on every seek is the thing to avoid.
        match self.gops.binary_search_by(|g| g.frame_start.cmp(&frame)) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        }
    }

    /// Decode the geometry for a global frame index.
    fn frame(&mut self, bytes: &[u8], frame: u32) -> Result<MeshFrame, ImportError> {
        let gop_index = self.gop_for(frame);
        let gop = self.gops[gop_index];
        let target = (frame.saturating_sub(gop.frame_start)) as usize;

        let chunk = parse_chunk(bytes, &gop)?;
        let geometry: Vec<BlockRef> = chunk
            .blocks
            .iter()
            .copied()
            .filter(|b| b.kind == block::GEOMETRY_I || b.kind == block::GEOMETRY_PB)
            .collect();
        if geometry.is_empty() {
            return Err(malformed("a chunk carries no geometry blocks"));
        }

        // Where a cold decode has to start: the nearest intra frame at or before the target.
        //
        // Not always frame zero. Captures encoded from a coherent source are inter-coded and have
        // one intra frame per GOP; captures rebuilt per frame are *all* intra, and starting from
        // the top of the GOP would decode thirty frames to show one. Both shapes appear in the
        // same directory of real files.
        let mut earliest = 0usize;
        for i in (0..=target.min(geometry.len().saturating_sub(1))).rev() {
            if geometry[i].kind == block::GEOMETRY_I {
                earliest = i;
                break;
            }
        }

        // Resume from the last decoded frame when it is in the same GOP and lies between that
        // intra frame and the target, which is the case for ordinary forward playback, and makes
        // stepping one frame cost one block.
        let mut cursor = earliest;
        let mut geometry_state: Option<QuantGeometry> = None;
        if let Some(state) = &self.state {
            if state.gop == gop_index && state.position >= earliest && state.position <= target {
                cursor = state.position;
                geometry_state = Some(state.geometry.clone());
            }
        }

        // True once this frame's topology has been re-sent, which happens on every intra frame
        // decoded here and never on a resumed run that decodes none.
        let mut fresh_topology = false;
        let first = if geometry_state.is_some() {
            cursor + 1
        } else {
            cursor
        };

        for step in first..=target {
            let Some(block) = geometry.get(step) else {
                break;
            };
            let data = &bytes[block.offset..block.offset + block.length];
            geometry_state = Some(if block.kind == block::GEOMETRY_I {
                fresh_topology = true;
                decode_intra(data)?
            } else {
                let previous = geometry_state
                    .as_ref()
                    .ok_or_else(|| malformed("a predicted frame with no keyframe before it"))?;
                decode_predicted(data, previous)?
            });
        }

        let geometry_state =
            geometry_state.ok_or_else(|| malformed("a GOP has no decodable geometry"))?;
        let out = to_mesh_frame(
            &geometry_state,
            &self.superblock,
            &chunk.bounds,
            fresh_topology,
        );
        self.state = Some(DecodedState {
            gop: gop_index,
            position: target,
            geometry: geometry_state,
        });
        Ok(out)
    }

    /// Coded texture frames carried by the chunk covering a frame, in presentation order.
    fn texture_blocks(&self, bytes: &[u8], frame: u32) -> Vec<(u32, bool, std::ops::Range<usize>)> {
        let gop_index = self.gop_for(frame);
        let Some(gop) = self.gops.get(gop_index) else {
            return Vec::new();
        };
        let Ok(chunk) = parse_chunk(bytes, gop) else {
            return Vec::new();
        };
        let Some(block) = chunk.blocks.iter().find(|b| b.kind == block::TEXTURE_COLOR) else {
            return Vec::new();
        };
        parse_texture_block(
            &bytes[block.offset..block.offset + block.length],
            gop.frame_start,
            // The returned ranges index the whole file, not the block. Returning block-relative
            // ranges and indexing the file with them is a bug that costs nothing at parse time
            // and produces a decoder fed 400 KB of container header: it accepts every frame and
            // emits nothing, which looks exactly like a missing codec.
            block.offset,
        )
    }
}

/// A chunk's texture block: `frame_count u16`, reserved `u16`, a table of
/// `[size u32][flags u8]`, then the coded frames back to back.
fn parse_texture_block(
    data: &[u8],
    frame_start: u32,
    base: usize,
) -> Vec<(u32, bool, std::ops::Range<usize>)> {
    let mut r = Reader::at(data, 0);
    let Some(count) = r.u16() else {
        return Vec::new();
    };
    if r.u16().is_none() {
        return Vec::new();
    }
    let mut sizes = Vec::new();
    for _ in 0..count {
        let Some(size) = r.u32() else {
            return Vec::new();
        };
        let Some(flags) = r.u8() else {
            return Vec::new();
        };
        sizes.push((size as usize, flags & 1 == 1));
    }
    let mut out = Vec::new();
    for (i, (size, key)) in sizes.into_iter().enumerate() {
        let start = r.pos;
        if r.take(size).is_none() {
            break;
        }
        out.push((
            frame_start + i as u32,
            key,
            base + start..base + start + size,
        ));
    }
    out
}

/// The texture side of a capture, when the platform can decode it.
struct TextureTrack {
    codec_label: String,
    width: u32,
    height: u32,
    decoder: Option<viewer_media::StreamDecoder>,
    /// Why there is no decoder, if there is none.
    reason: Option<String>,
    /// The last coded frame handed to the decoder, so replaying a frame costs nothing.
    fed: Option<u32>,
}

impl TextureTrack {
    fn open(bytes: &[u8], header: &Header, superblock: &Superblock) -> Option<TextureTrack> {
        let is_video = matches!(
            superblock.texture_blob,
            TextureBlob::VideoVp9 | TextureBlob::VideoAv1
        );
        if !is_video || superblock.texture_width == 0 {
            return None;
        }
        // The track directory's FourCC is the authority; the superblock's blob format and the
        // header's codec byte are corroboration.
        let fourcc = texture_track_fourcc(bytes, header.track_dir_offset);
        let codec = match (fourcc.as_deref(), superblock.texture_blob) {
            (Some("AV01"), _) | (_, TextureBlob::VideoAv1) => viewer_media::Codec::Av1,
            (Some("VP09"), _) | (_, TextureBlob::VideoVp9) => viewer_media::Codec::Vp9,
            _ => return None,
        };

        let width = superblock.texture_width as u32;
        let height = superblock.texture_height as u32;
        let (decoder, reason) = match viewer_media::StreamDecoder::new(codec, width, height) {
            Ok(d) => {
                let name = d.name();
                (
                    Some(d),
                    (!name.is_empty()).then_some(format!("decoded by {name}")),
                )
            }
            Err(e) => (None, Some(e.to_string())),
        };
        Some(TextureTrack {
            codec_label: codec.label().to_string(),
            width,
            height,
            decoder,
            reason,
            fed: None,
        })
    }

    fn is_decoding(&self) -> bool {
        self.decoder.is_some()
    }

    fn description(&self) -> String {
        match &self.reason {
            Some(reason) => format!(
                "{} {}x{} \u{2014} {reason}",
                self.codec_label, self.width, self.height
            ),
            None => format!("{} {}x{}", self.codec_label, self.width, self.height),
        }
    }
}

/// The frame source the player drives.
struct AresSource {
    path: PathBuf,
    /// Opened lazily on the decode thread, so the memory map belongs to the thread that reads it.
    source: Option<viewer_io::Source>,
    decoder: GeometryDecoder,
    texture: Option<TextureTrack>,
    timeline: Timeline,
    served: Option<u32>,
    #[allow(dead_code)]
    node: NodeId,
}

impl FrameSource for AresSource {
    fn timeline(&self) -> Timeline {
        self.timeline.clone()
    }

    fn frame_at(&mut self, pts_us: u64) -> Result<Option<Frame>, ImportError> {
        let index = self
            .timeline
            .frame_at(pts_us as f64 / 1_000_000.0)
            .min(self.timeline.frame_count.saturating_sub(1));
        if self.served == Some(index) {
            return Ok(None);
        }

        if self.source.is_none() {
            self.source = Some(viewer_io::Source::open(&self.path)?);
        }
        // The map is only read here, and the decoder needs it borrowed for the whole call, so it
        // is taken out and put back rather than borrowed across a `&mut self` method.
        let source = self.source.take().expect("just opened");
        let result = self.decode(source.bytes(), index);
        self.source = Some(source);
        let (mesh, image) = result?;

        self.served = Some(index);
        Ok(Some(Frame {
            index,
            pts_us,
            image,
            mesh: Some(mesh),
        }))
    }
}

impl AresSource {
    fn decode(
        &mut self,
        bytes: &[u8],
        index: u32,
    ) -> Result<(MeshFrame, Option<Image>), ImportError> {
        let mesh = self.decoder.frame(bytes, index)?;

        let Some(track) = self.texture.as_mut() else {
            return Ok((mesh, None));
        };
        let Some(decoder) = track.decoder.as_mut() else {
            return Ok((mesh, None));
        };

        // Going backwards means the decoder's buffered state is worthless: an inter-coded stream
        // has to be re-entered at a keyframe.
        if track.fed.is_some_and(|fed| fed > index) {
            decoder.flush();
            track.fed = None;
        }

        // Feed every coded frame from the GOP's keyframe up to the one wanted, skipping any
        // already fed. Skipping matters: without it, playing to frame 29 of a GOP re-feeds frames
        // 0 to 28 thirty times over, which is quadratic in the GOP length.
        let coded = self.decoder.texture_blocks(bytes, index);
        let mut image = None;
        for (frame_index, is_key, range) in coded {
            if frame_index > index {
                break;
            }
            if track.fed.is_some_and(|fed| fed >= frame_index) {
                continue;
            }
            // Only the frame being shown needs its pixels. The ones before it have to be decoded
            // so the stream stays decodable, and their colour conversion would be thrown away
            // one line later: on a 2048 square texture that conversion is the single largest cost
            // on this path, so converting a whole group of pictures to display one of them is
            // what makes falling behind compound.
            let wanted = frame_index == index;
            let decoded = if wanted {
                decoder.decode(&bytes[range], is_key, frame_index as u64)
            } else {
                decoder.decode_without_pixels(&bytes[range], is_key, frame_index as u64)
            };
            match decoded {
                Ok(Some(frame)) => {
                    track.fed = Some(frame_index);
                    // A decoder runs a picture or two behind its input, so the frame that comes
                    // back here is not always the one just fed. Anything without pixels was one
                    // of the catch-up frames and is not for showing.
                    if !frame.rgba.is_empty() {
                        image = Some(Image {
                            name: "texture".into(),
                            width: frame.width,
                            height: frame.height,
                            rgba: frame.rgba,
                        });
                    }
                }
                Ok(None) => track.fed = Some(frame_index),
                // A texture that stops decoding should not stop the geometry. The message is
                // already on the scene's metadata; dropping the decoder stops it repeating.
                Err(_) => {
                    track.decoder = None;
                    break;
                }
            }
        }
        Ok((mesh, image))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 64-byte header of a real capture: `daniel-s0.ares`, 272 frames at 30 fps.
    const REAL_HEADER: [u8; 64] = [
        0x41, 0x52, 0x45, 0x53, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xf0,
        0x41, 0x10, 0x01, 0x00, 0x00, 0xab, 0x58, 0x8a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xec, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08,
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1e, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xca, 0x4a, 0xb7, 0xb7,
    ];

    #[test]
    fn the_header_of_a_real_capture_reads_correctly() {
        let h = parse_header(&REAL_HEADER).expect("a real ARES header");
        assert_eq!(h.version, (0, 2));
        assert_eq!(h.geometry_profile, 0, "mesh IPB");
        assert_eq!(h.texture_codec, TextureCodec::Av1);
        assert_eq!(h.intra_codec, 0, "meshopt");
        assert_eq!(h.fps, 30.0);
        assert_eq!(h.frame_count, 272);
        assert_eq!(h.duration_us, 9_066_667);
        assert_eq!(h.superblock_offset, 64);
        assert_eq!(h.gop_index_offset, 236);
        assert_eq!(h.track_dir_offset, 520);
    }

    #[test]
    fn a_file_that_is_not_ares_is_refused_before_anything_is_allocated() {
        assert!(parse_header(b"not an ares file at all, but long enough to be one").is_err());
        assert!(parse_header(&[]).is_err());
        assert!(parse_header(&REAL_HEADER[..40]).is_err());
    }

    #[test]
    fn a_future_major_version_is_refused_rather_than_guessed_at() {
        let mut h = REAL_HEADER;
        h[super::h::VERSION_MAJOR] = 1;
        let e = parse_header(&h).unwrap_err().to_string();
        assert!(e.contains("version"), "{e}");
    }

    #[test]
    fn the_reader_never_walks_off_the_end() {
        let data = [1u8, 2, 3];
        let mut r = Reader::at(&data, 0);
        assert_eq!(r.u16(), Some(0x0201));
        assert_eq!(r.u32(), None, "a short read must fail, not wrap");
        let mut r = Reader::at(&data, 99);
        assert_eq!(r.u8(), None);
        assert_eq!(
            r.take(usize::MAX),
            None,
            "an overflowing length must not panic"
        );
    }

    #[test]
    fn dequantization_maps_the_integer_range_onto_the_bounding_box() {
        let superblock = Superblock {
            bounds: Aabb {
                min: [-1.0, 0.0, 2.0],
                max: [1.0, 4.0, 6.0],
            },
            quant_bits_pos: 14,
            quant_bits_uv: 14,
            normal_encoding: 0,
            gop_length: 30,
            texture_offset: 0,
            texture_length: 0,
            texture_blob: TextureBlob::None,
            texture_width: 0,
            texture_height: 0,
            meta: Vec::new(),
        };
        let levels = (1u32 << 14) - 1;
        let q = QuantGeometry {
            vertex_count: 3,
            positions: vec![
                0,
                0,
                0,
                0, // the minimum corner
                levels as u16,
                levels as u16,
                levels as u16,
                0, // the maximum corner
                (levels / 2) as u16,
                (levels / 2) as u16,
                (levels / 2) as u16,
                0,
            ],
            uvs: vec![0, 0, 65535, 65535, 32768, 16384],
            normals: Vec::new(),
            indices: vec![0, 1, 2],
        };
        let frame = to_mesh_frame(&q, &superblock, &superblock.bounds, true);
        assert_eq!(frame.positions[0], [-1.0, 0.0, 2.0]);
        assert_eq!(frame.positions[1], [1.0, 4.0, 6.0]);
        for (got, want) in frame.positions[2].iter().zip([0.0, 2.0, 4.0]) {
            assert!((got - want).abs() < 1e-3, "{got} vs {want}");
        }
        assert_eq!(frame.uvs[0], [0.0, 0.0]);
        assert_eq!(frame.uvs[1], [1.0, 1.0]);
    }

    #[test]
    fn predicted_positions_wrap_at_sixteen_bits_like_the_encoder_subtracted_them() {
        // The encoder stores `cur - prev` as a 16-bit two's complement value, so a vertex that
        // crossed zero has to wrap back the same way. Saturating instead would pin it.
        let previous = QuantGeometry {
            vertex_count: 1,
            positions: vec![10, 65_530, 0, 0],
            uvs: vec![0, 0],
            normals: vec![0; 4],
            indices: vec![0],
        };
        let deltas: [u16; 4] = [(-20i16) as u16, 10, 5, 0];
        let mut positions = previous.positions.clone();
        for (p, d) in positions.iter_mut().zip(deltas.iter()) {
            *p = p.wrapping_add(*d);
        }
        assert_eq!(positions[0], 65_526, "10 - 20 must wrap to 65526");
        assert_eq!(positions[1], 4, "65530 + 10 must wrap to 4");
    }

    #[test]
    fn octahedral_normals_come_back_unit_length_over_the_whole_square() {
        for i in 0..=20 {
            for j in 0..=20 {
                let x = i as f32 / 10.0 - 1.0;
                let y = j as f32 / 10.0 - 1.0;
                let n = octahedral_to_normal(x, y);
                let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                assert!(
                    (len - 1.0).abs() < 1e-4,
                    "{x},{y} gave {n:?} of length {len}"
                );
            }
        }
        // The four cardinal directions have to land where the encoding says.
        assert_eq!(octahedral_to_normal(0.0, 0.0), [0.0, 0.0, 1.0]);
        let down = octahedral_to_normal(1.0, 0.0);
        assert!(
            (down[0] - 1.0).abs() < 1e-4 && down[2].abs() < 1e-4,
            "{down:?}"
        );
    }

    #[test]
    fn a_legacy_byte_normal_decodes_to_a_unit_vector() {
        let data = [127u8, 0, 0, 0, 0, 127, 0, 0];
        let n = decode_normals(&data, 2, 0);
        assert_eq!(n[0], [1.0, 0.0, 0.0]);
        assert_eq!(n[1], [0.0, 1.0, 0.0]);
        // A degenerate normal must not produce NaN, which would take the whole triangle with it.
        assert_eq!(decode_normals(&[0, 0, 0, 0], 1, 0)[0], [0.0, 1.0, 0.0]);
    }

    #[test]
    fn a_texture_block_table_is_walked_into_frame_ranges() {
        let mut data = Vec::new();
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&4u32.to_le_bytes());
        data.push(1); // key frame
        data.extend_from_slice(&3u32.to_le_bytes());
        data.push(0);
        data.extend_from_slice(b"AAAABBB");

        let frames = parse_texture_block(&data, 30, 0);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, 30);
        assert!(frames[0].1, "the first frame is a keyframe");
        assert_eq!(&data[frames[0].2.clone()], b"AAAA");
        assert_eq!(frames[1].0, 31);
        assert!(!frames[1].1);
        assert_eq!(&data[frames[1].2.clone()], b"BBB");
    }

    #[test]
    fn texture_frame_ranges_are_absolute_file_offsets() {
        // The ranges are used to slice the whole file, not the block. Getting this wrong feeds
        // the decoder container bytes, which it accepts without ever producing a picture.
        let mut data = Vec::new();
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&4u32.to_le_bytes());
        data.push(1);
        data.extend_from_slice(b"AAAA");

        let base = 1_000_000usize;
        let frames = parse_texture_block(&data, 0, base);
        assert_eq!(frames[0].2.start, base + 9);
        assert_eq!(frames[0].2.len(), 4);
    }

    #[test]
    fn a_truncated_texture_block_yields_what_it_can_rather_than_panicking() {
        let mut data = Vec::new();
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&4u32.to_le_bytes());
        data.push(1);
        data.extend_from_slice(&9999u32.to_le_bytes());
        data.push(0);
        data.extend_from_slice(b"AAAA");
        let frames = parse_texture_block(&data, 0, 0);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn the_gop_search_lands_on_the_gop_containing_the_frame() {
        let decoder = GeometryDecoder {
            superblock: Superblock {
                bounds: Aabb::EMPTY,
                quant_bits_pos: 14,
                quant_bits_uv: 14,
                normal_encoding: 0,
                gop_length: 30,
                texture_offset: 0,
                texture_length: 0,
                texture_blob: TextureBlob::None,
                texture_width: 0,
                texture_height: 0,
                meta: Vec::new(),
            },
            gops: (0..10)
                .map(|i| Gop {
                    start_pts_us: i as u64 * 1_000_000,
                    frame_start: i * 30,
                    frame_count: 30,
                    byte_offset: 0,
                    byte_length: 0,
                })
                .collect(),
            state: None,
        };
        assert_eq!(decoder.gop_for(0), 0);
        assert_eq!(decoder.gop_for(29), 0);
        assert_eq!(decoder.gop_for(30), 1);
        assert_eq!(decoder.gop_for(31), 1);
        assert_eq!(decoder.gop_for(299), 9);
        // Past the end clamps to the last GOP rather than indexing out of range.
        assert_eq!(decoder.gop_for(10_000), 9);
    }
}
