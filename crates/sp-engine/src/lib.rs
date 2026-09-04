//! The playback engine: transport state machine, virtual clock, render
//! pyramids and viewport reduction (`docs/DESIGN.md` §5.4, §11).
//!
//! The engine is where "100 M samples at 60 fps" is won. Nothing above it ever
//! sees a raw sample: a frame asks for a [`reduce::TraceSnapshot`] over the
//! current [`viewport::Viewport`] and gets back at most one `(min, max)` pair
//! per pixel column, read out of a [`pyramid`] level a few kilobytes wide. The
//! crate knows about time, samples and cells; `sp-app` is the only crate that
//! knows about pixels.

pub mod clock;
pub mod error;
pub mod pyramid;
pub mod reduce;
pub mod source;
pub mod transport;
pub mod viewport;

pub use clock::Clock;
pub use error::{EngineError, Result};
pub use pyramid::{Pyramid, PyramidBuilder, PyramidHeader, Reduction, BASE_SHIFT};
pub use reduce::{
    trace, ColumnReader, LogicLevel, TraceDescriptor, TraceForm, TraceGeometry, TraceSnapshot,
    TraceStyle,
};
pub use source::{BuildControl, BuildProgress, ColumnSource};
pub use transport::{LoopMode, Transport, TransportState};
pub use viewport::{Amplitude, FollowMode, Viewport, SCROLL_ANCHOR};
