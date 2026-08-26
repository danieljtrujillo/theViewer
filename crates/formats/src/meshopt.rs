//! The meshoptimizer vertex codec, versions 0 and 1.
//!
//! Ported from the scalar decoder in `meshoptimizer`'s `vertexcodec.cpp` (MIT, Arseny Kapoulkine),
//! which is vendored in the `meshopt` crate's `vendor/src`. Ported rather than linked because the
//! binding crate hard-codes a dynamic `-lstdc++` on `windows-gnu`, and a `viewer.exe` that needs
//! `libstdc++-6.dll` on PATH cannot be launched from Explorer — see `crates/app/build.rs`. The
//! pure-Rust `meshopt-rs` crate implements only version 0, and every `.ares` file measured here is
//! version 1, so neither existing option covers the case.
//!
//! The format, briefly, because none of it is guessable from the code alone:
//!
//! - One header byte, `0xa0 | version`.
//! - Then blocks of up to 256 vertices, each **transposed**: all vertices' byte 0, then all of
//!   byte 1, and so on. Neighbouring vertices differ in their low bytes and rarely in their high
//!   ones, and transposing puts the bytes that compress well next to each other.
//! - Each byte plane is delta-coded against the previous vertex, zigzagged, then packed at 0, 2, 4
//!   or 8 bits per value (version 1 adds 1 bit), in groups of 16 with a 2-bit selector each. A
//!   value of all-ones escapes to a literal byte from a second stream running alongside.
//! - A **tail** at the very end of the buffer holds the last vertex — which is what the first
//!   block deltas against — and, in version 1, one control byte per four bytes of stride.
//!
//! Version 1 adds two things on top: per-four-byte-column control selecting between packed, all
//! zero, and literal, and a per-column *channel* mode choosing whether deltas are taken over 1, 2
//! or 4 bytes at a time, the last of those as a rotate-and-XOR rather than a subtraction.

/// `0xa0` in the high nibble, version in the low.
const HEADER: u8 = 0xA0;
/// The highest version this decoder understands.
const MAX_VERSION: u8 = 1;

const BLOCK_SIZE_BYTES: usize = 8192;
const BLOCK_MAX_VERTICES: usize = 256;
const GROUP_SIZE: usize = 16;
/// Most bytes one 16-value group can consume: 8 packed bytes plus 16 escapes.
const GROUP_DECODE_LIMIT: usize = 24;
const TAIL_MIN_V0: usize = 32;
const TAIL_MIN_V1: usize = 24;

const BITS_V0: [u8; 4] = [0, 2, 4, 8];
const BITS_V1: [u8; 5] = [0, 1, 2, 4, 8];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Not a meshopt vertex buffer at all.
    BadHeader,
    /// A version this decoder does not implement.
    UnsupportedVersion(u8),
    /// Ran out of input.
    Truncated,
    /// Input left over after the last block, which means the stream and the declared vertex count
    /// disagree.
    ExtraBytes,
    /// A channel mode the format does not define.
    BadChannel,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::BadHeader => write!(f, "not a meshopt vertex buffer"),
            DecodeError::UnsupportedVersion(v) => {
                write!(f, "meshopt vertex codec version {v} is not supported")
            }
            DecodeError::Truncated => write!(f, "the meshopt vertex buffer is truncated"),
            DecodeError::ExtraBytes => {
                write!(f, "the meshopt vertex buffer has trailing bytes")
            }
            DecodeError::BadChannel => write!(f, "the meshopt vertex buffer names a channel mode that does not exist"),
        }
    }
}

/// Vertices per block, which is implicitly part of the format and cannot be changed.
fn block_vertices(vertex_size: usize) -> usize {
    let result = (BLOCK_SIZE_BYTES / vertex_size) & !(GROUP_SIZE - 1);
    result.min(BLOCK_MAX_VERTICES)
}

/// Decode a meshopt vertex buffer into `destination`, which must be exactly
/// `vertex_count * vertex_size` bytes.
pub fn decode_vertex_buffer(
    destination: &mut [u8],
    vertex_count: usize,
    vertex_size: usize,
    buffer: &[u8],
) -> Result<(), DecodeError> {
    if vertex_size == 0 || vertex_size > 256 || vertex_size % 4 != 0 {
        return Err(DecodeError::BadHeader);
    }
    if destination.len() != vertex_count * vertex_size {
        return Err(DecodeError::Truncated);
    }
    let Some((&header, mut data)) = buffer.split_first() else {
        return Err(DecodeError::Truncated);
    };
    if header & 0xF0 != HEADER {
        return Err(DecodeError::BadHeader);
    }
    let version = header & 0x0F;
    if version > MAX_VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }

    let control_size = if version == 0 { 0 } else { vertex_size / 4 };
    let tail_size = vertex_size + control_size;
    let tail_min = if version == 0 { TAIL_MIN_V0 } else { TAIL_MIN_V1 };
    let tail_padded = tail_size.max(tail_min);
    if data.len() < tail_padded {
        return Err(DecodeError::Truncated);
    }

    // The tail sits at the very end of the buffer, past every block. Its first `vertex_size` bytes
    // are the vertex the *first* block deltas against, which is why decoding cannot start until
    // the end of the stream has been read.
    let tail = &data[data.len() - tail_size..];
    let mut last_vertex = [0u8; 256];
    last_vertex[..vertex_size].copy_from_slice(&tail[..vertex_size]);
    let channels = &tail[vertex_size..];

    let per_block = block_vertices(vertex_size);
    let mut offset = 0usize;
    while offset < vertex_count {
        let n = per_block.min(vertex_count - offset);
        data = decode_block(
            data,
            &mut destination[offset * vertex_size..(offset + n) * vertex_size],
            n,
            vertex_size,
            &mut last_vertex,
            channels,
            version,
        )?;
        offset += n;
    }

    // Exactly the tail should remain. Anything else means the count and the stream disagree, and
    // the decoded vertices cannot be trusted.
    if data.len() != tail_padded {
        return Err(DecodeError::ExtraBytes);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_block<'a>(
    mut data: &'a [u8],
    vertex_data: &mut [u8],
    vertex_count: usize,
    vertex_size: usize,
    last_vertex: &mut [u8; 256],
    channels: &[u8],
    version: u8,
) -> Result<&'a [u8], DecodeError> {
    // The four byte-planes of one column, laid end to end at a stride of `vertex_count`. Each is
    // decoded to the group-aligned length, so the planes overlap by up to 15 bytes — harmless,
    // because only the first `vertex_count` bytes of each are ever read back.
    let mut planes = [0u8; BLOCK_MAX_VERTICES * 4];
    let mut transposed = [0u8; BLOCK_SIZE_BYTES];

    let aligned = vertex_count.div_ceil(GROUP_SIZE) * GROUP_SIZE;
    let control_size = if version == 0 { 0 } else { vertex_size / 4 };
    if data.len() < control_size {
        return Err(DecodeError::Truncated);
    }
    let control = &data[..control_size];
    data = &data[control_size..];

    for k in (0..vertex_size).step_by(4) {
        let control_byte = if version == 0 { 0 } else { control[k / 4] };

        for j in 0..4 {
            let mode = (control_byte >> (j * 2)) & 3;
            let plane = j * vertex_count;
            match mode {
                // Literal: the plane is stored uncompressed.
                3 => {
                    if data.len() < vertex_count {
                        return Err(DecodeError::Truncated);
                    }
                    planes[plane..plane + vertex_count].copy_from_slice(&data[..vertex_count]);
                    data = &data[vertex_count..];
                }
                // Every delta in the plane is zero.
                2 => {
                    let end = (plane + aligned).min(planes.len());
                    planes[plane..end].fill(0);
                }
                _ => {
                    // Version 0 always uses the 0/2/4/8 ladder; version 1 slides a window into
                    // the 0/1/2/4/8 ladder according to the control bits.
                    let bits: &[u8] = if version == 0 {
                        &BITS_V0
                    } else {
                        &BITS_V1[mode as usize..mode as usize + 4]
                    };
                    data = decode_bytes(data, &mut planes[plane..], aligned, bits)?;
                }
            }
        }

        let channel = if version == 0 { 0 } else { channels[k / 4] };
        match channel & 3 {
            0 => decode_deltas::<1>(
                &planes,
                &mut transposed[k..],
                vertex_count,
                vertex_size,
                &last_vertex[k..],
                0,
                false,
            ),
            1 => decode_deltas::<2>(
                &planes,
                &mut transposed[k..],
                vertex_count,
                vertex_size,
                &last_vertex[k..],
                0,
                false,
            ),
            2 => decode_deltas::<4>(
                &planes,
                &mut transposed[k..],
                vertex_count,
                vertex_size,
                &last_vertex[k..],
                (32 - (channel >> 4)) & 31,
                true,
            ),
            _ => return Err(DecodeError::BadChannel),
        }
    }

    let block_bytes = vertex_count * vertex_size;
    vertex_data.copy_from_slice(&transposed[..block_bytes]);
    last_vertex[..vertex_size]
        .copy_from_slice(&transposed[vertex_size * (vertex_count - 1)..block_bytes]);
    Ok(data)
}

/// Undo the delta coding for one four-byte column, writing interleaved vertices.
///
/// `WIDTH` is how many bytes the deltas were taken over: 1, 2 or 4. At width 4 the operation is a
/// rotate and XOR rather than an addition, which is what the encoder uses for float columns whose
/// exponents barely change.
fn decode_deltas<const WIDTH: usize>(
    planes: &[u8],
    transposed: &mut [u8],
    vertex_count: usize,
    vertex_size: usize,
    last_vertex: &[u8],
    rotation: u8,
    xor: bool,
) {
    let mut plane_offset = 0usize;
    let mut last_offset = 0usize;

    for k in (0..4).step_by(WIDTH) {
        let mut vertex_offset = k;

        let mut previous: u32 = 0;
        for j in 0..WIDTH {
            previous |= (last_vertex[last_offset + j] as u32) << (8 * j);
        }

        for i in 0..vertex_count {
            let mut value: u32 = 0;
            for j in 0..WIDTH {
                value |= (planes[plane_offset + i + vertex_count * j] as u32) << (8 * j);
            }

            value = if xor {
                rotate(value, rotation) ^ previous
            } else {
                unzigzag(value, WIDTH).wrapping_add(previous) & mask(WIDTH)
            };

            for j in 0..WIDTH {
                transposed[vertex_offset + j] = (value >> (j * 8)) as u8;
            }
            previous = value;
            vertex_offset += vertex_size;
        }

        plane_offset += vertex_count * WIDTH;
        last_offset += WIDTH;
    }
}

fn mask(width: usize) -> u32 {
    if width >= 4 {
        u32::MAX
    } else {
        (1u32 << (width * 8)) - 1
    }
}

/// Left rotate, 32-bit. `r == 0` must be a no-op, which a bare `v >> 32` would not be.
fn rotate(v: u32, r: u8) -> u32 {
    v.rotate_left(r as u32)
}

/// Map an unsigned value back to the signed delta it encodes: 0, -1, 1, -2, 2 ...
fn unzigzag(v: u32, width: usize) -> u32 {
    let m = mask(width);
    ((0u32.wrapping_sub(v & 1)) ^ (v >> 1)) & m
}

/// Decode `count` bytes into `buffer`, in groups of 16 with a 2-bit width selector each.
fn decode_bytes<'a>(
    mut data: &'a [u8],
    buffer: &mut [u8],
    count: usize,
    bits: &[u8],
) -> Result<&'a [u8], DecodeError> {
    debug_assert!(count % GROUP_SIZE == 0);
    // Four selectors to a byte, rounded up.
    let header_size = (count / GROUP_SIZE).div_ceil(4);
    if data.len() < header_size {
        return Err(DecodeError::Truncated);
    }
    let header = &data[..header_size];
    data = &data[header_size..];

    for i in (0..count).step_by(GROUP_SIZE) {
        if data.len() < GROUP_DECODE_LIMIT {
            return Err(DecodeError::Truncated);
        }
        let group = i / GROUP_SIZE;
        let selector = (header[group / 4] >> ((group % 4) * 2)) & 3;
        data = decode_group(data, &mut buffer[i..i + GROUP_SIZE], bits[selector as usize]);
    }
    Ok(data)
}

/// One group of sixteen values at a fixed bit width.
///
/// Two cursors run at once: the packed values, and a stream of escape bytes starting immediately
/// after them. A packed value of all ones means "take the next byte from the escape stream", which
/// is what lets a mostly-small plane carry the occasional large delta without widening everything.
/// The caller guarantees at least 24 readable bytes, which is the most any group can consume.
fn decode_group<'a>(data: &'a [u8], buffer: &mut [u8], bits: u8) -> &'a [u8] {
    match bits {
        0 => {
            buffer[..GROUP_SIZE].fill(0);
            data
        }
        8 => {
            buffer[..GROUP_SIZE].copy_from_slice(&data[..GROUP_SIZE]);
            &data[GROUP_SIZE..]
        }
        bits => {
            let per_byte = 8 / bits as usize;
            let packed_bytes = GROUP_SIZE / per_byte;
            let mut escape = packed_bytes;
            let mut out = 0usize;

            for byte_index in 0..packed_bytes {
                let mut byte = data[byte_index];
                // The 1-bit path packs its values in the opposite order to the others, so the
                // byte is reversed before it is peeled apart. Missing this decodes every 1-bit
                // group backwards, which looks like plausible noise rather than an error.
                if bits == 1 {
                    byte = byte.reverse_bits();
                }
                let sentinel = (1u16 << bits) - 1;
                for _ in 0..per_byte {
                    let encoded = (byte >> (8 - bits)) as u16;
                    byte = byte.wrapping_shl(bits as u32);
                    if encoded == sentinel {
                        buffer[out] = data[escape];
                        escape += 1;
                    } else {
                        buffer[out] = encoded as u8;
                    }
                    out += 1;
                }
            }
            &data[escape..]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_that_is_not_meshopt_is_refused() {
        let mut out = [0u8; 8];
        assert_eq!(
            decode_vertex_buffer(&mut out, 1, 8, &[0x00; 64]),
            Err(DecodeError::BadHeader)
        );
        assert_eq!(
            decode_vertex_buffer(&mut out, 1, 8, &[]),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn a_future_version_is_named_rather_than_guessed_at() {
        let mut out = [0u8; 8];
        let mut buffer = vec![0xA0 | 7];
        buffer.extend_from_slice(&[0u8; 64]);
        assert_eq!(
            decode_vertex_buffer(&mut out, 1, 8, &buffer),
            Err(DecodeError::UnsupportedVersion(7))
        );
    }

    #[test]
    fn a_vertex_size_the_format_does_not_allow_is_refused() {
        let mut out = [0u8; 7];
        // The codec only handles multiples of four.
        assert_eq!(
            decode_vertex_buffer(&mut out, 1, 7, &[0xA1; 64]),
            Err(DecodeError::BadHeader)
        );
    }

    #[test]
    fn a_truncated_buffer_never_panics() {
        // Every prefix of a plausible buffer must produce an error, not an index out of range.
        let mut buffer = vec![0xA1u8];
        buffer.extend((0..200).map(|i| (i * 37 % 251) as u8));
        for cut in 0..buffer.len() {
            let mut out = vec![0u8; 8 * 4];
            let _ = decode_vertex_buffer(&mut out, 4, 8, &buffer[..cut]);
        }
    }

    #[test]
    fn zigzag_maps_small_deltas_to_small_codes() {
        // 0, -1, 1, -2, 2 ... which is what makes a delta plane compress at two bits.
        assert_eq!(unzigzag(0, 1), 0);
        assert_eq!(unzigzag(1, 1), 0xFF);
        assert_eq!(unzigzag(2, 1), 1);
        assert_eq!(unzigzag(3, 1), 0xFE);
        assert_eq!(unzigzag(4, 1), 2);
        // Width 2 and 4 use the same mapping in their own precision.
        assert_eq!(unzigzag(1, 2), 0xFFFF);
        assert_eq!(unzigzag(1, 4), 0xFFFF_FFFF);
        assert_eq!(unzigzag(2, 4), 1);
    }

    #[test]
    fn rotation_by_zero_is_the_identity() {
        // Written as a shift this is undefined behaviour in C and a wrong answer in Rust.
        assert_eq!(rotate(0x1234_5678, 0), 0x1234_5678);
        assert_eq!(rotate(0x0000_0001, 1), 0x0000_0002);
        assert_eq!(rotate(0x8000_0000, 1), 0x0000_0001);
    }

    #[test]
    fn the_block_size_is_the_one_the_format_fixes() {
        // 8192 / stride, rounded down to a multiple of 16, capped at 256.
        assert_eq!(block_vertices(4), 256);
        assert_eq!(block_vertices(8), 256);
        assert_eq!(block_vertices(32), 256);
        assert_eq!(block_vertices(64), 128);
        assert_eq!(block_vertices(128), 64);
    }

    #[test]
    fn a_zero_bit_group_decodes_to_zeros_and_consumes_nothing() {
        let data = [0xFFu8; 32];
        let mut buffer = [0xAAu8; 16];
        let rest = decode_group(&data, &mut buffer, 0);
        assert_eq!(buffer, [0u8; 16]);
        assert_eq!(rest.len(), data.len());
    }

    #[test]
    fn an_eight_bit_group_is_a_straight_copy() {
        let data: Vec<u8> = (0..32).collect();
        let mut buffer = [0u8; 16];
        let rest = decode_group(&data, &mut buffer, 8);
        assert_eq!(&buffer[..], &data[..16]);
        assert_eq!(rest.len(), 16);
    }

    #[test]
    fn a_two_bit_group_escapes_on_all_ones() {
        // Four packed bytes, then the escape stream. 0b11 means "take the next escape byte".
        let mut data = vec![0b00_01_10_11, 0b00_00_00_00, 0b00_00_00_00, 0b00_00_00_00];
        data.push(0x7F); // the one escape
        data.extend_from_slice(&[0u8; 32]);

        let mut buffer = [0xAAu8; 16];
        let rest = decode_group(&data, &mut buffer, 2);
        // Values come out most significant pair first.
        assert_eq!(&buffer[..4], &[0, 1, 2, 0x7F]);
        assert_eq!(&buffer[4..], &[0u8; 12]);
        // Four packed bytes plus one escape consumed.
        assert_eq!(data.len() - rest.len(), 5);
    }

    #[test]
    fn a_one_bit_group_reads_its_byte_in_reverse() {
        // The 1-bit path is the only one whose values run low bit first. A group whose first
        // value escapes has bit 0 set, not bit 7.
        let mut data = vec![0b0000_0001, 0b0000_0000];
        data.push(0x42);
        data.extend_from_slice(&[0u8; 32]);

        let mut buffer = [0xAAu8; 16];
        let rest = decode_group(&data, &mut buffer, 1);
        assert_eq!(buffer[0], 0x42, "the first value should have escaped");
        assert_eq!(&buffer[1..], &[0u8; 15]);
        assert_eq!(data.len() - rest.len(), 3);
    }

    #[test]
    fn a_four_bit_group_packs_two_values_per_byte() {
        let mut data = vec![0x12u8, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0];
        data.push(0x99); // for the 0xF nibble, which escapes
        data.extend_from_slice(&[0u8; 32]);

        let mut buffer = [0u8; 16];
        let rest = decode_group(&data, &mut buffer, 4);
        assert_eq!(&buffer[..4], &[1, 2, 3, 4]);
        assert_eq!(buffer[14], 0x99, "0xF must escape");
        assert_eq!(buffer[15], 0x0);
        assert_eq!(data.len() - rest.len(), 9);
    }
}
