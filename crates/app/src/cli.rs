//! Header-only format identification from the command line.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use viewer_io::{Confidence, Source};
use viewer_scene::Timeline;

pub fn usage() {
    eprintln!(
        "viewer {}\n\
         \n\
         viewer                           open the window\n\
         viewer <path>                    open the window on a file\n\
         viewer identify <path>...        print format identification\n\
         viewer identify --dir <folder>   identify every file in a folder\n\
         viewer info <path>...            identify, import, and print scene contents\n\
         viewer shot <in> <out.png>       render one frame to a PNG, with no window\n\
         \x20                                 [--size WxH] [--time SECONDS]\n\
         viewer open-with <action>        add, remove or check the Open with entry\n\
         \n\
         Identification reads header bytes only. See docs/FORMATS.md.",
        env!("CARGO_PKG_VERSION")
    );
}

pub fn identify(args: &[String]) -> ExitCode {
    let paths = match collect(args) {
        Ok(p) if p.is_empty() => {
            eprintln!("viewer: no files given");
            return ExitCode::from(2);
        }
        Ok(p) => p,
        Err(e) => {
            eprintln!("viewer: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut unknown = 0usize;
    for path in &paths {
        if !report(path) {
            unknown += 1;
        }
    }
    if paths.len() > 1 {
        println!("{} files, {} unrecognized", paths.len(), unknown);
    }
    ExitCode::SUCCESS
}

fn collect(args: &[String]) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--dir" {
            let dir = args.get(i + 1).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "--dir needs a folder")
            })?;
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    out.push(entry.path());
                }
            }
            i += 2;
        } else {
            out.push(PathBuf::from(&args[i]));
            i += 1;
        }
    }
    out.sort();
    Ok(out)
}

fn report(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    let source = match Source::open(path) {
        Ok(s) => s,
        Err(e) => {
            println!("{name}\n    {e}\n");
            return false;
        }
    };

    let candidates = source.identify();
    println!("{name}");
    println!("    {}", crate::ui::human_bytes(source.len()));
    for (i, c) in candidates.iter().enumerate() {
        let marker = if i == 0 { "->" } else { "  " };
        let support = if c.format.is_supported() {
            ""
        } else {
            "   no importer"
        };
        println!(
            "    {marker} {:<34} {:<11}{support}",
            c.format.label(),
            confidence(c.confidence)
        );
        println!("       {}", c.note);
    }
    println!();

    let best = &candidates[0];
    best.format.is_supported() || best.confidence >= Confidence::Structural
}

/// Identify, then actually import, and report what the scene holds.
pub fn info(args: &[String]) -> ExitCode {
    let paths = match collect(args) {
        Ok(p) if p.is_empty() => {
            eprintln!("viewer: no files given");
            return ExitCode::from(2);
        }
        Ok(p) => p,
        Err(e) => {
            eprintln!("viewer: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut failed = 0usize;
    for path in &paths {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        let source = match Source::open(path) {
            Ok(s) => s,
            Err(e) => {
                println!("{name}\n    {e}\n");
                failed += 1;
                continue;
            }
        };
        let best = source.identify().remove(0);
        println!("{name}");
        println!(
            "    {:<32} {}",
            best.format.label(),
            crate::ui::human_bytes(source.len())
        );

        let started = std::time::Instant::now();
        match viewer_formats::open_path(&best.format, source.bytes(), path) {
            Ok(opened) => {
                let scene = opened.scene;
                let s = scene.summary();
                let elapsed = started.elapsed();
                let b = scene.bounds();
                // Whatever the format said about itself: codec, frame rate, projection, the
                // orientation tag a phone wrote. Printed before the derived numbers because it is
                // what someone diagnosing a file actually wants to see.
                for (key, value) in &scene.meta {
                    println!("    {key:<12} {value}");
                }
                if s.triangles > 0 {
                    println!("    {:<12} {}", "triangles", s.triangles);
                }
                if s.vertices > 0 {
                    println!("    {:<12} {}", "vertices", s.vertices);
                }
                if s.points > 0 {
                    println!("    {:<12} {}", "points", s.points);
                }
                if s.splats > 0 {
                    println!("    {:<12} {}", "splats", s.splats);
                }
                if !b.is_empty() {
                    println!(
                        "    {:<12} {:.3} {:.3} {:.3} to {:.3} {:.3} {:.3}",
                        "bounds", b.min[0], b.min[1], b.min[2], b.max[0], b.max[1], b.max[2]
                    );
                }
                println!("    {:<12} {:.1} ms", "import", elapsed.as_secs_f64() * 1000.0);

                if let Some(frames) = opened.frames {
                    if !decode_probe(frames, scene.timeline.as_ref()) {
                        failed += 1;
                    }
                }
            }
            Err(e) => {
                println!("    {e}");
                failed += 1;
            }
        }
        println!();
    }

    if failed > 0 {
        println!("{} of {} failed", failed, paths.len());
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Decode the opening second of a moving file and report what came back.
///
/// Opening a video proves the container parsed; only decoding proves the machine has a codec for
/// it. Since that is exactly the thing that varies from machine to machine, `info` does it rather
/// than reporting a successful open and leaving the real question unanswered.
fn decode_probe(mut frames: Box<dyn viewer_formats::FrameSource>, timeline: Option<&Timeline>) -> bool {
    const PROBE_FRAMES: u32 = 8;

    let step_us = timeline
        .filter(|t| t.fps > 0.0)
        .map(|t| (1_000_000.0 / t.fps) as u64)
        .unwrap_or(33_333);

    let started = std::time::Instant::now();
    let mut decoded = 0u32;
    let mut with_image = 0u32;
    let mut with_mesh = 0u32;
    let mut shape = Vec::new();
    for i in 0..PROBE_FRAMES {
        match frames.frame_at(i as u64 * step_us) {
            Ok(Some(frame)) => {
                decoded += 1;
                if let Some(image) = &frame.image {
                    with_image += 1;
                    shape.retain(|s: &String| !s.ends_with("px"));
                    shape.push(format!("{}x{} px", image.width, image.height));
                }
                if let Some(mesh) = &frame.mesh {
                    with_mesh += 1;
                    shape.retain(|s: &String| !s.ends_with("vertices"));
                    shape.push(format!("{} vertices", mesh.positions.len()));
                    // The *decoded* extent, which is the thing a wrong unprojection gets wrong.
                    // The scene's own bounds are whatever the importer declared before any frame
                    // arrived, and for a capture that is the camera frustum, not the subject.
                    if !mesh.bounds.is_empty() {
                        let e = mesh.bounds.extent();
                        shape.retain(|s: &String| !s.ends_with("m"));
                        shape.push(format!("{:.2} x {:.2} x {:.2} m", e[0], e[1], e[2]));
                    }
                }
            }
            // A source that holds its frame is not an error; a still that landed here would.
            Ok(None) => {}
            Err(e) => {
                println!("    {:<12} {e}", "decode");
                return false;
            }
        }
    }

    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    // Images and geometry are counted separately because a volumetric capture can decode all its
    // geometry and none of its texture, and that is exactly the case worth seeing at a glance.
    let mut parts = vec![format!("{decoded} of {PROBE_FRAMES} frames")];
    if with_mesh > 0 {
        parts.push(format!("{with_mesh} with geometry"));
    }
    if with_image > 0 {
        parts.push(format!("{with_image} with a picture"));
    }
    parts.extend(shape);
    parts.push(format!("{elapsed:.1} ms"));
    println!("    {:<12} {}", "decode", parts.join(", "));
    decoded > 0
}

fn confidence(c: Confidence) -> &'static str {
    match c {
        Confidence::Magic => "magic",
        Confidence::Structural => "structural",
        Confidence::Heuristic => "heuristic",
        Confidence::ExtensionOnly => "extension",
    }
}

/// `viewer shot <input> <output.png> [--size WxH] [--time SECONDS]`
///
/// Renders without a window. Useful on its own, and the only way to check what the renderer
/// produced on a machine whose display is not compositing — which is the situation on any build
/// agent, and on this one.
pub fn shot(args: &[String]) -> ExitCode {
    let mut positional: Vec<&str> = Vec::new();
    let mut options = crate::shot::Options::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--size" => {
                let Some((width, height)) = args.get(i + 1).and_then(|s| parse_size(s)) else {
                    eprintln!("viewer: --size wants WIDTHxHEIGHT, as in 1920x1080");
                    return ExitCode::from(2);
                };
                options.width = width;
                options.height = height;
                i += 2;
            }
            "--time" => {
                let Some(seconds) = args.get(i + 1).and_then(|s| s.parse::<f64>().ok()) else {
                    eprintln!("viewer: --time wants a number of seconds");
                    return ExitCode::from(2);
                };
                options.time_us = (seconds.max(0.0) * 1_000_000.0) as u64;
                i += 2;
            }
            other => {
                positional.push(other);
                i += 1;
            }
        }
    }

    let [input, output] = positional[..] else {
        eprintln!("viewer: shot needs an input file and an output .png");
        return ExitCode::from(2);
    };

    match crate::shot::render(Path::new(input), Path::new(output), &options) {
        Ok(()) => {
            println!("{output}  {}x{}", options.width, options.height);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("viewer: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_size(spec: &str) -> Option<(u32, u32)> {
    let (w, h) = spec.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// `viewer open-with [add | remove | status]`
///
/// The same registration the first-run prompt offers, from a shell. Worth having for three
/// reasons: it is scriptable, it is testable, and someone who declined the prompt should not have
/// to hunt through a menu to change their mind.
pub fn open_with(args: &[String]) -> ExitCode {
    let action = args.first().map(String::as_str).unwrap_or("status");
    match action {
        "status" => {
            let registered = crate::shell::is_registered();
            println!(
                "{}",
                if registered {
                    "registered for the current user"
                } else {
                    "not registered"
                }
            );
            println!("consent    {:?}", crate::shell::consent());
            if let Some(path) = crate::shell::state_path() {
                println!("state      {}", path.display());
            }
            println!("types      {} extensions", crate::shell::EXTENSIONS.len());
            ExitCode::SUCCESS
        }
        "add" => {
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("viewer: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match crate::shell::register(&exe) {
                Ok(()) => {
                    crate::shell::record_consent(crate::shell::Consent::Granted);
                    println!("added to the Open with menu for {}", exe.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("viewer: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "remove" => match crate::shell::unregister() {
            Ok(()) => {
                crate::shell::record_consent(crate::shell::Consent::Deferred);
                println!("removed from the Open with menu");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("viewer: {e}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("viewer: unknown open-with action {other:?}; try add, remove or status");
            ExitCode::from(2)
        }
    }
}
