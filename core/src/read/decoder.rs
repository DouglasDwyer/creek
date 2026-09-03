use std::io::{Read, Seek};
use std::{error::Error, fmt::Debug};

use super::DataBlock;
use crate::FileInfo;

/// A source of encoded audio data for a [`Decoder`].
///
/// This is automatically implemented for any type that is `Read + Seek + Send + Sync + 'static`,
/// such as [`std::fs::File`] or [`std::io::Cursor<Vec<u8>>`](std::io::Cursor). This is what allows
/// a [`ReadDiskStream`](crate::ReadDiskStream) to stream from something other than a file on disk
/// (an in-memory buffer, an embedded asset, a memory-mapped file, etc.).
pub trait ReadSeekSource: Read + Seek + Send + Sync + 'static {}

impl<T: Read + Seek + Send + Sync + 'static> ReadSeekSource for T {}

/// A type that decodes a file in a read stream.
pub trait Decoder: Sized + 'static {
    /// The data type of a single sample. (i.e. `f32`)
    type T: Copy + Clone + Default + Send;

    /// Any additional options for opening a file with this decoder.
    type AdditionalOpts: Send + Default + Debug;

    /// Any additional information on the file.
    type FileParams: Clone + Send;

    /// The error type while opening the file.
    type OpenError: Error + Send;

    /// The error type when a fatal error occurs.
    type FatalError: Error + Send;

    /// The default number of frames in a prefetch block.
    const DEFAULT_BLOCK_SIZE: usize;

    /// The default number of prefetch blocks in a cache block. This will cause a cache to be
    /// used whenever the stream is seeked to a frame in the range:
    ///
    /// `[cache_start, cache_start + (num_cache_blocks * block_size))`
    ///
    /// If this is 0, then the cache is only used when seeked to exactly `cache_start`.
    const DEFAULT_NUM_CACHE_BLOCKS: usize;

    /// The number of prefetch blocks to store ahead of the cache block. This must be
    /// sufficiently large to ensure enough to time to fill the buffer in the worst
    /// case latency scenario.
    const DEFAULT_NUM_LOOK_AHEAD_BLOCKS: usize;

    /// Start reading from `source`, beginning at `start_frame`.
    ///
    /// `source` is any `Read + Seek` stream (see [`ReadSeekSource`]), for example an open
    /// [`File`](std::fs::File) or an [`io::Cursor`](std::io::Cursor) over an in-memory buffer.
    ///
    /// Please note this algorithm depends on knowing the exact number of frames in a file.
    /// Do **not** return an approximate length in the returned `FileInfo`.
    fn new(
        source: Box<dyn ReadSeekSource>,
        start_frame: usize,
        block_size: usize,
        additional_opts: Self::AdditionalOpts,
    ) -> Result<(Self, FileInfo<Self::FileParams>), Self::OpenError>;

    /// Seek to a frame in the file. If a frame lies outside of the end of the file,
    /// set the read position the end of the file instead of returning an error.
    fn seek(&mut self, frame: usize) -> Result<(), Self::FatalError>;

    /// Decode data into the `data_block` starting from your current internal read position.
    /// This is streaming, meaning the next call to `decode()` should pick up where the
    /// previous left off.
    ///
    /// Fill each channel in the data block with `block_size` number of frames (you should
    /// have gotten this value from `Decoder::new()`). If there isn't enough data left
    /// because the end of the file has been reached, then only fill up how ever many frames
    /// are left. If the end of the file has already been reached since the last call to
    /// `decode()`, then do nothing.
    ///
    /// Each channel Vec in `data_block` will have a length of zero.
    fn decode(&mut self, data_block: &mut DataBlock<Self::T>) -> Result<(), Self::FatalError>;

    /// Return the current read position.
    fn current_frame(&self) -> usize;
}
