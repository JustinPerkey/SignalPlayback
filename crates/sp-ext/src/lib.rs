//! External stages: an algorithm that already exists as a compiled library,
//! run as an ordinary pipeline stage (`docs/DESIGN.md` §9.9).
//!
//! The harness exists to test algorithms, and most algorithms worth testing
//! were not written in Rust. This crate loads a DLL / `.so` / `.dylib` that
//! speaks the flat C ABI in [`abi`], hands it **one group at a time**, and
//! turns what comes back into the same `StageOutput` a built-in stage
//! produces. From the pipeline editor, the results screen and the run schema
//! it is a stage like any other.
//!
//! ```text
//! ExtLibrary::open ──describe──► StageDescriptor ──register──► StageRegistry
//!        │                                                          │
//!        └── ExtStage::process ──marshal──► sp_process ──read──► StageOutput
//! ```
//!
//! Every `unsafe` line in the workspace that touches a foreign library is in
//! here: `sp-proc` still knows nothing about who implements a stage.

pub mod abi;
pub mod allow;
pub mod descriptor;
pub mod error;
pub mod library;
pub mod stage;

pub use allow::AllowList;
pub use descriptor::{Concurrency, LibraryDescriptor, KIND_PREFIX};
pub use error::{ExtError, Result};
pub use library::{register, ExtLibrary, SymbolSource};
pub use stage::ExtStage;
