pub mod classifier;
pub mod hdr;
pub mod hw_probe;
pub mod matrix;

pub use matrix::{choose_codec, Codec, CodecMatrix, StreamRequirements};
