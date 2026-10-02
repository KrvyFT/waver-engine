//! Audio callback, command drain, and cpal output stream.

mod devices;
mod engine;
mod stream;

pub use devices::{audio_catalog, default_selection};
pub use engine::{BLOCK, Engine};
pub use stream::spawn_output_for;
pub use stream::{AudioRuntime, EngineError, spawn_output};
