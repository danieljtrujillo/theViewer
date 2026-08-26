# Troubleshooting

Symptoms first, with the underlying cause and the fix. Build problems are in
[BUILDING.md](BUILDING.md); this file covers what goes wrong at run time and the traps that cost
real time during development.

## Running

### A system error box appears before the window, only when Explorer launches the app

The executable is depending on `libstdc++-6.dll`. It works when the build script launches it,
because that puts MinGW on PATH first, and fails the moment Explorer launches it from the **Open
with** menu.

```powershell
objdump -p target\release\viewer.exe | Select-String -Pattern "DLL Name"
```

No MinGW DLL should appear. The mechanism that prevents it lives in `crates/formats/build.rs` and
is described under [Self-containment](BUILDING.md#self-containment). A dependency that compiles C++
and links it dynamically will reintroduce this, so the check is worth running after any dependency
change.

### identify, info, shot and open-with print nothing

A release build detaches from the console. Rebuild with the console attached:

```powershell
cargo build --release -p viewer-app --features console
```

Debug builds always keep it.

### The window opens and exits immediately, with no message

Historically this was a WGSL shader compile failure. `target` is a reserved word in WGSL, and using
it as an identifier fails silently at pipeline creation, which surfaces only as the window closing.

Any new shader work should be checked against the reserved word list before anything else.

### A screenshot of the running app is blank

The development machine's desktop does not composite a GPU window into a screen capture. The app is
working; the capture is not. Force-killing the app in this state also leaves ghost pixels that
survive a desktop redraw.

Render offscreen instead:

```powershell
viewer shot input.ext out.png --size 1600x900 --time 2.5
```

`shot` needs no display and no window.

## Video and pictures

### A video opens but shows nothing, and the viewer names a codec

The machine has no decoder for that codec. This app ships none and asks Windows, which answers for
whatever is installed.

| Codec | Where it comes from |
|---|---|
| H.264 | Always present |
| HEVC | Present for video. HEIF images need the separate paid HEVC Video Extensions |
| VP9 | Free Microsoft Store extension, often absent |
| AV1 | Free Microsoft Store extension, often absent |

Installing the named extension from the Microsoft Store resolves it. The file is not broken.

### A HEIC photograph reports as supported and then fails with 0xC00D5212

HEIF needs two Store packages, not one. The free **HEIF Image Extension** reads the container. The
pixels are HEVC and need the separate paid **HEVC Video Extensions**. A machine with only the first
reports HEIF as supported and then fails at decode.

### Every video is mirrored vertically

Media Foundation's RGB32 output is bottom-up, reported as a negative `MF_MT_DEFAULT_STRIDE`.
Ignoring the sign flips every frame. This is fixed in `crates/media/src/mf.rs` and is recorded here
because it is the first thing to suspect if the symptom returns.

### A video decodes for a while and then stops producing frames

A decoder MFT that does not set `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` needs the caller to allocate
the output sample. Passing a null one is not an error: `ProcessOutput` succeeds, produces nothing,
and the picture never arrives.

### JPEG 2000 will not open

Neither this build nor Windows has a decoder. The format is identified so the message can say so.

## Panoramas

### A flat photograph is shown wrapped around the viewer

Detection fell through to the 2:1 aspect ratio guess. The toolbar shows the file as a panorama and
the **360** button switches it back to flat in one click. The details panel reports which evidence
was used.

### A panorama is shown flat

Nothing in the file declared it and the name and shape gave no signal. The **360** button turns it
on. A panorama switched on this way is treated as a full 360-degree turn; VR180 files that declared
themselves arrive with their own arc already set.

### A .mkv panorama is not detected

Matroska spherical metadata is not read. MP4's is. The file name and the 2:1 shape are the only
signals available, and the 360 button covers the rest.

## Geometry and point clouds

### A LiDAR cloud renders almost black

LAS colour is nominally 16-bit and roughly half the writers in circulation store 8-bit values
without scaling them. The importer decides the shift from the values rather than applying it
unconditionally. A file that still renders dark is worth reporting with its writer software
recorded in the header.

### A georeferenced cloud looks flat, or collapses to a plane

Coordinates were not recentred before narrowing to `f32`. A UTM easting has nine significant digits
and `f32` has seven, so a centimetre step vanishes entirely. Both LiDAR readers subtract the
cloud's centre in `f64` and record the offset in the scene metadata, where the details panel shows
it.

### A PCD binary_compressed cloud is a plausible-looking cloud of nonsense

`binary_compressed` is field-major. Decompressing without un-transposing produces exactly that.

### A DICOM series is stacked in the wrong order

Slice order comes from `ImagePositionPatient` projected onto the slice normal, not from
`InstanceNumber`. The two disagree often enough that using `InstanceNumber` is the bug rather than
the fix.

### DICOM pixel values look wrong

PixelData is usually encapsulated, so a raw `to_bytes()` fails. Decoding goes through
`dicom-pixeldata`'s `decode_pixel_data()`, which also applies rescale slope and intercept, so
values come out in the units the scanner recorded.

## Volumetric

### An .ares texture track decodes no pictures at all

Texture frame ranges are offsets within their block, not within the file. Slicing the whole file
with them feeds the decoder container bytes, which it accepts frame after frame without ever
emitting a picture. The symptom is indistinguishable from a missing codec until the offsets are
checked.

### An .ares capture plays geometry but nothing else, or the reverse

`viewer info` counts them separately for this reason:

```
decode       8 of 8 frames, 8 with geometry, 8 with a picture, 2048x2048 px, 0.9 x 1.8 x 0.7 m
```

A capture that decodes all its geometry and none of its texture is a codec problem. One that
decodes neither is a container problem.

### A .4ds file reports its container and stops

The mesh and DXT1 texture blocks need `BridgeCodec4DS.dll`, a licensed 4DViews component that
cannot be redistributed. The container is parsed and reported so the file's shape is visible.

### A .sog file is identified but will not open

There is no importer. It is a ZIP of `meta.json` plus WebP attribute planes, the decode is
documented, and no sample exists on the development machine to verify an implementation against.

### Depthkit output has a halo of smeared colour

Chroma smear is not depth. A saturation gate at 0.3 removes it, and a hue guard at 0.93 stops the
far clip aliasing onto the near one. Both thresholds are in `crates/formats/src/depthkit.rs`.

## Splats

### Splat colour does not change with the viewpoint

Spherical harmonic bands above degree 0 are parsed and stored but never evaluated. Colour is
view-independent today.

### An SPZ v4 file is identified as v1 to v3

The version dispatch ran in the wrong order. In v1 to v3 the `NGSP` magic sits inside the gzip
stream, so a file whose first bytes are literally `NGSP` is v4 or newer, and the plaintext test has
to run before the gzip test.

### A splat capture opens with the camera far away

The raw bounding box of a splat capture usually extends well past the subject, and the raw box is
what the camera frames today. `F` refits at any time. SPZ 4's Adobe vendor extension `0xADBE0002`
carries safe orbit bounds intended for exactly this, and reading it is not implemented.

## Shell integration

### The Open with entry does not appear

```powershell
viewer open-with status
```

Reports whether the registration exists, what consent was recorded, where the consent flag lives,
and how many extensions are covered. `viewer open-with add` registers, `remove` unregisters. Both
are per-user and change no existing default.

### The first-run prompt never appears again

It is asked at most once per run, only when nothing is registered, and never again after a refusal.
Delete `%LOCALAPPDATA%\3D Viewer\shell-prompt` to see it again. The flag is a file rather than a
registry value for exactly this reason.
