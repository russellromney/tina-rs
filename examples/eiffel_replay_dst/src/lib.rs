//! Tina-vs-Tokio: deterministic replay vs. wall-clock drift.
//!
//! The Tina side runs under [`tina_sim::Simulator`] with seeded
//! fault injection. Two runs at the same seed produce
//! byte-identical traces (same length, same `stable_trace_hash`).
//! A run at a *different* seed produces a different fingerprint —
//! that's the proof the replay property is non-trivial.
//!
//! The Tokio side runs the same nominal workload twice on a
//! `current_thread` runtime. The message *sequence* is stable, but
//! the wall-clock *timings* drift between runs because there is no
//! deterministic-replay story for live Tokio.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares feel.

pub mod tina_impl;
pub mod tokio_impl;
