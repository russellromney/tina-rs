//! Tina-vs-Tokio: a worker that panics on poison messages.
//!
//! Both sides run the same job script. Each `Job::Work(_)` should be
//! processed; each `Job::Poison` should panic the worker and trigger
//! a restart. The comparison is "Tokio's hand-rolled
//! `JoinHandle::is_panic` / respawn loop" vs "Tina's supervisor +
//! restart budget."
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares feel.

pub mod tina_impl;
pub mod tokio_impl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Work(u32),
    Poison,
}

/// Fixed job script. Both sides receive the same sequence; the
/// comparison is about implementation feel, not workload.
pub fn job_script() -> Vec<Job> {
    vec![
        Job::Work(1),
        Job::Work(2),
        Job::Poison,
        Job::Work(3),
        Job::Poison,
        Job::Work(4),
        Job::Work(5),
    ]
}

/// What each side observed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub processed: u32,
    pub poisoned: u32,
    pub restarts: u32,
    pub exit_clean: bool,
}
