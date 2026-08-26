# Format Inventory

Target: a lightweight replacement for Windows 3D Viewer that opens *any* 3D representation —
meshes, point clouds, Gaussian splats, neural fields, volumetric video, volumes, CAD.

**Timing.** Microsoft deprecated 3D Viewer in February 2026, removed the built-in app in June
2026, and pulled it from the Microsoft Store in July 2026. Its official replacement advice is
"use a browser-based viewer." There is currently no shipped Windows desktop 3D viewer. That is
the gap.

This document is the input to the loader architecture. It is a survey, not yet a commitment:
every row carries a **tier** that says how much weight the format is allowed to add to the app.

Verified against the state of the standards as of **2026-08-25**.

Rows marked **[ARES]** are **first-party and authoritative**: ARES is this project owner's own
format, and those figures, byte layouts and decode findings come from the ARES Runtime
Specification and its measured results (`G:\StableAudio\4 - Volumetric Interop\ares`). They are
the most reliable rows in this document, not the least. Rows marked **[unverified]** are the
opposite case: vendor claims that have not been checked against a real file.

**Direction of travel.** This inventory is scoped to a viewer. The same importer set is the
front half of a converter that writes `.ares` from any of these formats, which is the likely next
project. Nothing here should be built in a way that forecloses that, and the
importer-to-`Scene`-to-exporter shape in [ARCHITECTURE.md](ARCHITECTURE.md) already allows it.
The viewer ships first.

---

## What is built, as of 2026-08-26

This document remains a survey. [HANDOFF.md](../HANDOFF.md) is the status; the short version is
that these sections are implemented and verified against real files:

| Section | State |
|---|---|
| §A.1 parity set | glTF/GLB, OBJ, PLY (all four variants), STL, FBX |
| §C splats | SPZ v1–v4, INRIA and SuperSplat PLY. Degree-0 colour only |
| §E.1 `.ares` | Container, meshopt geometry, VP9/AV1 texture track. Verified on real captures |
| §E.1 `.4ds` | Container read and reported. Blocks blocked, as tiered |
| §E.4 Depthkit CPP | Sidecar, layout detection, hue depth decode, unprojection |
| §F volumes | DICOM series, NIfTI, NRRD |
| §H images | PNG, JPEG, GIF, BMP, WebP, TIFF, TGA, QOI, ICO, DDS, PNM, Radiance, OpenEXR |
| §K spatial media | 360 photos and video, equirectangular, mono and stereo |
| Video generally | Whatever Windows Media Foundation can decode on the machine |

Two findings from building §E and §K are worth folding back into the survey:

- **The `.mp4` ambiguity resolves in two different places.** Spherical metadata is *in* the file,
  so the importer reads the boxes. Depthkit is *beside* it, so only something holding a path can
  tell — which means the sniffer, which never touches the file system, structurally cannot answer
  the question. §E.4's "needs intrinsics from a sidecar" is not an inconvenience; it is what
  decides where the detection lives.
- **§E.4's "highest value-per-effort in this entire section" was right.** Depthkit took roughly a
  day's work against a real capture set and produced a moving surface. The three decode gates that
  make it work — saturation, hue wrap, grazing triangles — are not in any published spec and were
  each found the hard way; they are recorded in `crates/formats/src/depthkit.rs`.

---

## Tiering — how "lightweight" is enforced

The base binary must stay small and cold-start fast. Formats earn their way in:

| Tier | Meaning | Cost budget |
|---|---|---|
| **T0 — Core** | Statically linked. Opens on double-click with zero extra load. Covers the overwhelming majority of files people actually open. | < ~50 KB parser each, no heap spikes |
| **T1 — Bundled plugin** | Lazy-loaded module shipped in the box, resolved on first use of that type. | < ~2 MB each |
| **T2 — Heavy plugin** | Large dependency (CAD kernel, USD, full video stack). Downloaded on first use, cached. | 10–100 MB, opt-in |
| **T3 — Bridge** | Cannot be rendered directly. Import transcodes to a T0/T1 representation and caches the result. | one-time conversion cost |
| **T4 — Blocked** | Needs proprietary/licensed binaries the app may not redistribute. User supplies the DLL/SDK; app detects and wires it. | zero shipped |

Four rules follow from this and apply everywhere below.

1. **Sniff by magic bytes, never by extension.** `.ply` alone is four different things
   (mesh, point cloud, 3DGS, compressed 3DGS). `.mp4` is five (plain video, Depthkit CPP,
   Microsoft HoloVideo, V3C/V-PCC, spatial MV-HEVC). Extension picks the *candidate set*;
   the header picks the parser.
2. **Never fully materialize what you can stream.** A 34 M-splat scene, a 4911-frame volcap,
   and a 200 M-point LiDAR tile all have to open without a 10 GB resident set. Memory-map +
   quantized GPU-resident buffers + windowed decode are the default posture, not an optimization.
3. **Parsers are an attack surface.** Microsoft disabled FBX in 3D Viewer in February 2024 over a
   security vulnerability and never re-enabled it — that is the direct ancestor of this app's
   biggest risk. Every third-party parser (FBX, USD, CAD, video, archive) runs sandboxed with a
   hard memory ceiling and no filesystem or network reach.
4. **The standards landscape consolidated in 2026 and the app should bet on it.** glTF is
   becoming the convergence point for splats *and* volumetric video (see §L). Design the
   internal scene model to match glTF 2.1 + `KHR_gaussian_splatting` semantics so imports are
   near-lossless and a future export path is free.

---

## A. Polygon meshes & scenes

### A.1 The 3D Viewer parity set — all T0

These are what the app is replacing. All must open instantly with no plugin resolution.

| Ext | Format | Encoding | Notes |
|---|---|---|---|
| `.glb` | glTF 2.0 / **2.1** binary | JSON chunk + BIN chunk | The primary target. Must handle `KHR_draco_mesh_compression`, `EXT_meshopt_compression`, `KHR_texture_basisu`, `KHR_materials_*`, `EXT_mesh_gpu_instancing`. **glTF 2.1 adds a 64-bit GLB variant** that lifts the 4 GB ceiling — parse the header length field defensively |
| `.gltf` | glTF 2.0 / 2.1 JSON | JSON + external `.bin` + images | Resolve relative URIs; refuse absolute/remote by default |
| `.obj` | Wavefront OBJ | ASCII | + `.mtl` sidecar. Pathologically slow if parsed naively — needs a bulk/SIMD float scanner |
| `.ply` | Stanford PLY | ASCII / binary LE / binary BE | See §C.1 — a PLY may be a splat cloud; dispatch on property names |
| `.stl` | Stereolithography | ASCII / binary | No topology, no UVs. Needs vertex welding to render sanely |
| `.3mf` | 3D Manufacturing Format | ZIP + XML | Microsoft's own. Production extension, beam lattice, multi-material |
| `.fbx` | Autodesk FBX | binary (7.x) + ASCII | Use **ufbx** (MIT, single-file C), not the Autodesk SDK. Covers 6.x–7.7. **Sandbox it** — see rule 3 |

### A.2 glTF 2.1 — what changed (ratification anticipated Q4 2026)

Backward-compatible revision, aimed at large composed scenes rather than single assets:

- **64-bit GLB** — removes the 4 GB file ceiling.
- **Embedded preview thumbnails** — displayable without a renderer. *Directly useful:* this is
  how the app populates Explorer-style thumbnails and a gallery view without decoding geometry.
- **Multi-file scene graphs** — standardized composition/delivery across files (digital twins,
  BIM, smart cities, geospatial).
- **Progressive delivery** and new spatial primitives.
- **Promoted to core:** WebP textures, node visibility toggle, first-class Unique IDs (UIDs).

Plan the loader for 2.1 now; 2.0 files remain valid input.

### A.3 Interchange & DCC — T0/T1

| Ext | Format | Tier | Notes |
|---|---|---|---|
| `.usd` `.usda` `.usdc` `.usdz` | **OpenUSD** (ASCII / crate binary / zip package) | T1/T2 | **AOUSD Core Specification 1.0 was ratified 2025-12-17** and formally specifies USDA, USDC and USDZ — for the first time there is a real spec to implement against instead of "whatever Pixar's code does." Core Spec **1.1** (animation, massive-scene scaling, compliance testing) is in progress for 2026. Full OpenUSD is enormous; use **tinyusdz** (MIT) for the viewer subset. `.usdz` is an *uncompressed* zip — stream members in place |
| `.abc` | Alembic | T1 | Ogawa or HDF5 backing. The de-facto baked-geometry-cache format; **also a main volumetric-video interchange** (see §E) |
| `.dae` | COLLADA | T1 | XML. Large and slow; common from SketchUp/KMZ |
| `.3ds` | Autodesk 3DS | T1 | Legacy chunked binary, 16-bit vertex limits |
| `.ase` | ASCII Scene Export | T1 | Legacy |
| `.blend` | Blender | T3 | Undocumented but self-describing (DNA block). Read-only mesh extraction is feasible; full evaluation is not |
| `.wrl` `.vrml` | VRML 1.0/2.0 | T1 | |
| `.x3d` `.x3dv` `.x3db` `.x3dz` | X3D (XML / classic / binary / zipped) | T1 | VRML's ISO successor |
| `.off` | Object File Format | T0 | Trivial ASCII, common in academic meshes |
| `.gts` | GNU Triangulated Surface | T1 | |
| `.x` | DirectX retained-mode | T1 | Text + binary |
| `.ms3d` | MilkShape 3D | T1 | |
| `.lwo` `.lws` | LightWave object / scene | T1 | |
| `.lxo` | Modo | T1 | |
| `.c4d` | Cinema 4D | T3 | Proprietary; no open reader |
| `.ma` `.mb` | Maya ASCII / binary | T3 | `.ma` is parseable ASCII; `.mb` is not |
| `.max` | 3ds Max | T3 | OLE compound file, proprietary payload |
| `.ac` | AC3D | T1 | |
| `.ogex` | OpenGEX | T1 | |
| `.mesh` `.mesh.xml` | Ogre3D | T1 | |
| `.m3d` | Model 3D | T1 | |
| `.iqm` `.iqe` | Inter-Quake Model | T1 | Skeletal, game-oriented |
| `.vrm` | VRM 0.x / 1.0 avatar | T1 | glTF + `VRMC_*` extensions; humanoid rig, springbones, MToon |
| `.pmx` `.pmd` | MikuMikuDance model | T1 | Very common in the hobby/VTuber space |
| `.nff` `.raw` `.cob` `.sib` `.ndo` `.q3o` `.q3s` `.b3d` `.irr` `.irrmesh` `.ter` `.hmp` `.xgl` `.zgl` `.csm` `.uc` | Assimp long tail | T2 | Not worth hand-writing. One sandboxed Assimp plugin covers all of them at once |

### A.4 Game / engine formats — T1

| Ext | Format | Notes |
|---|---|---|
| `.md2` `.md3` | Quake II / III | Vertex-animated |
| `.md5mesh` `.md5anim` | Doom 3 | Skeletal, ASCII |
| `.mdl` | Quake / Half-Life / Source | Same extension, three unrelated formats — dispatch on magic (`IDPO`, `IDST`, `IDSQ`) |
| `.vtx` `.vvd` `.phy` | Source model sidecars | Must be read together with `.mdl` |
| `.bsp` | Quake/Source level | Whole-level geometry + lightmaps |
| `.smd` `.vta` | Studiomdl source | ASCII |
| `.pk3` `.vpk` `.pak` `.wad` | Archives containing the above | Treat as virtual filesystems |
| `.uasset` | Unreal | T3/T4 — engine-version-coupled, needs a per-version schema |
| `.unity3d` `.assets` `.bundle` | Unity serialized | T3 — AssetStudio-class extraction |

---

## B. Point clouds & LiDAR

Scanning output is where "low memory" stops being a nicety. Everything here streams.

| Ext | Format | Tier | Notes |
|---|---|---|---|
| `.ply` | PLY as point cloud | T0 | No faces + `x y z [red green blue] [nx ny nz]` |
| `.pcd` | Point Cloud Library | T0 | ASCII / binary / binary_compressed (LZF) |
| `.xyz` `.pts` `.txt` | Plain ASCII points | T0 | Delimiter and column order must be sniffed or prompted |
| `.las` | ASPRS LAS | T1 | Point formats 0–10; classification, GPS time, RGB, waveform |
| `.laz` | LASzip-compressed LAS | T1 | Chunked — the chunk table enables partial decode |
| `.copc.laz` | **Cloud-Optimized Point Cloud** | T1 | LAZ with an embedded octree index. **The right target for huge scans** — true LOD streaming, open range requests |
| `.ept.json` | Entwine Point Tile index | T1 | Octree of LAZ chunks over HTTP |
| `.e57` | ASTM E57 | T1 | XML index + binary sections; multi-scan with poses **and** embedded panoramic imagery |
| `.ptx` | Leica structured scan | T1 | Grid-structured; carries the scanner transform |
| `.bpf` | Binary Point File | T2 | |
| `.pnts` | Cesium 3D Tiles point tile | T1 | See §H |
| `.fls` `.fws` | FARO scan | T4 | Proprietary; FARO SDK |
| `.zfs` `.zfprj` | Zoller+Fröhlich | T4 | Proprietary |
| `.rcp` `.rcs` | Autodesk ReCap project/scan | T4 | Proprietary container |
| `.pod` | Bentley Pointools | T4 | |

---

## C. Gaussian splats (3DGS)

The fastest-moving category, and the one that changed most in 2026. Roughly a dozen formats in
three years, most of them one project's export — but the ecosystem is now visibly consolidating
around **PLY for archive, SPZ/SOG for delivery, and glTF for interchange.**

**Canonical internal form:** position (quantized to scene AABB), scale (3), rotation (quat),
opacity, SH degree 0 (DC color), optional SH 1–4. That is what every format below reduces to.
Match `KHR_gaussian_splatting` semantics exactly (see §C.3) so import and export are lossless.

### C.1 The formats

| Ext | Format | Tier | Layout |
|---|---|---|---|
| `.ply` | **INRIA 3DGS reference output** — the de facto archive format | T0 | binary_little_endian, per-vertex: `x y z`, `nx ny nz` (written but unused), `f_dc_0..2`, `f_rest_0..44`, `opacity`, `scale_0..2`, `rot_0..3`. Detect by presence of `f_dc_*` + `scale_*` + `rot_*`. Lossless, universally readable, what every trainer emits |
| `.ply` | **SuperSplat / PlayCanvas compressed PLY** | T0 | Chunked quantized variant: element `chunk` + packed `vertex`. Same extension, different schema — sniff the element list |
| `.spz` | **Niantic SPZ ("Splat Zip")** — MIT, and the delivery favourite | T0 | See §C.2 — **four versions in the wild, v4 as of 2026-05-05** |
| `.sog` | **PlayCanvas SOG — "Spatially Ordered Gaussians"** (v2 current) | T1 | Attributes sorted into **Morton order**, quantized, and stored as **lossless WebP images** + `meta.json`, bundled into a single `.sog`. Positions are 16-bit split across two images and dequantized in a log-transformed space; scales and colors go through small **codebooks**. ~15–20× smaller than PLY (4 M gaussians: 1 GB PLY → 42 MB). Morton order means **the GPU can consume it directly with no reprocessing**. Decodes through the image pipeline you already have — best effort-to-payoff ratio in this table after SPZ |
| `.splat` | antimatter15 splat viewer | T0 | Flat 32 B/splat: pos `f32×3`, scale `f32×3`, RGBA `u8×4`, rot `u8×4`. No SH, no header at all — sniffing must use `size % 32 == 0` plus value-range heuristics |
| `.ksplat` | mkkellogg GaussianSplats3D | T1 | Compression levels 0/1/2, section headers, bucketed positions |
| `.spx` | SPX (Reall3d / gsbox ecosystem) | T1 | Combines encoding ideas from `.splat`, `.spz` and `.sog` and adds **block compression for progressive loading** of large models; extensible, with an optional data-protection mechanism. Spec: `reall3d-com/Reall3dViewer/SPX_EN.md`. Converter: `gotoeasy/gsbox` (Go, single binary, converts PLY/SPLAT/SPX/SPZ/KSPLAT/SOG — useful as a test-corpus generator) |
| `.splatv` | antimatter15 animated splat | T1 | Time-varying variant — bridges into §E |
| `.psht` | Jawset **Postshot** project | T3 | Native project; Postshot exports `.ply`/`.ksplat`/`.spz` — use those |
| glTF `KHR_gaussian_splatting` | **Khronos splat extension** | T1 | See §C.3. The standardization endpoint — makes splats first-class inside `.glb`/`.gltf` and inside 3D Tiles |
| `.ply` / `.splat` / `.spz` from Luma, Polycam, Scaniverse, World Labs Marble | vendor exports | T0 | All emit one of the above — no separate importer needed |

### C.2 SPZ in detail — four versions, and you need all of them **[ARES-verified for v2]**

SPZ is MIT-licensed, developed by Niantic Spatial, and emitted by **Scaniverse** and
**World Labs Marble**; Adobe Photoshop reads it. It is the most likely single format to matter.

**v1–v3 (gzip era).** The whole file is a gzip stream. Inflate to get a 16-byte header —
`magic u32 = 0x5053474E ("NGSP")`, `version u32`, `numPoints u32`, `shDegree u8`,
`fractionalBits u8`, `flags u8` (bit 0 = antialiased), `reserved u8` — followed by **tightly
packed per-attribute contiguous arrays**, not interleaved per point (so each attribute is a
`subarray()` with no striding):

```
positions : numPoints * 3 * int24   little-endian signed; metres = v / (1 << fractionalBits)
alphas    : numPoints * u8          alpha = a / 255
colors    : numPoints * 3 * u8      sh0 = (c/255 - 0.5) / 0.15 ; rgb = 0.5 + 0.28209479177387814 * sh0
scales    : numPoints * 3 * u8      metres = exp(s / 16 - 10)
rotations : numPoints * 3 * u8      xyz = (r - 127.5)/127.5 ; w = sqrt(max(0, 1 - x²-y²-z²))
sh        : numPoints * shDim * u8  shDim = 0 / 9 / 24 / 45 for degree 0/1/2/3
```

The exact byte total `16 + N*(9+1+3+3+3+shDim)` is a **free corruption gate** — verified
byte-exact on a 27.8 MB Marble export (v2, 1,920,000 splats, shDegree 0, fractionalBits 10 →
predicted 36,480,016 bytes, actual 36,480,016, zero remainder). v3 re-encoded rotations: ~3×
faster, 2.5% smaller.

**v4 (2026-05-05) — a redesign, not an increment:**

- **Plaintext header at a fixed offset, outside the compressed region**, beginning with the
  `NGSP` magic (which previously sat *inside* the gzip stream, behind the gzip magic). Point
  count, SH degree, version and flags are now readable **without decompressing** — exactly what a
  file browser wants for instant metadata and thumbnails.
- **Six parallel ZSTD streams, one per attribute** (positions, colors, scales, rotations, alphas,
  SH), replacing single-stream gzip. ~3× faster encode, 1.5–2× faster load, multi-core on both
  ends. On an 8.5 GB / 34 M-point PLY: encode 3m26s (v3) → 1m08s (v4).
- **The 10-million-point cap is gone** (it previously applied to both the PLY loader and the SPZ
  deserializer). Tens of millions of gaussians are now in scope — plan buffers accordingly.
- **Vendor extension chain**: each record carries a vendor-tagged ID, a length, and a payload, so
  unknown extensions are safely skipped. First one in the wild is `0xADBE0002` (Adobe) carrying
  safe orbit-camera bounds — **worth honouring, it gives the viewer a sane default camera.**
- **SH degree 4** (up from 3), and **configurable SH quantization** from 3 bits (~18×) to 8 bits
  (essentially zero MSE).

**Compatibility is asymmetric and the app must respect it:** v4 readers load v2/v3 through a
legacy path; v2/v3 readers hit a v4 file and fail fast with "unrecognized format." So: implement
v4 first, keep the gzip path for everything older, and sniff by *checking for the plaintext
`NGSP` at offset 0 before assuming gzip.*

### C.3 `KHR_gaussian_splatting` — the interchange endpoint

Release candidate announced **2026-02-03**; ratification was targeted for Q2 2026 and the
extension **is still marked Release Candidate as of 2026-08-25**, so track it but do not assume
frozen semantics. It defines an ellipse kernel with these attribute semantics:

| Attribute | Type | Component types |
|---|---|---|
| `POSITION` | VEC3 | (inherited) |
| `KHR_gaussian_splatting:ROTATION` | VEC4 | float, byte-normalized, short-normalized |
| `KHR_gaussian_splatting:SCALE` | VEC3 | float, ubyte, ushort (normalized variants) |
| `KHR_gaussian_splatting:OPACITY` | SCALAR | float, ubyte-normalized, ushort-normalized |
| `KHR_gaussian_splatting:SH_DEGREE_0_COEF_0` | VEC3 | float |
| `SH_DEGREE_1_COEF_[0-2]`, `SH_DEGREE_2_COEF_[0-4]`, `SH_DEGREE_3_COEF_[0-6]` | VEC3 | float |

SH is **in the base extension**, not a companion. Compression extensions are expected to *extend*
this base — Niantic has proposed **`KHR_gaussian_splatting_compression_spz`** to carry SPZ inside
glTF; in active review, not ratified. **Adopt these exact semantics as the app's internal splat
model.** Everything else in §C.1 then becomes a thin importer, and glTF export is nearly free.

### C.4 Practical notes that shape the implementation

- **Splat counts are 10–100× mesh vertex counts for the same scene.** One room capture measured
  1.92 M splats / 27.8 MB compressed; SPZ 4 targets 34 M+. Per-splat byte layout matters far more
  than for meshes — the mesh trick of padding to `u16×4` costs 8 B/splat here. **[ARES]**
- **SH degree 0 is the common case, not the fallback**, especially for generated environments
  (the Marble capture: shDegree 0, i.e. no view-dependent colour at all). Budget for degree 0 and
  treat 1–4 as the optional tier. **[ARES]**
- **Sorting is the real cost, not decode.** Correct alpha compositing needs a per-frame
  back-to-front sort of every splat. GPU radix sort or coarse bucketed sort is mandatory — a CPU
  sort of 2 M splats per frame is the single thing that will make the app feel heavy. SOG's
  Morton ordering helps here; so does reusing last frame's order under small camera deltas.
- **Absolute vs. AABB-relative quantization.** SPZ is absolute fixed-point, so a room-scale
  capture wastes most of its int24 range (measured raw AABB 73×71×48 m for a 16×16×26 m room).
  An AABB-relative internal form is strictly better at the same bit budget. **[ARES]**
- **Median splat is nearly flat** (min/max axis ratio ≈ 0.00–0.01) and nearly opaque (median
  alpha 0.988). Degenerate-thin ellipsoids are the norm, not an edge case — the rasterizer must
  not assume well-conditioned covariance. **[ARES]**

---

## D. Neural fields / NeRF

**The honest framing:** NeRF is not a file format. It is a set of network weights plus a runtime
that evaluates them. There is no `.nerf`, and there is still no standardization effort for one —
the industry's answer in 2026 was to converge on splats instead. Support therefore splits three
ways:

| Path | What it means | Tier |
|---|---|---|
| **Baked bundles** | Already converted to something rasterizable (proxy geometry + feature textures + a small MLP in a shader). These *are* viewable. | T1 |
| **Snapshots** | Framework-specific checkpoints. Viewable only by re-implementing that framework's evaluator. | T2/T3 |
| **Convert-on-import** | Run the model once to bake a mesh or splat cloud, cache the result, view that. | T3 |

| Ext / artifact | System | Tier | Notes |
|---|---|---|---|
| `.ingp` | NVIDIA instant-ngp snapshot | T2 | msgpack + zstd; multiresolution hash-grid weights. A wgpu evaluator is feasible but real work |
| `config.yml` + `.ckpt` | **Nerfstudio** | T3 | PyTorch checkpoint. Realistic path: `ns-export` to mesh/pointcloud/splat, or read Nerfstudio's own splat `.ply` |
| `transforms.json` | NGP/Blender camera convention | T0 | Not renderable geometry, but the viewer should recognize it and draw the **camera rig** — genuinely useful and nearly free |
| `cameras.bin` `images.bin` `points3D.bin` (+ `.txt`) | **COLMAP** sparse model | T0 | Sparse point cloud + camera frusta. Cheap to parse, very useful to visualize, and the universal companion to every splat/NeRF capture |
| `.npz` grids | Plenoxels / PlenOctrees | T2 | Explicit voxel/octree radiance grids — actually rasterizable without a neural net |
| `.obj` + feature `.png` + shader | **MobileNeRF** | T1 | Already a mesh + textures; the "NeRF" is a tiny MLP in the fragment shader |
| tri-plane `.png`/`.json` bundles | MERF / SMERF | T1 | Same idea, baked volumetric proxies |
| `.onnx` `.pt` `.safetensors` | Generic weights | T3 | Only meaningful with a matching architecture declaration |

**Recommendation:** (1) render baked bundles natively, (2) visualize the capture rig
(COLMAP / `transforms.json`) natively, (3) offer a one-click "bake to splats" bridge for
snapshots. Do **not** promise to open arbitrary checkpoints — that promise cannot be kept.

---

## E. Volumetric video

The category with the worst interoperability, and the one actively being fixed right now
(see §L). Every mature system is either proprietary-but-good or open-but-per-frame.

### E.1 Open / self-owned

| Ext | Format | Tier | Notes |
|---|---|---|---|
| `.ares` | **ARES** (GANTASMO) **[ARES]** | **T0** | Single-file chunked container, and the reference implementation is local. 64 B header: `magic "ARES"`, `version_major/minor`, `header_flags` (bit0 live, bit1 has_audio, bit2 crossorigin_hint), `geometry_profile` (0=mesh-IPB, 1=splat-IPB, 2=video-geometry), `texture_codec` (0=none,1=AV1,2=VP9,3=HEVC,4=AVC), `intra_codec` (0=meshopt,1=draco,2=raw), `entropy_codec`, `fps f32`, `frame_count u32`, `duration_us u64`, offsets to superblock / GOP index / track directory / first chunk, `header_crc32`. Superblock carries AABB, `quant_bits_pos`, `quant_bits_uv`, `normal_encoding`, `sh_degree`, `gop_length`, ABR `tier_count`, KV metadata. GOP index is a seek table (`start_pts_us`, `frame_start`, `frame_count`, `byte_offset`, `byte_length`, per-tier offsets, flags incl. forced_keyframe on topology change). Track directory: `0=geometry, 1=texture-color, 2=texture-aux, 3=audio, 4=metadata` with `codec_fourcc` (`AV01`/`VP09`/`MSHO`/`OPUS`). Then time-ordered `CNK0` chunks, one closed GOP each: meshopt-compressed quantized intra geometry + P/B residuals, one closed video GOP of texture, optional aux/audio/metadata. **Geometry stays quantized until the vertex shader dequantizes it** — precisely the low-memory posture this whole app wants. Measured: 49.7 MB / 1 file for a 272-frame capture vs 1.58 GB / 544 files raw |
| `.uvol` + manifest JSON | **UVOL / Universal Volumetric** (Etereal) | T1 | Per-frame Draco mesh + KTX2/Basis textures, JSON manifest. Open, and the closest open prior art. Many files per clip → request/IO amplification |
| per-frame `.obj`/`.ply` + `.png` folders | Raw mesh sequence | T0 | The universal lowest common denominator, and what every vendor bridge lands on. 544 files / 1.58 GB for a 9 s clip is typical — the viewer must treat a folder as a **single playable object** and must not open 544 file handles |
| `.abc` | Alembic sequence | T1 | Standard baked interchange out of Houdini/Maya; what Evercoast and others export |
| `.glb` sequences / **VVglTF** segments | Streaming glTF | T1 | Chunked glTF segments with rate adaptation |
| `.exr` deep / multi-channel | OpenEXR deep scanline/tile | T2 | Deep compositing data is genuinely volumetric — per-pixel sample lists with Z |

### E.2 Vendor / proprietary

| Ext | Format | Tier | Reality check |
|---|---|---|---|
| `.4ds` | **4DViews HOLOSYS** **[ARES]** | **T4** | Mesh + **DXT1** texture per frame. Decodable only through `BridgeCodec4DS.dll`, a licensed native plugin that cannot be redistributed. Known-good drive sequence: `CreateSequence` → query props → per frame `Play(false)` / `GotoFrame(i)` / `Play(true)` → poll `UpdateModel` until a new frame id arrives → decode DXT1 → `Stop` / `DestroySequence`. Only DXT1 is supported (ETC/PVRTC/ASTC hard-fail; desktop captures are DXT1). **Critical gotcha:** the UV atlas is repacked at irregular *topology reset* frames (measured at 0, 5, 12, 34, 35, 59, 75, 77 in the first 95 frames of one clip — irregular period, not a fixed cadence; detect via SHA1 of the trimmed index array). UV-seam vertices drift up to ~28 mm within a run and snap back at each run's keyframe, producing **visible pulsing cracks** unless welded per run (group by exact position bits, restrict to groups with ≥2 members touching an open boundary edge). Plan: detect a user-supplied DLL, wire it if present, otherwise offer transcode. Windows-only by nature |
| `.mp4` (HoloVideo) | **Microsoft MRCS / SVF HoloVideo** **[ARES]** | T4/T3 | Geometry packed *into* the video bitstream alongside texture, so one hardware decode recovers both. Historically readable only through a native/Unity plugin (`SVFUnityPlugin`). Practical path is the ARES `tools/svf-extract` route: Unity **Play-Mode** export to per-frame OBJ+PNG, then encode. Play-Mode is mandatory — `GL.IssuePluginEvent` + `WaitForEndOfFrame` are render-thread semantics that batchmode/EditMode will not drive |
| `.mp4` + `_metadata.json` / `.txt` | **Depthkit** (Core / Studio) | **T1** | The friendliest vendor format by a wide margin: one video frame carries **color and packed depth side by side** ("CPP" — combined per-pixel), with a sidecar holding intrinsics, depth range, and the perspective layout. Essentially a *geometry image* — fully decodable with an ordinary video decoder plus a small unprojection shader, no proprietary anything. **Highest value-per-effort in this entire section.** Also ingestible by Arcturus HoloSuite, so it doubles as a bridge |
| `.avv` | **Arcturus AVV** (HoloSuite) | T4 | Proprietary "Accelerated Volumetric Video" codec. Multi-resolution textures and **per-vertex motion vectors** for real-time playback and inter-frame interpolation. HoloSuite now natively imports 4DViews and Depthkit, so it is increasingly the *hub* other formats pass through — which makes AVV files more likely to appear on disk. No open decoder exists |
| `.ecm` / `.abc` / `.usd` exports | Evercoast | T3 | Ingest via the open export formats |
| vendor containers | 8i, Volograms, etc. | T3 | Volograms exports `.glb` sequences — use those |

### E.3 Standards

| Ext | Format | Tier | Notes |
|---|---|---|---|
| `.bin` / `.mp4` (V3C tracks) | **MPEG V-PCC** (ISO/IEC 23090-5) | T2 | Projects a point cloud onto 2D patches; encodes geometry/occupancy/attribute *videos* with a conventional codec. The canonical proof that geometry-through-video interoperates. A full decoder is heavy |
| `.bin` | **MPEG G-PCC** | T2 | Octree/predictive coding for sparse clouds |
| `.mp4` (V3C/MIV) | MPEG Immersive Video | T2 | Multi-view + depth |
| VAV over MPEG-2 TS | **VFA Volumetric Player Format** | T2 | See §L — mesh-based and patch-based compression, HTTP streaming, H.265 video, audio-visual container. Members-only spec; extension not publicly documented **[unverified]** |

### E.4 RGBD & 2.5D

| Ext | Format | Tier | Notes |
|---|---|---|---|
| `.mp4` side-by-side / top-bottom color+depth | Generic RGBD video | T1 | Same unprojection path as Depthkit; needs intrinsics from a sidecar or user input |
| `.mkv` | Azure Kinect recording | T1 | Matroska with depth + IR tracks, calibration in attachments |
| `.bag` | ROS bag / RealSense | T2 | Depth + color + intrinsics streams |
| `.oni` | OpenNI recording | T2 | Legacy Kinect/Xtion |

---

## F. Voxels & volume grids

| Ext | Format | Tier | Notes |
|---|---|---|---|
| `.vdb` | **OpenVDB** | T2 | Sparse hierarchical grids — the VFX standard for smoke/fire/clouds. Needs real raymarching, not rasterization |
| `.nvdb` | NanoVDB | T1 | GPU-friendly flattened VDB — the right target for a lightweight viewer |
| `.vox` | MagicaVoxel | T0 | Tiny chunked format, very common |
| `.qb` `.qbcl` | Qubicle | T1 | |
| `.binvox` | binvox | T0 | Trivial RLE voxel grid |
| `.schematic` `.nbt` `.litematic` `.schem` | Minecraft structures | T1 | NBT (gzip'd), needs a block→mesh palette |
| `.raw` + `.dat`/`.mhd` | Raw volume + header | T0 | Ubiquitous in scivis |
| `.vti` `.vtu` `.vtp` `.vtk` `.vtm` | VTK / XML VTK | T1 | Image data, unstructured grids, polydata |
| `.nrrd` | Nearly Raw Raster Data | T0 | Simple ASCII header + payload |
| `.mha` `.mhd` | MetaImage | T0 | |
| `.dcm` / `DICOMDIR` | **DICOM** | T1 | Medical CT/MRI — a *folder of slices is one volume*. Modality LUT, window/level, and slice-order sorting all required |
| `.nii` `.nii.gz` | NIfTI | T0 | Neuroimaging standard |
| `.mrc` | MRC / cryo-EM | T1 | |
| `.h5` `.hdf5` `.n5` `.zarr` | Chunked array stores | T2 | Bioimaging / large-scale volumes; inherently streamable |
| `.tif` `.ome.tif` | TIFF / OME-TIFF stacks | T1 | Multi-page = volume |

---

## G. CAD, BIM & NURBS

Where "lightweight" collides hardest with "every format." Solid-model kernels are tens of
megabytes and tessellation is expensive. **All T2 or T3 — one shared kernel plugin, downloaded on
first use, never in the base app.**

| Ext | Format | Tier | Notes |
|---|---|---|---|
| `.step` `.stp` | ISO 10303 (AP203/214/242) | T2 | The open CAD interchange. Needs OpenCASCADE or equivalent for B-rep tessellation |
| `.iges` `.igs` | IGES | T2 | Legacy, surfaces |
| `.brep` | OpenCASCADE native | T2 | |
| `.sat` `.sab` | ACIS | T3 | |
| `.x_t` `.x_b` | Parasolid | T3/T4 | Kernel is licensed |
| `.3dm` | Rhino / **openNURBS** | T1 | openNURBS SDK is freely available — cheapest real NURBS support, and `.3dm` files usually embed render meshes |
| `.skp` | SketchUp | T2 | SDK available; geometry is already tessellated |
| `.dwg` | AutoCAD | T3 | Proprietary; ODA/Teigha |
| `.dxf` | Drawing Exchange | T1 | ASCII/binary; mostly 2D but carries `3DFACE`/`MESH`/`SOLID` |
| `.ifc` `.ifcxml` `.ifczip` | **IFC / BIM** (STEP-based) | T2 | Huge files, rich semantics. IfcOpenShell-class dependency. glTF 2.1's multi-file scene graphs are aimed squarely at this use case |
| `.jt` | Siemens JT (ISO 14306) | T2 | Contains ready-made LOD tessellations — friendlier than STEP |
| `.3dxml` | Dassault 3DXML | T2 | ZIP+XML, tessellated |
| `.prc` | PRC (ISO 14739) | T2 | Tessellated and B-rep; the modern 3D-PDF payload |
| `.u3d` | Universal 3D (ECMA-363) | T1 | Legacy 3D-PDF payload, tessellated |
| `.pdf` | 3D PDF | T2 | Extract embedded U3D/PRC annotations |
| `.catpart` `.catproduct` `.prt` `.asm` `.sldprt` `.sldasm` `.ipt` `.iam` `.f3d` | Native MCAD | T3/T4 | Vendor SDKs, licensing, size. Realistic answer: detect, name the format, suggest STEP — do not pretend |

---

## H. Geospatial & tiled scenes

Large-scene formats. All are *inherently* streaming/LOD — a good fit for the memory posture.

| Ext | Format | Tier | Notes |
|---|---|---|---|
| `tileset.json` + `.b3dm` `.i3dm` `.pnts` `.cmpt` | **Cesium 3D Tiles 1.0** | T1 | Batched/instanced/point/composite tiles |
| `tileset.json` + `.glb` | 3D Tiles 1.1 | T1 | Tiles are plain glTF now — also the carrier for `KHR_gaussian_splatting` at city scale |
| `.slpk` `.i3s` | Esri Scene Layer Package | T2 | ZIP of nodes + geometry buffers |
| `.kml` `.kmz` | Google Earth | T1 | KMZ embeds COLLADA |
| `.gml` `.citygml` | CityGML | T2 | XML, semantic city models |
| `.cityjson` | CityJSON | T1 | Far lighter JSON alternative to CityGML |
| `.osm` `.pbf` | OpenStreetMap | T2 | Needs extrusion to become 3D |
| `.geotiff` `.tif` `.hgt` `.dem` `.asc` | DEM / terrain rasters | T1 | Heightfield → mesh |
| `.ter` | Terragen terrain | T1 | |
| `.mbtiles` `.pmtiles` | Tile archives | T2 | |

---

## I. Textures, materials & environment (companions)

Needed to render everything above; also worth opening standalone.

| Ext | Notes | Tier |
|---|---|---|
| `.png` `.jpg` `.jpeg` `.webp` `.bmp` `.tga` `.gif` | Standard LDR. **WebP is now core glTF 2.1** — and SOG is built entirely on lossless WebP, so this decoder earns its keep twice | T0 |
| `.avif` | AV1 still — same decoder as AV1 video | T0 |
| `.hdr` `.exr` | Radiance HDR / OpenEXR — IBL environments | T1 |
| `.tif` `.tiff` | Including 16/32-bit | T1 |
| `.dds` | DXT/BC — what `.4ds` textures decode from | T0 |
| `.ktx` `.ktx2` | Khronos texture container; **KTX2 + Basis (UASTC/ETC1S) is the target for GPU-resident textures** | T0 |
| `.basis` | Basis Universal | T1 |
| `.pvr` `.astc` | Mobile GPU formats | T1 |
| `.mtl` | OBJ material sidecar | T0 |
| `.mdl` `.mgfx` `.osl` | Material definitions | T3 — record, don't evaluate |

## J. Animation & mocap (companions)

| Ext | Notes | Tier |
|---|---|---|
| `.bvh` | Biovision hierarchy — skeletal mocap, ASCII | T0 |
| `.c3d` | Marker-based mocap | T1 |
| `.trc` | Track row column | T1 |
| `.fbx` `.glb` `.abc` `.usd` | Animation inside the mesh formats | inherits |
| `.smd` `.vta` | Source animation | T1 |
| `.anim` | Unity | T3 |

## K. Spatial media & lightfield

Adjacent, and increasingly what people actually have on disk.

| Ext | Format | Tier | Notes |
|---|---|---|---|
| `.mp4` `.mov` (spherical metadata V1/V2) | 360 / 180 VR video | T1 | Equirect or EAC; render on a sphere |
| `.jpg` with XMP GPano | 360 photo | T0 | |
| `.mov` MV-HEVC | **Apple spatial video** | T2 | Stereo pair in one HEVC stream |
| `.heic` | Apple spatial photo | T2 | |
| `.vr.jpg` | Google VR180 photo | T1 | |
| `.insv` `.insp` | Insta360 | T3 | Dual-fisheye, needs their stitching |
| `.lif` | Leia Image Format | T2 | Multiview + depth |
| `.lfp` `.lfr` | Lytro lightfield | T3 | Legacy |
| `.rcproj` `.psx` `.psz` | RealityCapture / Metashape projects | T3 | Show the sparse cloud + cameras if reachable |

---

## L. The 2026 standards picture — and why it changes the design

Three things converged this year, and they all point the same direction:

1. **AOUSD Core Specification 1.0 ratified 2025-12-17** — USDA, USDC and USDZ have a real written
   spec for the first time. 1.1 (animation, massive scenes, compliance testing) is in progress.
2. **glTF 2.1** (ratification anticipated Q4 2026) — 64-bit GLB, embedded thumbnails, multi-file
   scene graphs, progressive delivery, WebP/visibility/UIDs promoted to core. glTF stops being a
   single-asset format and becomes a scene format.
3. **glTF becomes the volumetric convergence point.** Khronos formed a **Volumetric Media
   Subgroup** in the 3D Formats WG to standardize capture, data formats and playback for
   volumetric video — point clouds, light fields, dynamic 3D performances. On **2026-05-29**
   Khronos and the **Volumetric Format Association (VFA)** entered a liaison agreement, and
   **the VFA will adopt glTF as the primary format for future VFA specifications.** In parallel,
   `KHR_gaussian_splatting` (RC) and the proposed `KHR_gaussian_splatting_compression_spz` bring
   splats into the same envelope.

**Design consequence.** The app should not invent a neutral scene model. It should adopt
**glTF 2.1 + `KHR_gaussian_splatting` semantics as its internal representation**, and treat
everything in §A–§K as importers into it. That gets three things at once: near-lossless import,
a free export path, and automatic alignment with wherever volumetric video standardizes next.
`.ares` remains the high-performance native playback container for mesh volcap **[ARES]** — its
GOP/track/chunk model is the streaming layer, not a competing scene model.

---

## Reality checks — the formats that cannot simply be "supported"

State these plainly in the UI rather than failing mysteriously:

1. **`.4ds`** — needs `BridgeCodec4DS.dll` under a 4DViews SDK license. Detect a user-supplied
   DLL and light up the loader; never ship one. Windows-only.
2. **Microsoft SVF / HoloVideo `.mp4`** — geometry-in-video, plugin-only. Offer the Unity
   Play-Mode transcode route; do not claim native playback.
3. **Arcturus `.avv`** — closed. No open decoder exists. Increasingly common because HoloSuite is
   becoming the hub other volumetric formats pass through.
4. **VFA Volumetric Player Format** — specification is members-only. Track it via the glTF liaison
   rather than reverse-engineering it.
5. **Native MCAD** (`.catpart`, `.sldprt`, `.prt`, `.x_t`, `.dwg`) — vendor SDKs, licensing, size.
   Detect and explain; suggest STEP.
6. **NeRF checkpoints** — not a format. Bake or decline.
7. **`.max`, `.c4d`, `.mb`** — proprietary DCC scene files with no open readers.
8. **SPZ v2/v3 vs v4** — not a "cannot support," but a hard compatibility asymmetry: v4 files are
   unreadable by older parsers. Implement v4 + legacy gzip path together or produce confusing
   failures.

---

## What this implies for the architecture

- **A tiny statically-linked core** covering §A.1, the T0 rows of §B/§C/§F, and `.ares`. That is
  roughly 25 formats and covers the realistic double-click distribution.
- **Everything else is a lazily-resolved, sandboxed plugin** behind one interface:
  `sniff(bytes) -> confidence`, `open(stream) -> Scene`, where `Scene` is a small set of primitives
  the renderer knows: `TriMesh`, `PointCloud`, `SplatCloud`, `VolumeGrid`, `Sequence<T>`,
  `CameraRig`.
- **Six renderer paths, not one:** rasterized mesh, point sprite, splat (sorted, alpha-blended),
  volume raymarch, video-textured mesh sequence, and unprojected RGBD. Every format above lands in
  exactly one of them.
- **One shared streaming layer.** `.ares` GOPs, COPC octants, 3D Tiles, LAZ chunks, SPZ 4's
  per-attribute ZSTD streams, SOG's WebP planes and Depthkit video frames are all the same problem:
  indexed, range-readable, decode-on-demand.
- **Metadata without decode.** SPZ 4's plaintext header and glTF 2.1's embedded thumbnails both
  exist so a browser can show a file's identity instantly. Build the inspector/thumbnail path on
  header reads only — it is also what makes the app feel lighter than anything it replaces.
