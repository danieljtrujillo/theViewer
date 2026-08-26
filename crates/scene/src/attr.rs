//! Accessor-shaped attribute buffers.
//!
//! This is the piece that makes the memory posture work. An attribute keeps the bytes exactly as
//! the source file stored them, plus enough type information for the renderer to hand them to the
//! GPU unchanged. Nothing widens `u16` positions to `f32` on the CPU just so the vertex shader can
//! narrow them again, `.ares` quantizes to 14 bits, SPZ to int24 fixed-point, SOG to 16-bit split
//! across two images, and all three reach the GPU still quantized.

use crate::Aabb;

/// Component types, matching glTF accessor component types plus the two the splat formats need.
///
/// `I24` has no glTF equivalent. It exists because SPZ stores positions as little-endian signed
/// 24-bit fixed point, and unpacking it early would defeat the entire point of this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentType {
    I8,
    U8,
    I16,
    U16,
    I24,
    U32,
    F32,
}

impl ComponentType {
    pub const fn size(self) -> usize {
        match self {
            ComponentType::I8 | ComponentType::U8 => 1,
            ComponentType::I16 | ComponentType::U16 => 2,
            ComponentType::I24 => 3,
            ComponentType::U32 | ComponentType::F32 => 4,
        }
    }

    pub const fn is_integer(self) -> bool {
        !matches!(self, ComponentType::F32)
    }
}

/// Attribute semantics. The splat variants are named to match `KHR_gaussian_splatting` exactly, so
/// that mapping to and from glTF is a rename and nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Semantic {
    Position,
    Normal,
    Tangent,
    TexCoord(u8),
    Color(u8),
    Joints(u8),
    Weights(u8),

    /// `KHR_gaussian_splatting:ROTATION` is a VEC4 quaternion.
    SplatRotation,
    /// `KHR_gaussian_splatting:SCALE` is a VEC3.
    SplatScale,
    /// `KHR_gaussian_splatting:OPACITY` is a SCALAR.
    SplatOpacity,
    /// `KHR_gaussian_splatting:SH_DEGREE_{degree}_COEF_{index}` is a VEC3.
    ///
    /// Degree 4 exists because SPZ 4 raised the ceiling from 3; the Khronos extension currently
    /// stops at 3, so a degree-4 import has no lossless glTF export yet. That is a real gap, not
    /// an oversight here.
    SplatSh {
        degree: u8,
        index: u8,
    },

    /// Anything a format carries that we do not model. Kept rather than dropped so the inspector
    /// can show it and a round-trip does not silently lose data.
    Custom(String),
}

impl Semantic {
    /// The glTF attribute name, where one exists. `None` means this semantic has no standardized
    /// spelling and cannot be exported losslessly.
    pub fn gltf_name(&self) -> Option<String> {
        Some(match self {
            Semantic::Position => "POSITION".into(),
            Semantic::Normal => "NORMAL".into(),
            Semantic::Tangent => "TANGENT".into(),
            Semantic::TexCoord(n) => format!("TEXCOORD_{n}"),
            Semantic::Color(n) => format!("COLOR_{n}"),
            Semantic::Joints(n) => format!("JOINTS_{n}"),
            Semantic::Weights(n) => format!("WEIGHTS_{n}"),
            Semantic::SplatRotation => "KHR_gaussian_splatting:ROTATION".into(),
            Semantic::SplatScale => "KHR_gaussian_splatting:SCALE".into(),
            Semantic::SplatOpacity => "KHR_gaussian_splatting:OPACITY".into(),
            // The extension defines degrees 0..=3 only.
            Semantic::SplatSh { degree, index } if *degree <= 3 => {
                format!("KHR_gaussian_splatting:SH_DEGREE_{degree}_COEF_{index}")
            }
            Semantic::SplatSh { .. } | Semantic::Custom(_) => return None,
        })
    }
}

/// A typed, possibly-quantized attribute buffer.
///
/// `count` is the number of *elements*, not bytes and not components: a VEC3 of 1,000 vertices has
/// `count == 1000` and `components == 3`.
#[derive(Debug, Clone)]
pub struct Attribute {
    pub semantic: Semantic,
    pub component: ComponentType,
    /// 1 for SCALAR, 2 for VEC2, 3 for VEC3, 4 for VEC4.
    pub components: u8,
    pub count: usize,
    /// Integer values map to `[0,1]` (unsigned) or `[-1,1]` (signed) rather than being read as
    /// literal integers. Mirrors the glTF accessor flag.
    pub normalized: bool,
    /// Dequantization range for integer positions. `Some` means the GPU reconstructs world units
    /// as `min + (v / MAX) * (max - min)`; this is how `.ares` and SOG both work.
    pub quant_range: Option<Aabb>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrError {
    /// The declared element count does not match the buffer length. This is the cheapest possible
    /// corruption gate and every importer should run it: the SPZ importer in particular, where
    /// the exact byte total is known in advance and a zero remainder is a strong integrity signal.
    LengthMismatch {
        expected: usize,
        actual: usize,
    },
    ZeroComponents,
}

impl std::fmt::Display for AttrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttrError::LengthMismatch { expected, actual } => {
                write!(f, "attribute buffer is {actual} bytes, expected {expected}")
            }
            AttrError::ZeroComponents => write!(f, "attribute declares zero components"),
        }
    }
}

impl std::error::Error for AttrError {}

impl Attribute {
    /// Build an attribute, checking that the buffer length matches the declared shape.
    pub fn new(
        semantic: Semantic,
        component: ComponentType,
        components: u8,
        count: usize,
        data: Vec<u8>,
    ) -> Result<Self, AttrError> {
        if components == 0 {
            return Err(AttrError::ZeroComponents);
        }
        let expected = count
            .saturating_mul(components as usize)
            .saturating_mul(component.size());
        if data.len() != expected {
            return Err(AttrError::LengthMismatch {
                expected,
                actual: data.len(),
            });
        }
        Ok(Attribute {
            semantic,
            component,
            components,
            count,
            normalized: false,
            quant_range: None,
            data,
        })
    }

    pub fn normalized(mut self) -> Self {
        self.normalized = true;
        self
    }

    pub fn quantized_to(mut self, range: Aabb) -> Self {
        self.quant_range = Some(range);
        self
    }

    pub fn stride(&self) -> usize {
        self.components as usize * self.component.size()
    }

    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    /// Read element `i` as `f32`, applying normalization and quantization range.
    ///
    /// This is the *slow* path, for tools that genuinely need CPU-side values: bounds computation
    /// on a format that did not record its own AABB, or the inspector. The render path never calls
    /// it; it uploads `data` and dequantizes in the shader.
    pub fn get_f32(&self, i: usize, out: &mut [f32]) -> bool {
        let n = self.components as usize;
        if i >= self.count || out.len() < n {
            return false;
        }
        let base = i * self.stride();
        for (c, slot) in out.iter_mut().take(n).enumerate() {
            let off = base + c * self.component.size();
            let raw = match self.component {
                ComponentType::F32 => f32::from_le_bytes([
                    self.data[off],
                    self.data[off + 1],
                    self.data[off + 2],
                    self.data[off + 3],
                ]),
                ComponentType::U8 => self.norm_u(self.data[off] as f64, u8::MAX as f64),
                ComponentType::I8 => self.norm_i(self.data[off] as i8 as f64, i8::MAX as f64),
                ComponentType::U16 => {
                    let v = u16::from_le_bytes([self.data[off], self.data[off + 1]]);
                    self.norm_u(v as f64, u16::MAX as f64)
                }
                ComponentType::I16 => {
                    let v = i16::from_le_bytes([self.data[off], self.data[off + 1]]);
                    self.norm_i(v as f64, i16::MAX as f64)
                }
                ComponentType::I24 => {
                    // Little-endian signed 24-bit, sign-extended into i32. SPZ's position encoding.
                    let v = (self.data[off] as u32)
                        | ((self.data[off + 1] as u32) << 8)
                        | ((self.data[off + 2] as u32) << 16);
                    let v = ((v << 8) as i32) >> 8;
                    self.norm_i(v as f64, 0x7F_FFFF as f64)
                }
                ComponentType::U32 => {
                    let v = u32::from_le_bytes([
                        self.data[off],
                        self.data[off + 1],
                        self.data[off + 2],
                        self.data[off + 3],
                    ]);
                    self.norm_u(v as f64, u32::MAX as f64)
                }
            };
            *slot = raw;
        }

        // A quantization range only makes sense for the three spatial components.
        if let Some(range) = self.quant_range {
            for (c, slot) in out.iter_mut().take(n.min(3)).enumerate() {
                let t = if self.normalized {
                    *slot as f64
                } else {
                    // Unnormalized integers span the full type range.
                    let max = match self.component {
                        ComponentType::U8 => u8::MAX as f64,
                        ComponentType::U16 => u16::MAX as f64,
                        ComponentType::U32 => u32::MAX as f64,
                        _ => 1.0,
                    };
                    *slot as f64 / max
                };
                *slot = (range.min[c] as f64 + t * (range.max[c] - range.min[c]) as f64) as f32;
            }
        }
        true
    }

    fn norm_u(&self, v: f64, max: f64) -> f32 {
        if self.normalized {
            (v / max) as f32
        } else {
            v as f32
        }
    }

    fn norm_i(&self, v: f64, max: f64) -> f32 {
        if self.normalized {
            // glTF's signed-normalized rule: clamp at -1.0 rather than reaching -1.00003.
            ((v / max).max(-1.0)) as f32
        } else {
            v as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_mismatch_is_caught_before_anything_allocates() {
        let err = Attribute::new(Semantic::Position, ComponentType::F32, 3, 2, vec![0; 20]);
        assert_eq!(
            err.unwrap_err(),
            AttrError::LengthMismatch {
                expected: 24,
                actual: 20
            }
        );
    }

    #[test]
    fn f32_positions_round_trip() {
        let mut data = Vec::new();
        for v in [1.0f32, 2.0, 3.0] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let a = Attribute::new(Semantic::Position, ComponentType::F32, 3, 1, data).unwrap();
        let mut out = [0.0f32; 3];
        assert!(a.get_f32(0, &mut out));
        assert_eq!(out, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn i24_sign_extends_correctly() {
        // -1 in little-endian signed 24-bit is FF FF FF.
        let a = Attribute::new(
            Semantic::Position,
            ComponentType::I24,
            3,
            1,
            vec![0xFF, 0xFF, 0xFF, 0x01, 0x00, 0x00, 0x00, 0x00, 0x80],
        )
        .unwrap();
        let mut out = [0.0f32; 3];
        assert!(a.get_f32(0, &mut out));
        assert_eq!(out[0], -1.0); // FF FF FF
        assert_eq!(out[1], 1.0); // 01 00 00
        assert_eq!(out[2], -8_388_608.0); // 00 00 80, the i24 minimum
    }

    #[test]
    fn u16_positions_dequantize_against_the_aabb() {
        // Mid-range value should land at the centre of the box.
        let a = Attribute::new(
            Semantic::Position,
            ComponentType::U16,
            3,
            1,
            vec![0xFF, 0x7F, 0xFF, 0x7F, 0xFF, 0x7F],
        )
        .unwrap()
        .quantized_to(Aabb {
            min: [0.0; 3],
            max: [10.0; 3],
        });
        let mut out = [0.0f32; 3];
        assert!(a.get_f32(0, &mut out));
        for v in out {
            assert!((v - 5.0).abs() < 0.001, "got {v}");
        }
    }

    #[test]
    fn out_of_range_index_is_refused_not_panicked() {
        let a = Attribute::new(Semantic::Position, ComponentType::F32, 3, 1, vec![0; 12]).unwrap();
        let mut out = [0.0f32; 3];
        assert!(!a.get_f32(9, &mut out));
    }

    #[test]
    fn splat_semantics_spell_out_as_khronos_names_them() {
        assert_eq!(
            Semantic::SplatRotation.gltf_name().unwrap(),
            "KHR_gaussian_splatting:ROTATION"
        );
        assert_eq!(
            Semantic::SplatSh {
                degree: 2,
                index: 4
            }
            .gltf_name()
            .unwrap(),
            "KHR_gaussian_splatting:SH_DEGREE_2_COEF_4"
        );
        // SPZ 4 allows degree 4; the extension does not. No lossless export exists.
        assert!(Semantic::SplatSh {
            degree: 4,
            index: 0
        }
        .gltf_name()
        .is_none());
    }
}
