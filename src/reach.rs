//! Whether a change reaches what a crate ships.
//!
//! A test plan asks which tests can observe a change. A release asks a
//! different question: does the change alter the binary built from this
//! crate? The answer follows the crate's normal and build dependencies only,
//! built with its default features, and leaves out the parts of a crate that
//! are never shipped.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::change::{ChangeSet, FileKind, RepoPath};
use crate::lockfile::Lockfile;
use crate::manifest::ManifestChange;
use crate::workspace::{PackageName, WorkspaceError, WorkspaceGraph};

/// Why a change reaches what a crate ships.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ReachReason {
    /// A shipped file of a crate the binary is built from changed.
    FileChanged {
        path: RepoPath,
        package: PackageName,
    },
    /// `Cargo.lock` resolves the crate with a different set of external
    /// packages: this one is new, gone, or resolved differently.
    LockChanged { name: String, version: String },
    /// The binary is built from a package whose entry in the root manifest's
    /// `[workspace.dependencies]` changed.
    WorkspaceDependencyChanged { name: String },
    /// A file that changes how every crate builds, and the change could not
    /// be narrowed.
    WorkspaceGlobal { path: RepoPath },
}

/// The answer for one crate.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PackageReach {
    pub reached: bool,
    pub reasons: Vec<ReachReason>,
}

/// The answer for each crate asked about.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReachReport {
    pub packages: BTreeMap<PackageName, PackageReach>,
    /// Changed files no workspace crate owns. Whether one of them reaches a
    /// release (a Dockerfile, a workflow) is for the caller to decide.
    pub outside: Vec<RepoPath>,
}

/// `Cargo.lock` at the two revisions being compared.
pub struct LockPair<'a> {
    pub base: &'a Lockfile,
    pub head: &'a Lockfile,
}

/// Directories of a crate that hold code cargo builds only for tests,
/// benchmarks and examples. Nothing in them is part of the crate's library
/// or binaries.
const NOT_SHIPPED: [&str; 3] = ["tests", "benches", "examples"];

/// Decides, for each of some crates, whether a change reaches what the crate
/// ships. Each optional input narrows one kind of workspace-wide file;
/// without it, a change to that file reaches every crate.
pub struct Reach<'a> {
    graph: &'a WorkspaceGraph,
    locks: Option<LockPair<'a>>,
    manifest_change: Option<&'a ManifestChange>,
}

impl<'a> Reach<'a> {
    pub fn new(graph: &'a WorkspaceGraph) -> Self {
        Self {
            graph,
            locks: None,
            manifest_change: None,
        }
    }

    /// Narrows a `Cargo.lock` change to the crates resolved differently.
    pub fn with_locks(mut self, locks: LockPair<'a>) -> Self {
        self.locks = Some(locks);
        self
    }

    /// Narrows a change to the root `Cargo.toml` to the crates built from
    /// the `[workspace.dependencies]` entries it touches.
    pub fn with_manifest_change(mut self, change: &'a ManifestChange) -> Self {
        self.manifest_change = Some(change);
        self
    }

    pub fn of(
        &self,
        changes: &ChangeSet,
        packages: &BTreeSet<PackageName>,
    ) -> Result<ReachReport, WorkspaceError> {
        let lockfile = self.graph.lockfile();
        let root_manifest = self.graph.root_manifest();
        let mut report = ReachReport::default();
        for file in changes.files() {
            if file.kind == FileKind::Outside {
                report.outside.push(file.path.clone());
            }
        }

        for package in packages {
            let built_from = self.graph.built_from(package)?;
            let names = self.graph.built_from_names(package)?;
            let mut reasons = BTreeSet::new();
            for file in changes.files() {
                let path = &file.path;
                match &file.kind {
                    FileKind::Outside => {}
                    FileKind::WorkspaceGlobal if lockfile.as_ref() == Some(path) => {
                        reasons.extend(self.lock_reasons(package, &names, path));
                    }
                    FileKind::WorkspaceGlobal if root_manifest.as_ref() == Some(path) => {
                        match self.manifest_change {
                            Some(ManifestChange::Dependencies(changed)) => {
                                reasons.extend(changed.intersection(&names).map(|name| {
                                    ReachReason::WorkspaceDependencyChanged { name: name.clone() }
                                }));
                            }
                            Some(ManifestChange::Everything) | None => {
                                reasons.insert(ReachReason::WorkspaceGlobal { path: path.clone() });
                            }
                        }
                    }
                    FileKind::WorkspaceGlobal => {
                        reasons.insert(ReachReason::WorkspaceGlobal { path: path.clone() });
                    }
                    FileKind::CrateManifest { package: owner } => {
                        if built_from.contains(owner) {
                            reasons.insert(ReachReason::FileChanged {
                                path: path.clone(),
                                package: owner.clone(),
                            });
                        }
                    }
                    FileKind::RustSource { package: owner }
                    | FileKind::CrateData { package: owner } => {
                        if built_from.contains(owner) && self.is_shipped(path, owner) {
                            reasons.insert(ReachReason::FileChanged {
                                path: path.clone(),
                                package: owner.clone(),
                            });
                        }
                    }
                }
            }
            let reach = PackageReach {
                reached: !reasons.is_empty(),
                reasons: reasons.into_iter().collect(),
            };
            report.packages.insert(package.clone(), reach);
        }
        Ok(report)
    }

    /// The external packages `package` is resolved with at one revision and
    /// not, or differently, at the other, among those its binary is built
    /// with. `shipped` names every package the binary is built from.
    fn lock_reasons(
        &self,
        package: &PackageName,
        shipped: &BTreeSet<String>,
        path: &RepoPath,
    ) -> BTreeSet<ReachReason> {
        let global = || BTreeSet::from([ReachReason::WorkspaceGlobal { path: path.clone() }]);
        let Some(locks) = &self.locks else {
            return global();
        };
        if locks.head.format_differs(locks.base) {
            return global();
        }
        // A crate one of the lockfiles does not list is new or gone, and its
        // own manifest is among the changed files.
        let (Some(before), Some(after)) = (
            locks.base.externals_of(package.as_str()),
            locks.head.externals_of(package.as_str()),
        ) else {
            return global();
        };
        // The lockfile lists optional dependencies whether or not a feature
        // turns them on. A package the graph knows by name but the binary is
        // not built with is not shipped; one it does not know at all is gone
        // at this revision and may have been shipped before, so it counts.
        before
            .symmetric_difference(&after)
            .filter(|external| {
                shipped.contains(&external.name) || !self.graph.has_package_named(&external.name)
            })
            .map(|external| ReachReason::LockChanged {
                name: external.name.clone(),
                version: external.version.clone(),
            })
            .collect()
    }

    /// Whether `path`, a file of the crate `owner`, can be part of what the
    /// crate offers its dependents.
    fn is_shipped(&self, path: &RepoPath, owner: &PackageName) -> bool {
        let Some(directory) = self.graph.directory_of(owner) else {
            return true;
        };
        let Ok(inside) = path.as_path().strip_prefix(directory) else {
            return true;
        };
        let top = inside.components().next().map(|part| part.as_str());
        !top.is_some_and(|top| NOT_SHIPPED.contains(&top))
    }
}
