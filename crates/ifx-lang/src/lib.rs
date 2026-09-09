//! Offline language analysis shared by CLI and LSP. No provider or transport dependencies.
pub mod authoring;
pub mod language;
pub mod lsp;
pub mod project;
pub mod simulator;
pub mod syntax;
pub use language::{Analysis, Compilation, analyze};
