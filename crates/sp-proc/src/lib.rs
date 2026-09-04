//! Pipeline orchestration. Knows nothing about DSP: this crate defines the
//! `Stage` contract, the registry, ports and the scheduler.
//!
//! Filled in at milestone M5 — Pipeline (see `docs/DESIGN.md` §16). The crate exists
//! now so the workspace dependency graph and CI are in place from M0.
