//! Lockfile changes that reach crates through optional dependencies, against
//! the fixture workspace in `tests/fixtures/optional`. `ext` stands in for
//! an external package:
//!
//! ```text
//! op-eval      --(optional, feature `evaluation`)--> ext
//! op-server    --> op-eval    feature `evaluation` = ["op-eval/evaluation"]
//! op-plain     --> op-eval    no feature turns `ext` on
//! op-pinned    --> op-eval    with features = ["evaluation"]
//! op-devtest   --(dev)--> op-eval  with features = ["evaluation"]
//! op-bydefault --(optional, on by default)--> ext
//! op-combo     --(optional, feature `with-eval`)--> op-eval
//!              feature `evaluation` = ["op-eval?/evaluation"]
//! ```

use std::collections::BTreeSet;

use camino::Utf8PathBuf;
use pion::lockfile::{LockChange, LockedPackage, Lockfile};
use pion::reach::{LockPair, Reach, ReachReason};
use pion::{ChangeSet, PackageName, Planner, Reason, RepoPath, TestPlan, WorkspaceGraph};

fn fixture() -> WorkspaceGraph {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/optional")
        .canonicalize_utf8()
        .expect("fixture workspace exists");
    WorkspaceGraph::load(&root.join("Cargo.toml"), &root).expect("fixture workspace loads")
}

fn ext_changed(graph: &WorkspaceGraph) -> TestPlan {
    plan_for(graph, &["Cargo.lock"], &[])
}

fn deployables(graph: &WorkspaceGraph, names: &[&str]) -> BTreeSet<PackageName> {
    names
        .iter()
        .map(|name| graph.package(name).expect("fixture crate"))
        .collect()
}

/// A plan for `paths`, where `Cargo.lock` means `ext` changed. With
/// `deployables`, it is a plan for those binaries, as `--for-package` makes
/// it.
fn plan_for(graph: &WorkspaceGraph, paths: &[&str], binaries: &[&str]) -> TestPlan {
    let paths = paths
        .iter()
        .map(|path| RepoPath::parse(path).expect("path"));
    let changes = ChangeSet::classify(paths, graph);
    let change = LockChange::Packages(vec![LockedPackage {
        name: "ext".into(),
        version: "1.0.0".into(),
        source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
        checksum: None,
    }]);
    let binaries = deployables(graph, binaries);
    let mut planner = Planner::new(graph).with_lock_change(&change);
    if !binaries.is_empty() {
        planner = planner.for_deployables(&binaries);
    }
    let mut plan = planner.plan(&changes).expect("plan");
    if !binaries.is_empty() {
        let built_from = graph.shipped(&binaries).expect("shipped").crates();
        plan.restrict_to(&built_from, |_| true);
    }
    plan
}

fn selected(plan: &TestPlan) -> Vec<&str> {
    plan.crates.keys().map(|name| name.as_str()).collect()
}

fn features(plan: &TestPlan, package: &str) -> Vec<String> {
    let (_, crate_plan) = plan
        .crates
        .iter()
        .find(|(name, _)| name.as_str() == package)
        .unwrap_or_else(|| panic!("{package} is selected"));
    crate_plan.features.iter().cloned().collect()
}

#[test]
fn only_crates_that_can_build_the_package_are_selected() {
    let plan = ext_changed(&fixture());

    let selected: Vec<&str> = plan.crates.keys().map(|name| name.as_str()).collect();
    // op-plain depends on op-eval, but nothing it can turn on builds `ext`.
    assert_eq!(
        selected,
        [
            "op-bydefault",
            "op-combo",
            "op-devtest",
            "op-eval",
            "op-pinned",
            "op-server"
        ]
    );
}

#[test]
fn an_optional_dependency_selects_the_feature_that_turns_it_on() {
    let plan = ext_changed(&fixture());

    // `full` turns `ext` on as well, but also more than it needs.
    assert_eq!(features(&plan, "op-eval"), ["evaluation"]);
    // A feature forwarded from another workspace crate is the crate's own.
    assert_eq!(features(&plan, "op-server"), ["evaluation"]);
    assert_eq!(
        plan.crates
            .iter()
            .find(|(name, _)| name.as_str() == "op-eval")
            .map(|(_, crate_plan)| crate_plan.reasons.clone()),
        Some(vec![Reason::DependsOnExternal {
            name: "ext".into(),
            version: "1.0.0".into(),
            features: vec!["evaluation".into()],
        }])
    );
}

#[test]
fn features_that_only_together_turn_the_dependency_on_are_all_selected() {
    let plan = ext_changed(&fixture());

    // Neither `with-eval` nor the weak `evaluation` builds `ext` alone.
    assert_eq!(features(&plan, "op-combo"), ["evaluation", "with-eval"]);
}

#[test]
fn a_dependency_on_by_default_needs_no_feature() {
    let plan = ext_changed(&fixture());

    for package in ["op-bydefault", "op-devtest", "op-pinned"] {
        assert!(features(&plan, package).is_empty(), "{package}");
    }
}

#[test]
fn the_features_reach_every_output_that_can_say_them() {
    let plan = ext_changed(&fixture());

    assert_eq!(
        plan.cargo_test_arguments(),
        [[
            "--package",
            "op-bydefault",
            "--package",
            "op-combo",
            "--package",
            "op-devtest",
            "--package",
            "op-eval",
            "--package",
            "op-pinned",
            "--package",
            "op-server",
            "--features",
            "op-combo/evaluation,op-combo/with-eval,op-eval/evaluation,op-server/evaluation",
        ]]
    );
    assert_eq!(
        plan.features(),
        [
            "op-combo/evaluation",
            "op-combo/with-eval",
            "op-eval/evaluation",
            "op-server/evaluation"
        ]
    );
    let entry = plan
        .matrix_include()
        .into_iter()
        .find(|entry| entry["package"] == "op-eval")
        .expect("op-eval has an entry");
    assert_eq!(entry["features"], serde_json::json!(["op-eval/evaluation"]));

    let outputs: BTreeSet<(&str, String)> = plan.github_outputs().into_iter().collect();
    assert!(outputs.contains(&(
        "features",
        r#"["op-combo/evaluation","op-combo/with-eval","op-eval/evaluation","op-server/evaluation"]"#
            .to_owned()
    )));
}

#[test]
fn a_saved_plan_keeps_the_features() {
    let plan = ext_changed(&fixture());

    let json = serde_json::to_string(&plan).expect("serializes");
    let read: TestPlan = serde_json::from_str(&json).expect("deserializes");

    assert_eq!(read, plan);
}

#[test]
fn a_binary_that_does_not_ship_the_package_selects_nothing_for_it() {
    let graph = fixture();

    // op-server's default build leaves `evaluation` off, so it never
    // contains `ext`: testing op-eval with the feature checks code it does
    // not ship.
    assert!(selected(&plan_for(&graph, &["Cargo.lock"], &["op-server"])).is_empty());
    assert!(selected(&plan_for(&graph, &["Cargo.lock"], &["op-plain"])).is_empty());
}

#[test]
fn a_binary_selects_only_the_features_it_turns_on() {
    let graph = fixture();
    let plan = plan_for(&graph, &["Cargo.lock"], &["op-pinned"]);

    // op-pinned turns `evaluation` on for op-eval, and so ships `ext`.
    assert_eq!(selected(&plan), ["op-eval", "op-pinned"]);
    assert_eq!(features(&plan, "op-eval"), ["evaluation"]);
    assert!(features(&plan, "op-pinned").is_empty());
}

#[test]
fn a_binary_still_selects_crates_for_other_reasons() {
    let graph = fixture();
    let plan = plan_for(&graph, &["Cargo.lock", "eval/src/lib.rs"], &["op-server"]);

    assert_eq!(selected(&plan), ["op-eval", "op-server"]);
    assert!(features(&plan, "op-eval").is_empty());
    assert_eq!(
        plan.crates[&graph.package("op-eval").expect("crate")].reasons,
        [Reason::FileChanged {
            path: RepoPath::parse("eval/src/lib.rs").expect("path")
        }]
    );
}

#[test]
fn a_binary_is_built_from_the_crates_its_features_turn_on() {
    let graph = fixture();
    let crates = |name: &str| -> Vec<String> {
        graph
            .built_from(&graph.package(name).expect("crate"))
            .expect("closure")
            .iter()
            .map(ToString::to_string)
            .collect()
    };

    // op-combo's optional op-eval is off by default.
    assert_eq!(crates("op-combo"), ["op-combo"]);
    assert_eq!(crates("op-server"), ["op-eval", "op-server"]);
}

/// `Cargo.lock` for the fixture, with `ext` as a registry package.
fn lock(ext_version: &str) -> String {
    let mut text = String::from("version = 4\n");
    for (member, dependencies) in [
        ("op-bydefault", "ext"),
        ("op-combo", "op-eval"),
        ("op-devtest", "op-eval"),
        ("op-eval", "ext"),
        ("op-pinned", "op-eval"),
        ("op-plain", "op-eval"),
        ("op-server", "op-eval"),
    ] {
        text += &format!(
            "\n[[package]]\nname = \"{member}\"\nversion = \"0.0.0\"\ndependencies = [\n \"{dependencies}\",\n]\n"
        );
    }
    text += &format!(
        "\n[[package]]\nname = \"ext\"\nversion = \"{ext_version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n"
    );
    text
}

#[test]
fn a_lock_change_reaches_only_the_binaries_that_ship_the_package() {
    let graph = fixture();
    let (base, head) = (
        Lockfile::parse(&lock("1.0.0")),
        Lockfile::parse(&lock("1.0.1")),
    );
    let changes = ChangeSet::classify([RepoPath::parse("Cargo.lock").expect("path")], &graph);
    let report = Reach::new(&graph)
        .with_locks(LockPair {
            base: &base,
            head: &head,
        })
        .of(
            &changes,
            &deployables(&graph, &["op-bydefault", "op-pinned", "op-server"]),
        )
        .expect("reach");

    let reached: Vec<&str> = report
        .packages
        .iter()
        .filter(|(_, reach)| reach.reached)
        .map(|(package, _)| package.as_str())
        .collect();
    assert_eq!(reached, ["op-bydefault", "op-pinned"]);
    assert_eq!(
        report.packages[&graph.package("op-pinned").expect("crate")].reasons,
        [
            ReachReason::LockChanged {
                name: "ext".into(),
                version: "1.0.0".into()
            },
            ReachReason::LockChanged {
                name: "ext".into(),
                version: "1.0.1".into()
            },
        ]
    );
}
