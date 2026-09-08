# Building

Windows only, targeting `x86_64-pc-windows-gnu`. The resulting executable needs nothing but
Windows, which is what lets Explorer launch it.

## Prerequisites

| Tool | Role |
|---|---|
| [rustup](https://rustup.rs) | Toolchain. `rust-toolchain.toml` pins the channel and the target. |
| [WinLibs MinGW](https://winlibs.com) | Supplies `dlltool`, which rustup does not ship, and the C and C++ compilers several dependencies need. |
| [Node.js](https://nodejs.org) | Optional. Generates the test corpus. Nothing in the build needs it. |

```powershell
winget install Rustlang.Rustup
winget install BrechtSanders.WinLibs.POSIX.MSVCRT --scope user
rustup toolchain install stable-x86_64-pc-windows-gnu
```

## Why windows-gnu

Visual Studio on the development machine has the MSVC compiler but no Windows 10 or 11 SDK, so the
`msvc` target cannot link. The `gnu` target is self-contained apart from `dlltool`, which is why
MinGW is a prerequisite.

The choice is recorded in `rust-toolchain.toml` and switching back is a two-step change: install a
Windows SDK, then edit that file. Nothing else in the workspace assumes `gnu`, except
`crates/formats/build.rs`, which is guarded on the target environment and does nothing under
`msvc`.

That file names a full toolchain, `stable-x86_64-pc-windows-gnu`, rather than a bare channel. The
tuple is load-bearing. A gnu *host* cannot be selected any other way, and the alternatives
(`targets = [...]` plus `--target`, or `[build] target` in `.cargo/config.toml`) move the output
to `target\x86_64-pc-windows-gnu\release\`, which breaks the launcher, the docs and the existing
**Open with** registration.

Some rustup versions reject a target tuple in the `channel` field with
`error: target tuple in channel name`. rustup 1.29.0 accepts it. CI works around it by setting
`RUSTUP_TOOLCHAIN`, which names the toolchain directly and stops rustup reading the file at all.

## PATH

Any `cargo` invocation needs the MinGW `bin` directory on PATH, because several dependencies
compile C and C++.

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.MSVCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin;$env:Path"
```

`tools\build-and-run.ps1` finds it on its own by searching
`%LOCALAPPDATA%\Microsoft\WinGet\Packages` for `dlltool.exe`, so the launcher needs no setup.

## Commands

```powershell
cargo build -p viewer-app                              # debug, console attached
cargo build --release -p viewer-app                    # release, console detached
cargo build --release -p viewer-app --features console # release, console attached
cargo test --workspace                                 # 362 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo doc --workspace --no-deps                        # RUSTDOCFLAGS=-D warnings in CI
```

The last four are the gates `.github/workflows/ci.yml` runs on every push, and all four pass on
`main`. `cargo fmt --all` fixes what the check reports.

`EXTENSIONS` in `crates/app/src/shell.rs` carries `#[rustfmt::skip]`. Its grouping comments carry
meaning, rustfmt reflows them into trailing comments that label the wrong line, and it never
reaches a fixed point on that array, so `cargo fmt --check` failed immediately after `cargo fmt`.

Release builds detach from the console so a double-click does not flash a terminal. The
`identify`, `info`, `shot` and `open-with` subcommands print to the console, so a release build
that runs them needs `--features console`. Debug builds always keep the console.

## Release profile

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

The release binary is a shipped artifact rather than a debug convenience. The whole pitch of the
app is that it is small and starts instantly, so the profile trades build time for size and
throughput. Fat LTO with a single codegen unit makes a clean release build take several minutes.

## The launcher

`Open theViewer.vbs` at the repository root runs `tools\build-and-run.ps1` through
`powershell -WindowStyle Hidden`, so nothing flashes on screen. The script:

1. Locates cargo under `%USERPROFILE%\.cargo\bin`, and shows a message box naming the winget
   command if it is missing.
2. Locates `dlltool.exe` under the winget package tree and prepends its directory to PATH.
3. Rebuilds only when a `.rs`, `.wgsl`, `.toml`, `.ico` or `.png` file under `crates\` or
   `assets\` is newer than `target\release\viewer.exe`. Assets count because the icon is compiled
   into the executable as a resource.
4. Shows a progress window for the length of the build.
5. Launches the binary, forwarding a file path if one was passed to the launcher.

Everything it does goes to `tools\build.log`, which is overwritten on each run. A failed build
raises a message box carrying the first few `error` lines and pointing at that log, because the
script runs with no console to print to.

### The progress window

A release build of this dependency tree runs several minutes and the launcher is deliberately
windowless, so without this a double-click sat there doing nothing visible.

The numbers are cargo's own, not an estimate. `CARGO_TERM_PROGRESS_WHEN=always` makes cargo emit
its progress line even when its output is redirected to a file, which is the only way to get a
real unit total; left alone it suppresses progress for anything that is not a terminal. The script
starts cargo with its output redirected, then a 250 ms timer reads the file through a shared handle
and parses two things out of it: `Building [...] 464/465` for the count, and the last
`Compiling <crate>` line for what is being worked on right now.

Three details are there to keep it honest rather than pretty:

- The percentage is floored. 464 of 465 rounds to 100, and a bar that reads 100% while the build
  is still going is the thing the window exists to prevent.
- When every unit is compiled and cargo has not yet said `Finished`, the status switches to
  `Linking and optimizing viewer.exe`. That is the fat LTO pass, it is frequently the longest
  single step, and cargo reports nothing at all while it runs.
- Cancel kills the compiler through `taskkill /T`, because `rustc` runs as a child process and
  would otherwise carry on with nothing watching it.

## Self-containment

The executable must not depend on any DLL that ships with the compiler. The failure is not subtle
but it is easy to miss: it works when the build script launches the app, because that puts MinGW
on PATH, and it fails with a system error box the moment Explorer launches it from the **Open
with** menu, before `main` runs.

Check it after any dependency change:

```powershell
objdump -p target\release\viewer.exe | Select-String -Pattern "DLL Name"
```

Every entry should be a Windows system DLL. `libstdc++-6.dll`, `libgcc_s_seh-1.dll` and
`libwinpthread-1.dll` must not appear.

### How it is prevented

`charls-sys`, behind DICOM's JPEG-LS support, compiles C++ and needs `libstdc++`. The default is to
link it dynamically. Two things stop that.

`link-cplusplus` is pulled in with `default-features = false, features = ["nothing"]`, which
suppresses the dynamic request. Nothing in this workspace calls it. It is a dependency purely to
turn that behaviour off.

`crates/formats/build.rs` then appends the C++ runtime and everything it depends on as link
arguments. Position on the link line is the whole trick, and it is not the position anyone
expects. `rustc` lays out the command as crate objects, rlibs, native libraries from build scripts,
its own runtime libraries, and finally everything from `rustc-link-arg`. A `rustc-link-lib`
therefore lands ahead of the rlibs that need it, and GNU `ld` resolves an archive only against
undefined symbols it has already seen. The runtime has to go at the very end.

The libraries go in as one `--start-group` because the dependency order is a cycle rather than a
chain: `libstdc++` calls into `libgcc` for thread-local storage, `libgcc` calls into
`libwinpthread` for its mutexes, and `libwinpthread` calls back into the C runtime.
`--start-group` makes the linker re-scan until nothing new resolves.

`crates/app/build.rs` includes the same file, because link arguments apply only to the package
that emits them and both the test harness and the binary need them. It has a second job as well:
compiling `assets/icon.ico` and a version block into `viewer.exe` through `windres`.

The same constraint is why the meshopt C++ bindings were replaced by a Rust port in
`crates/formats/src/meshopt.rs`. The bindings crate hard-codes a dynamic `-lstdc++` for
`windows-gnu`.

## Icons

```powershell
node tools\make-icons.mjs
```

Reads `assets\icon-source.png` and `assets\favicon-source.png` and writes `icon.png`, `icon.ico`,
`favicon.png` and `favicon.ico` beside them. The generated files are tracked, so this only needs
running when the artwork changes.

Like the corpus generator, it carries no dependencies: it decodes and encodes PNG through Node's
own zlib and assembles the ICO container by hand. Sizes 16 through 128 are written as 32-bit BMP
entries and 256 as PNG, which is what Windows expects.

`viewer.exe` gets its icon two ways, because two different things read it. Explorer, the taskbar
and the Open with menu read the resource `crates\app\build.rs` compiles in with `windres`, and the
window itself carries the PNG that `crates\app\src\ui.rs` embeds. A missing `windres` downgrades
to a build warning rather than a failure: a blank icon is cosmetic.

## Test corpus

```powershell
node tools\make-corpus.mjs
```

Writes `corpus\`:

| File | Contents |
|---|---|
| `cube.ply` | ASCII PLY mesh |
| `cube.stl` | Binary STL |
| `cube.glb` | Two-node GLB |
| `sphere.obj` | OBJ mesh |
| `textured.obj`, `textured.mtl`, `textures/` | OBJ with a material library and a texture |
| `splats.ply` | 60,000-splat 3DGS PLY torus |
| `panorama360.png` | Equirectangular test pattern carrying GPano metadata |
| `unlabelled-2to1.png` | The same pattern carrying nothing |
| `points.xyz` | 4,000-point coloured helix, `x y z r g b` |
| `survey.pts` | 2,000 points in the seven-column Leica layout, at UTM coordinates |
| `torus.off` | 800-vertex OFF torus with quad faces |
| `volume.mhd` plus `volume.raw` | 64-cubed MetaImage volume, split across two files |
| `volume.mha` | The same volume inline and zlib-compressed |

The two panoramas exist so both the declared and the guessed detection paths have a fixture.

The corpus is deterministic and is not tracked. `make-corpus.mjs` writes its PNGs by hand, so it
needs no image library and no npm install.

## Verifying without a display

The development machine's desktop does not composite a GPU window into a screen capture.
Screenshots of the running app come back blank even when it is working, and force-killing it
leaves ghost pixels that survive a desktop redraw.

Every visual claim in this repository was checked with the offscreen renderer instead:

```powershell
viewer shot corpus\splats.ply out.png --size 1600x900
viewer info corpus\splats.ply
```

`shot` needs no display and no window, which is also what makes the renderer checkable on a build
agent. `info` imports, decodes the opening frames, and reports how many carried geometry, how many
carried a picture, and the decoded extent. See [CLI.md](CLI.md).

## Dependency notes

`dicom-pixeldata` default features pull in `rav1e` and `ravif`, which are large and slow to build.
It is pinned to `native,rle,jpeg,deflate,openjp2,charls`.

`openusd` wants `zip` 8.x and this workspace pins 4.x, so two copies of `zip` are compiled today.

`cargo` reports a future-incompatibility notice for two transitive dependencies, `nom` 3.2.1 and
`quick-xml`. Neither is a direct dependency. Workspace code is clippy clean.
