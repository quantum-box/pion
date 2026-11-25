use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand, ValueEnum};
use pion::git::{self, Revision, Side};
use pion::lockfile::Lockfile;
use pion::manifest::ManifestChange;
use pion::reach::{LockPair, Reach, ReachReason};
use pion::scip::Index;
use pion::{
    ChangeSet, FallbackCause, Narrowing, Planner, Reason, ReferenceGraph, RepoPath, ScenarioReason,
    Selection, TestPlan, WorkspaceGraph, audit, records, scenario,
};

type Failure = Box<dyn std::error::Error>;

/// Prints a line to standard output. When the reader has gone away
/// (`cargo pion plan | head`), there is nobody left to tell, so stop quietly
/// instead of panicking as `println!` would.
macro_rules! say {
    ($($argument:tt)*) => {{
        use std::io::Write as _;
        if writeln!(std::io::stdout(), $($argument)*).is_err() {
            std::process::exit(0);
        }
    }};
}

/// `cargo pion ...` arrives as `cargo-pion pion ...`.
#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo")]
enum Cargo {
    /// Select the tests a change can affect
    #[command(subcommand, version)]
    Pion(Action),
}

#[derive(Subcommand)]
enum Action {
    /// Print which tests to run for the changes since a revision
    Plan(PlanArgs),
    /// Print whether the changes since a revision reach what a crate ships
    Reach(ReachArgs),
    /// Print a plan saved with `plan --format json` in another format
    Render(RenderArgs),
    /// Run the selected tests with cargo-nextest
    Run(RunArgs),
    /// Build the index `plan --scip` reads, with rust-analyzer
    Index(IndexArgs),
    /// List the failed tests of a full run that a plan did not select
    Audit(AuditArgs),
    /// Print what would run if given lines changed, one JSON object per line
    Impact(ImpactArgs),
}

#[derive(clap::Args)]
struct Selecting {
    /// Revision to compare against (branch, tag or commit)
    #[arg(long)]
    base: Revision,
    /// Revision to plan for; the working tree when omitted
    #[arg(long)]
    head: Option<Revision>,
    /// Path to the workspace `Cargo.toml`
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: Utf8PathBuf,
    /// A SCIP index of the working tree (see `cargo pion index`). With it,
    /// a change inside a crate selects only the tests that reach it
    #[arg(long)]
    scip: Option<Utf8PathBuf>,
    /// A directory of reference records written by `pion-rustc` while the
    /// workspace compiled. An alternative to `--scip` that costs no separate
    /// analysis
    #[arg(long, value_name = "DIR", conflicts_with = "scip")]
    refs: Option<Utf8PathBuf>,
    /// Which revision the index or the records describe. `base` lets one
    /// index, built once for a commit, serve every change made on top of it
    #[arg(long, value_enum, default_value_t = Indexed::Head, alias = "refs-revision")]
    scip_revision: Indexed,
    /// A directory of API scenario files to select from; repeatable
    #[arg(long = "scenarios", value_name = "DIR")]
    scenario_dirs: Vec<Utf8PathBuf>,
    /// Keep only the tests of the crates this workspace crate is built from
    /// (itself and its normal and build dependencies), and the scenarios
    /// that belong to one of them. For testing one deployable; repeatable
    #[arg(long = "for-package", value_name = "NAME")]
    for_packages: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Indexed {
    /// The revision given as `--base`
    Base,
    /// The working tree
    Head,
}

#[derive(clap::Args)]
struct PlanArgs {
    #[command(flatten)]
    selecting: Selecting,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// Exit with status 2 when a changed file is covered by no rule
    #[arg(long)]
    deny_uncovered: bool,
}

#[derive(clap::Args)]
struct ReachArgs {
    /// Revision to compare against (branch, tag or commit)
    #[arg(long)]
    base: Revision,
    /// Revision to decide for; the working tree when omitted
    #[arg(long)]
    head: Option<Revision>,
    /// Path to the workspace `Cargo.toml`
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: Utf8PathBuf,
    /// A workspace crate that is released as a binary; repeatable
    #[arg(long = "package", value_name = "NAME", required = true)]
    packages: Vec<String>,
    #[arg(long, value_enum, default_value_t = ReachFormat::Text)]
    format: ReachFormat,
}

#[derive(Clone, Copy, ValueEnum)]
enum ReachFormat {
    /// Each crate, whether the change reaches it, and why
    Text,
    /// The whole answer as JSON
    Json,
}

#[derive(clap::Args)]
struct RenderArgs {
    /// A plan written by `cargo pion plan --format json`
    #[arg(long)]
    plan: Utf8PathBuf,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(clap::Args)]
struct RunArgs {
    #[command(flatten)]
    selecting: Selecting,
    /// Arguments passed on to `cargo nextest run`
    #[arg(last = true)]
    nextest_args: Vec<String>,
}

#[derive(clap::Args)]
struct IndexArgs {
    /// Path to the workspace `Cargo.toml`
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: Utf8PathBuf,
    /// Where to write the index
    #[arg(long, default_value = "target/pion/index.scip")]
    output: Utf8PathBuf,
}

#[derive(clap::Args)]
struct AuditArgs {
    /// A plan written by `cargo pion plan --format json`
    #[arg(long)]
    plan: Utf8PathBuf,
    /// A JUnit report of a run of every test (cargo-nextest writes one)
    #[arg(long)]
    junit: Vec<Utf8PathBuf>,
    /// A log with the output of `cargo test`, such as a CI job log
    #[arg(long)]
    libtest: Vec<Utf8PathBuf>,
}

#[derive(clap::Args)]
struct ImpactArgs {
    /// Path to the workspace `Cargo.toml`
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: Utf8PathBuf,
    /// A SCIP index of the working tree (see `cargo pion index`)
    #[arg(long, required_unless_present = "refs")]
    scip: Option<Utf8PathBuf>,
    /// A directory of reference records written by `pion-rustc`
    #[arg(long, value_name = "DIR", conflicts_with = "scip")]
    refs: Option<Utf8PathBuf>,
    /// A directory of API scenario files to select from; repeatable
    #[arg(long = "scenarios", value_name = "DIR")]
    scenario_dirs: Vec<Utf8PathBuf>,
    /// Lines to treat as changed, as `path:line` with the path relative to
    /// the repository root and the line one-based
    #[arg(required = true, value_name = "PATH:LINE")]
    lines: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    /// Crates, tests and scenarios, with the reasons they were selected
    Text,
    /// The whole plan as JSON
    Json,
    /// A filterset for `cargo nextest run -E`
    Nextest,
    /// The selected crates, one per line
    Packages,
    /// A GitHub Actions matrix: one entry per crate, with its filterset
    GithubMatrix,
    /// The selected scenario files, one per line
    Scenarios,
    /// Arguments for `cargo test`, one invocation per line: first the crates
    /// that run every test, then the crates that run some, with the names
    CargoTest,
    /// `key=value` lines to append to `$GITHUB_OUTPUT`: everything later
    /// jobs need, from one run of pion
    GithubOutput,
}

fn main() -> ExitCode {
    let Cargo::Pion(action) = Cargo::parse();
    let result = match action {
        Action::Plan(args) => plan(&args),
        Action::Reach(args) => reach(&args),
        Action::Render(args) => render(&args),
        Action::Run(args) => run(&args),
        Action::Index(args) => index(&args),
        Action::Audit(args) => audit_plan(&args),
        Action::Impact(args) => impact(&args),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn plan(args: &PlanArgs) -> Result<ExitCode, Failure> {
    let (changes, plan) = select(&args.selecting)?;
    print_plan(&plan, args.format, Some(changes.files().len()))?;
    if args.deny_uncovered && !plan.uncovered.is_empty() {
        return Ok(ExitCode::from(2));
    }
    Ok(ExitCode::SUCCESS)
}

/// Planning reads the whole reference graph, so a caller that wants several
/// formats plans once, saves the plan as JSON and renders the rest from it.
fn render(args: &RenderArgs) -> Result<ExitCode, Failure> {
    let plan: TestPlan = serde_json::from_str(&std::fs::read_to_string(&args.plan)?)?;
    print_plan(&plan, args.format, None)?;
    Ok(ExitCode::SUCCESS)
}

/// `changed_files` is known while planning and not kept in a saved plan.
fn print_plan(
    plan: &TestPlan,
    format: Format,
    changed_files: Option<usize>,
) -> Result<(), Failure> {
    match format {
        Format::Json => say!("{}", serde_json::to_string_pretty(plan)?),
        Format::Nextest => match plan.nextest_filterset() {
            Some(filterset) => {
                say!("{filterset}");
                let features = plan.features();
                if !features.is_empty() {
                    // A filterset cannot say how to build; tell the caller.
                    eprintln!("note: build with --features {}", features.join(","));
                }
            }
            None => eprintln!("no test is affected"),
        },
        Format::Packages => {
            for package in plan.crates.keys() {
                say!("{package}");
            }
        }
        Format::GithubMatrix => {
            say!(
                "{}",
                serde_json::json!({ "include": plan.matrix_include() })
            );
        }
        Format::GithubOutput => {
            for (key, value) in plan.github_outputs() {
                say!("{key}={value}");
            }
        }
        Format::Scenarios => {
            for scenario in plan.scenarios.keys() {
                say!("{scenario}");
            }
        }
        Format::CargoTest => {
            for arguments in plan.cargo_test_arguments() {
                say!("{}", arguments.join(" "));
            }
        }
        Format::Text => print_text(changed_files, plan),
    }
    Ok(())
}

fn run(args: &RunArgs) -> Result<ExitCode, Failure> {
    let (_, plan) = select(&args.selecting)?;
    let Some(filterset) = plan.nextest_filterset() else {
        eprintln!("no test is affected");
        return Ok(ExitCode::SUCCESS);
    };
    // A filterset chooses among the tests nextest has built; it does not
    // limit what gets built. Naming the crates does.
    let packages = plan
        .crates
        .keys()
        .flat_map(|package| ["--package", package.as_str()]);
    let features = plan.features();
    let features = (!features.is_empty())
        .then(|| ["--features".to_owned(), features.join(",")])
        .into_iter()
        .flatten();
    let status = Command::new("cargo")
        .args(["nextest", "run", "--manifest-path"])
        .arg(&args.selecting.manifest_path)
        .args(packages)
        .args(features)
        .args(["-E", &filterset])
        .args(&args.nextest_args)
        .status()?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn index(args: &IndexArgs) -> Result<ExitCode, Failure> {
    let manifest = args.manifest_path.canonicalize_utf8()?;
    let workspace = manifest.parent().ok_or("the manifest path has no parent")?;
    if let Some(directory) = args.output.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let log_path = Utf8PathBuf::from(format!("{}.log", args.output));
    let started = Instant::now();
    // rust-analyzer reports thousands of diagnostics on a large workspace.
    // They go to a log file; a summary by kind is printed instead.
    let output = Command::new("rust-analyzer")
        .arg("scip")
        .arg(workspace)
        .arg("--output")
        .arg(&args.output)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    let log = String::from_utf8_lossy(&output.stderr);
    std::fs::write(&log_path, log.as_bytes())?;
    if !output.status.success() {
        return Err(format!("`rust-analyzer scip` failed; see {log_path}").into());
    }

    let size = std::fs::metadata(&args.output)?.len();
    say!(
        "{} ({} MB, {} s)",
        args.output,
        size / 1_000_000,
        started.elapsed().as_secs()
    );
    let diagnostics = summarize_diagnostics(&log);
    if !diagnostics.is_empty() {
        say!("rust-analyzer diagnostics (full log: {log_path}):");
    }
    for (kind, count) in diagnostics {
        say!("  {count:>6}  {kind}{}", diagnostic_note(&kind));
    }
    Ok(ExitCode::SUCCESS)
}

/// Counts rust-analyzer's `ERROR` and `WARN` lines by message, most frequent
/// first. The part of a message that names a place or an id is dropped so
/// that repeats of one problem count together.
fn summarize_diagnostics(log: &str) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for line in log.lines() {
        let message = ["ERROR ", "WARN "]
            .iter()
            .find_map(|level| line.split_once(level).map(|(_, message)| message));
        let Some(message) = message else { continue };
        let kind = [" def=", " at ", ": ", "("]
            .iter()
            .filter_map(|cut| message.find(cut))
            .min()
            .map_or(message, |end| &message[..end]);
        *counts.entry(kind.trim().to_owned()).or_default() += 1;
    }
    let mut kinds: Vec<(String, usize)> = counts.into_iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    kinds
}

/// What a known diagnostic means for selection.
fn diagnostic_note(kind: &str) -> &'static str {
    if kind.starts_with("Encountered enclosing definition with no name") {
        "\n          harmless: an item inside a function body has no named parent; \
         pion does not read that field"
    } else if kind.starts_with("Bug: definition") {
        "\n          a definition generated by a macro has no place in a source file; \
         references to it cannot be followed"
    } else {
        ""
    }
}

fn audit_plan(args: &AuditArgs) -> Result<ExitCode, Failure> {
    let plan: TestPlan = serde_json::from_str(&std::fs::read_to_string(&args.plan)?)?;
    if args.junit.is_empty() && args.libtest.is_empty() {
        return Err("give the results of a full run with --junit or --libtest".into());
    }
    let mut outcomes = Vec::new();
    for path in &args.junit {
        outcomes.extend(audit::read_junit(&std::fs::read_to_string(path)?));
    }
    for path in &args.libtest {
        // CI logs can hold bytes that are not UTF-8.
        let log = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
        outcomes.extend(audit::read_libtest(&log));
    }
    let failed = outcomes.iter().filter(|outcome| outcome.failed).count();
    let misses = audit::missed(&plan, &outcomes);
    say!(
        "{} test(s) ran, {failed} failed, {} failure(s) the plan did not select",
        outcomes.len(),
        misses.len()
    );
    for miss in &misses {
        say!("  {} {}", miss.package, miss.test);
    }
    Ok(if misses.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The text of `path` at the base and at the head, when `path` is among the
/// changed files and exists at both.
fn two_revisions(
    repository: &Utf8Path,
    base: &Revision,
    head: Option<&Revision>,
    path: Option<RepoPath>,
    changed: &[RepoPath],
) -> Result<Option<(String, String)>, Failure> {
    let Some(path) = path.filter(|path| changed.contains(path)) else {
        return Ok(None);
    };
    let before = git::file_at(repository, base, &path)?;
    let after = match head {
        Some(head) => git::file_at(repository, head, &path)?,
        None => std::fs::read_to_string(repository.join(path.as_path())).ok(),
    };
    Ok(before.zip(after))
}

fn reach(args: &ReachArgs) -> Result<ExitCode, Failure> {
    let manifest = args.manifest_path.canonicalize_utf8()?;
    let manifest_dir = manifest.parent().ok_or("the manifest path has no parent")?;
    let repository = git::repository_root(manifest_dir)?.canonicalize_utf8()?;
    let head = args.head.as_ref();

    let graph = WorkspaceGraph::load(&manifest, &repository)?;
    let paths = git::changed_paths(&repository, &args.base, head)?;
    let locks = two_revisions(&repository, &args.base, head, graph.lockfile(), &paths)?
        .map(|(before, after)| (Lockfile::parse(&before), Lockfile::parse(&after)));
    let manifest_change =
        two_revisions(&repository, &args.base, head, graph.root_manifest(), &paths)?
            .map(|(before, after)| ManifestChange::between(&before, &after));

    let mut packages = BTreeSet::new();
    for name in &args.packages {
        let package = graph
            .package(name)
            .ok_or_else(|| format!("`{name}` is not a crate of this workspace"))?;
        packages.insert(package);
    }
    let changes = ChangeSet::classify(paths, &graph);
    let mut reach = Reach::new(&graph);
    if let Some((base, head)) = &locks {
        reach = reach.with_locks(LockPair { base, head });
    }
    if let Some(change) = &manifest_change {
        reach = reach.with_manifest_change(change);
    }
    let report = reach.of(&changes, &packages)?;

    match args.format {
        ReachFormat::Json => say!("{}", serde_json::to_string_pretty(&report)?),
        ReachFormat::Text => {
            for (package, reach) in &report.packages {
                if !reach.reached {
                    say!("{package}: not reached");
                    continue;
                }
                say!("{package}: reached");
                // A wide change gives hundreds of reasons; a few explain it.
                const SHOWN: usize = 5;
                for reason in reach.reasons.iter().take(SHOWN) {
                    say!("  {}", describe_reach(reason));
                }
                if reach.reasons.len() > SHOWN {
                    say!("  ... and {} more", reach.reasons.len() - SHOWN);
                }
            }
            if !report.outside.is_empty() {
                say!(
                    "{} changed file(s) outside every crate",
                    report.outside.len()
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn describe_reach(reason: &ReachReason) -> String {
    match reason {
        ReachReason::FileChanged { path, package } => format!("changed in {package}: {path}"),
        ReachReason::LockChanged { name, version } => {
            format!("resolved differently: {name} {version}")
        }
        ReachReason::WorkspaceDependencyChanged { name } => {
            format!("workspace dependency changed: {name}")
        }
        ReachReason::WorkspaceGlobal { path } => format!("changes every build: {path}"),
    }
}

/// Builds the reference graph from whichever source was given.
fn reference_graph(
    scip: Option<&Utf8Path>,
    refs: Option<&Utf8Path>,
    graph: &WorkspaceGraph,
) -> Result<Option<ReferenceGraph>, Failure> {
    if let Some(path) = scip {
        let index = Index::decode(&std::fs::read(path)?)?;
        return Ok(Some(ReferenceGraph::build(&index, graph)));
    }
    if let Some(directory) = refs {
        let records = records::read_directory(directory)?;
        return Ok(Some(ReferenceGraph::from_records(&records, graph)));
    }
    Ok(None)
}

/// Plans for each given line on its own, as if only that line had changed.
/// Each result also carries the number of crates the crate-level plan would
/// run, to show what the reference graph saves.
fn impact(args: &ImpactArgs) -> Result<ExitCode, Failure> {
    let manifest = args.manifest_path.canonicalize_utf8()?;
    let manifest_dir = manifest.parent().ok_or("the manifest path has no parent")?;
    let repository = git::repository_root(manifest_dir)?.canonicalize_utf8()?;
    let graph = WorkspaceGraph::load(&manifest, &repository)?;
    let symbols = reference_graph(args.scip.as_deref(), args.refs.as_deref(), &graph)?
        .ok_or("give --scip or --refs")?;
    let (scenarios, scenario_roots) = scenario::discover(
        &args.scenario_dirs,
        &repository,
        &graph,
        Some(&symbols),
        |path| std::fs::read_to_string(repository.join(path.as_path())).ok(),
    )?;

    for target in &args.lines {
        let (file, line) = target
            .rsplit_once(':')
            .ok_or_else(|| format!("`{target}` is not `path:line`"))?;
        let line: u32 = line.parse()?;
        let path = RepoPath::parse(file)?;
        let changes = ChangeSet::classify([path.clone()], &graph);
        let lines = BTreeMap::from([(
            path.clone(),
            vec![git::ChangedLine {
                line: line.saturating_sub(1),
                gap_to: None,
                text: None,
                addition_only: false,
            }],
        )]);
        let narrowed = Planner::new(&graph)
            .with_narrowing(Narrowing {
                symbols: &symbols,
                lines: &lines,
            })
            .with_scenarios(&scenarios, &scenario_roots)
            .plan(&changes)?;
        let crate_level = TestPlan::for_changes(&changes, &graph)?;

        let whole = narrowed
            .crates
            .values()
            .filter(|plan| plan.selection == Selection::All)
            .count();
        let tests: usize = narrowed
            .crates
            .values()
            .map(|plan| match &plan.selection {
                Selection::All => 0,
                Selection::Tests(tests) => tests.len(),
            })
            .sum();
        let definition = symbols
            .definitions_at_line(&path, line.saturating_sub(1))
            .into_iter()
            .next()
            .map(ToString::to_string);
        let fallbacks: BTreeSet<String> = narrowed
            .crates
            .values()
            .flat_map(|plan| &plan.reasons)
            .filter_map(|reason| match reason {
                Reason::FellBack { cause } => Some(describe(&Reason::FellBack {
                    cause: cause.clone(),
                })),
                _ => None,
            })
            .collect();
        say!(
            "{}",
            serde_json::json!({
                "path": path,
                "line": line,
                "definition": definition,
                "crate_level_crates": crate_level.crates.len(),
                "whole_crates": whole,
                "narrowed_crates": narrowed.crates.len() - whole,
                "tests": tests,
                "scenarios": narrowed.scenarios.len(),
                "scenarios_total": scenarios.len(),
                "fallbacks": fallbacks,
            })
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Reads the workspace and the change, and plans with every input given.
fn select(args: &Selecting) -> Result<(ChangeSet, TestPlan), Failure> {
    let manifest = args.manifest_path.canonicalize_utf8()?;
    let manifest_dir = manifest.parent().ok_or("the manifest path has no parent")?;
    let repository = git::repository_root(manifest_dir)?.canonicalize_utf8()?;
    let head = args.head.as_ref();

    let graph = WorkspaceGraph::load(&manifest, &repository)?;
    let paths = git::changed_paths(&repository, &args.base, head)?;

    let lock_change = two_revisions(&repository, &args.base, head, graph.lockfile(), &paths)?
        .map(|(before, after)| Lockfile::parse(&after).changes_since(&Lockfile::parse(&before)));
    let manifest_change =
        two_revisions(&repository, &args.base, head, graph.root_manifest(), &paths)?
            .map(|(before, after)| ManifestChange::between(&before, &after));

    let symbols = reference_graph(args.scip.as_deref(), args.refs.as_deref(), &graph)?;
    let lines = match &symbols {
        Some(_) => {
            let side = match args.scip_revision {
                Indexed::Base => Side::Base,
                Indexed::Head => Side::Head,
            };
            git::changed_lines(&repository, &args.base, head, side)?
        }
        None => BTreeMap::new(),
    };

    // Routes are read from the same revision the index describes. At the
    // base revision that means asking git, so first find the few files that
    // register routes instead of asking for every file.
    let registering_routes = match args.scip_revision {
        Indexed::Base if symbols.is_some() && !args.scenario_dirs.is_empty() => {
            git::files_containing(&repository, &args.base, ".route(")?
        }
        _ => BTreeSet::new(),
    };
    let read_source = |path: &RepoPath| match args.scip_revision {
        Indexed::Base => registering_routes
            .contains(path)
            .then(|| git::file_at(&repository, &args.base, path).ok().flatten())
            .flatten(),
        Indexed::Head => std::fs::read_to_string(repository.join(path.as_path())).ok(),
    };
    let (scenarios, scenario_roots) = scenario::discover(
        &args.scenario_dirs,
        &repository,
        &graph,
        symbols.as_ref(),
        read_source,
    )?;

    let changes = ChangeSet::classify(paths, &graph);
    let mut planner = Planner::new(&graph).with_scenarios(&scenarios, &scenario_roots);
    if let Some(change) = &lock_change {
        planner = planner.with_lock_change(change);
    }
    if let Some(change) = &manifest_change {
        planner = planner.with_manifest_change(change);
    }
    if let Some(symbols) = &symbols {
        planner = planner.with_narrowing(Narrowing {
            symbols,
            lines: &lines,
        });
    }
    let mut plan = planner.plan(&changes)?;
    if !args.for_packages.is_empty() {
        let mut built_from = BTreeSet::new();
        for name in &args.for_packages {
            let package = graph
                .package(name)
                .ok_or_else(|| format!("`{name}` is not a crate of this workspace"))?;
            built_from.extend(graph.built_from(&package)?);
        }
        plan.restrict_to(&built_from, |scenario| {
            graph
                .owner_of(scenario)
                .is_some_and(|owner| built_from.contains(owner))
        });
    }
    Ok((changes, plan))
}

fn print_text(changed_files: Option<usize>, plan: &TestPlan) {
    let changed = changed_files.map_or(String::new(), |count| format!("{count} changed file(s), "));
    say!(
        "{changed}{} crate(s) and {} scenario(s) to test",
        plan.crates.len(),
        plan.scenarios.len()
    );
    for (package, crate_plan) in &plan.crates {
        let features = if crate_plan.features.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = crate_plan.features.iter().map(String::as_str).collect();
            format!(", with features: {}", names.join(", "))
        };
        match &crate_plan.selection {
            Selection::All => say!("\n{package} (all tests{features})"),
            Selection::Tests(tests) => {
                say!("\n{package} ({} test(s){features})", tests.len());
                for test in tests {
                    say!("  test: {test}");
                }
            }
        }
        for reason in &crate_plan.reasons {
            say!("  {}", describe(reason));
        }
    }
    for (scenario, reasons) in &plan.scenarios {
        say!("\n{scenario}");
        for reason in reasons {
            match reason {
                ScenarioReason::FileChanged => say!("  the scenario changed"),
                ScenarioReason::Reaches { changed, handler } => {
                    say!("  {handler} reaches {changed}")
                }
                ScenarioReason::CrateNotNarrowed { package } => {
                    say!("  {package} runs all tests, so handlers were not traced")
                }
                ScenarioReason::Untraced { package } => {
                    say!("  a request could not be tied to a handler, and {package} is affected")
                }
            }
        }
    }
    if !plan.uncovered.is_empty() {
        say!("\nnot covered by any rule:");
        for path in &plan.uncovered {
            say!("  {path}");
        }
    }
}

fn describe(reason: &Reason) -> String {
    match reason {
        Reason::FileChanged { path } => format!("changed: {path}"),
        Reason::DependsOnCrate { package } => format!("depends on: {package}"),
        Reason::DependsOnExternal {
            name,
            version,
            features,
        } if features.is_empty() => format!("depends on: {name} {version} (Cargo.lock)"),
        Reason::DependsOnExternal {
            name,
            version,
            features,
        } => format!(
            "depends on: {name} {version} (Cargo.lock), optional: enabled by feature {}",
            features.join(", ")
        ),
        Reason::DependsOnWorkspaceDependency { name } => {
            format!("workspace dependency changed: {name}")
        }
        Reason::Reaches { changed } => format!("reaches: {changed}"),
        Reason::FellBack { cause } => match cause {
            FallbackCause::WorkspaceGlobal { path } => {
                format!("workspace-wide input changed: {path}")
            }
            FallbackCause::NotInIndex { path } => format!("not in the index: {path}"),
            FallbackCause::OutsideDefinitions { path, line } => {
                format!("changed outside every definition: {path}:{line}")
            }
            FallbackCause::UnnamedTest { test } => format!("a reached test has no name: {test}"),
            FallbackCause::NotRecorded { package } => {
                format!("references were not recorded for: {package}")
            }
        },
    }
}
