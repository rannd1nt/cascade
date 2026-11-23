//! Media backends.

mod colour;
mod engine;
mod fit;
mod probe;
mod source_reader;
/// First-frame thumbnail decode + JPEG cache codec (
/// Public because the caller is another crate
/// (`cascade-ui` only — architecture ); the daemon never uses it.
pub mod thumbnail;

pub use engine::MediaEngineBackend;
pub use probe::probe;
pub use source_reader::SourceReaderBackend;
