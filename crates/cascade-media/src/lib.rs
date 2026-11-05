//! Media backends (design-m2 §2).

mod engine;
mod source_reader;

pub use engine::MediaEngineBackend;
pub use source_reader::SourceReaderBackend;
