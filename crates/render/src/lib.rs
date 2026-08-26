//! The renderer: six paths, no more.
//!
//! Three are implemented (mesh raster, point sprite, sorted splat). Volume raymarch,
//! video-textured sequence and unprojected RGBD are not.
//!
//! Notes carried from the format survey that constrain this crate:
//!
//! - Sorting is the splat budget, not decoding. SPZ 4 removed its 10 M-point cap and targets 34 M+,
//!   so the CPU radix sort in [`gpu`] is a stand-in for a compute-shader sort.
//! - Splat covariance is usually degenerate. Measured median min/max axis ratio is about 0.00-0.01,
//!   so near-flat ellipsoids are the norm and the projection must not assume otherwise.

#![deny(unsafe_code)]

pub mod camera;
pub mod gpu;
pub mod screen;
pub mod volume;

pub use camera::Camera;
pub use gpu::{GpuScene, Renderer, View, DEPTH_FORMAT};
pub use screen::{zoom_for_one_to_one, FlatView, ScreenUniform};
pub use viewer_scene::payload::RenderPath;
pub use volume::{Axis, Projection, VolumeMode, VolumeView};
