//! Synchronous filesystem layer.
//!
//! Everything here runs on the UI thread and returns `Result` instead of
//! panicking, so callers can decide how to surface a failure (status line,
//! modal, or startup abort). Recursive, cancellation-aware I/O lives in
//! `crate::tasks` and must never be implemented here.

pub mod panel;
