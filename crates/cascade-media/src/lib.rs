//! Media backends (design-m2 §2).

mod colour;
mod engine;
mod fit;
mod probe;
mod source_reader;
/// First-frame thumbnail decode + JPEG cache codec (design-m5 §7,
/// design-m5-p4 §11). Public because the caller is another crate
/// (`cascade-ui` only — architecture §3); the daemon never uses it.
pub mod thumbnail;

pub use engine::MediaEngineBackend;
pub use probe::probe;
pub use source_reader::SourceReaderBackend;
