//! Offline language analysis shared by CLI and LSP. No provider or transport dependencies.
pub mod language;
pub mod lsp;
pub mod simulator;
pub mod syntax;
pub use language::{Analysis, Compilation, analyze};
