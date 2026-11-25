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
use pion::lockfile::{LockChange, LockedPackage};
use pion::{ChangeSet, Planner, Reason, RepoPath, TestPlan, WorkspaceGraph};

fn fixture() -> WorkspaceGraph {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/optional")
        .canonicalize_utf8()
        .expect("fixture workspace exists");
    WorkspaceGraph::load(&root.join("Cargo.toml"), &root).expect("fixture workspace loads")
}

fn ext_changed(graph: &WorkspaceGraph) -> TestPlan {
    let changes = ChangeSet::classify([RepoPath::parse("Cargo.lock").expect("path")], graph);
    let change = LockChange::Packages(vec![LockedPackage {
        name: "ext".into(),
        version: "1.0.0".into(),
        source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
        checksum: None,
    }]);
    Planner::new(graph)
        .with_lock_change(&change)
        .plan(&changes)
        .expect("plan")
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
