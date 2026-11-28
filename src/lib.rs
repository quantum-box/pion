//! pion selects the tests a change can affect.
//!
//! Three levels of resolution, each narrowing the one before:
//!
//! 1. Crates, from the Cargo dependency graph ([`workspace`]).
//! 2. Tests inside a crate, from a reference graph built out of a
//!    rust-analyzer SCIP index ([`symbols`]).
//! 3. API scenarios, through the handlers their requests reach
//!    ([`routes`], [`scenario`]).
//!
//! [`plan::Planner`] combines them into a [`plan::TestPlan`], and [`audit`]
//! checks a plan against the results of a full test run.

pub mod audit;
pub mod change;
pub mod git;
pub mod lockfile;
pub mod manifest;
pub mod plan;
pub mod reach;
pub mod records;
pub mod reference_syntax;
pub mod routes;
pub mod scenario;
pub mod scip;
pub mod symbols;
pub mod workspace;

pub use change::{ChangeSet, ChangedFile, FileKind, RepoPath};
pub use plan::{
    CratePlan, FallbackCause, Narrowing, Planner, Reason, ScenarioReason, Selection, TestPlan,
};
pub use symbols::{ReferenceGraph, SymbolId, TestName};
pub use workspace::{Activation, PackageName, Shipped, WorkspaceGraph};
