//! Compression layer: zstd codec, dictionary training, sample collection.
//! See also: docs/compression.md.

pub mod codec;
pub mod recompression;
pub mod samples;
pub mod trainer;
pub mod training_queue;
