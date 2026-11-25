//! The Cargo workspace as a graph of crates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use camino::{Utf8Path, Utf8PathBuf};
use guppy::graph::feature::{FeatureId, FeatureSet, StandardFeatures};
use guppy::graph::{DependencyDirection, PackageGraph};
use guppy::{MetadataCommand, PackageId};
use serde::{Deserialize, Serialize};

use crate::change::RepoPath;

/// The name of a workspace crate, as written in its `Cargo.toml`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageName(String);

impl PackageName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("`cargo metadata` failed: {0}")]
    Metadata(#[from] guppy::Error),
    #[error("workspace crate `{0}` is not in the dependency graph")]
    UnknownPackage(PackageName),
    #[error("workspace root `{workspace}` is not inside the repository `{repository}`")]
    OutsideRepository {
        workspace: Utf8PathBuf,
        repository: Utf8PathBuf,
    },
}

/// The crates of one workspace and the dependencies between them.
pub struct WorkspaceGraph {
    graph: PackageGraph,
    /// The workspace root, relative to the repository root. Empty when the
    /// two are the same directory.
    root: Utf8PathBuf,
    /// Crate directory (relative to the repository root) -> crate.
    directories: BTreeMap<Utf8PathBuf, PackageName>,
    ids: BTreeMap<PackageName, PackageId>,
}

impl WorkspaceGraph {
    /// Runs `cargo metadata` for the workspace that owns `manifest_path`.
    /// `repository` is the root that changed paths are relative to.
    pub fn load(manifest_path: &Utf8Path, repository: &Utf8Path) -> Result<Self, WorkspaceError> {
        let mut command = MetadataCommand::new();
        command.manifest_path(manifest_path);
        let graph = command.build_graph()?;

        let workspace = graph.workspace().root();
        let root = workspace
            .strip_prefix(repository)
            .map_err(|_| WorkspaceError::OutsideRepository {
                workspace: workspace.to_owned(),
                repository: repository.to_owned(),
            })?
            .to_owned();

        let mut directories = BTreeMap::new();
        let mut ids = BTreeMap::new();
        for (path, package) in graph.workspace().iter_by_path() {
            let name = PackageName(package.name().to_owned());
            directories.insert(root.join(path), name.clone());
            ids.insert(name, package.id().clone());
        }
        Ok(Self {
            graph,
            root,
            directories,
            ids,
        })
    }

    /// The path of `path` inside the workspace root, or `None` when the file
    /// is outside the workspace.
    pub fn workspace_relative<'a>(&self, path: &'a RepoPath) -> Option<&'a Utf8Path> {
        path.as_path().strip_prefix(&self.root).ok()
    }

    /// The repository path of a file named relative to the workspace root,
    /// the form a SCIP index uses.
    pub fn repo_path(&self, workspace_relative: &str) -> Option<RepoPath> {
        RepoPath::parse(self.root.join(workspace_relative).as_str()).ok()
    }

    /// The repository path of the workspace's `Cargo.lock`.
    pub fn lockfile(&self) -> Option<RepoPath> {
        self.repo_path("Cargo.lock")
    }

    /// The workspace crate called `name`, if there is one.
    pub fn package(&self, name: &str) -> Option<PackageName> {
        self.ids
            .keys()
            .find(|package| package.as_str() == name)
            .cloned()
    }

    /// Every workspace crate.
    pub fn packages(&self) -> BTreeSet<PackageName> {
        self.ids.keys().cloned().collect()
    }

    /// The crate whose directory contains `path`. Crates can nest, so the
    /// deepest directory wins.
    pub fn owner_of(&self, path: &RepoPath) -> Option<&PackageName> {
        path.as_path()
            .ancestors()
            .skip(1)
            .find_map(|dir| self.directories.get(dir))
    }

    /// The workspace crates whose tests can observe a change to `changed`.
    ///
    /// A crate's tests are built from the crate, its dev-dependencies, and
    /// everything those depend on through normal and build dependencies. So a
    /// change reaches every crate that depends on a changed crate through
    /// normal or build dependencies, plus the crates that have one of those
    /// as a direct dependency of any kind. A dev-dependency is followed for
    /// that last step only: it is not part of what the crate offers its own
    /// dependents.
    pub fn affected_by(
        &self,
        changed: &BTreeSet<PackageName>,
    ) -> Result<BTreeMap<PackageName, Dependency>, WorkspaceError> {
        let mut affected = BTreeMap::new();
        for origin in changed {
            let id = self
                .ids
                .get(origin)
                .ok_or_else(|| WorkspaceError::UnknownPackage(origin.clone()))?;
            for dependent in self.tests_built_from(id)? {
                affected
                    .entry(dependent)
                    .or_insert_with(|| Dependency::DependsOn(origin.clone()));
            }
        }
        for package in changed {
            affected.insert(package.clone(), Dependency::Changed);
        }
        Ok(affected)
    }

    /// The workspace crates whose tests are built from the external package
    /// `name` at `version`, with the features each needs for that, or `None`
    /// when the dependency graph has no such package.
    ///
    /// `cargo metadata` lists optional dependencies whether or not a feature
    /// turns them on, so a crate that reaches the package only through one
    /// is not built from it by default. Such a crate is listed with the
    /// features of its own that turn the dependency on, and a crate none of
    /// whose features do is left out: its tests never build the package.
    ///
    /// With `shipped`, only what those binaries contain counts: a package
    /// they do not ship has no dependents, and a crate is listed only with
    /// features the binaries turn on for it.
    pub fn dependents_of_external(
        &self,
        name: &str,
        version: &str,
        shipped: Option<&Shipped<'_>>,
    ) -> Result<Option<BTreeMap<PackageName, Activation>>, WorkspaceError> {
        let mut dependents = BTreeMap::new();
        let mut found = false;
        for package in self.graph.packages() {
            if package.name() != name || package.version().to_string() != version {
                continue;
            }
            found = true;
            if let Some(shipped) = shipped
                && !shipped.features.contains_package(package.id())?
            {
                continue;
            }
            for dependent in self.tests_built_from(package.id())? {
                let Some(activation) = self.activation(&dependent, package.id(), shipped)? else {
                    continue;
                };
                match dependents.get(&dependent) {
                    // Two packages of the same name and version, from
                    // different sources: one reached by default is enough.
                    Some(Activation::Default) => {}
                    _ => {
                        dependents.insert(dependent, activation);
                    }
                }
            }
        }
        Ok(found.then_some(dependents))
    }

    /// Which features of the workspace crate `dependent` its tests need to be
    /// built from `target`, or `None` when no feature of it does. With
    /// `shipped`, only the features those binaries turn on for `dependent`
    /// are candidates.
    ///
    /// When several single features do, the one that turns on the fewest
    /// features in all is taken: it is the narrowest build that still
    /// includes `target`. When no single feature does but all together do,
    /// all are taken.
    fn activation(
        &self,
        dependent: &PackageName,
        target: &PackageId,
        shipped: Option<&Shipped<'_>>,
    ) -> Result<Option<Activation>, WorkspaceError> {
        let id = self.id(dependent)?;
        let metadata = self.graph.metadata(id)?;
        let feature_graph = self.graph.feature_graph();
        let defaults = self.default_features(id)?;
        let resolve = |extra: &[&str]| -> Result<FeatureSet<'_>, WorkspaceError> {
            let initials = defaults
                .iter()
                .copied()
                .chain(extra.iter().map(|feature| FeatureId::named(id, feature)));
            Ok(feature_graph
                .query_forward(initials)?
                // As in `tests_built_from`: the crate's own dev-dependencies
                // are built for its tests, other crates' are not.
                .resolve_with_fn(|_, link| !link.dev_only() || link.from().package_id() == id))
        };
        if resolve(&[])?.contains_package(target)? {
            return Ok(Some(Activation::Default));
        }
        let shipped_features = match shipped {
            Some(shipped) => Some(shipped.features.features_for(id)?),
            None => None,
        };
        let features: Vec<&str> = metadata
            .named_features()
            .filter(|feature| *feature != "default")
            .filter(|feature| match &shipped_features {
                Some(list) => list
                    .as_ref()
                    .is_some_and(|list| list.has_named_feature(feature)),
                None => true,
            })
            .collect();
        let mut best: Option<(usize, &str)> = None;
        for feature in &features {
            let set = resolve(&[feature])?;
            if set.contains_package(target)? && best.is_none_or(|(len, _)| set.len() < len) {
                best = Some((set.len(), feature));
            }
        }
        if let Some((_, feature)) = best {
            return Ok(Some(Activation::Features(vec![feature.to_owned()])));
        }
        if !features.is_empty() && resolve(&features)?.contains_package(target)? {
            let mut all: Vec<String> = features.into_iter().map(str::to_owned).collect();
            all.sort();
            return Ok(Some(Activation::Features(all)));
        }
        Ok(None)
    }

    /// What the binaries of `packages` are built from, built as shipped:
    /// with their default features, through normal and build dependencies,
    /// following only the optional dependencies those features turn on.
    pub fn shipped(&self, packages: &BTreeSet<PackageName>) -> Result<Shipped<'_>, WorkspaceError> {
        let mut initials = Vec::new();
        for package in packages {
            initials.extend(self.default_features(self.id(package)?)?);
        }
        let features = self
            .graph
            .feature_graph()
            .query_forward(initials)?
            .resolve_with_fn(|_, link| !link.dev_only());
        Ok(Shipped { features })
    }

    /// The workspace crates `package` is built from: itself and everything it
    /// ships, as [`shipped`](Self::shipped) follows it. Its dev-dependencies
    /// are not part of what gets shipped, so they are left out.
    pub fn built_from(
        &self,
        package: &PackageName,
    ) -> Result<BTreeSet<PackageName>, WorkspaceError> {
        Ok(self
            .shipped(&BTreeSet::from([package.clone()]))?
            .packages()
            .filter(|dependency| dependency.in_workspace())
            .map(|dependency| PackageName(dependency.name().to_owned()))
            .collect())
    }

    /// The names of every package `package` is built from, workspace crates
    /// and external packages alike, as [`shipped`](Self::shipped) follows it.
    pub fn built_from_names(
        &self,
        package: &PackageName,
    ) -> Result<BTreeSet<String>, WorkspaceError> {
        Ok(self.shipped(&BTreeSet::from([package.clone()]))?.names())
    }

    /// Whether the dependency graph has a package called `name`, at any
    /// version, whether or not a feature turns it on.
    pub fn has_package_named(&self, name: &str) -> bool {
        self.graph.packages().any(|package| package.name() == name)
    }

    fn id(&self, package: &PackageName) -> Result<&PackageId, WorkspaceError> {
        self.ids
            .get(package)
            .ok_or_else(|| WorkspaceError::UnknownPackage(package.clone()))
    }

    /// The base feature and the default features of a package: what
    /// `cargo build -p` turns on.
    fn default_features<'g>(
        &'g self,
        id: &'g PackageId,
    ) -> Result<Vec<FeatureId<'g>>, WorkspaceError> {
        Ok(self
            .graph
            .query_forward([id])?
            .to_feature_query(StandardFeatures::Default)
            .initials()
            .feature_ids(DependencyDirection::Forward)
            .collect())
    }

    /// The workspace crates whose tests are built from a package called
    /// `name`, at any version. Empty when the graph has no such package:
    /// nothing is built from it.
    pub fn dependents_of_named(&self, name: &str) -> Result<BTreeSet<PackageName>, WorkspaceError> {
        let mut dependents = BTreeSet::new();
        for package in self.graph.packages() {
            if package.name() == name {
                dependents.extend(self.tests_built_from(package.id())?);
            }
        }
        Ok(dependents)
    }

    /// The directory of `package`, relative to the repository root.
    pub fn directory_of(&self, package: &PackageName) -> Option<&Utf8Path> {
        self.directories
            .iter()
            .find(|(_, owner)| *owner == package)
            .map(|(directory, _)| directory.as_path())
    }

    /// The repository path of the workspace's root `Cargo.toml`.
    pub fn root_manifest(&self) -> Option<RepoPath> {
        self.repo_path("Cargo.toml")
    }

    fn tests_built_from(&self, id: &PackageId) -> Result<BTreeSet<PackageName>, WorkspaceError> {
        let built_from = self
            .graph
            .query_reverse([id])?
            .resolve_with_fn(|_, link| !link.dev_only());
        let mut crates = BTreeSet::new();
        for package in built_from.packages(DependencyDirection::Reverse) {
            let direct_dependents = package.reverse_direct_links().map(|link| link.from());
            for dependent in std::iter::once(package).chain(direct_dependents) {
                if dependent.in_workspace() {
                    crates.insert(PackageName(dependent.name().to_owned()));
                }
            }
        }
        Ok(crates)
    }
}

/// The features some binaries are built with, and so the packages they
/// contain. See [`WorkspaceGraph::shipped`].
pub struct Shipped<'g> {
    features: FeatureSet<'g>,
}

impl<'g> Shipped<'g> {
    fn packages(&self) -> impl Iterator<Item = guppy::graph::PackageMetadata<'g>> + '_ {
        self.features
            .packages_with_features(DependencyDirection::Forward)
            .map(|list| *list.package())
    }

    /// The names of the packages the binaries contain.
    pub fn names(&self) -> BTreeSet<String> {
        self.packages()
            .map(|package| package.name().to_owned())
            .collect()
    }

    /// The workspace crates the binaries contain.
    pub fn crates(&self) -> BTreeSet<PackageName> {
        self.packages()
            .filter(|package| package.in_workspace())
            .map(|package| PackageName(package.name().to_owned()))
            .collect()
    }
}

/// What a workspace crate's tests need to be built from a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// The crate's default features are enough.
    Default,
    /// These features of the crate, by name in name order, turn on the
    /// optional dependencies that lead to the package.
    Features(Vec<String>),
}

/// How a crate is tied to a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dependency {
    /// A file of the crate itself changed.
    Changed,
    /// The crate depends on a crate that changed. When several changed crates
    /// reach it, the first in name order is kept as the example.
    DependsOn(PackageName),
}
