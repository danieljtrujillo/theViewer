//! Volume raymarch resources.

use bytemuck::{Pod, Zeroable};
use viewer_scene::payload::{VolumeGrid, VoxelType};
use viewer_scene::Aabb;

/// Display controls, held by the app and written into the uniform each frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeView {
    /// Full width of the displayed intensity range, in normalized units.
    pub window: f32,
    /// Centre of that range.
    pub level: f32,
    pub mode: VolumeMode,
    pub axis: Axis,
    /// Position of the slice plane, 0 to 1.
    pub slice: f32,
    pub projection: Projection,
    /// Samples below this contribute nothing, which removes the air around a scanned subject.
    pub threshold: f32,
    pub opacity: f32,
}

impl Default for VolumeView {
    fn default() -> Self {
        VolumeView {
            window: 1.0,
            level: 0.5,
            mode: VolumeMode::Slice,
            axis: Axis::Axial,
            slice: 0.5,
            projection: Projection::Composite,
            threshold: 0.12,
            opacity: 0.06,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeMode {
    Slice,
    Raycast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Axial,
    Coronal,
    Sagittal,
}

impl Axis {
    pub fn label(self) -> &'static str {
        match self {
            Axis::Axial => "Axial",
            Axis::Coronal => "Coronal",
            Axis::Sagittal => "Sagittal",
        }
    }

    fn index(self) -> f32 {
        match self {
            Axis::Axial => 0.0,
            Axis::Coronal => 1.0,
            Axis::Sagittal => 2.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    Composite,
    /// Maximum intensity projection, the standard read for angiography and bone.
    MaximumIntensity,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct VolumeUniform {
    box_min: [f32; 4],
    box_max: [f32; 4],
    a: [f32; 4],
    b: [f32; 4],
}

impl VolumeUniform {
    pub(crate) fn new(bounds: &Aabb, view: &VolumeView) -> Self {
        VolumeUniform {
            box_min: [bounds.min[0], bounds.min[1], bounds.min[2], 0.0],
            box_max: [bounds.max[0], bounds.max[1], bounds.max[2], 0.0],
            a: [
                view.window.max(1e-4),
                view.level,
                match view.mode {
                    VolumeMode::Slice => 0.0,
                    VolumeMode::Raycast => 1.0,
                },
                view.axis.index(),
            ],
            b: [
                view.slice.clamp(0.0, 1.0),
                match view.projection {
                    Projection::Composite => 0.0,
                    Projection::MaximumIntensity => 1.0,
                },
                view.threshold,
                view.opacity,
            ],
        }
    }
}

/// The 36 corners of a unit cube, as two triangles per face.
pub(crate) const CUBE: [[f32; 3]; 36] = {
    const A: [f32; 3] = [0.0, 0.0, 0.0];
    const B: [f32; 3] = [1.0, 0.0, 0.0];
    const C: [f32; 3] = [1.0, 1.0, 0.0];
    const D: [f32; 3] = [0.0, 1.0, 0.0];
    const E: [f32; 3] = [0.0, 0.0, 1.0];
    const F: [f32; 3] = [1.0, 0.0, 1.0];
    const G: [f32; 3] = [1.0, 1.0, 1.0];
    const H: [f32; 3] = [0.0, 1.0, 1.0];
    [
        A, C, B, A, D, C, // back
        E, F, G, E, G, H, // front
        A, B, F, A, F, E, // bottom
        D, H, G, D, G, C, // top
        B, C, G, B, G, F, // right
        A, E, H, A, H, D, // left
    ]
};

/// Convert a grid to normalized 16-bit float samples.
///
/// Half precision rather than 8-bit because CT is 12-bit and MR routinely wider, and baking a
/// window into 8 bits would make the window control useless. Half is filterable everywhere WebGPU
/// runs, which 32-bit float is not.
///
/// Returns the samples plus the source range that produced them, so the window and level controls
/// can be reported in the original units.
pub fn to_half(grid: &VolumeGrid) -> (Vec<u8>, (f32, f32)) {
    let n = grid.voxel_count();
    let mut values: Vec<f32> = Vec::with_capacity(n);

    match grid.voxel {
        VoxelType::U8 => values.extend(grid.data.iter().map(|v| *v as f32)),
        VoxelType::U16 => values.extend(
            grid.data
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]) as f32),
        ),
        VoxelType::I16 => values.extend(
            grid.data
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32),
        ),
        VoxelType::F32 => values.extend(
            grid.data
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
        ),
    }
    values.truncate(n);
    values.resize(n, 0.0);

    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for v in &values {
        if v.is_finite() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    if !lo.is_finite() || !hi.is_finite() || hi <= lo {
        lo = 0.0;
        hi = 1.0;
    }
    let span = hi - lo;

    let mut out = Vec::with_capacity(n * 2);
    for v in &values {
        let t = if v.is_finite() {
            ((v - lo) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        out.extend_from_slice(&f32_to_f16(t).to_le_bytes());
    }
    (out, (lo, hi))
}

/// Round-to-nearest-even f32 to f16. Only finite inputs in 0 to 1 reach this, so the subnormal and
/// overflow paths exist for safety rather than for the data actually passed in.
fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let mut exponent = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    let mantissa = bits & 0x007F_FFFF;

    if exponent >= 0x1F {
        // Overflow to infinity, or pass a NaN through.
        return sign | 0x7C00 | if mantissa != 0 { 0x0200 } else { 0 };
    }
    if exponent <= 0 {
        if exponent < -10 {
            return sign;
        }
        // Subnormal: shift the implicit leading one back in.
        let m = mantissa | 0x0080_0000;
        let shift = (14 - exponent) as u32;
        let half = (m >> shift) as u16;
        let round = (m >> (shift - 1)) & 1;
        return sign | (half + round as u16);
    }
    let mut half_mantissa = (mantissa >> 13) as u16;
    // Round to nearest, ties to even.
    let remainder = mantissa & 0x1FFF;
    if remainder > 0x1000 || (remainder == 0x1000 && (half_mantissa & 1) == 1) {
        half_mantissa += 1;
        if half_mantissa == 0x400 {
            half_mantissa = 0;
            exponent += 1;
            if exponent >= 0x1F {
                return sign | 0x7C00;
            }
        }
    }
    sign | ((exponent as u16) << 10) | half_mantissa
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f16_to_f32(h: u16) -> f32 {
        let sign = ((h >> 15) & 1) as u32;
        let exp = ((h >> 10) & 0x1F) as u32;
        let man = (h & 0x3FF) as u32;
        let bits = if exp == 0 {
            if man == 0 {
                sign << 31
            } else {
                // Renormalize the subnormal.
                let mut e = -1i32;
                let mut m = man;
                while m & 0x400 == 0 {
                    m <<= 1;
                    e -= 1;
                }
                m &= 0x3FF;
                (sign << 31) | (((e + 127 + 15) as u32) << 23) | (m << 13)
            }
        } else if exp == 0x1F {
            (sign << 31) | 0x7F80_0000 | (man << 13)
        } else {
            (sign << 31) | ((exp + 127 - 15) << 23) | (man << 13)
        };
        f32::from_bits(bits)
    }

    #[test]
    fn half_conversion_round_trips_across_the_unit_range() {
        for i in 0..=100 {
            let v = i as f32 / 100.0;
            let back = f16_to_f32(f32_to_f16(v));
            assert!((back - v).abs() < 0.001, "{v} became {back}");
        }
    }

    #[test]
    fn half_conversion_handles_the_endpoints() {
        assert_eq!(f16_to_f32(f32_to_f16(0.0)), 0.0);
        assert!((f16_to_f32(f32_to_f16(1.0)) - 1.0).abs() < 1e-6);
    }

    fn grid(voxel: VoxelType, data: Vec<u8>, dims: [u32; 3]) -> VolumeGrid {
        VolumeGrid {
            dims,
            spacing: [1.0; 3],
            voxel,
            bounds: Aabb::EMPTY,
            data,
            window: None,
        }
    }

    #[test]
    fn u8_data_normalizes_across_its_own_range() {
        let g = grid(VoxelType::U8, vec![0, 128, 255, 64, 0, 0, 0, 0], [2, 2, 2]);
        let (half, range) = to_half(&g);
        assert_eq!(half.len(), 16);
        assert_eq!(range, (0.0, 255.0));
        let first = f16_to_f32(u16::from_le_bytes([half[0], half[1]]));
        let third = f16_to_f32(u16::from_le_bytes([half[4], half[5]]));
        assert!(first.abs() < 1e-3);
        assert!((third - 1.0).abs() < 1e-3);
    }

    #[test]
    fn signed_16_bit_data_maps_its_negative_floor_to_zero() {
        // CT stores Hounsfield units, where air is about -1000.
        let mut data = Vec::new();
        for v in [-1000i16, 0, 1000, 3000, 0, 0, 0, 0] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let g = grid(VoxelType::I16, data, [2, 2, 2]);
        let (half, range) = to_half(&g);
        assert_eq!(range, (-1000.0, 3000.0));
        let air = f16_to_f32(u16::from_le_bytes([half[0], half[1]]));
        assert!(air.abs() < 1e-3, "air should sit at the bottom of the range");
    }

    #[test]
    fn a_flat_volume_does_not_divide_by_zero() {
        let g = grid(VoxelType::U8, vec![7; 8], [2, 2, 2]);
        let (half, _) = to_half(&g);
        for c in half.chunks_exact(2) {
            assert!(f16_to_f32(u16::from_le_bytes([c[0], c[1]])).is_finite());
        }
    }

    #[test]
    fn a_short_buffer_is_padded_rather_than_panicking() {
        let g = grid(VoxelType::U8, vec![1, 2, 3], [2, 2, 2]);
        let (half, _) = to_half(&g);
        assert_eq!(half.len(), 8 * 2);
    }

    #[test]
    fn the_cube_is_twelve_triangles() {
        assert_eq!(CUBE.len(), 36);
        for c in CUBE.iter().flatten() {
            assert!(*c == 0.0 || *c == 1.0);
        }
    }

    #[test]
    fn the_uniform_carries_mode_and_axis_as_the_shader_reads_them() {
        let b = Aabb {
            min: [0.0; 3],
            max: [1.0; 3],
        };
        let u = VolumeUniform::new(
            &b,
            &VolumeView {
                mode: VolumeMode::Raycast,
                axis: Axis::Sagittal,
                ..Default::default()
            },
        );
        assert_eq!(u.a[2], 1.0);
        assert_eq!(u.a[3], 2.0);
    }
}
