# Command Line

```
viewer                           open the window
viewer <path>                    open the window on a file
viewer identify <path>...        print format identification
viewer identify --dir <folder>   identify every file in a folder
viewer info <path>...            identify, import, and print scene contents
viewer shot <in> <out.png>       render one frame to a PNG, with no window
                                 [--size WxH] [--time SECONDS]
viewer open-with <action>        add, remove or check the Open with entry
viewer --help                    print the above
```

Release builds detach from the console so a double-click does not flash a terminal. Every
subcommand here prints to the console, so a release build that runs them needs the console
attached:

```powershell
cargo build --release -p viewer-app --features console
```

Debug builds always keep the console. Every example below was captured from
`target\debug\viewer.exe` against the generated corpus.

## identify

Reads header bytes and decodes nothing, which makes it safe to run against confidential captures.
Every candidate is printed, ranked, with the reason it was produced.

```
> viewer identify corpus\splats.ply corpus\cube.glb corpus\panorama360.png

cube.glb
    1.6 KB
    -> glTF binary                        magic
       GLB magic; check the version field for the 64-bit variant

panorama360.png
    1.5 MB
    -> PNG image                          magic
       image magic number

splats.ply
    3.9 MB
    -> PLY Gaussian splats                magic
       PLY header declares f_dc_*, scale_* and rot_*

3 files, 0 unrecognized
```

The `->` marker is the leading candidate. `no importer` appears beside any format that is
recognized without being openable. The word in the second column is the confidence.

| Confidence | Meaning |
|---|---|
| `magic` | An unambiguous magic number. |
| `structural` | Declared sizes agree with the actual file length. |
| `heuristic` | A content-shaped guess: text that looks like OBJ, a length divisible by a record size. |
| `extension` | The file name is all there was. Never enough to commit to a parser. |

Paths are collected, sorted, then reported. `--dir <folder>` adds every file in a folder, and it
can be repeated and mixed with individual paths. A per-run total is printed when more than one file
was given.

| Situation | Exit code |
|---|---|
| Any number of files reported | 0 |
| No files given | 2 |
| A folder passed to `--dir` could not be read | 1 |

An unrecognized file is not a failure. `identify` reports `unrecognized` and moves on.

## info

Identifies, then actually imports, then decodes the opening frames of anything that moves.

```
> viewer info corpus\panorama360.png corpus\splats.ply corpus\textured.obj

panorama360.png
    PNG image                        1.5 MB
    format       PNG image
    pixels       4096 x 2048
    projection   equirectangular 360°
    projection source declared by the file
    bounds       -1.000 -1.000 -1.000 to 1.000 1.000 1.000
    import       1426.9 ms

splats.ply
    PLY Gaussian splats              3.9 MB
    format       PLY
    encoding     binary_little_endian
    splats       60000
    bounds       -1.360 -0.360 -1.360 to 1.360 0.360 1.360
    import       257.8 ms

textured.obj
    Wavefront OBJ                    260 B
    format       OBJ
    triangles    4
    vertices     8
    bounds       -1.200 -1.000 0.000 to 1.300 1.000 0.000
    import       14.2 ms
```

The metadata block comes first because it is what someone diagnosing a file actually wants: the
codec, the frame rate, the projection, the orientation tag a phone wrote, the evidence a panorama
was detected from. Derived counts follow.

Timings above are from a debug build. A release build is several times faster.

### The decode probe

Opening a video proves the container parsed. Only decoding proves the machine has a codec for it,
and that is exactly what varies from machine to machine, so `info` decodes eight frames rather
than reporting a successful open and leaving the real question unanswered.

```
    decode       8 of 8 frames, 8 with geometry, 8 with a picture, 2048x2048 px, 0.9 x 1.8 x 0.7 m
```

Geometry and pictures are counted separately because a volumetric capture can decode all its
geometry and none of its texture, and that case is worth seeing at a glance. The extent reported is
the decoded extent, which is the number a wrong unprojection gets wrong. The scene's own bounds are
whatever the importer declared before any frame arrived, and for an RGBD capture that is the camera
frustum rather than the subject.

A decode error is reported and counts as a failure:

```
    decode       no decoder installed for AV1; install the matching Video Extension from the Microsoft Store
```

| Situation | Exit code |
|---|---|
| Every file imported and decoded | 0 |
| Any file failed to open, import, or decode | 1 |
| No files given | 2 |

## shot

Renders one frame to a PNG with no window and no surface.

```
> viewer shot corpus\splats.ply out.png --size 1280x720
out.png  1280x720
```

| Option | Default | Meaning |
|---|---|---|
| `--size WxH` | `1280x720` | Output resolution. Accepts `x` or `X`. Clamped to 16 through 16,384 on each axis. |
| `--time SECONDS` | `0` | Presentation time to render, for anything that moves. Clamped at zero. |

The camera frames the scene bounds automatically. A panorama is rendered looking straight ahead
instead, since it has no outside to orbit and its centre is the view its author composed.

Moving files are decoded forward in presentation order up to `--time`, because that is the only
access every frame source supports. A seekable source skips ahead internally on the first call.

This is the only way to check what the renderer produced on a machine whose desktop does not
composite a GPU window into a screen capture, which is the situation on any build agent and on the
machine this project was written on. Screenshots of the running app come back blank there even when
it is working.

| Situation | Exit code |
|---|---|
| Rendered | 0 |
| Import, device or write failure | 1 |
| Missing or malformed arguments | 2 |

## open-with

Manages the Windows **Open with** entry. The same registration the first-run prompt offers,
scriptable and testable.

```
> viewer open-with status
registered for the current user
consent    Granted
state      C:\Users\<user>\AppData\Local\3D Viewer\shell-prompt
types      51 extensions
```

| Action | Effect |
|---|---|
| `status` | Reports registration, recorded consent, the consent file path, and the extension count. The default when no action is given. |
| `add` | Registers for the current user and records consent as granted. |
| `remove` | Unregisters and records consent as deferred, so the prompt can appear again. |

Registration is per-user, under `HKEY_CURRENT_USER`. It changes no existing default.

Recorded consent is one of `Unasked`, `Deferred`, `Refused`, `Granted`. Deleting the state file
returns it to `Unasked`, which is how to see the first-run prompt again.

| Situation | Exit code |
|---|---|
| Action succeeded | 0 |
| Registry write failed | 1 |
| Unknown action | 2 |

## Opening the window

```
viewer
viewer path\to\file.glb
```

A path that is not an existing file is ignored and the window opens empty. This matters because
Explorer passes arguments the app has no other way to reject.
