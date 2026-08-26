# plugins

Empty on purpose.

`crates/formats/` holds the T0 importers: statically linked, always present, no lazy resolution.
This directory is where T1 formats go, resolved lazily at run time.

A format earns a place here when it drags in a dependency tree the core should not carry. That
boundary is the T0 and T1 line from [../docs/FORMATS.md](../docs/FORMATS.md), and it asserts itself
rather than needing to be guessed in advance. Two candidates are already visible: STEP pulled in a
CAD kernel and USD pulled in `openusd`, and both are large enough that moving them out is a real
option if the binary grows past roughly 25 MB.

T2 formats go further out again. Parsers this project did not write and cannot audit run
out-of-process with a hard memory ceiling and no filesystem or network reach. The precedent is
Microsoft's own 3D Viewer, which disabled FBX over a parser CVE and never re-enabled it.
[../docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md) covers the security posture in full.
