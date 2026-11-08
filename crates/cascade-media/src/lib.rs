//! Media backends (design-m2 §2).

mod engine;
mod probe;
mod source_reader;

pub use engine::MediaEngineBackend;
pub use probe::probe;
pub use source_reader::SourceReaderBackend;
