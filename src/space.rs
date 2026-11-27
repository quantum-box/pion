use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{BufWriter, Write};
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use pion::scip::{Document, Index};
use serde::{Deserialize, Serialize};

type Failure = Box<dyn std::error::Error>;
type FileId = usize;
type Location = (FileId, u32);
type DependencyDelta = (String, String, usize);
type Sources = BTreeMap<usize, BTreeMap<String, String>>;

#[derive(Deserialize)]
struct Metadata {
    workspace_root: Utf8PathBuf,
    workspace_members: BTreeSet<String>,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: Utf8PathBuf,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    kind: Option<String>,
    path: Option<Utf8PathBuf>,
}

#[derive(Serialize)]
struct Node<'a> {
    name: &'a str,
    path: &'a Utf8Path,
    group: &'a str,
    position: [f64; 3],
}

#[derive(Serialize)]
struct Workspace<'a> {
    nodes: Vec<Node<'a>>,
    edges: Vec<(usize, usize, BTreeSet<&'a str>)>,
    changed_rust_paths: BTreeSet<String>,
    dependency_changes: Option<DependencyChanges>,
}

#[derive(Serialize)]
struct DependencyChanges {
    added: Vec<DependencyDelta>,
    removed: Vec<DependencyDelta>,
}

#[derive(Default, Serialize)]
struct References<'a> {
    files: Vec<(usize, &'a str)>,
    definitions: BTreeMap<FileId, BTreeMap<u32, BTreeSet<Location>>>,
    file_dependencies: BTreeMap<FileId, BTreeMap<FileId, u32>>,
    indexed_files: BTreeSet<FileId>,
    #[serde(skip)]
    dependency_symbols: BTreeMap<(FileId, FileId), BTreeSet<String>>,
}

pub fn write(
    manifest: &Utf8Path,
    output: &Utf8Path,
    scip: Option<&Utf8Path>,
    base_scip: Option<&Utf8Path>,
) -> Result<(), Failure> {
    let metadata = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(manifest)
        .output()?;
    if !metadata.status.success() {
        return Err(format!(
            "cargo metadata: {}",
            String::from_utf8_lossy(&metadata.stderr)
        )
        .into());
    }
    let mut metadata: Metadata = serde_json::from_slice(&metadata.stdout)?;
    metadata
        .packages
        .retain(|package| metadata.workspace_members.contains(&package.id));
    metadata
        .packages
        .sort_by(|a, b| (&a.name, &a.manifest_path).cmp(&(&b.name, &b.manifest_path)));
    let root = &metadata.workspace_root;
    let directories: BTreeMap<_, _> = metadata
        .packages
        .iter()
        .enumerate()
        .map(|(index, package)| {
            (
                package.manifest_path.parent().expect("absolute manifest"),
                index,
            )
        })
        .collect();
    let mut workspace = build_workspace(root, &metadata.packages, &directories)?;
    workspace.changed_rust_paths = changed_rust_paths(root)?;
    let sources = read_sources(root, &directories)?;
    let references = if let Some(path) = scip {
        index_references(
            &Index::decode(&fs::read(path)?)?,
            &workspace,
            &sources,
            base_scip.is_some(),
        )
    } else {
        References::default()
    };

    if let Some(base_scip) = base_scip {
        let base_index = Index::decode(&fs::read(base_scip)?)?;
        workspace.dependency_changes = Some(compare_dependencies(
            root,
            &workspace,
            &references,
            &base_index,
        )?);
    }

    if let Some(parent) = output.parent().filter(|path| !path.as_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut output = BufWriter::new(fs::File::create(output)?);
    let template = include_str!("space.html");
    let (header, rest) = template
        .split_once("__PION_WORKSPACE__")
        .expect("workspace marker");
    let (between, rest) = rest
        .split_once("__PION_SOURCE_SNAPSHOT__")
        .expect("source marker");
    let (before_references, footer) = rest
        .split_once("__PION_REFERENCES__")
        .expect("reference marker");
    output.write_all(header.as_bytes())?;
    write_json(&mut output, &workspace)?;
    output.write_all(between.as_bytes())?;
    write_json(&mut output, &sources)?;
    output.write_all(before_references.as_bytes())?;
    write_json(&mut output, &references)?;
    output.write_all(footer.as_bytes())?;
    output.flush()?;
    Ok(())
}

fn write_json(output: &mut impl Write, value: &impl Serialize) -> Result<(), Failure> {
    let json = serde_json::to_string(value)?;
    for segment in json.split_inclusive('<') {
        if let Some(prefix) = segment.strip_suffix('<') {
            output.write_all(prefix.as_bytes())?;
            output.write_all(br"\u003c")?;
        } else {
            output.write_all(segment.as_bytes())?;
        }
    }
    Ok(())
}

fn build_workspace<'a>(
    root: &Utf8Path,
    packages: &'a [Package],
    directories: &BTreeMap<&Utf8Path, usize>,
) -> Result<Workspace<'a>, Failure> {
    let paths = packages
        .iter()
        .map(|package| package.manifest_path.strip_prefix(root))
        .collect::<Result<Vec<_>, _>>()?;
    let group = |path: &'a Utf8Path| {
        path.parent()
            .and_then(|parent| parent.iter().next())
            .unwrap_or("root")
    };
    let groups: BTreeSet<_> = paths.iter().map(|path| group(path)).collect();
    let mut counts = BTreeMap::new();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for (index, (package, path)) in packages.iter().zip(paths).enumerate() {
        let group = group(path);
        let group_index = groups
            .iter()
            .position(|name| *name == group)
            .expect("known group");
        let layer = counts.entry(group).or_insert(0usize);
        let angle = std::f64::consts::TAU * group_index as f64 / groups.len() as f64;
        let spiral = *layer as f64 * std::f64::consts::PI * (3.0 - 5.0_f64.sqrt());
        let radius = (0.105 * (*layer as f64).sqrt()).min(0.8);
        let center = if groups.len() == 1 { 0.0 } else { 1.15 };
        nodes.push(Node {
            name: &package.name,
            path,
            group,
            position: [
                center * angle.cos() + radius * spiral.cos(),
                (((*layer * 7) % 17) as f64 - 8.0) * 0.1,
                center * angle.sin() + radius * spiral.sin(),
            ],
        });
        *layer += 1;
        let mut dependencies = BTreeMap::<usize, BTreeSet<&str>>::new();
        for dependency in &package.dependencies {
            if let Some(target) = dependency
                .path
                .as_deref()
                .and_then(|path| directories.get(path))
            {
                dependencies
                    .entry(*target)
                    .or_default()
                    .insert(dependency.kind.as_deref().unwrap_or("normal"));
            }
        }
        edges.extend(
            dependencies
                .into_iter()
                .map(|(target, kinds)| (index, target, kinds)),
        );
    }
    Ok(Workspace {
        nodes,
        edges,
        changed_rust_paths: BTreeSet::new(),
        dependency_changes: None,
    })
}

fn changed_rust_paths(root: &Utf8Path) -> Result<BTreeSet<String>, Failure> {
    let has_head = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()?
        .status
        .success();
    let mut diff = Command::new("git");
    diff.current_dir(root)
        .args(["diff", "--no-renames", "--name-only", "--relative", "-z"]);
    diff.arg(if has_head { "HEAD" } else { "--cached" });
    let output = diff.args(["--", "*.rs"]).output()?;
    if !output.status.success() {
        return Err(format!("git diff: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(std::str::from_utf8(&output.stdout)?
        .split_terminator('\0')
        .map(str::to_owned)
        .collect())
}

fn read_sources(
    root: &Utf8Path,
    directories: &BTreeMap<&Utf8Path, usize>,
) -> Result<Sources, Failure> {
    let paths = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-z", "--", "*.rs"])
        .output()?;
    if !paths.status.success() {
        return Err(format!("git ls-files: {}", String::from_utf8_lossy(&paths.stderr)).into());
    }
    let mut sources = Sources::new();
    for path in std::str::from_utf8(&paths.stdout)?.split_terminator('\0') {
        let path = root.join(path);
        let Some((directory, index)) = path
            .ancestors()
            .skip(1)
            .find_map(|directory| directories.get(directory).map(|index| (directory, *index)))
        else {
            continue;
        };
        let Ok(metadata) = path.symlink_metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        sources.entry(index).or_default().insert(
            path.strip_prefix(directory)?.to_string(),
            fs::read_to_string(&path)?,
        );
    }
    Ok(sources)
}

fn index_references<'a>(
    index: &Index,
    workspace: &Workspace<'_>,
    sources: &'a Sources,
    compare: bool,
) -> References<'a> {
    let mut references = References::default();
    let mut paths = HashMap::new();
    for (&package, files) in sources {
        let directory = workspace.nodes[package]
            .path
            .parent()
            .expect("manifest directory");
        for path in files.keys() {
            paths.insert(directory.join(path), references.files.len());
            references.files.push((package, path.as_str()));
        }
    }

    let documents: Vec<_> = index
        .documents
        .iter()
        .filter_map(|document| {
            paths
                .get(Utf8Path::new(&document.relative_path))
                .map(|&id| (id, document))
        })
        .collect();
    collect_references(documents, &mut references, true, compare);
    references
}

fn compare_dependencies(
    root: &Utf8Path,
    workspace: &Workspace<'_>,
    current: &References<'_>,
    base_index: &Index,
) -> Result<DependencyChanges, Failure> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["ls-tree", "-r", "--name-only", "-z", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Err(format!("git ls-tree: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    let base_paths: Vec<_> = std::str::from_utf8(&output.stdout)?
        .split_terminator('\0')
        .filter(|path| path.ends_with(".rs"))
        .map(str::to_owned)
        .collect();
    let base_ids: HashMap<_, _> = base_paths
        .iter()
        .enumerate()
        .map(|(id, path)| (path.as_str(), id))
        .collect();
    let documents = base_index
        .documents
        .iter()
        .filter_map(|document| {
            base_ids
                .get(document.relative_path.as_str())
                .map(|&id| (id, document))
        })
        .collect();
    let mut base = References::default();
    collect_references(documents, &mut base, false, true);

    let current_paths: Vec<_> = current
        .files
        .iter()
        .map(|&(crate_id, path)| {
            workspace.nodes[crate_id]
                .path
                .parent()
                .expect("manifest directory")
                .join(path)
                .into_string()
        })
        .collect();
    let current_ids: HashMap<_, _> = current_paths
        .iter()
        .enumerate()
        .map(|(id, path)| (path.as_str(), id))
        .collect();
    let base_edges = dependency_paths(&base, &base_paths);
    let current_edges = dependency_paths(current, &current_paths);
    // A present but unindexed file has unknown coverage on that side.
    let comparable = |path: &str, ids: &HashMap<&str, FileId>, index: &References<'_>| {
        ids.get(path)
            .is_none_or(|id| index.indexed_files.contains(id))
    };
    let added = changed_dependencies(&current_edges, &base_edges, |path| {
        comparable(path, &base_ids, &base)
    });
    let removed = changed_dependencies(&base_edges, &current_edges, |path| {
        comparable(path, &current_ids, current)
    });
    Ok(DependencyChanges { added, removed })
}

type DependencyPaths<'a> = BTreeMap<(&'a str, &'a str), &'a BTreeSet<String>>;

fn dependency_paths<'a>(
    references: &'a References<'_>,
    paths: &'a [String],
) -> DependencyPaths<'a> {
    references
        .dependency_symbols
        .iter()
        .map(|(&(from, to), symbols)| ((paths[from].as_str(), paths[to].as_str()), symbols))
        .collect()
}

fn changed_dependencies(
    updated: &DependencyPaths<'_>,
    previous: &DependencyPaths<'_>,
    comparable: impl Fn(&str) -> bool,
) -> Vec<DependencyDelta> {
    updated
        .iter()
        .filter_map(|(&(from, to), symbols)| {
            if !comparable(from) || !comparable(to) {
                return None;
            }
            let count = previous
                .get(&(from, to))
                .map_or(symbols.len(), |old| symbols.difference(old).count());
            (count > 0).then(|| (from.to_owned(), to.to_owned(), count))
        })
        .collect()
}

fn collect_references<'a>(
    documents: Vec<(FileId, &'a Document)>,
    references: &mut References<'_>,
    include_definitions: bool,
    compare: bool,
) {
    references
        .indexed_files
        .extend(documents.iter().map(|(file, _)| *file));
    let mut definitions = HashMap::<&str, BTreeSet<Location>>::new();
    let mut targets = HashMap::<&str, BTreeSet<Location>>::new();
    for &(file, document) in &documents {
        for occurrence in &document.occurrences {
            if occurrence.is_definition && !occurrence.symbol.starts_with("local ") {
                definitions
                    .entry(&occurrence.symbol)
                    .or_default()
                    .insert((file, occurrence.range.start.line + 1));
            }
        }
    }
    for (file, document) in documents {
        for occurrence in &document.occurrences {
            if !occurrence.is_definition && definitions.contains_key(occurrence.symbol.as_str()) {
                targets
                    .entry(&occurrence.symbol)
                    .or_default()
                    .insert((file, occurrence.range.start.line + 1));
            }
        }
    }
    let mut sites = BTreeMap::<Location, Vec<&str>>::new();
    for (symbol, locations) in definitions {
        if let (Some(&(definition_file, _)), Some(uses)) = (
            locations.first().filter(|_| locations.len() == 1),
            targets.get(symbol),
        ) {
            for &(referencing_file, _) in uses {
                if referencing_file != definition_file {
                    if compare {
                        references
                            .dependency_symbols
                            .entry((referencing_file, definition_file))
                            .or_default()
                            .insert(symbol.to_owned());
                    }
                    *references
                        .file_dependencies
                        .entry(referencing_file)
                        .or_default()
                        .entry(definition_file)
                        .or_default() += 1;
                }
            }
        }
        if include_definitions {
            for location in locations {
                sites.entry(location).or_default().push(symbol);
            }
        }
    }
    for ((file, line), symbols) in sites {
        // The HTML chooses declarations by line. Leave ambiguous lines to its text search.
        if let [symbol] = symbols.as_slice() {
            references
                .definitions
                .entry(file)
                .or_default()
                .insert(line, targets.get(symbol).cloned().unwrap_or_default());
        }
    }
}
