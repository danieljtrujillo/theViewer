//! T0 importers, statically linked, no lazy resolution.
//!
//! Each importer's only job is to turn bytes into a [`viewer_scene::Scene`] while keeping data in
//! source precision. See `docs/FORMATS.md` for which formats belong here and why the rest do not.
//!
//! Nothing is implemented yet; this crate exists so the boundary is real from the start. The
//! implementation order that follows from `docs/ARCHITECTURE.md`:
//!
//! 1. **PLY**: one parser covers four of the sniffer's verdicts, including 3DGS. Highest coverage
//!    per line of code in the whole project.
//! 2. **SPZ v4 + legacy gzip**: the format most likely to matter, and the one where getting the
//!    version dispatch wrong is most visible.
//! 3. **GLB/glTF**: the parity set's centre of gravity, and the internal model's own vocabulary.
//! 4. **OBJ / STL**: cheap, and they complete the "double-click anything" story.
//! 5. **`.ares`**: the reference implementation is already local, so this is wiring rather than
//!    research.

#![forbid(unsafe_code)]

pub mod ares;
pub mod depthkit;
pub mod fbx;
pub mod fourds;
pub mod gltf_import;
pub mod isobmff;
pub mod lidar;
pub mod meshopt;
pub mod metaimage;
pub mod movie;
pub mod mtl;
pub mod obj;
pub mod off;
pub mod pcd;
pub mod picture;
pub mod ply;
pub mod sidecar;
pub mod spherical;
pub mod splat;
pub mod spz;
pub mod step;
pub mod stl;
pub mod threemf;
pub mod usd;
pub mod volume;
pub mod xyz;

use viewer_io::FormatId;
use viewer_scene::{Image, MeshFrame, Scene, Timeline};

/// The result of opening a file: a scene, plus a way to pull frames if the file moves.
///
/// Two return values rather than one because a static scene and a playing one are genuinely
/// different things. Video, volumetric video and animated pictures all describe their *shape* once
/// a screen, a mesh, a material, and then stream their *contents*. Folding the frame source
/// into `Scene` would make every still importer carry a field it never sets.
pub struct Opened {
    pub scene: Scene,
    pub frames: Option<Box<dyn FrameSource>>,
}

impl Opened {
    pub fn still(scene: Scene) -> Opened {
        Opened {
            scene,
            frames: None,
        }
    }
}

/// A source of frames for a scene that moves.
///
/// Pull, not push: the player owns the clock and asks for the frame it needs. That is what makes
/// scrubbing, pausing and stepping work identically for a GIF, an MP4 and an `.ares` capture,
/// and it keeps the decoder from having to know anything about time.
///
/// `Send` because the player runs this on its own thread; a hardware video decode that takes
/// 12 ms must not be 12 ms of a dropped UI frame.
pub trait FrameSource: Send {
    fn timeline(&self) -> Timeline;

    /// The state of the scene at this presentation time.
    ///
    /// Returning `Ok(None)` means the frame already on screen is still the right one, which is the
    /// answer for most calls: at 60 Hz display and 30 fps content, half of them.
    fn frame_at(&mut self, pts_us: u64) -> Result<Option<Frame>, ImportError>;
}

/// What changed at one instant. Both fields are optional because most sources drive only one of
/// them: a video replaces the texture, a mesh sequence replaces the geometry, `.ares` does both.
#[derive(Default)]
pub struct Frame {
    pub index: u32,
    pub pts_us: u64,
    /// Replaces the contents of the scene's first dynamic image.
    pub image: Option<Image>,
    pub mesh: Option<MeshFrame>,
}

#[derive(Debug)]
pub enum ImportError {
    /// The sniffer identified the format, but no importer exists for it yet.
    Unimplemented(FormatId),
    /// The format can only be read from a file on disk, and no path was given.
    NeedsPath(FormatId),
    /// The file is the right format but its contents do not hold together, a length field that
    /// disagrees with the file, a truncated attribute buffer, a count that would overflow.
    /// Always prefer this over allocating on a number that came out of an untrusted file.
    Malformed {
        format: FormatId,
        reason: String,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Unimplemented(fmt) => {
                write!(f, "{} is recognized but not yet supported", fmt.label())
            }
            ImportError::NeedsPath(fmt) => {
                write!(f, "{} can only be read from a file on disk", fmt.label())
            }
            ImportError::Malformed { format, reason } => {
                write!(f, "{}: {reason}", format.label())
            }
            ImportError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<std::io::Error> for ImportError {
    fn from(e: std::io::Error) -> Self {
        ImportError::Io(e)
    }
}

/// Import a file whose format has already been identified.
///
/// `path` is the file's own location. Formats that reference companion files need it: OBJ's
/// `mtllib` and its textures, FBX's texture references, and DICOM, where the siblings in the same
/// folder are part of the same volume. Passing `None` disables all of that rather than failing,
/// which is what an in-memory import wants.
pub fn open(
    format: &FormatId,
    bytes: &[u8],
    path: Option<&std::path::Path>,
) -> Result<Opened, ImportError> {
    let base = path.and_then(|p| p.parent());
    let still = |r: Result<Scene, ImportError>| r.map(Opened::still);
    match format {
        FormatId::Ply(variant) => still(ply::import(bytes, *variant)),
        FormatId::Obj => still(obj::import(bytes, base)),
        FormatId::StlBinary => still(stl::import_binary(bytes)),
        FormatId::StlAscii => still(stl::import_ascii(bytes)),
        FormatId::Glb { .. } => still(gltf_import::import(bytes, true)),
        FormatId::GltfJson => still(gltf_import::import(bytes, false)),
        FormatId::Spz(encoding) => still(spz::import(bytes, *encoding)),
        FormatId::Fbx => still(fbx::import(bytes, base)),
        FormatId::Nrrd => still(volume::import_nrrd(bytes)),
        FormatId::Splat => still(splat::import(bytes)),
        FormatId::Pcd => still(pcd::import(bytes)),
        FormatId::Off => still(off::import(bytes)),
        FormatId::Xyz => still(xyz::import(bytes, path)),
        FormatId::ThreeMf => still(threemf::import(bytes)),
        FormatId::Step => still(step::import(bytes)),
        // USD composes layers, references and variants, so it needs the file on disk and its
        // neighbours reachable -- a byte slice is not a stage.
        FormatId::Usda | FormatId::Usdc | FormatId::Usdz => match path {
            Some(p) => still(usd::import(p)),
            None => Err(ImportError::NeedsPath(format.clone())),
        },
        // These stream through their own file handles: a LAZ chunk table and an E57 XML index
        // both want to seek, and a county LiDAR tile has no business being resident.
        FormatId::Las => match path {
            Some(p) => still(lidar::import_las(p)),
            None => Err(ImportError::NeedsPath(format.clone())),
        },
        // MetaImage splits into a header and a raw file more often than not, and even the
        // single-file form is read from disk rather than from the slice, so the two paths stay
        // one path.
        FormatId::MetaImage => match path {
            Some(p) => still(metaimage::import(p)),
            None => Err(ImportError::NeedsPath(format.clone())),
        },
        FormatId::E57 => match path {
            Some(p) => still(lidar::import_e57(p)),
            None => Err(ImportError::NeedsPath(format.clone())),
        },
        FormatId::Image(kind) => picture::import(bytes, *kind, path),
        // An MP4 with a Depthkit sidecar beside it is a volumetric capture wearing an ordinary
        // container. The sniffer cannot know that -- it never looks at the file system -- so the
        // question is asked here, where there is a path to ask it about.
        FormatId::Video(kind) => match path {
            Some(p) => match depthkit::detect(p) {
                Some(intrinsics) => depthkit::import(p, intrinsics),
                None => movie::import(p, bytes, *kind),
            },
            None => Err(ImportError::NeedsPath(format.clone())),
        },
        FormatId::Depthkit => match path {
            Some(p) => match depthkit::detect(p) {
                Some(intrinsics) => depthkit::import(p, intrinsics),
                None => Err(ImportError::Malformed {
                    format: format.clone(),
                    reason: "no Depthkit metadata sidecar next to this video".into(),
                }),
            },
            None => Err(ImportError::NeedsPath(format.clone())),
        },
        FormatId::Ares => match path {
            Some(p) => ares::import(p, bytes),
            None => Err(ImportError::NeedsPath(format.clone())),
        },
        // Readable as far as its container goes, and no further. Reporting what the container
        // says beats a bare "unsupported": someone holding a `.4ds` needs to know it is 500
        // frames of DXT1 and that the missing piece is a licensed DLL, not a bug here.
        FormatId::FourDs => Err(fourds::unsupported(bytes, bytes.len() as u64)),
        // These read through their own file handles: NIfTI to stream a gzip member without
        // holding the whole decompressed volume twice, DICOM to walk the rest of the series,
        // and the moving formats because a decoder needs the file open, not a slice of it.
        FormatId::Nifti => match path {
            Some(p) => still(volume::import_nifti(p)),
            None => Err(ImportError::NeedsPath(format.clone())),
        },
        FormatId::Dicom => match path {
            Some(p) => still(volume::import_dicom(p)),
            None => Err(ImportError::NeedsPath(format.clone())),
        },
        other => Err(ImportError::Unimplemented(other.clone())),
    }
}

/// Import a file whose format has already been identified, discarding any playback.
///
/// This is what the command line and the tests use, where nothing is going to drive a clock.
pub fn import(
    format: &FormatId,
    bytes: &[u8],
    path: Option<&std::path::Path>,
) -> Result<Scene, ImportError> {
    open(format, bytes, path).map(|o| o.scene)
}

/// Convenience wrapper for the common case of opening a file from disk.
pub fn open_path(
    format: &FormatId,
    bytes: &[u8],
    path: &std::path::Path,
) -> Result<Opened, ImportError> {
    open(format, bytes, Some(path))
}

pub fn import_path(
    format: &FormatId,
    bytes: &[u8],
    path: &std::path::Path,
) -> Result<Scene, ImportError> {
    import(format, bytes, Some(path))
}
