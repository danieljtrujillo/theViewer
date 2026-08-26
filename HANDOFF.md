# Handoff

State as of 2026-08-26. **323 tests passing, clippy clean.** The last release build was 13.3 MB and
self-contained; the formats added since then have not been release-built yet (see *Next steps*).

## Run it

Double-click **`Open 3D Viewer.vbs`**. It rebuilds when sources changed, then opens the window.
Build output goes to `tools/build.log`.

The app is currently **registered in the Windows Open with menu** for `target\release\viewer.exe`,
because the first-run prompt was accepted. `viewer open-with remove` undoes it;
`viewer open-with status` reports it.

## Toolchain, and why it is unusual

The build targets `x86_64-pc-windows-gnu`, pinned in `rust-toolchain.toml`. Visual Studio 2026 is
installed with the MSVC compiler but **no Windows 10/11 SDK**, so the `msvc` target cannot link.
The `gnu` target needs `dlltool` from MinGW binutils, which rustup does not ship, so WinLibs is
installed at:

```
%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.MSVCRT_*\mingw64\bin
```

**Any `cargo` invocation needs that directory on PATH**, because several dependencies compile C and
C++. `tools/build-and-run.ps1` finds it automatically. From a shell:

```
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.MSVCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin;$env:Path"
```

MinGW is needed to **build**. The resulting `viewer.exe` needs nothing but Windows. See trap 9.

Release builds detach from the console. `--features console` keeps it attached, which the
`identify`, `info`, `shot` and `open-with` commands need.

## What works

| Format | Extensions | Verified against |
|---|---|---|
| glTF 2.0 / 2.1 | `.glb` `.gltf` | corpus |
| Wavefront OBJ | `.obj` | corpus, ARES bake output |
| PLY: mesh, points, 3DGS, SuperSplat | `.ply` | corpus |
| STL binary and ASCII | `.stl` | corpus |
| SPZ v1 to v4 | `.spz` | not yet |
| FBX | `.fbx` | `D:\3d\logo2.fbx`, a Unity asset |
| **3MF** | `.3mf` | `D:\T-Rex.3mf`, 13,814 triangles, mm to m correct |
| **STEP** | `.step` `.stp` | `D:\New folder\...\200113-waltzWheel (1).step`, 2 shells, 95,401 triangles |
| **OpenUSD** | `.usda` `.usdc` `.usdz` `.usd` | synthetic stages: composition, units, up-axis, transforms |
| **`.splat`** | `.splat` | synthetic |
| **PCD** | `.pcd` | synthetic: ascii, binary, binary_compressed |
| **LAS / LAZ** | `.las` `.laz` | synthetic |
| **E57** | `.e57` | synthetic |
| DICOM series | `.dcm` | a 132-slice MR series |
| NIfTI, NRRD | `.nii` `.nii.gz` `.nrrd` | not yet |
| Pictures | 13 raster formats | real photographs |
| **HEIF / AVIF / anything WIC knows** | `.heic` `.avif` and the rest | `D:\Pictures\IMG_2998.HEIC`, correctly reports the missing HEVC codec |
| 360 photos and video | any of the above | corpus panoramas |
| Video | `.mp4` `.mov` `.mkv` `.webm` `.avi` `.wmv` | H.264 phone video; **VP9 `.mkv` 1080p** |
| ARES volumetric | `.ares` | `daniel-coherent-v4.ares`, geometry plus VP9 texture |
| Depthkit RGBD | `.mp4` + sidecar | a real capture, 458 frames |
| 4DViews | `.4ds` | container reported; blocks need a licensed DLL |

Renderer paths: mesh raster (textured, unlit when textured, dynamic per-frame geometry), point
sprite, sorted splat, volume raymarch, flat picture, panorama.

## How to check things without a display

`viewer shot <in> <out.png> [--size WxH] [--time SECONDS]` renders one frame with no window.
**This machine's desktop does not composite a GPU window into a screen capture**. Screenshots of
the running app come back blank even when it is working, and force-killing it leaves ghost pixels
that survive a desktop redraw. Every visual claim above was checked with `shot`.

`viewer info` imports, then decodes the opening frames and reports how many carried geometry, how
many carried a picture, and the decoded extent.

## Next steps, in order

1. **Release build and a real-file sweep.** Everything since the last release build is debug-tested
   only. `cargo build --release -p viewer-app`, check the binary still has no MinGW DLL imports
   (`objdump -p target/release/viewer.exe | grep -i "DLL Name"`), and re-run the table above.
2. **Binary size.** STEP pulled in `truck` (a CAD kernel) and USD pulled in `openusd`; both are
   large. Measure, and if the binary has grown past ~25 MB consider moving one or both behind the
   T1 plugin boundary that `docs/ARCHITECTURE.md` already describes.
3. **Two copies of `zip`.** `openusd` wants 8.x and this workspace pinned 4.x. Bump ours to 8 so
   only one is compiled.
4. **`.sog`** is the last geometry format still detected without an importer. It is a ZIP of
   `meta.json` plus WebP attribute planes; the decode is documented but there is **no sample on
   this machine to verify against**, which is why it was not attempted blind.
5. **Audio.** Video plays silent. WASAPI output plus A/V sync against `Player`'s existing clock.
6. Update `README.md` and `docs/FORMATS.md`, which still describe the state before this batch.

## Not done

- **Audio.** Video plays silent.
- **`.sog`**, see above.
- **Matroska spherical metadata** is not read; MP4's is. A `.mkv` panorama falls back to the file
  name and the 2:1 shape.
- **Apple spatial video (MV-HEVC)** and **Insta360** are recognized but not unpacked.
- **JPEG 2000**. Neither this build nor Windows has a decoder.
- Spherical harmonic bands above degree 0 are parsed and stored but never evaluated, so splat
  colour is view-independent.
- Attribute data widens to `f32` on upload, and `.ares` positions are dequantized on the CPU. The
  largest outstanding deviation from `docs/ARCHITECTURE.md`; the two halves are one fix.
- Splat depth sorting is a CPU radix sort, not a compute shader.
- Volume slice mode draws the plane in 3D space, so it tilts with the camera.
- The NV12 to RGBA conversion is scalar; at 2048² it is measurable.
- USD reads meshes only: no materials, no instancing through `PointInstancer`, no subdivision.
- STEP has no colour: the tessellated shells arrive with the default material.

## Traps worth remembering

1. **`target` is a reserved word in WGSL.** A silent shader compile failure that showed up only as
   the window exiting with no message.
2. **Bind group indices are per shader module.** The mesh, splat, volume and screen shaders are
   separate files for this reason.
3. **DICOM PixelData is usually encapsulated**, so `to_bytes()` fails. It goes through
   `dicom-pixeldata`'s `decode_pixel_data()`, which also applies rescale slope and intercept.
4. **DICOM slice order comes from `ImagePositionPatient` projected on the slice normal**, not
   `InstanceNumber`.
5. **Sniff PLY by truncating at `end_header` before decoding UTF-8.**
6. **SPZ v4 must be tested before the gzip check.**
7. **Container magics rank one level below a specific verdict.**
8. **`dicom-pixeldata` default features pull rav1e and ravif.** Pinned to
   `native,rle,jpeg,deflate,openjp2,charls`.
9. **The executable must not need `libstdc++-6.dll`.** It works when the build script launches it,
   because that puts MinGW on PATH, and fails with a system error box the moment Explorer launches
   it from the Open with menu. Fixed in `crates/formats/build.rs`, which `crates/app/build.rs`
   `include!`s. The reasoning is not obvious: `rustc` places build-script link *arguments* after
   its own runtime libraries, so the C++ runtime and everything it needs have to be appended as one
   `--start-group` at the end of the line. This is also why the `meshopt` C++ bindings were
   replaced by a port. See trap 16.
10. **A wgpu pipeline and its render pass must agree on the depth attachment.** The picture paths
    write no depth and test none, but still declare a depth state.
11. **Buffers a player rewrites need `COPY_DST` from the start.**
12. **Media Foundation's RGB32 output is bottom-up**, reported as a negative
    `MF_MT_DEFAULT_STRIDE`. Ignoring the sign mirrors every video vertically.
13. **A decoder MFT that does not set `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` needs the caller to
    allocate the output sample.** Passing a null one is not an error: `ProcessOutput` succeeds,
    produces nothing, and the picture never arrives.
14. **`.ares` texture frame ranges are offsets within their block.** Slicing the whole file with
    them feeds the decoder container bytes, which it accepts frame after frame without ever
    emitting a picture, indistinguishable from a missing codec until the offsets are checked.
15. **`.ares` geometry blocks are not always I-then-P.** Captures rebuilt per frame are entirely
    intra; the decoder dispatches on each block's own type and seeks to the nearest intra frame at
    or before the target.
16. **meshopt's vertex codec is at version 1 in every real `.ares` file.** `meshopt-rs` implements
    version 0 only. `crates/formats/src/meshopt.rs` is a port of the scalar decoder for both.
17. **Depthkit's chroma smear is not depth.** A saturation gate at 0.3 removes it; a hue guard at
    0.93 stops the far clip aliasing onto the near one.
18. **A `while let Ok(Pattern) = rx.try_recv()` loop silently eats messages that do not match.** In
    the player's coalescing loop that meant swallowing the stop message and hanging the join.
19. **HEIF needs two Store packages, not one.** The free *HEIF Image Extension* reads the container;
    the pixels are HEVC and need the separate paid *HEVC Video Extensions*. A machine with only the
    first reports HEIF as supported and then fails with `0xC00D5212`.
20. **A raw string containing `"#` ends an `r#"..."#` literal.** 3MF test fixtures embed
    `displaycolor="#FF0000"`, so they need `r##"..."##`.
21. **`las` 0.11 is column-oriented.** `read_points(n)` returns a `PointData` whose `x()`, `y()`,
    `rgb()` are iterators borrowing it; they must be collected before the next read.
22. **LAS colour is 16-bit but half the writers store 8-bit values unscaled.** Shifting
    unconditionally renders those files almost black, so the shift is decided from the values.
23. **PCD `binary_compressed` is field-major.** Decompressing without un-transposing yields a
    plausible-looking cloud of nonsense.
24. **Georeferenced coordinates must be recentred before narrowing to `f32`.** A UTM easting has
    nine significant digits and `f32` has seven, so a centimetre step vanishes entirely. Both LiDAR
    readers subtract the cloud's centre in `f64` and record the offset in the scene metadata.

## Layout

```
crates/scene/     Scene model, materials, images, screens, frames.
crates/io/        Magic-byte sniffing, memory-mapped reads.
crates/formats/   Importers, sidecar and texture resolution, meshopt, ARES, Depthkit, STEP, USD.
crates/media/     Windows Media Foundation and WIC. Ships no codecs.
crates/render/    wgpu. mesh.wgsl, splat.wgsl, volume.wgsl, screen.wgsl and their Rust sides.
crates/app/       Window, viewport, controls, playback, shell integration, command line.
```

`docs/FORMATS.md` is the format survey. `docs/ARCHITECTURE.md` records the decisions.

## Prior art borrowed

Volume viewer conventions come from the author's own WebGL2 viewer in
`C:\Users\Cyboman\Documents\Dev\Josh Med\Josh Med\medsuite\app\viewer.js`. Credited in
`crates/render/src/volume.wgsl`.

The `.ares` container reader follows the author's own reference implementation in
`G:\StableAudio\4 - Volumetric Interop\ares\packages\core\src`; the Depthkit decode gates come from
that repository's `docs/rgbd-rebuild-pipeline.md`. Both are credited in the files that use them. No
capture content from either set was read, rendered or reproduced. The Depthkit capture set is
under NDA and was verified by measurement only.

The meshopt vertex decoder is a port of the scalar path in `vertexcodec.cpp` from meshoptimizer
(MIT, Arseny Kapoulkine). Credited in `crates/formats/src/meshopt.rs`.
