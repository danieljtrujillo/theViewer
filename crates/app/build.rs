// See `crates/formats/build.rs`, which this is.
//
// Link arguments apply only to the package that emits them, and both that crate's test harness
// and this crate's binary need the same ones. Including the file rather than copying it is what
// keeps the two from drifting apart.
include!("../formats/build.rs");
