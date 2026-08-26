//! Identify a file from its header bytes.
//!
//! The extension picks the candidate set; the header picks the parser. `docs/FORMATS.md` §Ground
//! rules explains why, and the ambiguities that force it:
//!
//! - `.ply` is four different formats — mesh, point cloud, 3DGS, SuperSplat-compressed 3DGS.
//! - `.spz` is two encodings — v4's plaintext header, or v1–v3 where the `NGSP` magic sits *inside*
//!   a gzip stream. Checking gzip first would misclassify every v4 file.
//! - `.mp4` is five things — plain video, Depthkit CPP, Microsoft HoloVideo, V3C/V-PCC, MV-HEVC.
//! - `.splat` has no magic at all; only its 32-byte stride distinguishes it, which can never be
//!   more than a weak structural guess.
//!
//! Everything returns *ranked candidates*, never a single verdict, so a weak guess can never
//! outrank a real magic-byte match.

/// How much of a file the sniffer wants. Sized for the longest header that still matters: a 3DGS
/// PLY declares ~62 properties before `end_header`, and NIfTI's magic sits at offset 344.
pub const HEADER_BYTES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Confidence {
    /// The extension is all we have. Never enough to commit to a parser on its own.
    ExtensionOnly,
    /// Content-shaped guess — text that looks like OBJ, a length divisible by the record size.
    Heuristic,
    /// Structure checks out: declared sizes agree with the actual file length.
    Structural,
    /// An unambiguous magic number.
    Magic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlyVariant {
    Mesh,
    PointCloud,
    /// INRIA 3DGS reference output: `f_dc_*` + `scale_*` + `rot_*` per vertex.
    GaussianSplat,
    /// SuperSplat / PlayCanvas compressed PLY: an element named `chunk` alongside packed vertices.
    CompressedGaussianSplat,
}

/// Still-image encodings. The ones this list omits are omitted because the decoder is a heavy
/// dependency, not because the magic is hard: AVIF and HEIF both need a full AV1 / HEVC decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Bmp,
    Tiff,
    WebP,
    Tga,
    Qoi,
    Ico,
    Dds,
    Pnm,
    Farbfeld,
    /// Radiance RGBE — high dynamic range, and the usual container for a light probe.
    Hdr,
    OpenExr,
    /// Recognized, not decoded: needs an AV1 decoder.
    Avif,
    /// Recognized, not decoded: needs an HEVC decoder.
    Heif,
    /// Recognized, not decoded: needs a JPEG 2000 decoder.
    Jpeg2000,
    /// Recognized, not decoded: vector, not raster.
    Svg,
}

impl ImageKind {
    /// Whether this encoding can be decoded at all.
    ///
    /// AVIF, HEIF and JPEG 2000 have no Rust decoder in this build, but the platform hosts one
    /// on most machines, so they are offered rather than refused; the importer says plainly when
    /// the codec turns out to be missing. SVG is different in kind — it is vector, and there are
    /// no pixels to sample at any size.
    pub fn is_decodable(self) -> bool {
        !matches!(self, ImageKind::Svg)
    }

    pub fn label(self) -> &'static str {
        match self {
            ImageKind::Png => "PNG image",
            ImageKind::Jpeg => "JPEG image",
            ImageKind::Gif => "GIF image",
            ImageKind::Bmp => "BMP image",
            ImageKind::Tiff => "TIFF image",
            ImageKind::WebP => "WebP image",
            ImageKind::Tga => "Targa image",
            ImageKind::Qoi => "QOI image",
            ImageKind::Ico => "Windows icon",
            ImageKind::Dds => "DDS texture",
            ImageKind::Pnm => "Netpbm image",
            ImageKind::Farbfeld => "farbfeld image",
            ImageKind::Hdr => "Radiance HDR image",
            ImageKind::OpenExr => "OpenEXR image",
            ImageKind::Avif => "AVIF image",
            ImageKind::Heif => "HEIF image",
            ImageKind::Jpeg2000 => "JPEG 2000 image",
            ImageKind::Svg => "SVG vector image",
        }
    }
}

/// Video containers. What plays is decided by the platform decoder, not by this list — Windows
/// Media Foundation ships no codecs of its own and answers for whatever is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoKind {
    Mp4,
    QuickTime,
    Matroska,
    WebM,
    Avi,
    /// ASF, which is what `.wmv` and `.wma` are.
    Asf,
    MpegTs,
    MpegPs,
    Flv,
    Ogg,
}

impl VideoKind {
    pub fn label(self) -> &'static str {
        match self {
            VideoKind::Mp4 => "MP4 video",
            VideoKind::QuickTime => "QuickTime video",
            VideoKind::Matroska => "Matroska video",
            VideoKind::WebM => "WebM video",
            VideoKind::Avi => "AVI video",
            VideoKind::Asf => "ASF / WMV video",
            VideoKind::MpegTs => "MPEG transport stream",
            VideoKind::MpegPs => "MPEG program stream",
            VideoKind::Flv => "Flash video",
            VideoKind::Ogg => "Ogg media",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpzEncoding {
    /// v1–v3: the whole file is a gzip stream; `NGSP` is inside it.
    Gzip,
    /// v4+ (2026-05-05): plaintext header at offset 0, then per-attribute ZSTD streams.
    PlaintextV4,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FormatId {
    // --- T0, the parity set ---
    Glb { sixty_four_bit: bool },
    GltfJson,
    Obj,
    Ply(PlyVariant),
    StlBinary,
    StlAscii,
    ThreeMf,
    Fbx,

    // --- T0 splats ---
    Spz(SpzEncoding),
    Splat,
    Sog,

    /// ISO 10303 Part 21: the CAD boundary-representation exchange format.
    Step,

    // --- T0 pictures and moving pictures ---
    Image(ImageKind),
    Video(VideoKind),

    // --- T0 volumetric ---
    Ares,
    /// An MP4 carrying colour and packed depth side by side, plus a metadata sidecar. Never a
    /// verdict from the header alone: the container is an ordinary MP4 and the evidence is the
    /// file sitting next to it, so only an importer with a path can reach this.
    Depthkit,
    /// 4DViews HOLOSYS. The container is readable; the mesh and DXT1 texture blocks are not,
    /// without the licensed `BridgeCodec4DS.dll`.
    FourDs,

    // --- T0 points and volumes ---
    Pcd,
    Las,
    E57,
    Nrrd,
    Nifti,
    Dicom,

    // --- Recognized but not yet handled. Worth identifying anyway: naming a format the app
    //     cannot open is a far better failure than a spinner that never resolves. ---
    Usda,
    Usdc,
    /// A USD package: a ZIP with an uncompressed layer inside it.
    Usdz,
    Zip,
    Gzip,
    Isobmff,

    Unknown,
}

impl FormatId {
    /// Whether an importer exists for this format today.
    pub fn is_supported(&self) -> bool {
        match self {
            FormatId::Image(k) => k.is_decodable(),
            // The container is parsed and reported, but the mesh and texture blocks need a
            // licensed native codec that cannot be redistributed.
            FormatId::FourDs => false,
            FormatId::Zip
            | FormatId::Gzip
            | FormatId::Isobmff
            | FormatId::Unknown => false,
            _ => true,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            FormatId::Glb { .. } => "glTF binary",
            FormatId::GltfJson => "glTF JSON",
            FormatId::Obj => "Wavefront OBJ",
            FormatId::Ply(PlyVariant::Mesh) => "PLY mesh",
            FormatId::Ply(PlyVariant::PointCloud) => "PLY point cloud",
            FormatId::Ply(PlyVariant::GaussianSplat) => "PLY Gaussian splats",
            FormatId::Ply(PlyVariant::CompressedGaussianSplat) => "PLY Gaussian splats (compressed)",
            FormatId::StlBinary => "STL (binary)",
            FormatId::StlAscii => "STL (ASCII)",
            FormatId::ThreeMf => "3MF",
            FormatId::Fbx => "FBX",
            FormatId::Spz(SpzEncoding::Gzip) => "SPZ (v1-v3, gzip)",
            FormatId::Spz(SpzEncoding::PlaintextV4) => "SPZ (v4+)",
            FormatId::Splat => "splat (antimatter15)",
            FormatId::Sog => "SOG (Spatially Ordered Gaussians)",
            FormatId::Step => "STEP (ISO 10303)",
            FormatId::Image(k) => k.label(),
            FormatId::Video(k) => k.label(),
            FormatId::Ares => "ARES volumetric",
            FormatId::Depthkit => "Depthkit RGBD video",
            FormatId::FourDs => "4DViews HOLOSYS",
            FormatId::Pcd => "PCD point cloud",
            FormatId::Las => "LAS / LAZ",
            FormatId::E57 => "ASTM E57",
            FormatId::Nrrd => "NRRD volume",
            FormatId::Nifti => "NIfTI volume",
            FormatId::Dicom => "DICOM",
            FormatId::Usda => "OpenUSD (ASCII)",
            FormatId::Usdc => "OpenUSD (crate)",
            FormatId::Usdz => "OpenUSD (package)",
            FormatId::Zip => "ZIP container",
            FormatId::Gzip => "gzip stream",
            FormatId::Isobmff => "ISOBMFF (MP4/MOV)",
            FormatId::Unknown => "unrecognized",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub format: FormatId,
    pub confidence: Confidence,
    /// Why this candidate was produced, or what still needs checking. Shown in the inspector.
    pub note: &'static str,
}

impl Candidate {
    fn new(format: FormatId, confidence: Confidence, note: &'static str) -> Self {
        Candidate {
            format,
            confidence,
            note,
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn starts_with(h: &[u8], magic: &[u8]) -> bool {
    h.len() >= magic.len() && &h[..magic.len()] == magic
}

fn at(h: &[u8], offset: usize, magic: &[u8]) -> bool {
    h.len() >= offset + magic.len() && &h[offset..offset + magic.len()] == magic
}

fn le_u32(h: &[u8], offset: usize) -> Option<u32> {
    let b = h.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn ext_of(name: Option<&str>) -> Option<String> {
    let name = name?.to_ascii_lowercase();
    // Two-part extensions matter here: `.copc.laz` and `.nii.gz` mean different things from
    // `.laz` and `.gz` alone.
    for multi in [".copc.laz", ".nii.gz", ".ome.tif", ".ept.json"] {
        if name.ends_with(multi) {
            return Some(multi[1..].to_string());
        }
    }
    name.rsplit_once('.').map(|(_, e)| e.to_string())
}

/// Identify a file from its leading bytes.
///
/// `header` should be up to [`HEADER_BYTES`]; `file_len` is the total file size, which several
/// structural checks need; `name` is an optional file name used only as a tie-breaker.
///
/// Returns candidates sorted by descending confidence. An empty result is impossible — an
/// unrecognized file yields a single [`FormatId::Unknown`].
pub fn sniff(header: &[u8], file_len: u64, name: Option<&str>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let ext = ext_of(name);
    let ext = ext.as_deref();

    // ---- Unambiguous magic numbers, in the order that resolves collisions correctly ----

    if starts_with(header, b"ARES") {
        out.push(Candidate::new(
            FormatId::Ares,
            Confidence::Magic,
            "ARES container header",
        ));
    }

    // SPZ v4 MUST be tested before gzip. In v1-v3 the NGSP magic lives *inside* the gzip stream,
    // so a file whose first bytes are literally "NGSP" is v4 or newer. Getting this order wrong
    // misidentifies every v4 file, and v4 is the current release.
    if starts_with(header, b"NGSP") {
        out.push(Candidate::new(
            FormatId::Spz(SpzEncoding::PlaintextV4),
            Confidence::Magic,
            "SPZ v4+ plaintext header: point count and SH degree readable without decompressing",
        ));
    } else if starts_with(header, &[0x1F, 0x8B]) {
        // A gzip stream. SPZ v1-v3 is the interesting case, but so is `.nii.gz`, so only claim SPZ
        // when the name agrees -- confirming it properly means inflating, which sniffing must not do.
        if ext == Some("spz") {
            out.push(Candidate::new(
                FormatId::Spz(SpzEncoding::Gzip),
                Confidence::Structural,
                "gzip stream named .spz: expect the NGSP header after inflating (v1-v3)",
            ));
        } else if ext == Some("nii.gz") {
            out.push(Candidate::new(
                FormatId::Nifti,
                Confidence::Structural,
                "gzip stream named .nii.gz",
            ));
        }
        out.push(Candidate::new(
            FormatId::Gzip,
            Confidence::Magic,
            "gzip stream; inflate then re-sniff",
        ));
    }

    if starts_with(header, b"glTF") {
        // glTF 2.1 introduces a 64-bit GLB to lift the 4 GB ceiling. Read the version rather than
        // assuming the classic 12-byte header.
        let version = le_u32(header, 4).unwrap_or(2);
        let sixty_four_bit = version >= 3 || file_len > u32::MAX as u64;
        out.push(Candidate::new(
            FormatId::Glb { sixty_four_bit },
            Confidence::Magic,
            "GLB magic; check the version field for the 64-bit variant",
        ));
    }

    if starts_with(header, b"ply\n") || starts_with(header, b"ply\r\n") {
        let variant = ply_variant(header);
        out.push(Candidate::new(
            FormatId::Ply(variant),
            Confidence::Magic,
            match variant {
                PlyVariant::GaussianSplat => "PLY header declares f_dc_*, scale_* and rot_*",
                PlyVariant::CompressedGaussianSplat => "PLY header declares a 'chunk' element",
                PlyVariant::Mesh => "PLY header declares a face element",
                PlyVariant::PointCloud => "PLY header declares vertices but no faces",
            },
        ));
    }

    if starts_with(header, b"Kaydara FBX Binary") {
        out.push(Candidate::new(
            FormatId::Fbx,
            Confidence::Magic,
            "FBX binary; parse sandboxed -- this format has CVE history",
        ));
    }

    if starts_with(header, b"# .PCD") {
        out.push(Candidate::new(
            FormatId::Pcd,
            Confidence::Magic,
            "PCD header",
        ));
    }

    if starts_with(header, b"LASF") {
        out.push(Candidate::new(
            FormatId::Las,
            Confidence::Magic,
            "LAS/LAZ header; compression and COPC indexing are declared further in",
        ));
    }

    if starts_with(header, b"ASTM-E57") {
        out.push(Candidate::new(
            FormatId::E57,
            Confidence::Magic,
            "E57 file signature",
        ));
    }

    if starts_with(header, b"NRRD") {
        out.push(Candidate::new(
            FormatId::Nrrd,
            Confidence::Magic,
            "NRRD magic",
        ));
    }

    // DICOM's preamble is 128 bytes of anything, then "DICM".
    if at(header, 128, b"DICM") {
        out.push(Candidate::new(
            FormatId::Dicom,
            Confidence::Magic,
            "DICM at offset 128; a folder of these is one volume, not many files",
        ));
    }

    // NIfTI-1 stores its magic at offset 344, after the 348-byte header.
    if at(header, 344, b"n+1\0") || at(header, 344, b"ni1\0") {
        out.push(Candidate::new(
            FormatId::Nifti,
            Confidence::Magic,
            "NIfTI-1 magic at offset 344",
        ));
    }

    if starts_with(header, b"#usda") {
        out.push(Candidate::new(
            FormatId::Usda,
            Confidence::Magic,
            "OpenUSD ASCII; AOUSD Core Spec 1.0 defines this format",
        ));
    }

    if starts_with(header, b"PXR-USDC") {
        out.push(Candidate::new(
            FormatId::Usdc,
            Confidence::Magic,
            "OpenUSD crate binary",
        ));
    }

    if starts_with(header, b"PK\x03\x04") {
        // ZIP is a container, not a format. 3MF, USDZ, SLPK and SOG bundles all look like this at
        // offset 0, so the real answer needs the central directory -- which sniffing does not read.
        match ext {
            Some("3mf") => out.push(Candidate::new(
                FormatId::ThreeMf,
                Confidence::Structural,
                "ZIP named .3mf; confirm 3D/3dmodel.model in the central directory",
            )),
            Some("usdz") => out.push(Candidate::new(
                FormatId::Usdz,
                Confidence::Structural,
                "ZIP named .usdz; the layer inside is stored uncompressed by the specification",
            )),
            Some("sog") => out.push(Candidate::new(
                FormatId::Sog,
                Confidence::Structural,
                "bundle named .sog; confirm meta.json plus the WebP attribute planes",
            )),
            _ => {}
        }
        out.push(Candidate::new(
            FormatId::Zip,
            Confidence::Magic,
            "ZIP container; open the central directory to resolve 3MF / USDZ / SLPK / SOG",
        ));
    }

    // ISO 10303-21 begins with its own name, optionally after a byte order mark or whitespace.
    if let Some(text) = leading_text(&header[..header.len().min(512)]) {
        if text.trim_start().starts_with("ISO-10303-21") {
            out.push(Candidate::new(
                FormatId::Step,
                Confidence::Magic,
                "ISO-10303-21 header; the geometry inside is NURBS and must be tessellated",
            ));
        }
    }

    if starts_with(header, b"4DS0") {
        out.push(Candidate::new(
            FormatId::FourDs,
            Confidence::Magic,
            "4DS0 container; the frame directory is readable but the blocks need BridgeCodec4DS.dll",
        ));
    }

    // ISOBMFF: a size field then "ftyp", then a four-character brand that says which of the
    // family this is. The brand separates a still AVIF from a video MP4 from a QuickTime movie,
    // which is worth reading rather than reporting all three as "some ISO container".
    if at(header, 4, b"ftyp") {
        match header.get(8..12) {
            // A still, or a still sequence. Both need an AV1 or HEVC decoder we do not carry.
            Some(b"avif") | Some(b"avis") => out.push(Candidate::new(
                FormatId::Image(ImageKind::Avif),
                Confidence::Magic,
                "ISOBMFF with an AVIF brand: an AV1 still",
            )),
            Some(b"heic") | Some(b"heix") | Some(b"heim") | Some(b"heis") | Some(b"mif1")
            | Some(b"msf1") => out.push(Candidate::new(
                FormatId::Image(ImageKind::Heif),
                Confidence::Magic,
                "ISOBMFF with an HEIF brand: an HEVC still",
            )),
            Some(b"qt  ") => out.push(Candidate::new(
                FormatId::Video(VideoKind::QuickTime),
                Confidence::Magic,
                "QuickTime movie; spherical and spatial metadata live in the track boxes",
            )),
            _ => out.push(Candidate::new(
                FormatId::Video(VideoKind::Mp4),
                Confidence::Magic,
                "MP4: plain video unless a Depthkit sidecar, spherical metadata or an \
                 MV-HEVC track says otherwise",
            )),
        }
        // Still worth naming the container, because the brand is a claim and the boxes are the
        // proof: Depthkit, HoloVideo and V3C all wear an ordinary MP4 brand.
        out.push(Candidate::new(
            FormatId::Isobmff,
            Confidence::Magic,
            "MP4/MOV: could also be Depthkit CPP, HoloVideo, V3C/V-PCC or MV-HEVC",
        ));
    }

    // ---- Still images ----

    if let Some(kind) = image_magic(header, file_len) {
        out.push(Candidate::new(
            FormatId::Image(kind),
            // BMP has only a two-byte magic, which is far too common to call unambiguous on its
            // own; the length field in the header is what makes the verdict solid.
            if kind == ImageKind::Bmp {
                Confidence::Structural
            } else {
                Confidence::Magic
            },
            image_note(kind),
        ));
    }

    // ---- Video and audio containers ----

    if let Some(kind) = video_magic(header) {
        out.push(Candidate::new(
            FormatId::Video(kind),
            Confidence::Magic,
            match kind {
                VideoKind::MpegTs => "0x47 sync bytes at the 188-byte packet cadence",
                VideoKind::Matroska | VideoKind::WebM => "EBML header with a video DocType",
                _ => "container magic; the codecs inside are the platform decoder's problem",
            },
        ));
    }

    // ---- Structural checks: declared sizes must agree with the real file length ----

    // Binary STL: 80-byte header, u32 triangle count, then exactly 50 bytes per triangle.
    if file_len >= 84 {
        if let Some(tris) = le_u32(header, 80) {
            let expected = 84u64 + tris as u64 * 50;
            if expected == file_len {
                out.push(Candidate::new(
                    FormatId::StlBinary,
                    Confidence::Structural,
                    "triangle count matches the file length exactly",
                ));
            }
        }
    }

    // `.splat` has no header whatsoever -- 32 bytes per splat and nothing else. This can never be
    // better than a guess, so it only fires when the name agrees and the stride divides evenly.
    if file_len > 0 && file_len % 32 == 0 && ext == Some("splat") {
        out.push(Candidate::new(
            FormatId::Splat,
            Confidence::Heuristic,
            "length divides by the 32-byte splat stride; the format carries no magic at all",
        ));
    }

    // ---- Text heuristics, last, and only when nothing structural matched ----

    if out.is_empty() {
        if let Some(text) = leading_text(header) {
            let lower = text.to_ascii_lowercase();
            if lower.starts_with("solid") {
                out.push(Candidate::new(
                    FormatId::StlAscii,
                    Confidence::Heuristic,
                    "leading 'solid' keyword",
                ));
            }
            if looks_like_obj(&lower) {
                out.push(Candidate::new(
                    FormatId::Obj,
                    Confidence::Heuristic,
                    "vertex and face directives in the leading lines",
                ));
            }
            if lower.trim_start().starts_with('{') && lower.contains("\"asset\"") {
                out.push(Candidate::new(
                    FormatId::GltfJson,
                    Confidence::Heuristic,
                    "JSON with a glTF asset block",
                ));
            }
        }
    }

    // ---- Extension-only fallback ----

    if out.is_empty() {
        if let Some(e) = ext {
            if let Some(f) = format_for_extension(e) {
                out.push(Candidate::new(
                    f,
                    Confidence::ExtensionOnly,
                    "no recognizable header; identified from the file name alone",
                ));
            }
        }
    }

    if out.is_empty() {
        out.push(Candidate::new(
            FormatId::Unknown,
            Confidence::ExtensionOnly,
            "no magic, no matching structure, no known extension",
        ));
    }

    // A container magic identifies the wrapper, not the format inside it, so it is worth one
    // confidence level less than a verdict naming an actual format. Without this a bare `gzip`
    // outranks `SPZ v1-v3` on every legacy splat file, and `ZIP` outranks `3MF` on every 3MF.
    // The sort is stable, so ties keep push order, which is specific-before-generic by construction.
    out.sort_by_key(|c| {
        let level = c.confidence as i32 - if is_container(&c.format) { 1 } else { 0 };
        std::cmp::Reverse(level)
    });
    out
}

fn is_container(f: &FormatId) -> bool {
    matches!(f, FormatId::Zip | FormatId::Gzip | FormatId::Isobmff)
}

/// Still-image encodings, identified from the header alone.
///
/// TGA is deliberately absent: it has no leading magic at all, only an optional footer 18 bytes
/// from the end, so it can never be identified from a header and falls through to the
/// extension-only path.
fn image_magic(h: &[u8], file_len: u64) -> Option<ImageKind> {
    if starts_with(h, b"\x89PNG\r\n\x1A\n") {
        return Some(ImageKind::Png);
    }
    if starts_with(h, b"\xFF\xD8\xFF") {
        return Some(ImageKind::Jpeg);
    }
    if starts_with(h, b"GIF87a") || starts_with(h, b"GIF89a") {
        return Some(ImageKind::Gif);
    }
    if starts_with(h, b"RIFF") && at(h, 8, b"WEBP") {
        return Some(ImageKind::WebP);
    }
    if starts_with(h, b"qoif") {
        return Some(ImageKind::Qoi);
    }
    if starts_with(h, b"DDS ") {
        return Some(ImageKind::Dds);
    }
    if starts_with(h, b"farbfeld") {
        return Some(ImageKind::Farbfeld);
    }
    if starts_with(h, b"#?RADIANCE\n") || starts_with(h, b"#?RGBE\n") {
        return Some(ImageKind::Hdr);
    }
    // OpenEXR's magic is 0x76 0x2f 0x31 0x01, which is 20000630 read as a little-endian i32.
    if starts_with(h, &[0x76, 0x2F, 0x31, 0x01]) {
        return Some(ImageKind::OpenExr);
    }
    // TIFF byte order mark plus the 42 that every TIFF since 1986 carries. BigTIFF says 43.
    if starts_with(h, b"II\x2A\x00")
        || starts_with(h, b"MM\x00\x2A")
        || starts_with(h, b"II\x2B\x00")
        || starts_with(h, b"MM\x00\x2B")
    {
        return Some(ImageKind::Tiff);
    }
    // JPEG 2000, both the JP2 box form and the raw codestream.
    if starts_with(h, b"\x00\x00\x00\x0CjP  \r\n\x87\n") || starts_with(h, b"\xFF\x4F\xFF\x51") {
        return Some(ImageKind::Jpeg2000);
    }
    // ICO and CUR share a header; only the type field at offset 2 differs. Four bytes of mostly
    // zeros is far too weak on its own -- a binary STL whose 80-byte comment starts with zeros
    // would match -- so the directory has to hold together as well: a plausible image count, and
    // a first entry whose pixel data lies inside the file and after the directory itself.
    if starts_with(h, &[0x00, 0x00, 0x01, 0x00]) || starts_with(h, &[0x00, 0x00, 0x02, 0x00]) {
        let count = le_u16(h, 4).unwrap_or(0) as u64;
        let size = le_u32(h, 14).unwrap_or(0) as u64;
        let offset = le_u32(h, 18).unwrap_or(0) as u64;
        if count > 0
            && count < 256
            && size > 0
            && offset >= 6 + count * 16
            && offset.saturating_add(size) <= file_len
        {
            return Some(ImageKind::Ico);
        }
    }
    // Netpbm: 'P', a digit 1 to 7, then whitespace. Three bytes is not much of a magic, so the
    // whitespace check is doing real work here.
    if h.len() >= 3 && h[0] == b'P' && h[1] >= b'1' && h[1] <= b'7' && h[2].is_ascii_whitespace() {
        return Some(ImageKind::Pnm);
    }
    // BMP: "BM" is two bytes and appears everywhere, so require the declared size to match.
    if starts_with(h, b"BM") && le_u32(h, 2).is_some_and(|n| n as u64 == file_len) {
        return Some(ImageKind::Bmp);
    }
    // SVG is text. Accept a leading <svg, or an XML declaration whose document element is <svg.
    if let Some(text) = leading_text(&h[..h.len().min(1024)]) {
        let head = text.trim_start();
        if head.starts_with("<svg") || (head.starts_with("<?xml") && text.contains("<svg")) {
            return Some(ImageKind::Svg);
        }
    }
    None
}

fn image_note(kind: ImageKind) -> &'static str {
    match kind {
        ImageKind::Bmp => "BM plus a length field that matches the file exactly",
        ImageKind::Ico => "icon directory with a plausible image count",
        ImageKind::Pnm => "Netpbm magic followed by whitespace",
        ImageKind::Svg => "XML whose document element is <svg>: vector, so there is nothing to sample",
        ImageKind::Avif | ImageKind::Heif | ImageKind::Jpeg2000 => {
            "decoded by the platform, which needs the matching codec installed"
        }
        ImageKind::Hdr | ImageKind::OpenExr => "high dynamic range; tone mapped for display",
        _ => "image magic number",
    }
}

/// Video and media containers, from the header alone.
fn video_magic(h: &[u8]) -> Option<VideoKind> {
    if starts_with(h, b"RIFF") && at(h, 8, b"AVI ") {
        return Some(VideoKind::Avi);
    }
    // ASF, and therefore WMV and WMA: a 16-byte GUID.
    if starts_with(
        h,
        &[
            0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62,
            0xCE, 0x6C,
        ],
    ) {
        return Some(VideoKind::Asf);
    }
    if starts_with(h, b"FLV\x01") {
        return Some(VideoKind::Flv);
    }
    if starts_with(h, b"OggS") {
        return Some(VideoKind::Ogg);
    }
    // EBML. Matroska and WebM share it; the DocType string a few bytes in decides which.
    if starts_with(h, &[0x1A, 0x45, 0xDF, 0xA3]) {
        let probe = &h[..h.len().min(256)];
        if find(probe, b"webm").is_some() {
            return Some(VideoKind::WebM);
        }
        return Some(VideoKind::Matroska);
    }
    // MPEG program stream pack header.
    if starts_with(h, &[0x00, 0x00, 0x01, 0xBA]) {
        return Some(VideoKind::MpegPs);
    }
    // Transport stream: 0x47 every 188 bytes. One sync byte is meaningless; four in cadence is
    // the accepted test, and it is what every demuxer uses.
    if h.len() >= 565 && h[0] == 0x47 && h[188] == 0x47 && h[376] == 0x47 && h[564] == 0x47 {
        return Some(VideoKind::MpegTs);
    }
    None
}

fn le_u16(h: &[u8], offset: usize) -> Option<u16> {
    let b = h.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

/// Which of the four PLY formats this is, read from the ASCII header.
///
/// Order matters: the compressed-3DGS check must precede the plain-3DGS check, because a
/// SuperSplat file declares both a `chunk` element and splat-shaped properties.
pub fn ply_variant(header: &[u8]) -> PlyVariant {
    // Truncate at `end_header` in the raw bytes *before* decoding. Only the header is text; a
    // binary body is not valid UTF-8, so decoding the whole buffer first fails and loses the
    // property names. Truncating also stops body bytes that happen to spell `f_dc_` from voting.
    let end = find(header, b"end_header").unwrap_or(header.len());
    let head = String::from_utf8_lossy(&header[..end]).to_ascii_lowercase();
    let head = head.as_str();

    if head.contains("element chunk") {
        return PlyVariant::CompressedGaussianSplat;
    }
    // The INRIA reference schema. All three families must be present -- `scale_` and `rot_` alone
    // would also match some scanner output.
    if head.contains("f_dc_") && head.contains("scale_") && head.contains("rot_") {
        return PlyVariant::GaussianSplat;
    }
    if head.contains("element face") {
        // A face element declaring zero faces is still a point cloud in practice.
        if head.contains("element face 0") {
            return PlyVariant::PointCloud;
        }
        return PlyVariant::Mesh;
    }
    PlyVariant::PointCloud
}

/// Interpret the header as text, if it plausibly is text. Stops at the first NUL, and refuses
/// anything with a high proportion of control bytes.
fn leading_text(header: &[u8]) -> Option<&str> {
    let end = header.iter().position(|&b| b == 0).unwrap_or(header.len());
    let slice = &header[..end];
    if slice.is_empty() {
        return None;
    }
    let controls = slice
        .iter()
        .filter(|&&b| b < 0x09 || (0x0E..0x20).contains(&b))
        .count();
    if controls * 20 > slice.len() {
        return None;
    }
    std::str::from_utf8(slice).ok()
}

fn looks_like_obj(lower: &str) -> bool {
    let mut vertices = 0usize;
    let mut directives = 0usize;
    for line in lower.lines().take(64) {
        let line = line.trim_start();
        if line.starts_with("v ") || line.starts_with("vt ") || line.starts_with("vn ") {
            vertices += 1;
        }
        if line.starts_with("f ")
            || line.starts_with("mtllib ")
            || line.starts_with("usemtl ")
            || line.starts_with("o ")
            || line.starts_with("g ")
        {
            directives += 1;
        }
    }
    vertices >= 3 && directives >= 1
}

fn format_for_extension(ext: &str) -> Option<FormatId> {
    Some(match ext {
        "obj" => FormatId::Obj,
        "stl" => FormatId::StlAscii,
        "gltf" => FormatId::GltfJson,
        "glb" => FormatId::Glb {
            sixty_four_bit: false,
        },
        "ares" => FormatId::Ares,
        // `.splat` is deliberately absent. Its 32-byte stride is the only evidence the format
        // offers, so a file that fails the stride check is not a splat file no matter what it is
        // called, and the name must not resurrect a verdict the arithmetic already rejected.
        "sog" => FormatId::Sog,
        "pcd" => FormatId::Pcd,
        "las" | "laz" | "copc.laz" => FormatId::Las,
        "e57" => FormatId::E57,
        "nrrd" => FormatId::Nrrd,
        "nii" | "nii.gz" => FormatId::Nifti,
        "dcm" => FormatId::Dicom,
        "usda" => FormatId::Usda,
        "usdz" => FormatId::Usdz,
        "usd" => FormatId::Usdc,
        "usdc" => FormatId::Usdc,
        "4ds" => FormatId::FourDs,
        "step" | "stp" | "p21" => FormatId::Step,

        // Targa is the only image format here that has to arrive by name: it has no magic.
        "tga" | "targa" | "icb" | "vda" | "vst" => FormatId::Image(ImageKind::Tga),
        "png" => FormatId::Image(ImageKind::Png),
        "jpg" | "jpeg" | "jpe" | "jfif" => FormatId::Image(ImageKind::Jpeg),
        "gif" => FormatId::Image(ImageKind::Gif),
        "bmp" | "dib" => FormatId::Image(ImageKind::Bmp),
        "tif" | "tiff" | "ome.tif" => FormatId::Image(ImageKind::Tiff),
        "webp" => FormatId::Image(ImageKind::WebP),
        "qoi" => FormatId::Image(ImageKind::Qoi),
        "ico" | "cur" => FormatId::Image(ImageKind::Ico),
        "dds" => FormatId::Image(ImageKind::Dds),
        "pbm" | "pgm" | "ppm" | "pnm" | "pam" => FormatId::Image(ImageKind::Pnm),
        "ff" => FormatId::Image(ImageKind::Farbfeld),
        "hdr" | "rgbe" => FormatId::Image(ImageKind::Hdr),
        "exr" => FormatId::Image(ImageKind::OpenExr),
        "avif" => FormatId::Image(ImageKind::Avif),
        "heic" | "heif" => FormatId::Image(ImageKind::Heif),
        "jp2" | "j2k" | "jpf" | "jpx" => FormatId::Image(ImageKind::Jpeg2000),
        "svg" => FormatId::Image(ImageKind::Svg),

        "mp4" | "m4v" | "3gp" | "3g2" => FormatId::Video(VideoKind::Mp4),
        "mov" | "qt" => FormatId::Video(VideoKind::QuickTime),
        "mkv" => FormatId::Video(VideoKind::Matroska),
        "webm" => FormatId::Video(VideoKind::WebM),
        "avi" => FormatId::Video(VideoKind::Avi),
        "wmv" | "asf" => FormatId::Video(VideoKind::Asf),
        "ts" | "m2ts" | "mts" => FormatId::Video(VideoKind::MpegTs),
        "mpg" | "mpeg" | "vob" => FormatId::Video(VideoKind::MpegPs),
        "flv" => FormatId::Video(VideoKind::Flv),
        "ogv" | "ogg" => FormatId::Video(VideoKind::Ogg),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(header: &[u8], len: u64, name: Option<&str>) -> Candidate {
        sniff(header, len, name).into_iter().next().unwrap()
    }

    // --- The SPZ ordering bug this module exists to prevent ---

    #[test]
    fn spz_v4_plaintext_header_beats_the_gzip_check() {
        let mut h = b"NGSP".to_vec();
        h.extend_from_slice(&[4, 0, 0, 0]);
        let c = top(&h, 4096, Some("scan.spz"));
        assert_eq!(c.format, FormatId::Spz(SpzEncoding::PlaintextV4));
        assert_eq!(c.confidence, Confidence::Magic);
    }

    #[test]
    fn spz_v2_gzip_is_still_recognized_when_the_name_agrees() {
        let h = [0x1F, 0x8B, 0x08, 0x00, 0, 0, 0, 0];
        let c = top(&h, 4096, Some("marble.spz"));
        assert_eq!(c.format, FormatId::Spz(SpzEncoding::Gzip));
    }

    #[test]
    fn a_bare_gzip_stream_is_not_claimed_as_spz() {
        let h = [0x1F, 0x8B, 0x08, 0x00, 0, 0, 0, 0];
        let c = top(&h, 4096, Some("archive.gz"));
        assert_eq!(c.format, FormatId::Gzip);
    }

    #[test]
    fn nii_gz_resolves_through_its_two_part_extension() {
        let h = [0x1F, 0x8B, 0x08, 0x00, 0, 0, 0, 0];
        let c = top(&h, 4096, Some("brain.nii.gz"));
        assert_eq!(c.format, FormatId::Nifti);
    }

    // --- PLY's four-way dispatch ---

    #[test]
    fn ply_gaussian_splat_is_detected_from_its_property_names() {
        let h = b"ply\nformat binary_little_endian 1.0\nelement vertex 1920000\n\
                  property float x\nproperty float y\nproperty float z\n\
                  property float f_dc_0\nproperty float f_dc_1\nproperty float f_dc_2\n\
                  property float opacity\nproperty float scale_0\nproperty float rot_0\n\
                  end_header\n";
        assert_eq!(ply_variant(h), PlyVariant::GaussianSplat);
        assert_eq!(top(h, 1000, Some("a.ply")).format, FormatId::Ply(PlyVariant::GaussianSplat));
    }

    #[test]
    fn supersplat_compressed_ply_outranks_the_plain_splat_schema() {
        // Declares both a chunk element and splat-shaped properties; chunk must win.
        let h = b"ply\nformat binary_little_endian 1.0\nelement chunk 4000\n\
                  property float min_x\nelement vertex 1000000\n\
                  property float f_dc_0\nproperty float scale_0\nproperty float rot_0\n\
                  end_header\n";
        assert_eq!(ply_variant(h), PlyVariant::CompressedGaussianSplat);
    }

    #[test]
    fn ply_with_faces_is_a_mesh_and_without_them_a_point_cloud() {
        let mesh = b"ply\nformat ascii 1.0\nelement vertex 8\nproperty float x\n\
                     element face 12\nproperty list uchar int vertex_indices\nend_header\n";
        assert_eq!(ply_variant(mesh), PlyVariant::Mesh);

        let cloud = b"ply\nformat ascii 1.0\nelement vertex 8\nproperty float x\nend_header\n";
        assert_eq!(ply_variant(cloud), PlyVariant::PointCloud);
    }

    #[test]
    fn a_zero_face_ply_is_treated_as_a_point_cloud() {
        let h = b"ply\nformat ascii 1.0\nelement vertex 8\nproperty float x\n\
                  element face 0\nproperty list uchar int vertex_indices\nend_header\n";
        assert_eq!(ply_variant(h), PlyVariant::PointCloud);
    }

    #[test]
    fn a_binary_splat_ply_is_detected_despite_a_non_utf8_body() {
        // The body of a real 3DGS PLY is float32 data and is not valid UTF-8. Decoding the whole
        // buffer before truncating at end_header loses every property name and downgrades the file
        // to a plain point cloud.
        let mut src = b"ply\nformat binary_little_endian 1.0\nelement vertex 2\n\
                        property float x\nproperty float y\nproperty float z\n\
                        property float f_dc_0\nproperty float opacity\n\
                        property float scale_0\nproperty float rot_0\n\
                        end_header\n"
            .to_vec();
        src.extend_from_slice(&[0xFF, 0xFE, 0x80, 0x00, 0xC0, 0xDE, 0x81, 0x7F]);
        assert_eq!(ply_variant(&src), PlyVariant::GaussianSplat);
        assert_eq!(
            top(&src, 4096, Some("scene.ply")).format,
            FormatId::Ply(PlyVariant::GaussianSplat)
        );
    }

    #[test]
    fn property_names_after_end_header_cannot_influence_the_verdict() {
        // Binary body happens to contain the ASCII bytes "f_dc_0". It must not be read.
        let h = b"ply\nformat binary_little_endian 1.0\nelement vertex 3\nproperty float x\n\
                  element face 1\nproperty list uchar int vertex_indices\nend_header\nf_dc_0 scale_ rot_";
        assert_eq!(ply_variant(h), PlyVariant::Mesh);
    }

    // --- Structural checks ---

    #[test]
    fn binary_stl_is_confirmed_by_arithmetic_not_by_extension() {
        let mut h = vec![0u8; 84];
        h[80..84].copy_from_slice(&12u32.to_le_bytes());
        let c = top(&h, 84 + 12 * 50, Some("part.stl"));
        assert_eq!(c.format, FormatId::StlBinary);
        assert_eq!(c.confidence, Confidence::Structural);
    }

    #[test]
    fn a_wrong_triangle_count_does_not_produce_a_binary_stl_verdict() {
        let mut h = vec![0u8; 84];
        h[80..84].copy_from_slice(&999u32.to_le_bytes());
        let cands = sniff(&h, 84 + 12 * 50, Some("part.stl"));
        assert!(!cands.iter().any(|c| c.format == FormatId::StlBinary));
    }

    #[test]
    fn splat_stays_a_heuristic_because_the_format_has_no_magic() {
        let h = vec![0u8; 320];
        let c = top(&h, 320, Some("scene.splat"));
        assert_eq!(c.format, FormatId::Splat);
        assert_eq!(c.confidence, Confidence::Heuristic);
    }

    #[test]
    fn a_length_that_does_not_divide_by_32_is_not_a_splat_file() {
        let h = vec![0u8; 100];
        let cands = sniff(&h, 100, Some("scene.splat"));
        assert!(!cands.iter().any(|c| c.format == FormatId::Splat));
    }

    // --- The rest ---

    #[test]
    fn ares_header_is_recognized() {
        let mut h = b"ARES".to_vec();
        h.extend_from_slice(&[0, 1]);
        assert_eq!(top(&h, 49_700_000, Some("daniel-s0.ares")).format, FormatId::Ares);
    }

    #[test]
    fn glb_reports_the_sixty_four_bit_variant_for_oversized_files() {
        let mut h = b"glTF".to_vec();
        h.extend_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            top(&h, 1024, None).format,
            FormatId::Glb { sixty_four_bit: false }
        );
        // Past the 4 GB ceiling it cannot be a classic GLB regardless of the version field.
        assert_eq!(
            top(&h, 5_000_000_000, None).format,
            FormatId::Glb { sixty_four_bit: true }
        );
    }

    #[test]
    fn mp4_reports_video_first_and_still_names_the_container_underneath() {
        // The brand says video, so that is the verdict -- but Depthkit, HoloVideo and V3C all
        // wear an ordinary MP4 brand, so the container candidate has to survive too.
        let h = b"\0\0\0\x20ftypisom";
        let cands = sniff(h, 100_000, Some("capture.mp4"));
        assert_eq!(cands[0].format, FormatId::Video(VideoKind::Mp4));
        let container = cands
            .iter()
            .find(|c| c.format == FormatId::Isobmff)
            .expect("the ISOBMFF container candidate must still be offered");
        assert!(container.note.contains("Depthkit"));
    }

    #[test]
    fn the_ftyp_brand_separates_a_still_from_a_movie() {
        let avif = b"\0\0\0\x20ftypavif";
        assert_eq!(
            top(avif, 100_000, Some("photo.avif")).format,
            FormatId::Image(ImageKind::Avif)
        );
        let mov = b"\0\0\0\x14ftypqt  ";
        assert_eq!(
            top(mov, 100_000, Some("clip.mov")).format,
            FormatId::Video(VideoKind::QuickTime)
        );
        let heic = b"\0\0\0\x18ftypheic";
        assert_eq!(
            top(heic, 100_000, Some("IMG_0001.heic")).format,
            FormatId::Image(ImageKind::Heif)
        );
    }

    // --- Images ---

    #[test]
    fn image_magics_beat_their_extensions() {
        // Every one of these is named wrong on purpose: the header has to win.
        let cases: [(&[u8], ImageKind); 6] = [
            (b"\x89PNG\r\n\x1A\n\0\0\0\rIHDR", ImageKind::Png),
            (b"\xFF\xD8\xFF\xE0\0\x10JFIF", ImageKind::Jpeg),
            (b"GIF89a\x10\0\x10\0", ImageKind::Gif),
            (b"RIFF\x24\0\0\0WEBPVP8 ", ImageKind::WebP),
            (b"qoif\0\0\x01\0\0\0\x01\0\x04\x00", ImageKind::Qoi),
            (b"II\x2A\x00\x08\0\0\0", ImageKind::Tiff),
        ];
        for (header, kind) in cases {
            let c = top(header, 4096, Some("mislabelled.stl"));
            assert_eq!(c.format, FormatId::Image(kind), "for {kind:?}");
            assert_eq!(c.confidence, Confidence::Magic);
        }
    }

    #[test]
    fn bmp_needs_its_length_field_to_agree_with_the_file() {
        // "BM" alone is two bytes and turns up inside plenty of unrelated binaries.
        let mut h = b"BM".to_vec();
        h.extend_from_slice(&4096u32.to_le_bytes());
        h.extend_from_slice(&[0; 10]);
        assert_eq!(
            top(&h, 4096, Some("a.bmp")).format,
            FormatId::Image(ImageKind::Bmp)
        );
        // Wrong length: falls back to the extension, and says so.
        let c = top(&h, 9999, Some("a.bmp"));
        assert_eq!(c.format, FormatId::Image(ImageKind::Bmp));
        assert_eq!(c.confidence, Confidence::ExtensionOnly);
    }

    #[test]
    fn a_binary_stl_whose_header_starts_with_zeros_is_not_mistaken_for_an_icon() {
        // 00 00 01 00 is the ICO magic and also an entirely ordinary start to an STL comment.
        let mut h = vec![0u8; 84];
        h[2] = 1;
        h[4] = 4; // an icon count that would otherwise look plausible
        h[80..84].copy_from_slice(&12u32.to_le_bytes());
        let cands = sniff(&h, 84 + 12 * 50, Some("part.stl"));
        assert!(!cands
            .iter()
            .any(|c| c.format == FormatId::Image(ImageKind::Ico)));
        assert_eq!(cands[0].format, FormatId::StlBinary);
    }

    #[test]
    fn targa_arrives_by_name_because_it_has_no_header_magic() {
        let h = [0u8, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let c = top(&h, 4096, Some("crate.tga"));
        assert_eq!(c.format, FormatId::Image(ImageKind::Tga));
        assert_eq!(c.confidence, Confidence::ExtensionOnly);
    }

    #[test]
    fn formats_we_can_name_but_not_decode_report_themselves_honestly() {
        // AVIF and HEIF are delegated to the platform, so they are offered rather than refused;
        // whether this particular machine has the codec is the importer's news to break.
        assert!(FormatId::Image(ImageKind::Avif).is_supported());
        assert!(FormatId::Image(ImageKind::Heif).is_supported());
        // SVG is different in kind: vector, with no pixels to sample at any size.
        assert!(!FormatId::Image(ImageKind::Svg).is_supported());
        assert!(FormatId::Image(ImageKind::Png).is_supported());
        assert!(FormatId::Video(VideoKind::Mp4).is_supported());
        // The 4DS container is read and reported; its blocks need a licensed codec.
        assert!(!FormatId::FourDs.is_supported());
    }

    // --- Video ---

    #[test]
    fn video_containers_are_recognized_by_magic() {
        let cases: [(&[u8], VideoKind); 4] = [
            (b"RIFF\x24\0\0\0AVI LIST", VideoKind::Avi),
            (b"\x1A\x45\xDF\xA3\x01\0\0\0webm\0", VideoKind::WebM),
            (b"FLV\x01\x05\0\0\0\x09", VideoKind::Flv),
            (b"OggS\0\x02\0\0", VideoKind::Ogg),
        ];
        for (header, kind) in cases {
            assert_eq!(
                top(header, 1_000_000, None).format,
                FormatId::Video(kind),
                "for {kind:?}"
            );
        }
    }

    #[test]
    fn a_transport_stream_needs_four_sync_bytes_in_cadence() {
        let mut h = vec![0u8; 600];
        for i in [0usize, 188, 376, 564] {
            h[i] = 0x47;
        }
        assert_eq!(
            top(&h, 1_000_000, None).format,
            FormatId::Video(VideoKind::MpegTs)
        );
        // A lone 0x47 proves nothing.
        let mut lonely = vec![0u8; 600];
        lonely[0] = 0x47;
        assert_eq!(top(&lonely, 1_000_000, None).format, FormatId::Unknown);
    }

    #[test]
    fn a_matroska_file_without_the_webm_doctype_stays_matroska() {
        let h = b"\x1A\x45\xDF\xA3\x01\0\0\0matroska\0";
        assert_eq!(
            top(h, 1_000_000, Some("film.mkv")).format,
            FormatId::Video(VideoKind::Matroska)
        );
    }

    #[test]
    fn the_4ds_container_is_named_even_though_it_cannot_be_decoded() {
        let mut h = b"4DS0".to_vec();
        h.extend_from_slice(&[0; 80]);
        let c = top(&h, 40_420_433, Some("performance.4ds"));
        assert_eq!(c.format, FormatId::FourDs);
        assert_eq!(c.confidence, Confidence::Magic);
        assert!(c.note.contains("BridgeCodec4DS"));
    }

    #[test]
    fn dicom_magic_sits_at_offset_128() {
        let mut h = vec![0u8; 132];
        h[128..132].copy_from_slice(b"DICM");
        assert_eq!(top(&h, 500_000, Some("slice001.dcm")).format, FormatId::Dicom);
    }

    #[test]
    fn nifti_magic_sits_at_offset_344() {
        let mut h = vec![0u8; 348];
        h[344..348].copy_from_slice(b"n+1\0");
        assert_eq!(top(&h, 500_000, Some("brain.nii")).format, FormatId::Nifti);
    }

    #[test]
    fn obj_is_recognized_from_its_directives() {
        let h = b"# exported\nmtllib scene.mtl\nv 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 0.0 1.0 0.0\nf 1 2 3\n";
        let c = top(h, 512, Some("scene.obj"));
        assert_eq!(c.format, FormatId::Obj);
    }

    #[test]
    fn a_zip_named_3mf_ranks_above_the_bare_zip_verdict() {
        let h = b"PK\x03\x04\x14\x00\x00\x00";
        let cands = sniff(h, 4096, Some("part.3mf"));
        assert_eq!(cands[0].format, FormatId::ThreeMf);
        assert!(cands.iter().any(|c| c.format == FormatId::Zip));
    }

    #[test]
    fn unrecognized_input_yields_exactly_one_unknown_candidate() {
        let h = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        let cands = sniff(&h, 8, Some("mystery.bin"));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].format, FormatId::Unknown);
    }

    #[test]
    fn empty_input_does_not_panic() {
        let cands = sniff(&[], 0, None);
        assert_eq!(cands[0].format, FormatId::Unknown);
    }

    #[test]
    fn candidates_come_back_ranked_by_descending_effective_level() {
        // Raw confidence alone is not monotonic across the result, because a container magic is
        // demoted one level. The ordering that must hold is the effective one.
        let h = b"PK\x03\x04\x14\x00\x00\x00";
        let cands = sniff(h, 4096, Some("bundle.sog"));
        let level = |c: &Candidate| c.confidence as i32 - if is_container(&c.format) { 1 } else { 0 };
        for w in cands.windows(2) {
            assert!(level(&w[0]) >= level(&w[1]));
        }
    }

    #[test]
    fn a_specific_format_outranks_the_container_it_arrived_in() {
        // ZIP carries a real magic and SOG only has a name, yet SOG is the useful answer.
        let zip = sniff(b"PK\x03\x04\x14\x00\x00\x00", 4096, Some("bundle.sog"));
        assert_eq!(zip[0].format, FormatId::Sog);
        assert_eq!(zip[0].confidence, Confidence::Structural);
        assert_eq!(zip[1].format, FormatId::Zip);
        assert_eq!(zip[1].confidence, Confidence::Magic);
    }

    #[test]
    fn containers_that_name_a_wrapper_rather_than_a_format_stay_unsupported() {
        // A bare ZIP or ISOBMFF verdict says what the file is wrapped in, not what it holds.
        assert!(!FormatId::Isobmff.is_supported());
        assert!(!FormatId::Zip.is_supported());
        assert!(!FormatId::Unknown.is_supported());
        // These name an actual format, and each has an importer.
        assert!(FormatId::Usdc.is_supported());
        assert!(FormatId::Ares.is_supported());
        assert!(FormatId::Step.is_supported());
        assert!(FormatId::Spz(SpzEncoding::PlaintextV4).is_supported());
    }
}
