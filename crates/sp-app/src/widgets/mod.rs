//! Reusable drawing widgets (`docs/DESIGN.md` §4.1).
//!
//! These are the only place in the application that turns engine output into
//! geometry; screens compose them and own their state.

pub mod glyph;
pub mod histogram;
pub mod panes;
pub mod scope;
