//! Base for file implementations layered over another file.
//!
//! A layered file (for example a checksumming or encrypting wrapper) implements the
//! file operations by forwarding them to an existing [`File`], the underlying file.
//! [`LayeredFileImpl`] holds that file and copies its DMA alignment constants, so the
//! wrapper presents the same alignment requirements as the file it wraps.

use crate::engine::core::file::{File, FileImplBase};

/// Common state of a layered file implementation.
///
/// It performs the standard setup (storing the underlying file and copying its
/// alignments into [`FileImplBase`]); the I/O methods themselves are left to the type
/// that embeds it.
pub struct LayeredFileImpl {
    /// Alignment constants and maximum request lengths, copied from the underlying file.
    pub base: FileImplBase,
    pub(crate) _underlying_file: File,
}

impl LayeredFileImpl {
    /// Wraps `underlying_file`, taking its memory, read, write and overwrite DMA
    /// alignments as this file's own.
    pub fn new(underlying_file: File) -> Self {
        let base = FileImplBase {
            _memory_dma_alignment: underlying_file.memory_dma_alignment() as u32,
            _disk_read_dma_alignment: underlying_file.disk_read_dma_alignment() as u32,
            _disk_write_dma_alignment: underlying_file.disk_write_dma_alignment() as u32,
            _disk_overwrite_dma_alignment: underlying_file.disk_overwrite_dma_alignment() as u32,
            ..FileImplBase::default()
        };
        LayeredFileImpl { base, _underlying_file: underlying_file }
    }

    /// The underlying file, mutably, for forwarding I/O calls.
    #[inline]
    pub fn underlying_file(&mut self) -> &mut File {
        &mut self._underlying_file
    }

    /// The underlying file, shared, for forwarding I/O calls.
    #[inline]
    pub fn underlying_file_ref(&self) -> &File {
        &self._underlying_file
    }
}
