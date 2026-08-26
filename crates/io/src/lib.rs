//! Format identification and the shared streaming layer.
//!
//! Two jobs, both of which every format in `docs/FORMATS.md` depends on:
//!
//! - [`sniff`] identifies a file from its **header bytes**, never its extension. The extension is
//!   at most a tie-breaker.
//! - [`Source`] is the memory-mapped, range-readable view every streaming format reads through,
//!   `.ares` GOPs, COPC octants, LAZ chunks and DICOM series are the same problem wearing
//!   different hats.

// `deny` rather than `forbid`: the memory map below is the single permitted exception, and
// `forbid` cannot be locally overridden.
#![deny(unsafe_code)]

pub mod sniff;

pub use sniff::{
    sniff, Candidate, Confidence, FormatId, ImageKind, PlyVariant, SpzEncoding, VideoKind,
    HEADER_BYTES,
};

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// A file opened for reading, memory-mapped where the platform allows it.
///
/// Mapping rather than reading is the whole point: a 200 M-point LiDAR tile or an 8.5 GB splat
/// capture must be *addressable* without being resident. Importers should slice this, not copy it.
pub struct Source {
    path: PathBuf,
    map: memmap2::Mmap,
}

impl Source {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;

        // SAFETY: memmap2's `map` is unsafe because another process may mutate or truncate the
        // file while it is mapped. A read-only viewer accepts that: the failure mode is garbage
        // bytes, which every parser here already validates against, or SIGBUS on truncation. This
        // is the only unsafe block in the workspace.
        #[allow(unsafe_code)]
        let map = unsafe { memmap2::Mmap::map(&file) }?;
        Ok(Source { path, map })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> u64 {
        self.map.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.map
    }

    /// The leading slice a sniffer needs. Short files return everything they have.
    pub fn header(&self) -> &[u8] {
        let n = (self.map.len()).min(HEADER_BYTES);
        &self.map[..n]
    }

    /// A bounds-checked range read. Returns `None` rather than panicking, because every offset
    /// here ultimately comes from a length field inside an untrusted file.
    pub fn range(&self, offset: u64, len: u64) -> Option<&[u8]> {
        let start = usize::try_from(offset).ok()?;
        let len = usize::try_from(len).ok()?;
        let end = start.checked_add(len)?;
        self.map.get(start..end)
    }

    /// Identify this file. Convenience over `sniff(self.header(), self.len(), name)`.
    pub fn identify(&self) -> Vec<Candidate> {
        let name = self.path.file_name().and_then(|s| s.to_str());
        sniff(self.header(), self.len(), name)
    }
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("path", &self.path)
            .field("len", &self.len())
            .finish()
    }
}
