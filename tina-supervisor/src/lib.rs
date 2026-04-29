#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Supervisor configuration for `tina-rs`.
//!
//! This crate intentionally starts small. Shared restart policy vocabulary
//! lives in `tina`; runtime-owned mutable supervision state lives in runtime
//! crates. `tina-supervisor` owns the configuration surface that joins those
//! pieces without turning the trait crate into a mechanism crate.

use tina::{RestartBudget, RestartPolicy};

/// Runtime-agnostic supervisor configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SupervisorConfig {
    policy: RestartPolicy,
    budget: RestartBudget,
}

impl SupervisorConfig {
    /// Creates a supervisor configuration from a restart policy and budget.
    pub const fn new(policy: RestartPolicy, budget: RestartBudget) -> Self {
        Self { policy, budget }
    }

    /// Returns the restart policy used for supervised child failures.
    pub const fn policy(&self) -> RestartPolicy {
        self.policy
    }

    /// Returns the restart budget used for supervised child failures.
    pub const fn budget(&self) -> RestartBudget {
        self.budget
    }
}
