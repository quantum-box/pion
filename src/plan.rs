//! The test plan: which tests to run for a change, and why.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::change::{ChangeSet, FileKind, RepoPath};
use crate::git::ChangedLine;
use crate::lockfile::LockChange;
use crate::manifest::ManifestChange;
use crate::scenario::Scenario;
use crate::symbols::{ReferenceGraph, SymbolId, TestName};
use crate::workspace::{Activation, Dependency, PackageName, WorkspaceError, WorkspaceGraph};

/// Which tests of a crate to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    /// Every test of the crate.
    All,
    /// Only the tests the change reaches through the reference graph.
    Tests(BTreeSet<TestName>),
}

/// Why a crate's tests were selected. Every selection carries its reasons so
/// that the plan can be explained and audited.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Reason {
    /// A file of the crate changed.
    FileChanged { path: RepoPath },
    /// The crate depends on a crate that changed.
    DependsOnCrate { package: PackageName },
    /// The crate depends on an external package that `Cargo.lock` now
    /// resolves differently. `features` are the crate's own features that
    /// turn on the optional dependencies leading to it; empty when the
    /// crate's default features already do.
    DependsOnExternal {
        name: String,
        version: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
    },
    /// The crate is built from a package whose entry in the root manifest's
    /// `[workspace.dependencies]` changed.
    DependsOnWorkspaceDependency { name: String },
    /// A test refers, directly or through other definitions, to a changed
    /// definition.
    Reaches { changed: SymbolId },
    /// Selection could not be narrowed and fell back to something wider.
    FellBack { cause: FallbackCause },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum FallbackCause {
    /// A file that changes how every crate builds.
    WorkspaceGlobal { path: RepoPath },
    /// The reference graph has no entry for the file: it is new, deleted, or
    /// not compiled in the configuration the index was built for.
    NotInIndex { path: RepoPath },
    /// A changed line is outside every definition: an import, an attribute,
    /// an `impl` header, a macro call. `line` is one-based.
    OutsideDefinitions { path: RepoPath, line: u32 },
    /// A reached test's name cannot be derived from its symbol.
    UnnamedTest { test: SymbolId },
    /// The crate's references could not be recorded, and the change touches
    /// the crate or one it depends on.
    NotRecorded { package: PackageName },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CratePlan {
    pub selection: Selection,
    pub reasons: Vec<Reason>,
    /// Features of the crate to turn on when running its tests, by name:
    /// without them the build does not include what the reasons name.
    /// Empty when the default features are enough.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub features: BTreeSet<String>,
}

/// Why an API scenario was selected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ScenarioReason {
    /// The scenario file itself changed.
    FileChanged,
    /// A handler the scenario calls refers to a changed definition.
    Reaches {
        changed: SymbolId,
        handler: SymbolId,
    },
    /// The crate under test runs all its tests, so the change could not be
    /// traced to handlers.
    CrateNotNarrowed { package: PackageName },
    /// A request of the scenario could not be tied to a handler, and the
    /// crate under test is affected by the change.
    Untraced { package: PackageName },
}

/// The tests to run for one [`ChangeSet`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TestPlan {
    /// Crates whose tests run. Crates not listed run nothing.
    pub crates: BTreeMap<PackageName, CratePlan>,
    /// API scenarios to run. Scenarios not listed are skipped.
    pub scenarios: BTreeMap<RepoPath, Vec<ScenarioReason>>,
    /// Changed files no rule covers. They select no test; a caller that gates
    /// a deployment on this plan should treat a non-empty list as a failure.
    pub uncovered: Vec<RepoPath>,
}

/// What narrowing inside a crate needs: the reference graph and the changed
/// lines of each file.
pub struct Narrowing<'a> {
    pub symbols: &'a ReferenceGraph,
    pub lines: &'a BTreeMap<RepoPath, Vec<ChangedLine>>,
}

/// Builds a [`TestPlan`]. Each optional input narrows the plan further;
/// without any, a changed crate and its dependents run all their tests.
pub struct Planner<'a> {
    graph: &'a WorkspaceGraph,
    lock_change: Option<&'a LockChange>,
    manifest_change: Option<&'a ManifestChange>,
    narrowing: Option<Narrowing<'a>>,
    scenarios: &'a [Scenario],
    scenario_roots: &'a [RepoPath],
}

impl<'a> Planner<'a> {
    pub fn new(graph: &'a WorkspaceGraph) -> Self {
        Self {
            graph,
            lock_change: None,
            manifest_change: None,
            narrowing: None,
            scenarios: &[],
            scenario_roots: &[],
        }
    }

    /// Narrows a `Cargo.lock` change to the crates that depend on the
    /// packages it touches.
    pub fn with_lock_change(mut self, change: &'a LockChange) -> Self {
        self.lock_change = Some(change);
        self
    }

    /// Narrows a change to the root `Cargo.toml` to the crates built from
    /// the `[workspace.dependencies]` entries it touches.
    pub fn with_manifest_change(mut self, change: &'a ManifestChange) -> Self {
        self.manifest_change = Some(change);
        self
    }

    /// Narrows a change inside a crate to the tests that reach it.
    pub fn with_narrowing(mut self, narrowing: Narrowing<'a>) -> Self {
        self.narrowing = Some(narrowing);
        self
    }

    /// Selects among these scenarios. `roots` are the directories that hold
    /// scenario files: a change under one selects scenarios, not crates.
    pub fn with_scenarios(mut self, scenarios: &'a [Scenario], roots: &'a [RepoPath]) -> Self {
        self.scenarios = scenarios;
        self.scenario_roots = roots;
        self
    }

    pub fn plan(&self, changes: &ChangeSet) -> Result<TestPlan, WorkspaceError> {
        // Crates that run every test, with the reasons.
        let mut whole: BTreeMap<PackageName, BTreeSet<Reason>> = BTreeMap::new();
        // Crates whose own files changed in a way that cannot be narrowed.
        let mut origins = BTreeSet::new();
        // Crates with a change attributed to a definition that is not a
        // test function.
        let mut production_crates = BTreeSet::new();
        // Crates with a change attributed to any definition.
        let mut narrowed_crates = BTreeSet::new();
        let mut changed_symbols = BTreeSet::new();
        let mut uncovered = Vec::new();
        let lockfile = self.graph.lockfile();
        let root_manifest = self.graph.root_manifest();

        for file in changes.files() {
            let path = &file.path;
            if self.is_scenario_file(path) {
                continue;
            }
            match &file.kind {
                FileKind::WorkspaceGlobal => {
                    let narrowed = if lockfile.as_ref() == Some(path) {
                        self.lock_dependents(&mut whole)?
                    } else if root_manifest.as_ref() == Some(path) {
                        self.manifest_dependents(&mut whole)?
                    } else {
                        false
                    };
                    if !narrowed {
                        let cause = FallbackCause::WorkspaceGlobal { path: path.clone() };
                        for package in self.graph.packages() {
                            whole.entry(package).or_default().insert(Reason::FellBack {
                                cause: cause.clone(),
                            });
                        }
                    }
                }
                FileKind::Outside => uncovered.push(path.clone()),
                FileKind::RustSource { package } if self.narrowing.is_some() => {
                    match self.attribute(path) {
                        Ok(symbols) => {
                            let only_tests = symbols.iter().all(|symbol| {
                                self.narrowing
                                    .as_ref()
                                    .is_some_and(|n| n.symbols.test(symbol).is_some())
                            });
                            if !only_tests {
                                production_crates.insert(package.clone());
                            }
                            narrowed_crates.insert(package.clone());
                            changed_symbols.extend(symbols);
                        }
                        Err(cause) => {
                            origins.insert(package.clone());
                            whole
                                .entry(package.clone())
                                .or_default()
                                .insert(Reason::FellBack { cause });
                        }
                    }
                }
                FileKind::RustSource { package }
                | FileKind::CrateManifest { package }
                | FileKind::CrateData { package } => {
                    origins.insert(package.clone());
                    whole
                        .entry(package.clone())
                        .or_default()
                        .insert(Reason::FileChanged { path: path.clone() });
                }
            }
        }

        for (package, dependency) in self.graph.affected_by(&origins)? {
            if let Dependency::DependsOn(origin) = dependency {
                whole
                    .entry(package)
                    .or_default()
                    .insert(Reason::DependsOnCrate { package: origin });
            }
        }

        // A crate whose references were not recorded cannot be narrowed: when
        // a narrowed change touches it or anything it depends on, it runs
        // every test.
        if let Some(narrowing) = &self.narrowing {
            let unrecorded = narrowing.symbols.unrecorded();
            if !unrecorded.is_empty() {
                for package in self.graph.affected_by(&narrowed_crates)?.into_keys() {
                    if unrecorded.contains(&package) {
                        let cause = FallbackCause::NotRecorded {
                            package: package.clone(),
                        };
                        whole
                            .entry(package)
                            .or_default()
                            .insert(Reason::FellBack { cause });
                    }
                }
            }
        }

        // Tests reached through the reference graph, in crates that do not
        // already run everything.
        let reached = match &self.narrowing {
            Some(narrowing) => narrowing.symbols.affected_by(&changed_symbols),
            None => BTreeMap::new(),
        };
        let mut tests: BTreeMap<PackageName, (BTreeSet<TestName>, BTreeSet<Reason>)> =
            BTreeMap::new();
        if let Some(narrowing) = &self.narrowing {
            for (symbol, origin) in &reached {
                let Some(test) = narrowing.symbols.test(symbol) else {
                    continue;
                };
                match &test.name {
                    Some(name) => {
                        let (names, reasons) = tests.entry(test.package.clone()).or_default();
                        names.insert(name.clone());
                        reasons.insert(Reason::Reaches {
                            changed: origin.clone(),
                        });
                    }
                    None => {
                        let cause = FallbackCause::UnnamedTest {
                            test: symbol.clone(),
                        };
                        whole
                            .entry(test.package.clone())
                            .or_default()
                            .insert(Reason::FellBack { cause });
                    }
                }
            }
        }

        let scenarios =
            self.select_scenarios(changes, &whole, &origins, &production_crates, &reached)?;

        let mut crates = BTreeMap::new();
        for (package, (names, reasons)) in tests {
            if !whole.contains_key(&package) {
                let plan = CratePlan {
                    selection: Selection::Tests(names),
                    reasons: reasons.into_iter().collect(),
                    features: BTreeSet::new(),
                };
                crates.insert(package, plan);
            }
        }
        for (package, reasons) in whole {
            let features = reasons
                .iter()
                .filter_map(|reason| match reason {
                    Reason::DependsOnExternal { features, .. } => Some(features),
                    _ => None,
                })
                .flatten()
                .cloned()
                .collect();
            let plan = CratePlan {
                selection: Selection::All,
                reasons: reasons.into_iter().collect(),
                features,
            };
            crates.insert(package, plan);
        }
        Ok(TestPlan {
            crates,
            scenarios,
            uncovered,
        })
    }

    fn is_scenario_file(&self, path: &RepoPath) -> bool {
        self.scenario_roots
            .iter()
            .any(|root| path.as_path().starts_with(root.as_path()))
    }

    /// Adds the crates that depend on the packages a lockfile change touches.
    /// Returns `false` when the change cannot be narrowed.
    fn lock_dependents(
        &self,
        whole: &mut BTreeMap<PackageName, BTreeSet<Reason>>,
    ) -> Result<bool, WorkspaceError> {
        let Some(LockChange::Packages(packages)) = self.lock_change else {
            return Ok(false);
        };
        let mut selected = Vec::new();
        for package in packages {
            let dependents = self
                .graph
                .dependents_of_external(&package.name, &package.version)?;
            // A package the graph does not know cannot be traced.
            let Some(dependents) = dependents else {
                return Ok(false);
            };
            selected.push((package, dependents));
        }
        for (package, dependents) in selected {
            for (dependent, activation) in dependents {
                let features = match activation {
                    Activation::Default => Vec::new(),
                    Activation::Features(features) => features,
                };
                whole
                    .entry(dependent)
                    .or_default()
                    .insert(Reason::DependsOnExternal {
                        name: package.name.clone(),
                        version: package.version.clone(),
                        features,
                    });
            }
        }
        Ok(true)
    }

    /// Adds the crates built from the `[workspace.dependencies]` entries a
    /// root manifest change touches. Returns `false` when the change cannot
    /// be narrowed.
    fn manifest_dependents(
        &self,
        whole: &mut BTreeMap<PackageName, BTreeSet<Reason>>,
    ) -> Result<bool, WorkspaceError> {
        let Some(ManifestChange::Dependencies(names)) = self.manifest_change else {
            return Ok(false);
        };
        for name in names {
            for dependent in self.graph.dependents_of_named(name)? {
                whole
                    .entry(dependent)
                    .or_default()
                    .insert(Reason::DependsOnWorkspaceDependency { name: name.clone() });
            }
        }
        Ok(true)
    }

    /// The definitions the changed lines of `path` belong to.
    fn attribute(&self, path: &RepoPath) -> Result<BTreeSet<SymbolId>, FallbackCause> {
        let not_in_index = || FallbackCause::NotInIndex { path: path.clone() };
        let narrowing = self.narrowing.as_ref().ok_or_else(not_in_index)?;
        if !narrowing.symbols.has_file(path) {
            return Err(not_in_index());
        }
        let lines = narrowing.lines.get(path).ok_or_else(not_in_index)?;
        let mut symbols = BTreeSet::new();
        for changed in lines {
            let mut owners = narrowing.symbols.definitions_at_line(path, changed.line);
            // A change between two lines is inside a definition only when
            // both lines are.
            if let Some(next) = changed.gap_to {
                let after = narrowing.symbols.definitions_at_line(path, next);
                owners.retain(|owner| after.contains(owner));
            }
            if !owners.is_empty() {
                symbols.extend(owners.into_iter().cloned());
            } else if !is_blank_or_comment(changed) {
                return Err(FallbackCause::OutsideDefinitions {
                    path: path.clone(),
                    line: changed.line + 1,
                });
            }
        }
        Ok(symbols)
    }

    fn select_scenarios(
        &self,
        changes: &ChangeSet,
        whole: &BTreeMap<PackageName, BTreeSet<Reason>>,
        origins: &BTreeSet<PackageName>,
        production_crates: &BTreeSet<PackageName>,
        reached: &BTreeMap<SymbolId, SymbolId>,
    ) -> Result<BTreeMap<RepoPath, Vec<ScenarioReason>>, WorkspaceError> {
        if self.scenarios.is_empty() {
            return Ok(BTreeMap::new());
        }
        let changed_paths: BTreeSet<&RepoPath> =
            changes.files().iter().map(|file| &file.path).collect();
        // Every crate the change touches at all, narrowed or not. A crate
        // whose only changes are inside test functions is left out: a test
        // is not part of the binary a scenario talks to.
        let touched: BTreeSet<PackageName> = origins.union(production_crates).cloned().collect();
        let mut affected: BTreeSet<PackageName> =
            self.graph.affected_by(&touched)?.into_keys().collect();
        affected.extend(whole.keys().cloned());

        let mut selected = BTreeMap::new();
        for scenario in self.scenarios {
            let mut reasons = BTreeSet::new();
            if changed_paths.contains(&scenario.path) {
                reasons.insert(ScenarioReason::FileChanged);
            }
            for handler in &scenario.handlers {
                if let Some(origin) = reached.get(handler) {
                    reasons.insert(ScenarioReason::Reaches {
                        changed: origin.clone(),
                        handler: handler.clone(),
                    });
                }
            }
            if let Some(package) = &scenario.package {
                if whole.contains_key(package) {
                    reasons.insert(ScenarioReason::CrateNotNarrowed {
                        package: package.clone(),
                    });
                } else if scenario.untraced && affected.contains(package) {
                    reasons.insert(ScenarioReason::Untraced {
                        package: package.clone(),
                    });
                }
            }
            if !reasons.is_empty() {
                selected.insert(scenario.path.clone(), reasons.into_iter().collect());
            }
        }
        Ok(selected)
    }
}

/// Lines added on their own that hold no code. Outside a definition, such
/// lines change nothing a test can observe.
fn is_blank_or_comment(changed: &ChangedLine) -> bool {
    let Some(text) = &changed.text else {
        return false;
    };
    let holds_no_code = |line: &str| line.trim().is_empty() || line.trim().starts_with("//");
    changed.addition_only && text.lines().all(holds_no_code)
}

impl TestPlan {
    /// Plans at crate resolution: a changed crate and every workspace crate
    /// that depends on it run all their tests.
    pub fn for_changes(
        changes: &ChangeSet,
        graph: &WorkspaceGraph,
    ) -> Result<Self, WorkspaceError> {
        Planner::new(graph).plan(changes)
    }

    /// Whether the plan runs `test` of `package`. The crate name may be
    /// written with `_` for `-`, as cargo names test binaries.
    pub fn selects(&self, package: &str, test: &str) -> bool {
        let same_crate = |name: &str| name.replace('-', "_") == package.replace('-', "_");
        self.crates
            .iter()
            .find(|(name, _)| same_crate(name.as_str()))
            .is_some_and(|(_, plan)| match &plan.selection {
                Selection::All => true,
                Selection::Tests(names) => names.iter().any(|name| name.as_str() == test),
            })
    }

    /// Keeps the crates in `crates` and the scenarios `keep_scenario`
    /// accepts, and drops the rest. For a caller that tests one deployable:
    /// the tests of a crate it is not built from say nothing about it.
    pub fn restrict_to(
        &mut self,
        crates: &BTreeSet<PackageName>,
        keep_scenario: impl Fn(&RepoPath) -> bool,
    ) {
        self.crates.retain(|package, _| crates.contains(package));
        self.scenarios.retain(|scenario, _| keep_scenario(scenario));
    }

    /// The selection as arguments for `cargo test`, one list per invocation,
    /// for workspaces that do not use nextest:
    ///
    /// - the crates that run all their tests, as `--package` arguments;
    /// - the crates that run some, followed by `-- --exact` and the names.
    ///
    /// The second invocation gives every name to every crate in it, so a
    /// test runs in a crate that did not select it when another crate
    /// selected a test of the same name. That runs more, never less.
    ///
    /// A crate that needs features to build what it was selected for adds
    /// `--features <crate>/<feature>` to its invocation.
    ///
    /// The caller's own arguments go before these: the list may end with the
    /// arguments of the test binary.
    pub fn cargo_test_arguments(&self) -> Vec<Vec<String>> {
        let packages = |all: bool| -> Vec<String> {
            let crates: BTreeMap<&PackageName, &CratePlan> = self
                .crates
                .iter()
                .filter(|(_, plan)| (plan.selection == Selection::All) == all)
                .collect();
            let mut arguments: Vec<String> = crates
                .keys()
                .flat_map(|package| ["--package".to_owned(), package.to_string()])
                .collect();
            let features = Self::qualified_features(crates);
            if !features.is_empty() {
                arguments.extend(["--features".to_owned(), features.join(",")]);
            }
            arguments
        };
        let mut invocations = Vec::new();
        let whole = packages(true);
        if !whole.is_empty() {
            invocations.push(whole);
        }
        let mut some = packages(false);
        if !some.is_empty() {
            let names: BTreeSet<&str> = self
                .crates
                .values()
                .filter_map(|plan| match &plan.selection {
                    Selection::All => None,
                    Selection::Tests(names) => Some(names),
                })
                .flatten()
                .map(TestName::as_str)
                .collect();
            some.extend(["--".to_owned(), "--exact".to_owned()]);
            some.extend(names.into_iter().map(str::to_owned));
            invocations.push(some);
        }
        invocations
    }

    /// The features every selected crate needs, as `<crate>/<feature>`: the
    /// form `--features` takes when several crates are built at once.
    pub fn features(&self) -> Vec<String> {
        Self::qualified_features(&self.crates)
    }

    fn qualified_features<'p>(
        crates: impl IntoIterator<Item = (&'p PackageName, &'p CratePlan)>,
    ) -> Vec<String> {
        crates
            .into_iter()
            .flat_map(|(package, plan)| {
                plan.features
                    .iter()
                    .map(move |feature| format!("{package}/{feature}"))
            })
            .collect()
    }

    /// The nextest filterset that runs the selection of one crate.
    pub fn crate_filterset(package: &PackageName, plan: &CratePlan) -> String {
        match &plan.selection {
            Selection::All => format!("package(={package})"),
            Selection::Tests(names) => {
                let tests: Vec<String> =
                    names.iter().map(|name| format!("test(={name})")).collect();
                format!("(package(={package}) & ({}))", tests.join(" | "))
            }
        }
    }

    /// The plan as GitHub Actions step outputs, so that one job can plan and
    /// every later job can read what it needs from `needs.<job>.outputs`:
    ///
    /// - `any`: `true` when a test or scenario is selected.
    /// - `packages`: the selected crates, as a JSON array.
    /// - `filtersets`: crate -> the nextest filterset for that crate.
    /// - `filterset`: one filterset for everything; empty when no test is
    ///   selected.
    /// - `features`: the features the selected crates need, as a JSON array
    ///   of `<crate>/<feature>`; pass them to `--features`.
    /// - `matrix`: `{"include":[{"package":…,"filterset":…,"features":[…]}]}`
    ///   for `strategy.matrix`.
    /// - `scenarios`: the selected scenario files, as a JSON array.
    /// - `uncovered`: changed files no rule covers, as a JSON array.
    ///
    /// Each value is on one line.
    pub fn github_outputs(&self) -> Vec<(&'static str, String)> {
        let filtersets: BTreeMap<&PackageName, String> = self
            .crates
            .iter()
            .map(|(package, plan)| (package, Self::crate_filterset(package, plan)))
            .collect();
        let include = self.matrix_include();
        let json = |value: serde_json::Value| value.to_string();
        vec![
            (
                "any",
                (!self.crates.is_empty() || !self.scenarios.is_empty()).to_string(),
            ),
            (
                "packages",
                json(serde_json::json!(self.crates.keys().collect::<Vec<_>>())),
            ),
            ("filtersets", json(serde_json::json!(filtersets))),
            ("filterset", self.nextest_filterset().unwrap_or_default()),
            ("features", json(serde_json::json!(self.features()))),
            ("matrix", json(serde_json::json!({ "include": include }))),
            (
                "scenarios",
                json(serde_json::json!(self.scenarios.keys().collect::<Vec<_>>())),
            ),
            ("uncovered", json(serde_json::json!(self.uncovered))),
        ]
    }

    /// One GitHub Actions matrix entry per selected crate: its name, its
    /// nextest filterset and the features it needs as `<crate>/<feature>`.
    pub fn matrix_include(&self) -> Vec<serde_json::Value> {
        self.crates
            .iter()
            .map(|(package, plan)| {
                serde_json::json!({
                    "package": package,
                    "filterset": Self::crate_filterset(package, plan),
                    "features": Self::qualified_features([(package, plan)]),
                })
            })
            .collect()
    }

    /// A nextest filterset that runs exactly the selected tests, or `None`
    /// when no test is selected.
    pub fn nextest_filterset(&self) -> Option<String> {
        if self.crates.is_empty() {
            return None;
        }
        let terms: Vec<String> = self
            .crates
            .iter()
            .map(|(package, plan)| Self::crate_filterset(package, plan))
            .collect();
        Some(terms.join(" + "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crate_plan(selection: Selection, features: &[&str]) -> CratePlan {
        CratePlan {
            selection,
            reasons: Vec::new(),
            features: features
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
        }
    }

    fn name(raw: &str) -> PackageName {
        serde_json::from_value(serde_json::json!(raw)).expect("a package name")
    }

    fn some_tests() -> Selection {
        let test: TestName = serde_json::from_value(serde_json::json!("t")).expect("a test name");
        Selection::Tests(BTreeSet::from([test]))
    }

    #[test]
    fn each_invocation_turns_on_the_features_of_its_own_crates() {
        let plan = TestPlan {
            crates: BTreeMap::from([
                (name("a"), crate_plan(Selection::All, &["x", "y"])),
                (name("b"), crate_plan(Selection::All, &[])),
                (name("c"), crate_plan(some_tests(), &["z"])),
            ]),
            ..TestPlan::default()
        };

        assert_eq!(
            plan.cargo_test_arguments(),
            [
                vec!["--package", "a", "--package", "b", "--features", "a/x,a/y"],
                vec!["--package", "c", "--features", "c/z", "--", "--exact", "t"],
            ]
        );
        assert_eq!(plan.features(), ["a/x", "a/y", "c/z"]);
    }

    #[test]
    fn a_plan_saved_before_features_existed_still_reads() {
        let json = r#"{
            "crates": {"a": {"selection": "all", "reasons": [
                {"reason": "depends_on_external", "name": "e", "version": "1.0.0"}
            ]}},
            "scenarios": {},
            "uncovered": []
        }"#;
        let plan: TestPlan = serde_json::from_str(json).expect("reads");

        let crate_plan = &plan.crates[&name("a")];
        assert!(crate_plan.features.is_empty());
        assert_eq!(
            crate_plan.reasons,
            [Reason::DependsOnExternal {
                name: "e".into(),
                version: "1.0.0".into(),
                features: Vec::new(),
            }]
        );
        assert_eq!(plan.cargo_test_arguments(), [["--package", "a"]]);
    }
}
