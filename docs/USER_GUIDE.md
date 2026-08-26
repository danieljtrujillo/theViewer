# User Guide

The complete manual: every control, every view mode, and what the app does with each kind of file.

For the command line, see [CLI.md](CLI.md). For which formats work, see [FORMATS.md](FORMATS.md).
For why the app is built the way it is, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Contents

1. [Opening a file](#1-opening-a-file)
2. [The window](#2-the-window)
3. [View modes](#3-view-modes)
4. [Controls](#4-controls)
5. [Meshes and scenes](#5-meshes-and-scenes)
6. [Point clouds and LiDAR](#6-point-clouds-and-lidar)
7. [Gaussian splats](#7-gaussian-splats)
8. [Volumes](#8-volumes)
9. [Pictures](#9-pictures)
10. [Panoramas and stereo](#10-panoramas-and-stereo)
11. [Video and volumetric video](#11-video-and-volumetric-video)
12. [Playback](#12-playback)
13. [The details panel](#13-the-details-panel)
14. [Shell integration](#14-shell-integration)
15. [Codecs](#15-codecs)
16. [Limits](#16-limits)

---

## 1. Opening a file

Four ways in.

The launcher `Open 3D Viewer.vbs` at the repository root rebuilds the viewer when the sources have
changed and then opens the window. Build output goes to `tools/build.log`. A file path passed to
the launcher is forwarded to the viewer.

`target\release\viewer.exe` runs directly, with or without a file path. A path that is not an
existing file is ignored and the window opens empty.

Dropping a file onto the window opens it. `Ctrl+O` opens the system file picker.

The Windows **Open with** menu lists the viewer once it has been registered. Registration is
offered on first launch, and section 14 covers it.

Opening one file of a DICOM series stacks the whole series in the same folder into a single
volume. Opening an OBJ or an FBX loads its materials and textures from the same folder
automatically.

## 2. The window

Four regions.

The toolbar runs across the top: Open, Fit, then the file name, the identified format, and
whatever controls the current file needs. A confidence word appears beside the format when the
verdict came from something weaker than a magic number or a structural check. `heuristic` means
the format was inferred from content patterns, `extension` means it was inferred from the file
name alone.

The viewport fills the rest of the window.

A right side panel appears for volumes only, carrying the window, level and projection controls.

The playback bar overlays the bottom of the viewport for anything that moves. It fades out about a
third of a second after the pointer leaves the window while playback continues, and stays visible
whenever the file is paused.

The overflow menu at the right end of the toolbar holds the **Open with** registration controls and
the Details toggle.

## 3. View modes

The mode follows the content and is never chosen by hand.

| Mode | Entered by | Left drag |
|---|---|---|
| Orbit | Meshes, point clouds, splats, volumes | Orbit |
| Flat | Photographs, video, animated pictures | Pan |
| Panorama | Equirectangular photographs and video | Look around |

Flat and Panorama are two presentations of the same picture, and the 360 button in the toolbar
switches between them. Everything else is Orbit.

## 4. Controls

The same gestures mean the same things everywhere. What they do follows the content.

| Input | Orbit | Flat | Panorama |
|---|---|---|---|
| Left drag | Orbit | Pan | Look around |
| Right or middle drag | Pan | | |
| Wheel | Zoom, or step slices on a volume in slice mode | Zoom | Field of view, 20 to 110 degrees |
| Double click | | Fit against one to one | |
| `F` | Fit | Fit | Recentre |

Panorama drag is negated relative to orbit. Inside a sphere the content should follow the cursor,
the way dragging a map does. Orbiting a model is the opposite gesture, where the object follows and
the camera does not.

Playback keys, active whenever the file has more than one frame:

| Key | Action |
|---|---|
| `Space` | Play or pause |
| `Left` and `Right` | Step one frame, which also pauses |
| `Home` | Jump to the start |
| `End` | Jump to the end |
| `L` | Loop on or off |

File keys:

| Key | Action |
|---|---|
| `Ctrl+O` | Open |
| Drop a file | Open |

Zoom is multiplicative, so the step feels the same at every scale. Pan scales by camera distance,
so the grab point tracks the cursor.

## 5. Meshes and scenes

Working formats: glTF 2.0 and 2.1 in both binary and JSON spellings, Wavefront OBJ, PLY, STL in
both binary and ASCII, FBX, 3MF, STEP, and OpenUSD.

Meshes render textured and unlit when a base colour texture is present, and shaded when it is not.
Backface culling is off by default, because scans and per-frame reconstructions carry inconsistent
winding and culling them hides half the surface.

Normals are derived when the source carries none. STL has no usable topology at all, so its normals
are always derived.

3MF converts millimetres to metres on import, so a printer file arrives at real scale. STEP is
tessellated through a CAD kernel and arrives without colour, since the format carries none that
this importer reads.

USD reads meshes only. Materials, `PointInstancer` instancing and subdivision surfaces are not
read.

## 6. Point clouds and LiDAR

Working formats: PLY point clouds, PCD in ascii, binary and binary_compressed spellings, LAS, LAZ,
and E57.

Points render as screen-space sprites. A size slider appears in the toolbar for any file that
carries points, running from 1 to 12 pixels.

Georeferenced coordinates are recentred in `f64` before narrowing to `f32`, and the offset is
recorded in the scene metadata where the details panel shows it. A UTM easting has nine significant
digits and `f32` has seven, so without recentring a centimetre step vanishes entirely.

LAS colour is nominally 16-bit, and about half the writers in circulation store 8-bit values
without scaling them. The importer decides the shift from the values themselves rather than
applying it unconditionally, which would render those files almost black.

LAS, LAZ and E57 files are capped at 20,000,000 points. PLY and PCD are read in full.

## 7. Gaussian splats

Working formats: PLY 3DGS from the INRIA reference implementation, SuperSplat and PlayCanvas
compressed PLY, SPZ v1 through v4, and the antimatter15 `.splat` layout.

Splats are sorted back to front every frame and rasterized as projected ellipsoids. Attributes stay
in source precision all the way to the GPU: SPZ int24 positions arrive as int24 and are
dequantized in the vertex shader against the file's own bounding box.

Spherical harmonic coefficients above degree 0 are parsed and stored but never evaluated, so splat
colour is currently view-independent.

The camera frames the raw bounding box, which for a splat capture is often larger than the
subject. `F` refits at any time. SPZ 4's Adobe vendor extension `0xADBE0002` carries safe orbit
bounds that would frame these better; the field is modelled in the scene but no importer reads it
yet.

The depth sort is a CPU radix sort. It is the budget on this path, not decoding.

## 8. Volumes

Working formats: DICOM series, NIfTI including gzip, and NRRD.

Opening one slice of a DICOM series stacks every slice in the folder into one volume. Slice order
comes from `ImagePositionPatient` projected onto the slice normal rather than from
`InstanceNumber`, because the two disagree often enough to matter. Rescale slope and intercept are
applied, so values are in the units the scanner recorded.

Physical voxel spacing is honoured, so anatomy renders at the right aspect ratio.

The right side panel carries the controls.

| Control | Effect |
|---|---|
| Slice against Volume | Single plane, or full raymarch |
| Axi, Cor, Sag | Slice plane orientation, in slice mode |
| Slice | Plane position, 0 to 1 |
| MIP | Maximum intensity projection, in volume mode |
| Threshold | Samples below this contribute nothing, which removes the air around the subject |
| Opacity | Accumulation rate along the ray |
| Window | Full width of the displayed intensity range, in source units |
| Level | Centre of that range, in source units |
| Reset | Restores the defaults while keeping the current mode, axis and projection |

Window and level are presented in the source units the file recorded, not in normalized ones.
DICOM almost always declares a default window, and that is what the panel opens on.

The wheel steps through slices while slice mode is active, which is the console convention.
Everywhere else the wheel zooms.

Samples are converted to normalized 16-bit float on upload. Half precision rather than 8-bit
because CT is 12-bit and MR routinely wider, and baking a window into 8 bits would make the window
control useless.

## 9. Pictures

Decoded in this build: PNG, JPEG, GIF, BMP, WebP, TIFF, Targa, QOI, Windows icon, DDS, Netpbm,
farbfeld, Radiance HDR, and OpenEXR.

Decoded by Windows: HEIF, AVIF, and anything else the platform has a codec for. JPEG 2000 has no
decoder in this build and none in Windows.

EXIF orientation is honoured, so photographs from a phone are the right way up.

Radiance and OpenEXR are tone mapped rather than clipped, so a high dynamic range light probe
shows its range instead of a white rectangle.

Animated GIF, animated WebP and APNG play through the same playback bar as video. GIF and APNG
have no seek structure worth the name, so reaching frame 400 means decoding 400 frames. The whole
animation is decoded up front and held, capped at 4,096 frames or 512 MB, whichever comes first.

Zoom 1.0 is fit rather than one texel per pixel, because fit is what opening a photograph should
do. Double click switches between fit and one to one. Below fit the pan offset resets to zero,
which stops a zoomed-out picture from sitting in a corner. Zoom range is 0.05 to 64.

## 10. Panoramas and stereo

An equirectangular photograph or video is wrapped around the viewer rather than shown flat.
Detection takes the first answer it gets from four sources.

| Source | Evidence |
|---|---|
| XMP `GPano` metadata | Declared by the file |
| MP4 spherical boxes, V1 `uuid` XML and V2 `st3d` or `sv3d` | Declared by the file |
| The file name | Inferred |
| A 2:1 aspect ratio | Guessed |

The last is an explicit guess and the toolbar puts it one click from being corrected. The 360
button toggles between the panorama and the flat presentation in both directions.

`horizontal_fov` is stored as an angle rather than a full or half flag, so VR180 (180 degrees) and
GPano's partial panoramas are both representable.

Over-under and side-by-side stereo pairs show one eye at a time. An `L` or `R` button appears in
the toolbar for any file that carries a pair. Aspect is corrected for the packing, so a 2:1
over-under VR180 frame is treated as the 1:1 half sphere it is.

Matroska spherical metadata is not read. A `.mkv` panorama falls back to the file name and the 2:1
shape.

## 11. Video and volumetric video

| Format | Notes |
|---|---|
| `.mp4` `.mov` `.mkv` `.webm` `.avi` `.wmv` | Decoded by Windows Media Foundation |
| `.ares` | ARES volumetric video: animated geometry plus a video texture track |
| `.mp4` with a Depthkit sidecar | RGBD capture, unprojected to a moving surface |
| `.4ds` | 4DViews HOLOSYS: the container is read and reported, the blocks need a licensed codec |

An MP4 with a Depthkit metadata sidecar beside it is a volumetric capture wearing an ordinary
container. The sniffer cannot know that, because it never looks at the file system, so the question
is asked by the importer, which has a path to ask it about. Both sidecar spellings are accepted:
`<name>.txt` and `<name>_metadata.json`.

Depthkit frames are unprojected per frame using the sidecar's intrinsics. Chroma smear around the
subject is removed by a saturation gate at 0.3, and a hue guard at 0.93 stops the far clip aliasing
onto the near one.

ARES captures carry geometry and a texture track in the same container. Geometry blocks are not
always intra-then-predicted: captures rebuilt per frame are entirely intra, so the decoder
dispatches on each block's own type and seeks to the nearest intra frame at or before the target.

4DViews containers are parsed and reported rather than refused. Someone holding a `.4ds` needs to
know it is a specific number of frames of DXT1 and that the missing piece is a licensed DLL, not a
bug here.

Video plays silent. Audio is not implemented.

## 12. Playback

Anything that moves gets the same three controls, overlaid on the picture: play, a scrub line, and
one timecode reading current position against duration.

The bar fades in when the pointer enters the window and out about a third of a second after it
leaves, while playback continues. A paused file keeps the bar visible.

Clicking or dragging the scrub line seeks. Pressing play at the very end restarts from the
beginning, which is what every other player does.

Decoding runs on its own thread and the clock lives on the UI thread. The player asks for the frame
it needs at the presentation time it has reached, and the source answers with a frame or with
nothing, meaning the frame already on screen is still correct. At 60 Hz display and 30 fps content
about half of all requests get that answer.

Wall-clock deltas are clamped, so a window drag or a display sleep does not jump the clock forward
by seconds.

A decode failure appears as a message above the playback bar rather than as a dialog.

## 13. The details panel

The overflow menu's Details entry opens a panel showing the metadata the file carried: title,
capture rig, colour space, codec, orientation, panorama evidence, coordinate offsets for
georeferenced clouds, and whatever else the format recorded. The entry is disabled for files that
carried none.

The panel is deliberately a dump of key and value pairs rather than a curated set. Every format
carries provenance worth showing, and modelling all of it would be endless.

## 14. Shell integration

On first launch the viewer offers to add itself to the Windows **Open with** menu. The
registration is per-user, under `HKEY_CURRENT_USER`. It changes no existing default and it is
reversible.

51 extensions are registered. The list is explicit rather than derived from the sniffer's
extension table, because it is a promise to the operating system and a format that is merely
recognized should not appear in a menu that implies it opens.

| Action | Where |
|---|---|
| Add | The first-run prompt, the toolbar overflow menu, or `viewer open-with add` |
| Remove | The toolbar overflow menu, or `viewer open-with remove` |
| Check | `viewer open-with status` |

The question is asked at most once per run, only when nothing is registered, and never again after
a refusal. A deferral is worth revisiting once, since someone who said not now on first launch has
since chosen to open the app a second time.

The consent flag lives in a file at `%LOCALAPPDATA%\3D Viewer\shell-prompt` rather than in a
registry value, so clearing it to see the prompt again does not need regedit.

## 15. Codecs

**What plays depends on the machine.** This app ships no codecs. It asks Windows, and Windows
answers for whatever is installed.

| Codec | Availability |
|---|---|
| H.264 | Always present |
| HEVC | Present, but HEIF images additionally need the paid HEVC Video Extensions |
| VP9 | Arrives as a free Microsoft Store extension, often absent |
| AV1 | Arrives as a free Microsoft Store extension, often absent |

When a codec is missing the viewer names it and says which Store extension supplies it, rather
than reporting the file as broken.

HEIF needs two Store packages, not one. The free HEIF Image Extension reads the container, and the
pixels are HEVC and need the separate paid HEVC Video Extensions. A machine with only the first
reports HEIF as supported and then fails with `0xC00D5212`.

Bundling a software decoder instead would add tens of megabytes, lose the hardware path, and drag
in patent obligations, all to decode files the operating system can already decode.

## 16. Limits

Known and deliberate, as of 2026-08-26.

- Audio. Video plays silent.
- `.sog` is detected without an importer. It is a ZIP of `meta.json` plus WebP attribute planes.
  The decode is documented, and no sample exists on the development machine to verify against.
- Matroska spherical metadata is not read. MP4's is.
- Apple spatial video (MV-HEVC) and Insta360 are recognized but not unpacked.
- JPEG 2000 has no decoder in this build and none in Windows.
- Spherical harmonic bands above degree 0 are parsed and stored but never evaluated.
- Attribute data widens to `f32` on upload, and `.ares` positions are dequantized on the CPU. This
  is the largest outstanding deviation from [ARCHITECTURE.md](ARCHITECTURE.md).
- Splat depth sorting is a CPU radix sort rather than a compute shader.
- Volume slice mode draws the plane in 3D space, so it tilts with the camera.
- The NV12 to RGBA conversion is scalar. At 2048 by 2048 it is measurable.
- USD reads meshes only.
- STEP has no colour. Tessellated shells arrive with the default material.
- 4DViews mesh and texture blocks need `BridgeCodec4DS.dll`, which cannot be redistributed.
