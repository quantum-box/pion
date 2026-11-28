//! The reference graph: which definition refers to which.
//!
//! Built from a SCIP index. A node is a definition (function, method, type,
//! field, constant). An edge says "when this changes, that is affected".

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::change::RepoPath;
use crate::records::Records;
use crate::reference_syntax::{ReferenceKind, ReferenceSyntax};
use crate::scip::{Index, Occurrence, Position, Range};
use crate::workspace::{PackageName, WorkspaceGraph};

/// The identity of a definition across files and crates, as SCIP spells it:
/// `rust-analyzer cargo <package> <version> <descriptors>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SymbolId(String);

impl SymbolId {
    /// Keeps the symbols that name something outside one function body.
    /// Locals (`local 3`) and modules are not nodes of the graph: a local
    /// cannot be referenced from elsewhere, and a module is mentioned by
    /// every path that goes through it.
    fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.starts_with("local ") || raw.ends_with('/') {
            return None;
        }
        Some(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The part after the scheme, manager, package and version.
    fn descriptors(&self) -> &str {
        self.0.splitn(5, ' ').nth(4).unwrap_or("")
    }

    /// A struct, enum, union or trait.
    fn is_type(&self) -> bool {
        self.0.ends_with('#')
    }

    /// A function or method.
    fn is_callable(&self) -> bool {
        self.0.ends_with("().")
    }

    /// `module::path::function` for a free function, the form test names
    /// take. `None` for anything else.
    fn function_path(&self) -> Option<String> {
        let path = self.descriptors().strip_suffix("().")?;
        let is_identifier =
            |part: &str| !part.is_empty() && part.chars().all(|c| c.is_alphanumeric() || c == '_');
        let parts: Vec<&str> = path.split('/').collect();
        parts
            .iter()
            .all(|part| is_identifier(part))
            .then(|| parts.join("::"))
    }

    /// The last name, for a function or method: `find` in `…Repo#find().`.
    fn callable_name(&self) -> Option<&str> {
        let path = self.descriptors().strip_suffix("().")?;
        let start = path
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(0, |index| index + 1);
        (start < path.len()).then(|| &path[start..])
    }

    /// `(trait, method)` when this is a method declared in a trait:
    /// `…/UserRepository#find().`.
    fn trait_method(&self) -> Option<(&str, &str)> {
        let path = self.descriptors().strip_suffix("().")?;
        let (owner, method) = path.rsplit_once('#')?;
        let owner = owner.rsplit('/').next()?;
        let is_identifier =
            |part: &str| !part.is_empty() && part.chars().all(|c| c.is_alphanumeric() || c == '_');
        (owner != "impl" && is_identifier(owner) && is_identifier(method))
            .then_some((owner, method))
    }

    /// `(trait, method)` when this is a method of a trait impl:
    /// `…/impl#[MemoryRepository][UserRepository]find().`.
    fn trait_impl_method(&self) -> Option<(&str, &str)> {
        let path = self.descriptors().strip_suffix("().")?;
        let (_, after_impl) = path.rsplit_once("impl#")?;
        let (_, rest) = bracket_group(after_impl)?;
        let (trait_name, method) = bracket_group(rest)?;
        // `PartialOrd<Self>` names the trait `PartialOrd`.
        let trait_name = trait_name.trim_matches('`');
        let trait_name = trait_name.split('<').next()?;
        Some((trait_name, method))
    }
}

impl fmt::Display for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.0.starts_with("rust-analyzer ") {
            // A path the compiler wrote, readable as it is.
            return self.0.fmt(f);
        }
        // The descriptors are what a reader recognizes; the header repeats.
        let package = self.0.split(' ').nth(2).unwrap_or("");
        write!(f, "{package}::{}", self.descriptors())
    }
}

/// Splits `[inside]rest` into `(inside, rest)`. Brackets inside backticks do
/// not close the group.
fn bracket_group(text: &str) -> Option<(&str, &str)> {
    let body = text.strip_prefix('[')?;
    let mut in_backticks = false;
    for (index, c) in body.char_indices() {
        match c {
            '`' => in_backticks = !in_backticks,
            ']' if !in_backticks => return Some((&body[..index], &body[index + 1..])),
            _ => {}
        }
    }
    None
}

/// The name nextest gives a test: `module::path::function`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TestName(String);

impl TestName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TestName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A test function found in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    pub package: PackageName,
    /// `None` when the test's name cannot be derived from its symbol. Such a
    /// test can only be selected by running its whole crate.
    pub name: Option<TestName>,
}

#[derive(Debug, Clone)]
struct Definition {
    symbol: SymbolId,
    /// Where the name is written.
    name: Range,
    /// The whole item: attributes, signature and body.
    ///
    /// An item written inside a function that carries an attribute macro
    /// (a mock declared in a `#[tokio::test]` function) gets the range of
    /// that whole function, so several definitions can share one range.
    item: Range,
}

#[derive(Debug, Clone)]
pub struct Reference {
    pub range: Range,
    pub symbol: SymbolId,
    /// The mentioned definition is a function or method.
    pub callable: bool,
}

#[derive(Debug, Default)]
struct FileSymbols {
    /// Sorted so that an enclosing definition comes before what it encloses.
    definitions: Vec<Definition>,
    /// Sorted by position.
    references: Vec<Reference>,
}

/// Attribute macros that turn a function into a test. Matched on the macro's
/// own name, so `tokio::test` and `sqlx::test` count as `test`.
const TEST_ATTRIBUTES: [&str; 3] = ["test:", "rstest:", "test_case:"];

fn is_test_attribute(symbol: &str) -> bool {
    let name = symbol.rsplit(['/', ' ']).next().unwrap_or("");
    TEST_ATTRIBUTES.contains(&name)
}

/// Which definition refers to which, for one workspace.
#[derive(Debug, Default)]
pub struct ReferenceGraph {
    registration_dependents: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    metadata_dependents: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    metadata_consumers: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    level: SelectionLevel,
    files: BTreeMap<RepoPath, FileSymbols>,
    /// Definition -> the definitions affected when it changes.
    dependents: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    tests: BTreeMap<SymbolId, TestCase>,
    /// Function or method name -> the definitions with that name.
    callables: BTreeMap<String, BTreeSet<SymbolId>>,
    /// Crates the graph knows it is missing records for.
    unrecorded: BTreeSet<PackageName>,
}

/// Weakest reference evidence to keep for a verified body-only patch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SelectionLevel {
    #[default]
    All,
    Runtime,
    Direct,
}

impl ReferenceGraph {
    /// Builds the graph from an index of the workspace `graph` describes.
    /// Files no workspace crate owns are left out.
    pub fn build(index: &Index, graph: &WorkspaceGraph) -> Self {
        Self::build_with_source(index, graph, SelectionLevel::All, |_| None)
    }

    pub fn build_with_source(
        index: &Index,
        graph: &WorkspaceGraph,
        level: SelectionLevel,
        read: impl Fn(&RepoPath) -> Option<String>,
    ) -> Self {
        let mut built = Self {
            level,
            ..Self::default()
        };
        for document in &index.documents {
            let Some(path) = graph.repo_path(&document.relative_path) else {
                continue;
            };
            let Some(package) = graph.owner_of(&path).cloned() else {
                continue;
            };
            let syntax = read(&path)
                .map(|source| ReferenceSyntax::parse(&source))
                .unwrap_or_default();
            let file = built.add_document(&document.occurrences, &package, &syntax);
            built.files.insert(path, file);
        }
        built.link_trait_methods();
        built
    }

    /// Builds the graph from the records the compiler wrote for each crate
    /// of the workspace `graph` describes (see [`crate::records`]).
    pub fn from_records(records: &Records, graph: &WorkspaceGraph) -> Self {
        let mut built = Self {
            unrecorded: records
                .unrecorded
                .iter()
                .filter_map(|name| graph.package(name))
                .collect(),
            ..Self::default()
        };
        let line_start = |line: u32| Position {
            line: line.saturating_sub(1),
            column: 0,
        };
        for record in &records.crates {
            let package = graph.package(&record.package);
            for definition in &record.definitions {
                let symbol = SymbolId(definition.id.clone());
                if definition.callable {
                    built
                        .callables
                        .entry(definition.name.clone())
                        .or_default()
                        .insert(symbol.clone());
                }
                if let (Some(test), Some(package)) = (&definition.test, &package) {
                    built.tests.insert(
                        symbol.clone(),
                        TestCase {
                            package: package.clone(),
                            name: Some(TestName(test.clone())),
                        },
                    );
                }
                let place = (
                    &definition.file,
                    definition.first_line,
                    definition.last_line,
                );
                let (Some(file), Some(first), Some(last)) = place else {
                    continue;
                };
                let Some(path) = graph.repo_path(file) else {
                    continue;
                };
                // The same file is compiled more than once (the library and
                // its tests); keep each definition once.
                let definitions = &mut built.files.entry(path).or_default().definitions;
                if definitions.iter().all(|known| known.symbol != symbol) {
                    definitions.push(Definition {
                        symbol,
                        name: Range {
                            start: line_start(first),
                            end: line_start(first),
                        },
                        item: Range {
                            start: line_start(first),
                            end: Position {
                                line: last.saturating_sub(1),
                                column: u32::MAX,
                            },
                        },
                    });
                }
            }
            for mention in &record.mentions {
                let Some(path) = graph.repo_path(&mention.file) else {
                    continue;
                };
                let start = Position {
                    line: mention.line.saturating_sub(1),
                    column: mention.column,
                };
                built
                    .files
                    .entry(path)
                    .or_default()
                    .references
                    .push(Reference {
                        range: Range { start, end: start },
                        symbol: SymbolId(mention.to.clone()),
                        callable: mention.callable,
                    });
            }
            for (changed, affected) in &record.affects {
                built.affects(&SymbolId(changed.clone()), &SymbolId(affected.clone()));
            }
        }
        for file in built.files.values_mut() {
            file.definitions.sort_by(|a, b| a.item.outer_first(&b.item));
            file.references
                .sort_by_key(|reference| reference.range.start);
            file.references
                .dedup_by(|a, b| a.range == b.range && a.symbol == b.symbol);
        }
        built
    }

    fn add_document(
        &mut self,
        occurrences: &[Occurrence],
        package: &PackageName,
        syntax: &ReferenceSyntax,
    ) -> FileSymbols {
        let mut definitions = Vec::new();
        let mut mentions = Vec::new();
        for occurrence in occurrences {
            if occurrence.is_definition {
                let symbol = SymbolId::parse(&occurrence.symbol);
                if let (Some(symbol), Some(item)) = (symbol, occurrence.enclosing) {
                    definitions.push(Definition {
                        symbol,
                        name: occurrence.range,
                        item,
                    });
                }
            } else {
                mentions.push(occurrence);
            }
        }
        definitions.sort_by(|a, b| a.item.outer_first(&b.item));
        mentions.sort_by_key(|occurrence| occurrence.range.start);

        for definition in &definitions {
            if let Some(name) = definition.symbol.callable_name() {
                self.callables
                    .entry(name.to_owned())
                    .or_default()
                    .insert(definition.symbol.clone());
            }
        }

        // Walk the file once, keeping the definitions that enclose the
        // current position on a stack. The top is the innermost.
        let mut stack: Vec<&Definition> = Vec::new();
        let mut pending = definitions.iter().peekable();
        let mut members = Vec::new();
        let mut references = Vec::new();
        let end_of_file = Position {
            line: u32::MAX,
            column: u32::MAX,
        };
        let positions = mentions
            .iter()
            .map(|mention| (mention.range.start, Some(*mention)))
            .chain([(end_of_file, None)]);
        for (position, mention) in positions {
            while let Some(definition) = pending.next_if(|next| next.item.start <= position) {
                close_ended(&mut stack, definition.item.start);
                if let Some(parent) = stack.last() {
                    // A struct or enum is affected when one of its fields
                    // or variants changes. A trait is not affected by the
                    // body of one of its methods: callers refer to the
                    // method itself.
                    let is_member = parent.symbol.is_type()
                        && !definition.symbol.is_callable()
                        && definition
                            .symbol
                            .as_str()
                            .starts_with(parent.symbol.as_str())
                        && parent.item.encloses(&definition.item);
                    if is_member {
                        members.push((definition.symbol.clone(), parent.symbol.clone()));
                    }
                }
                stack.push(definition);
            }
            close_ended(&mut stack, position);
            let (Some(mention), Some(innermost)) = (mention, stack.last()) else {
                // The end of the file, or a mention outside every
                // definition, such as a `use`.
                continue;
            };
            // Definitions that share the innermost range are one function and
            // the items written inside it. Those items cannot be named from
            // outside the function, so a mention anywhere in the range counts
            // as the function's own: the definition whose name comes first.
            let owner = stack
                .iter()
                .rev()
                .take_while(|definition| definition.item == innermost.item)
                .min_by_key(|definition| definition.name.start)
                .unwrap_or(innermost);
            if is_test_attribute(&mention.symbol) {
                self.tests.insert(
                    owner.symbol.clone(),
                    TestCase {
                        package: package.clone(),
                        name: owner.symbol.function_path().map(TestName),
                    },
                );
                continue;
            }
            let Some(symbol) = SymbolId::parse(&mention.symbol) else {
                continue;
            };
            if symbol != owner.symbol {
                let kind = syntax.kind(position);
                let edges = match kind {
                    ReferenceKind::Ordinary => &mut self.dependents,
                    ReferenceKind::Registration if symbol.is_callable() => {
                        &mut self.registration_dependents
                    }
                    ReferenceKind::Metadata if symbol.is_callable() => {
                        &mut self.metadata_dependents
                    }
                    ReferenceKind::MetadataConsumer => &mut self.metadata_consumers,
                    _ => &mut self.dependents,
                };
                edges
                    .entry(symbol.clone())
                    .or_default()
                    .insert(owner.symbol.clone());
            }
            references.push(Reference {
                range: mention.range,
                callable: symbol.is_callable(),
                symbol,
            });
        }
        for (member, owner) in members {
            self.affects(&member, &owner);
        }
        FileSymbols {
            definitions,
            references,
        }
    }

    /// Records that a change to `changed` affects `dependent`.
    fn affects(&mut self, changed: &SymbolId, dependent: &SymbolId) {
        self.dependents
            .entry(changed.clone())
            .or_default()
            .insert(dependent.clone());
    }

    /// Ties each method of a trait impl to the trait method it implements.
    ///
    /// A call through `dyn Trait` or a generic refers to the trait method,
    /// not to an impl. So a change to an impl affects whatever depends on the
    /// trait method. The reverse is not recorded: a caller that uses the
    /// trait's default body refers to the trait method itself, and tying the
    /// trait method to its impls would make every impl of a common trait
    /// affect every other.
    /// The index names the trait of an impl by its bare name; when two traits
    /// share a name, an impl is tied to both.
    fn link_trait_methods(&mut self) {
        let mut declared: BTreeMap<(String, String), Vec<SymbolId>> = BTreeMap::new();
        let mut implemented = Vec::new();
        for definition in self.files.values().flat_map(|file| &file.definitions) {
            let symbol = &definition.symbol;
            if let Some((trait_name, method)) = symbol.trait_method() {
                declared
                    .entry((trait_name.to_owned(), method.to_owned()))
                    .or_default()
                    .push(symbol.clone());
            } else if let Some((trait_name, method)) = symbol.trait_impl_method() {
                implemented.push((symbol.clone(), trait_name.to_owned(), method.to_owned()));
            }
        }
        for (implementation, trait_name, method) in implemented {
            let Some(declarations) = declared.get(&(trait_name, method)) else {
                continue;
            };
            for declaration in declarations.clone() {
                self.affects(&implementation, &declaration);
            }
        }
    }

    /// Crates whose references could not be recorded. What refers to what
    /// inside them is unknown, so they cannot be narrowed.
    pub fn unrecorded(&self) -> &BTreeSet<PackageName> {
        &self.unrecorded
    }

    /// The files the index covers.
    pub fn paths(&self) -> impl Iterator<Item = &RepoPath> {
        self.files.keys()
    }

    pub fn has_file(&self, path: &RepoPath) -> bool {
        self.files.contains_key(path)
    }

    /// The definitions a change on `line` (zero-based) belongs to: the
    /// innermost one whose item spans the line. Empty when no definition
    /// does.
    ///
    /// Several definitions can share that innermost range. In a rust-analyzer
    /// index they are a function and the items written inside it, and the
    /// function, whose name comes first, stands for all. In compiler records
    /// they are the items one macro call generated, with no order among
    /// them, and the change belongs to each.
    pub fn definitions_at_line(&self, path: &RepoPath, line: u32) -> BTreeSet<&SymbolId> {
        let Some(file) = self.files.get(path) else {
            return BTreeSet::new();
        };
        let spanning = || {
            file.definitions
                .iter()
                .rev()
                .filter(move |definition| definition.item.contains_line(line))
        };
        let Some(innermost) = spanning().next() else {
            return BTreeSet::new();
        };
        let sharing = || spanning().filter(|definition| definition.item == innermost.item);
        let first = sharing().map(|definition| definition.name.start).min();
        sharing()
            .filter(|definition| Some(definition.name.start) == first)
            .map(|definition| &definition.symbol)
            .collect()
    }

    /// The references written in `path` between two positions.
    pub fn references_between(
        &self,
        path: &RepoPath,
        start: Position,
        end: Position,
    ) -> impl Iterator<Item = &Reference> {
        self.files
            .get(path)
            .into_iter()
            .flat_map(|file| &file.references)
            .filter(move |reference| start <= reference.range.start && reference.range.start < end)
    }

    /// The functions and methods with this name, anywhere in the workspace.
    pub fn callables_named(&self, name: &str) -> impl Iterator<Item = &SymbolId> {
        self.callables.get(name).into_iter().flatten()
    }

    pub fn test(&self, symbol: &SymbolId) -> Option<&TestCase> {
        self.tests.get(symbol)
    }

    /// Every definition a change to `changed` can affect, the changed ones
    /// included, each with the changed definition it was reached from.
    pub fn affected_by(&self, changed: &BTreeSet<SymbolId>) -> BTreeMap<SymbolId, SymbolId> {
        // A symbol may be reachable both through a registration and through
        // a call. Keep both states so the stronger path cannot be masked.
        let mut origin = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::new();
        for symbol in changed {
            queue.push_back((symbol.clone(), symbol.clone(), false));
        }
        while let Some((symbol, from, registered)) = queue.pop_front() {
            if !seen.insert((symbol.clone(), registered)) {
                continue;
            }
            origin.entry(symbol.clone()).or_insert_with(|| from.clone());
            for (edges, keep, next_registered) in [
                (&self.dependents, true, registered),
                (
                    &self.registration_dependents,
                    self.level != SelectionLevel::Direct,
                    true,
                ),
                (
                    &self.metadata_dependents,
                    self.level == SelectionLevel::All,
                    true,
                ),
                (
                    &self.metadata_consumers,
                    !registered || self.level == SelectionLevel::All,
                    registered,
                ),
            ] {
                if keep {
                    for dependent in edges.get(&symbol).into_iter().flatten() {
                        queue.push_back((dependent.clone(), from.clone(), next_registered));
                    }
                }
            }
        }
        origin
    }
}

/// Pops the definitions that end at or before `position`.
fn close_ended(stack: &mut Vec<&Definition>, position: Position) {
    while stack
        .last()
        .is_some_and(|definition| definition.item.end <= position)
    {
        stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(descriptors: &str) -> SymbolId {
        SymbolId(format!("rust-analyzer cargo fx 0.0.0 {descriptors}"))
    }

    #[test]
    fn levels_preserve_strong_paths_and_direct_metadata_dependencies() {
        let changed = symbol("changed().");
        let registered = symbol("router().");
        let direct_test = symbol("direct_test().");
        let metadata = symbol("metadata().");
        let http_test = symbol("http_test().");
        let attr = symbol("Doc#");
        let mut graph = ReferenceGraph::default();
        graph.affects(&changed, &direct_test);
        graph.affects(&registered, &http_test);
        graph.affects(&metadata, &direct_test);
        graph
            .registration_dependents
            .insert(changed.clone(), BTreeSet::from([registered.clone()]));
        graph
            .metadata_dependents
            .insert(changed.clone(), BTreeSet::from([attr.clone()]));
        graph
            .metadata_consumers
            .insert(registered.clone(), BTreeSet::from([metadata.clone()]));

        let edits = BTreeSet::from([changed.clone()]);
        assert!(graph.affected_by(&edits).contains_key(&metadata));
        graph.level = SelectionLevel::Runtime;
        let runtime = graph.affected_by(&edits);
        assert!(runtime.contains_key(&http_test));
        assert!(runtime.contains_key(&direct_test));
        assert!(!runtime.contains_key(&attr));
        assert!(!runtime.contains_key(&metadata));
        assert!(
            graph
                .affected_by(&BTreeSet::from([registered.clone()]))
                .contains_key(&metadata)
        );
        graph.level = SelectionLevel::Direct;
        let direct = graph.affected_by(&edits);
        assert!(!direct.contains_key(&http_test));
        assert!(direct.contains_key(&direct_test));
        // Another reference to the same symbol pair may be a real call.
        graph.affects(&changed, &registered);
        assert!(graph.affected_by(&edits).contains_key(&http_test));
    }

    #[test]
    fn locals_and_modules_are_not_nodes() {
        assert_eq!(SymbolId::parse("local 3"), None);
        assert_eq!(SymbolId::parse("rust-analyzer cargo fx 0.0.0 token/"), None);
        assert!(SymbolId::parse("rust-analyzer cargo fx 0.0.0 token/Token#").is_some());
    }

    #[test]
    fn a_free_function_has_a_test_path() {
        assert_eq!(
            symbol("token/tests/token_roundtrip().")
                .function_path()
                .as_deref(),
            Some("token::tests::token_roundtrip")
        );
        assert_eq!(
            symbol("token/impl#[Token]is_expired().").function_path(),
            None
        );
    }

    #[test]
    fn trait_methods_and_their_impls_are_recognized() {
        assert_eq!(
            symbol("repo/UserRepository#find().").trait_method(),
            Some(("UserRepository", "find"))
        );
        assert_eq!(
            symbol("token/impl#[Token]is_expired().").trait_method(),
            None
        );
        assert_eq!(
            symbol("repo/impl#[MemoryRepository][UserRepository]find().").trait_impl_method(),
            Some(("UserRepository", "find"))
        );
        assert_eq!(
            symbol("cmp/impl#[u32][`PartialOrd<Self>`]gt().").trait_impl_method(),
            Some(("PartialOrd", "gt"))
        );
        assert_eq!(
            symbol("token/impl#[Token]is_expired().").trait_impl_method(),
            None
        );
    }

    #[test]
    fn test_attributes_are_matched_on_the_macro_name() {
        assert!(is_test_attribute(
            "rust-analyzer cargo core https://example/core macros/builtin/test:"
        ));
        assert!(is_test_attribute(
            "rust-analyzer cargo tokio-macros 2.0.0 test:"
        ));
        assert!(!is_test_attribute(
            "rust-analyzer cargo fx 0.0.0 tests/test()."
        ));
        assert!(!is_test_attribute(
            "rust-analyzer cargo core https://example/core macros/builtin/derive:"
        ));
    }
}
