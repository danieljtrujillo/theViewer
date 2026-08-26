# Architecture

Decisions taken 2026-08-25. Companion to [FORMATS.md](FORMATS.md), which is the format survey
this design has to satisfy.

## The two decisions

**Runtime: Rust + wgpu, native.** One binary, no webview, no Chromium. wgpu targets Vulkan on
Windows/Linux, DX12 on Windows and Metal on macOS from a single codebase, and gives real compute
shaders — which the splat sorter and the volume raymarcher both need. Rust's memory safety is not
incidental here: FBX was disabled in Microsoft's own 3D Viewer over a parser CVE and never
re-enabled, and this app is fundamentally *a large collection of parsers for untrusted binary
files*. That is the single biggest risk in the project and the language choice is the first line
of defence against it.

**v1 scope: all four families.** Parity set + splats, `.ares` + volumetric video, point clouds +
LiDAR, and volumes. That means **all six renderer paths ship in v1**. The upside is that the
streaming layer is genuinely shared — ARES GOPs, COPC octants, LAZ chunks and DICOM series are the
same indexed-range-read problem — so building them together avoids retrofitting. The cost is
concentrated in one place: volumes are the only family that needs a subsystem (raymarching) that
nothing else requires.

## The keystone: adopt glTF's data model, don't invent one

The single most consequential decision. The internal scene representation is
**glTF 2.1 semantics + `KHR_gaussian_splatting` attribute semantics**, not a neutral model of our
own design.

Why:

- Every format in FORMATS.md becomes a *thin importer* rather than a translation into an invented
  vocabulary.
- Export is nearly free, and glTF is the one format everything else can read.
- glTF is where the industry is converging for both splats and volumetric video — Khronos'
  Volumetric Media Subgroup, and the VFA's May 2026 commitment to adopt glTF as its primary
  format. Aligning now means aligning with wherever volcap standardizes next.

Concretely this means an **accessor-shaped attribute model**: buffers carry a component type and a
normalization flag, and data stays in whatever precision it arrived in. That is not just spec
fidelity — it is the memory posture. A `u16`-quantized position array stays `u16` from file to GPU
buffer, and the vertex shader dequantizes against the AABB. This is exactly what `.ares` already
does, and it is why `.ares` fits this design without adaptation.

`.ares` is **not** a competing scene model. Its GOP / track / chunk machinery is the *streaming
layer*; the geometry it yields lands in the same `Scene` as everything else.

## Crate layout

```
crates/
  scene/     Scene, Node, Payload, attribute buffers, AABB. No dependencies.
             Everything imports into this. The stable centre of the design.
  io/        Sniffing (magic bytes, not extensions), memory-mapped reads,
             range-read + chunk-index abstraction shared by every streaming format.
  formats/   T0 parsers, statically linked. glTF/GLB, OBJ, PLY (4-way), STL, 3MF,
             FBX (ufbx), SPZ v1-v4, .splat, SOG, .ares, PCD/XYZ, NRRD/NIfTI.
  render/    wgpu. Six paths: mesh raster, point sprite, sorted splat,
             volume raymarch, video-textured sequence, unprojected RGBD.
  app/       Binary: window, input, camera, UI, file association.
plugins/     T1+ formats, resolved lazily. Empty for now.
```

Five crates, not fifteen. A format earns its own crate when it drags in a dependency tree the core
should not carry — that boundary *is* the T0/T1 line from FORMATS.md, so it will assert itself
naturally rather than needing to be guessed up front.

## Sniffing, not extensions

`io::sniff` returns ranked candidates from a header read, never from the file name. The cases that
force this design:

| Ambiguity | Resolution |
|---|---|
| `.ply` is 4 formats | Parse the ASCII header; dispatch on the property list — `f_dc_*` + `scale_*` + `rot_*` means 3DGS, an element named `chunk` means SuperSplat-compressed, faces mean mesh, none of the above means point cloud |
| `.spz` is 2 encodings | Plaintext `NGSP` at offset 0 means v4+; gzip magic `1f 8b` means v1–v3, whose `NGSP` sits *inside* the compressed stream. **Check v4 first** — getting this backwards fails on the most common splat format in circulation |
| `.mp4` is 5 things | Plain video, Depthkit CPP, Microsoft HoloVideo, V3C/V-PCC, or MV-HEVC spatial. Needs box-level inspection plus sidecar detection |
| `.splat` has no header | 32 bytes/splat, no magic at all. `len % 32 == 0` plus value-range plausibility, and only as a low-confidence fallback |
| `.glb` has two widths | Version field distinguishes standard from the glTF 2.1 64-bit variant |

Confidence is ranked rather than boolean so that a weak structural guess (`.splat`) never
outranks a real magic-byte match.

## Six renderer paths

Every payload type maps to exactly one:

| Path | Feeds from |
|---|---|
| Mesh raster | TriMesh — the parity set, CAD tessellation, volcap frames |
| Point sprite | PointCloud — PLY/PCD/XYZ, LAS/LAZ/COPC, E57, COLMAP |
| Sorted splat | SplatCloud — every §C format. GPU radix sort, back-to-front, per frame |
| Volume raymarch | VolumeGrid — DICOM, NIfTI, NRRD, NanoVDB, VDB |
| Video-textured sequence | Sequence&lt;TriMesh&gt; + video track — `.ares`, UVOL, per-frame folders |
| Unprojected RGBD | Depthkit CPP, generic RGBD video, Azure Kinect |

## Performance posture

Four rules, each traceable to a measurement in FORMATS.md:

1. **Quantized end to end.** Positions stay `u16`/`int24` from file to GPU; the vertex shader
   dequantizes against the AABB. Never widen to `f32` on the CPU just to narrow again.
2. **Sorting is the splat budget, not decoding.** SPZ 4 removed the 10 M-point cap and targets
   34 M+. A CPU sort at that scale is not viable — GPU radix, with frame-to-frame order reuse
   under small camera deltas.
3. **Header reads for identity.** SPZ 4's plaintext header and glTF 2.1's embedded thumbnails both
   exist precisely so a browser can show a file's identity without decoding it. The inspector and
   thumbnail paths read headers only.
4. **A folder can be one object.** A 544-file volcap sequence opens as a single playable item and
   must never hold 544 file handles open.

## Security posture

The FBX CVE is the design precedent. Every parser that is not ours runs sandboxed with a hard
memory ceiling and no filesystem or network reach; T2 plugins (Assimp, CAD kernels, video stacks)
are out-of-process. Bounds and allocation limits are enforced *before* a length field from a file
is trusted — a corrupt `vertex_count` must fail, not allocate.

---

# Addendum, 2026-08-26: pictures, moving pictures, and the shell

The decisions below were taken when image, video, 360 and volumetric-video support landed. They
extend the design above rather than revising it.

## A picture is not geometry

The tempting shortcut is to import an image as a textured quad and let the orbit camera handle it.
It is a bad viewer. A photograph opened that way arrives somewhere in a 3D scene at an arbitrary
size and has to be *hunted for*, and a 360 panorama arrives inside-out.

So the scene model gained a fifth payload, `Screen`, which carries pixels plus a **projection**
(flat or equirectangular) and a **stereo layout**. It maps to two new renderer paths, taking the
count from six to seven and eight:

| Path | Feeds from |
|---|---|
| Flat picture | `Screen` with `Flat` — a screen-space rectangle, fitted, pannable, zoomable |
| Panorama | `Screen` with `Equirect` — a full-screen triangle; each fragment turns its own direction into a texture coordinate |

The panorama is computed per fragment rather than drawn on a sphere mesh on purpose: a tessellated
sphere kinks visibly at the poles unless it is absurdly dense, and the per-fragment form is both
exact and cheaper.

The viewport's gestures are then **re-bound per mode** — drag orbits a model, pans a photograph,
and looks around a panorama — while staying in the same places. Same hand, different content.

## Three sources of evidence for "is this a panorama?"

Metadata, then the file name, then the aspect ratio, and the viewer **says which one it used**.
That ranking matters because the three are not equally good: XMP `GPano` and the MP4 `st3d`/`sv3d`
boxes are statements of fact by the capture rig; `_360` in a name is a convention; and 2:1 is
merely a shape that panoramas happen to share with book scans and banners. The shape guess is
gated on a resolution floor and, like every guess here, is one toolbar click from being corrected.

The same principle governs Depthkit detection, which cannot be a sniffer verdict at all: the
container is an ordinary MP4 and the only evidence is the sidecar sitting next to it. Questions
about the file system are asked by importers, which have a path; the sniffer never looks past the
header bytes it was handed.

## Video: the platform decodes, we do not

**Windows Media Foundation**, through `IMFSourceReader` for files and `IMFTransform` for the bare
frame streams inside `.ares`. This ships no codecs and never will. Bundling a software decoder
would add tens of megabytes, lose the hardware path, and drag in patent obligations, all to decode
files the operating system already decodes.

The consequence is stated plainly in the UI rather than hidden: **what plays depends on the
machine.** H.264 and HEVC are always present; VP9 and AV1 arrive as Store extensions and are
frequently absent. A missing codec produces a sentence naming it, not a failure code.

## Pull, not push

Everything that moves — MP4, animated GIF, `.ares`, Depthkit — implements one trait,
`FrameSource`, with one method: *what does the scene look like at this presentation time?* The
player owns the clock and asks; the decoder knows nothing about time.

That is what makes scrubbing, pausing and stepping work identically across four unrelated
formats, and it is why the playback bar has one implementation. The decode runs on its own thread
because a hardware video decode takes several milliseconds and a slow one takes twenty, neither of
which is an acceptable pause at 60 Hz. One request is outstanding at a time, and a scrub that
outruns the decoder **coalesces** into "decode the newest position" rather than queueing every
position it passed through.

## The executable must be self-contained

Not an aesthetic preference. Registering in the **Open with** menu means Explorer launches
`viewer.exe` directly, with nothing but the system directories on PATH. A binary that needs
`libstdc++-6.dll` beside the compiler works when the build script launches it and fails with a
system error box when a user double-clicks a file — the worst possible split, because it passes
every test performed by the person who built it.

Two dependencies compile C++ behind DICOM's pixel decoding, so the C++ runtime is linked
statically, in `crates/formats/build.rs`. Getting there required understanding that `rustc` puts
build-script link *arguments* after its own runtime libraries, so the C++ runtime and everything
it needs have to be appended as a `--start-group` at the very end of the line. The same reasoning
retired the `meshopt` C++ bindings in favour of a port of its scalar decoder: one C++ dependency
was worth solving, a second was not.

## Shell integration, and its limits

Everything written to the registry lives under `HKEY_CURRENT_USER`, needs no elevation, and can be
removed by the same app that added it. The registration *appends* to each extension's
`OpenWithProgids` list and changes no existing default — Windows decides defaults through its own
Settings UI, and an application that routes around that is behaving like malware whatever its
intent. Consent is asked once, on first run, with a "never ask again" that is honoured.

## What this cost, and what it still owes

- Dynamic meshes and dynamic textures widen to `f32` and RGBA8 on the CPU, which is the same
  deviation from the quantized-end-to-end posture that the static paths already have. `.ares`
  positions arrive `u16`-quantized and are dequantized on the CPU rather than in the vertex
  shader; fixing it fixes both at once.
- The NV12 to RGBA conversion is scalar. At 2048² it is measurable.
- Audio is not decoded. Video plays silent.
