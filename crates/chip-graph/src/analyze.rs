//! Static analysis of a Rust repository into an [`ArchitectureGraph`].
//!
//! This reads files and parses them. It never executes source, runs Cargo or Git, touches
//! the network or the environment. Every relationship it records is structural: a fact
//! visible in the text of the source (a `mod`, a `use`, a `pub fn`, an `impl`). Where a
//! name cannot be resolved unambiguously the edge is simply not recorded; the graph may
//! miss relationships, but it does not guess between candidates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::Visit;
use syn::{ImplItem, Item, UseTree, Visibility};

use crate::model::{ArchitectureGraph, GraphEdge, GraphEdgeKind, GraphNode, GraphNodeKind};

const SKIPPED_DIRS: [&str; 4] = [".git", "target", ".chip", "node_modules"];

#[derive(Debug)]
pub enum AnalyzeError {
    Io(String),
    Manifest(String),
}

impl fmt::Display for AnalyzeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnalyzeError::Io(m) => write!(f, "io error: {m}"),
            AnalyzeError::Manifest(m) => write!(f, "manifest error: {m}"),
        }
    }
}

impl std::error::Error for AnalyzeError {}

/// Measurements about one analysis. Kept apart from the graph so timings can never
/// influence the snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnalysisStats {
    /// Source files and manifests read and parsed.
    pub files_scanned: usize,
    /// Rust files that failed to parse; they still appear as File nodes, with no contents.
    pub unparsed_files: usize,
    pub parse_time: Duration,
    pub graph_time: Duration,
}

/// The nearest ancestor that looks like a repository root: a `.git` entry or a workspace
/// manifest, else the nearest directory with a `Cargo.toml`.
pub fn find_repository_root(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let manifest = fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
        if dir.join(".git").exists() || manifest.contains("[workspace]") {
            return Some(dir.to_path_buf());
        }
    }
    start
        .ancestors()
        .find(|d| d.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

pub fn analyze(root: &Path) -> Result<ArchitectureGraph, AnalyzeError> {
    analyze_with_stats(root).map(|(graph, _)| graph)
}

pub fn analyze_with_stats(root: &Path) -> Result<(ArchitectureGraph, AnalysisStats), AnalyzeError> {
    let mut files = BTreeSet::new();
    walk(root, "", &mut files)?;

    let mut crates = Vec::new();
    let mut stats = AnalysisStats::default();
    for file in files
        .iter()
        .filter(|f| f.rsplit('/').next() == Some("Cargo.toml"))
    {
        let started = Instant::now();
        let text = fs::read_to_string(root.join(file))
            .map_err(|e| AnalyzeError::Io(format!("{file}: {e}")))?;
        let table: toml::Table =
            toml::from_str(&text).map_err(|e| AnalyzeError::Manifest(format!("{file}: {e}")))?;
        stats.parse_time += started.elapsed();
        stats.files_scanned += 1;
        if let Some(info) = CrateInfo::from_manifest(file, &table, &files) {
            crates.push(info);
        }
    }
    crates.sort_by(|a, b| a.dir.cmp(&b.dir));

    let mut b = Builder::new(root, &files);
    b.parse_time = stats.parse_time;
    b.files_scanned = stats.files_scanned;
    b.build(&crates);
    let (nodes, edges, parse_time, files_scanned, unparsed) = b.finish(&crates);

    let started = Instant::now();
    let graph = ArchitectureGraph::from_parts(nodes, edges);
    stats.graph_time = started.elapsed();
    stats.parse_time = parse_time;
    stats.files_scanned = files_scanned;
    stats.unparsed_files = unparsed;
    Ok((graph, stats))
}

fn walk(root: &Path, rel: &str, out: &mut BTreeSet<String>) -> Result<(), AnalyzeError> {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let entries = fs::read_dir(&dir).map_err(|e| AnalyzeError::Io(format!("{rel}: {e}")))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| AnalyzeError::Io(format!("{rel}: {e}")))?;
        let ty = entry
            .file_type()
            .map_err(|e| AnalyzeError::Io(format!("{rel}: {e}")))?;
        if ty.is_symlink() {
            continue;
        }
        names.push((
            entry.file_name().to_string_lossy().into_owned(),
            ty.is_dir(),
        ));
    }
    names.sort();
    for (name, is_dir) in names {
        let child = join(rel, &name);
        if is_dir {
            if !SKIPPED_DIRS.contains(&name.as_str()) {
                walk(root, &child, out)?;
            }
        } else {
            out.insert(child);
        }
    }
    Ok(())
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() || dir == "." {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

fn parent_dir(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn file_name(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file)
}

fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

fn clean(ident: &proc_macro2::Ident) -> String {
    ident.to_string().trim_start_matches("r#").to_string()
}

fn is_config(file: &str) -> bool {
    let name = file_name(file);
    if matches!(
        name,
        "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
    ) {
        return true;
    }
    let dir = parent_dir(file);
    dir == ".cargo"
        || dir.ends_with("/.cargo")
        || dir == ".github/workflows"
        || dir.ends_with("/.github/workflows")
}

fn crate_id(dir: &str) -> String {
    format!("crate:{dir}")
}

fn module_key(root: &str, path: &[String]) -> String {
    let mut key = root.to_string();
    for segment in path {
        key.push_str("::");
        key.push_str(segment);
    }
    key
}

fn module_id(krate: &str, key: &str) -> String {
    format!("module:{krate}:{key}")
}

fn file_id(file: &str) -> String {
    format!("file:{file}")
}

struct CrateInfo {
    dir: String,
    package: String,
    /// `(name used in source, package name)` for each dependency.
    deps: Vec<(String, String)>,
    lib: Option<String>,
    bins: BTreeMap<String, String>,
    tests: Vec<String>,
}

impl CrateInfo {
    fn from_manifest(
        manifest: &str,
        table: &toml::Table,
        files: &BTreeSet<String>,
    ) -> Option<CrateInfo> {
        let package = table
            .get("package")?
            .as_table()?
            .get("name")?
            .as_str()?
            .to_string();
        let dir = parent_dir(manifest);
        let dir = if dir.is_empty() {
            ".".to_string()
        } else {
            dir.to_string()
        };

        let mut deps = Vec::new();
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(entries) = table.get(section).and_then(|v| v.as_table()) {
                for (key, spec) in entries {
                    let package = spec
                        .as_table()
                        .and_then(|t| t.get("package"))
                        .and_then(|p| p.as_str())
                        .unwrap_or(key);
                    deps.push((key.replace('-', "_"), package.to_string()));
                }
            }
        }
        deps.sort();
        deps.dedup();

        let custom = |section: &str| -> Option<&toml::Table> { table.get(section)?.as_table() };
        let lib_path = custom("lib")
            .and_then(|t| t.get("path"))
            .and_then(|p| p.as_str())
            .map(|p| join(&dir, p))
            .unwrap_or_else(|| join(&dir, "src/lib.rs"));
        let lib = files.contains(&lib_path).then_some(lib_path);

        let mut bins = BTreeMap::new();
        let main = join(&dir, "src/main.rs");
        if files.contains(&main) {
            bins.insert(package.clone(), main.clone());
        }
        let bin_dir = join(&dir, "src/bin/");
        for file in files.iter().filter(|f| f.starts_with(&bin_dir)) {
            let rest = &file[bin_dir.len()..];
            match rest.split('/').collect::<Vec<_>>().as_slice() {
                [name] if name.ends_with(".rs") => {
                    bins.insert(name.trim_end_matches(".rs").to_string(), file.clone());
                }
                [name, "main.rs"] => {
                    bins.insert((*name).to_string(), file.clone());
                }
                _ => {}
            }
        }
        if let Some(declared) = table.get("bin").and_then(|b| b.as_array()) {
            for entry in declared.iter().filter_map(|e| e.as_table()) {
                let Some(name) = entry.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                let path = match entry.get("path").and_then(|p| p.as_str()) {
                    Some(p) => join(&dir, p),
                    None if name == package && files.contains(&main) => main.clone(),
                    None => join(&dir, &format!("src/bin/{name}.rs")),
                };
                if files.contains(&path) {
                    bins.insert(name.to_string(), path);
                }
            }
        }

        let tests_dir = join(&dir, "tests/");
        let tests: Vec<String> = files
            .iter()
            .filter(|f| {
                f.starts_with(&tests_dir)
                    && f.ends_with(".rs")
                    && !f[tests_dir.len()..].contains('/')
            })
            .cloned()
            .collect();

        Some(CrateInfo {
            dir,
            package,
            deps,
            lib,
            bins,
            tests,
        })
    }

    fn lib_name(&self) -> String {
        self.package.replace('-', "_")
    }

    fn owns(&self, file: &str) -> bool {
        self.dir == "." || file.starts_with(&format!("{}/", self.dir))
    }
}

struct SymbolRec {
    id: String,
    name: String,
    krate: String,
    module: String,
    kind: &'static str,
}

struct UseRec {
    krate: String,
    root: String,
    path: Vec<String>,
    file: String,
    target: Vec<String>,
}

struct ImplRec {
    krate: String,
    self_name: String,
    trait_name: String,
}

struct TestRec {
    id: String,
    krate: String,
    idents: BTreeSet<String>,
}

struct Builder<'a> {
    root: &'a Path,
    files: &'a BTreeSet<String>,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    symbols: Vec<SymbolRec>,
    uses: Vec<UseRec>,
    impls: Vec<ImplRec>,
    tests: Vec<TestRec>,
    modules: BTreeSet<(String, String)>,
    visited: BTreeSet<(String, String, String)>,
    parse_time: Duration,
    files_scanned: usize,
    unparsed: usize,
}

/// Where we are while walking one file's items.
struct Scope<'s> {
    krate: &'s str,
    root: &'s str,
    file: &'s str,
}

impl<'a> Builder<'a> {
    fn new(root: &'a Path, files: &'a BTreeSet<String>) -> Self {
        Builder {
            root,
            files,
            nodes: Vec::new(),
            edges: Vec::new(),
            symbols: Vec::new(),
            uses: Vec::new(),
            impls: Vec::new(),
            tests: Vec::new(),
            modules: BTreeSet::new(),
            visited: BTreeSet::new(),
            parse_time: Duration::ZERO,
            files_scanned: 0,
            unparsed: 0,
        }
    }

    fn node(&mut self, id: String, kind: GraphNodeKind) {
        self.nodes.push(GraphNode { id, kind });
    }

    fn edge(&mut self, from: &str, kind: GraphEdgeKind, to: &str) {
        self.edges.push(GraphEdge {
            from: from.to_string(),
            kind,
            to: to.to_string(),
        });
    }

    fn build(&mut self, crates: &[CrateInfo]) {
        let repo = "repository:.".to_string();
        self.node(repo.clone(), GraphNodeKind::Repository);

        for file in self.files {
            if file.ends_with(".rs") {
                self.nodes.push(GraphNode {
                    id: file_id(file),
                    kind: GraphNodeKind::File,
                });
                // The nearest enclosing crate owns the file.
                let owner = crates
                    .iter()
                    .filter(|c| c.owns(file))
                    .max_by_key(|c| if c.dir == "." { 0 } else { c.dir.len() + 1 });
                let from = owner.map_or(repo.clone(), |c| crate_id(&c.dir));
                self.edges.push(GraphEdge {
                    from,
                    kind: GraphEdgeKind::Contains,
                    to: file_id(file),
                });
            }
            if is_config(file) {
                let id = format!("config:{file}");
                self.node(id.clone(), GraphNodeKind::ConfigSurface);
                self.edge(&repo, GraphEdgeKind::Contains, &id);
            }
        }

        for c in crates {
            let id = crate_id(&c.dir);
            self.node(id.clone(), GraphNodeKind::Crate);
            self.edge(&repo, GraphEdgeKind::Contains, &id);

            if let Some(lib) = &c.lib {
                self.scan_root(c, &id, "crate", lib);
            }
            for (name, path) in &c.bins {
                let key = format!("bin/{name}");
                self.scan_root(c, &id, &key, path);
                let binary = format!("binary:{}:{name}", c.dir);
                self.node(binary.clone(), GraphNodeKind::BinaryTarget);
                self.edge(&id, GraphEdgeKind::Contains, &binary);
                self.edge(&binary, GraphEdgeKind::Targets, &module_id(&c.dir, &key));
            }
            for path in &c.tests {
                let stem = file_name(path).trim_end_matches(".rs");
                self.scan_root(c, &id, &format!("tests/{stem}"), path);
            }
        }
    }

    fn scan_root(&mut self, c: &CrateInfo, crate_node: &str, key: &str, file: &str) {
        let module = module_id(&c.dir, key);
        self.node(module.clone(), GraphNodeKind::Module);
        self.modules.insert((c.dir.clone(), key.to_string()));
        self.edge(crate_node, GraphEdgeKind::Contains, &module);
        self.edge(&module, GraphEdgeKind::Contains, &file_id(file));
        self.scan_file(&c.dir, key, Vec::new(), file, parent_dir(file));
    }

    fn scan_file(
        &mut self,
        krate: &str,
        root: &str,
        path: Vec<String>,
        file: &str,
        children_dir: &str,
    ) {
        let key = module_key(root, &path);
        if !self
            .visited
            .insert((krate.to_string(), key, file.to_string()))
        {
            return;
        }
        let started = Instant::now();
        let parsed = fs::read_to_string(self.root.join(file))
            .ok()
            .and_then(|text| syn::parse_file(&text).ok());
        self.parse_time += started.elapsed();
        self.files_scanned += 1;
        let Some(parsed) = parsed else {
            self.unparsed += 1;
            return;
        };
        let scope = Scope { krate, root, file };
        let mut path = path;
        self.scan_items(&scope, &mut path, children_dir, &parsed.items);
    }

    fn add_module(&mut self, scope: &Scope, parent: &[String], child: &[String], file: &str) {
        let parent_id = module_id(scope.krate, &module_key(scope.root, parent));
        let child_key = module_key(scope.root, child);
        let child_id = module_id(scope.krate, &child_key);
        self.node(child_id.clone(), GraphNodeKind::Module);
        self.modules.insert((scope.krate.to_string(), child_key));
        self.edge(&parent_id, GraphEdgeKind::Contains, &child_id);
        self.edge(&child_id, GraphEdgeKind::Contains, &file_id(file));
    }

    fn symbol(
        &mut self,
        scope: &Scope,
        path: &[String],
        tail: &str,
        name: &str,
        kind: &'static str,
    ) {
        let qualified = if path.is_empty() {
            tail.to_string()
        } else {
            format!("{}::{tail}", path.join("::"))
        };
        let id = format!("symbol:{}:{qualified}", scope.file);
        let module = module_key(scope.root, path);
        self.node(id.clone(), GraphNodeKind::Symbol);
        self.edge(
            &module_id(scope.krate, &module),
            GraphEdgeKind::Defines,
            &id,
        );
        self.symbols.push(SymbolRec {
            id,
            name: name.to_string(),
            krate: scope.krate.to_string(),
            module,
            kind,
        });
    }

    fn scan_items(
        &mut self,
        scope: &Scope,
        path: &mut Vec<String>,
        children_dir: &str,
        items: &[Item],
    ) {
        for item in items {
            match item {
                Item::Mod(m) => {
                    let name = clean(&m.ident);
                    if is_public(&m.vis) {
                        self.symbol(scope, path, &name, &name, "mod");
                    }
                    let parent = path.clone();
                    match &m.content {
                        Some((_, inner)) => {
                            path.push(name.clone());
                            self.add_module(scope, &parent, path, scope.file);
                            let dir = join(children_dir, &name);
                            self.scan_items(scope, path, &dir, inner);
                            path.pop();
                        }
                        None => {
                            let Some((target, dir)) =
                                self.resolve_mod_file(scope, children_dir, m, &name)
                            else {
                                continue;
                            };
                            path.push(name);
                            self.add_module(scope, &parent, path, &target);
                            let child = module_id(scope.krate, &module_key(scope.root, path));
                            self.edge(&file_id(scope.file), GraphEdgeKind::Imports, &child);
                            self.scan_file(scope.krate, scope.root, path.clone(), &target, &dir);
                            path.pop();
                        }
                    }
                }
                Item::Use(u) => {
                    let mut targets = Vec::new();
                    flatten(&u.tree, &mut Vec::new(), &mut targets);
                    for target in targets {
                        self.uses.push(UseRec {
                            krate: scope.krate.to_string(),
                            root: scope.root.to_string(),
                            path: path.clone(),
                            file: scope.file.to_string(),
                            target,
                        });
                    }
                }
                Item::Fn(f) => {
                    let name = clean(&f.sig.ident);
                    if is_public(&f.vis) {
                        self.symbol(scope, path, &name, &name, "fn");
                    }
                    if f.attrs.iter().any(is_test_attr) {
                        self.test(scope, path, &name, &f.block);
                    }
                }
                Item::Struct(s) if is_public(&s.vis) => {
                    let name = clean(&s.ident);
                    self.symbol(scope, path, &name, &name, "struct");
                }
                Item::Enum(e) if is_public(&e.vis) => {
                    let name = clean(&e.ident);
                    self.symbol(scope, path, &name, &name, "enum");
                }
                Item::Trait(t) if is_public(&t.vis) => {
                    let name = clean(&t.ident);
                    self.symbol(scope, path, &name, &name, "trait");
                }
                Item::Type(t) if is_public(&t.vis) => {
                    let name = clean(&t.ident);
                    self.symbol(scope, path, &name, &name, "type");
                }
                Item::Const(c) if is_public(&c.vis) => {
                    let name = clean(&c.ident);
                    self.symbol(scope, path, &name, &name, "const");
                }
                Item::Static(s) if is_public(&s.vis) => {
                    let name = clean(&s.ident);
                    self.symbol(scope, path, &name, &name, "static");
                }
                Item::Impl(i) => {
                    let Some(self_name) = type_name(&i.self_ty) else {
                        continue;
                    };
                    match &i.trait_ {
                        Some((_, trait_path, _)) => {
                            if let Some(last) = trait_path.segments.last() {
                                self.impls.push(ImplRec {
                                    krate: scope.krate.to_string(),
                                    self_name,
                                    trait_name: clean(&last.ident),
                                });
                            }
                        }
                        None => {
                            for member in &i.items {
                                if let ImplItem::Fn(m) = member {
                                    if is_public(&m.vis) {
                                        let name = clean(&m.sig.ident);
                                        let tail = format!("{self_name}::{name}");
                                        self.symbol(scope, path, &tail, &name, "method");
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn test(&mut self, scope: &Scope, path: &[String], name: &str, block: &syn::Block) {
        let qualified = if path.is_empty() {
            name.to_string()
        } else {
            format!("{}::{name}", path.join("::"))
        };
        let id = format!("test:{}:{qualified}", scope.file);
        self.node(id.clone(), GraphNodeKind::TestSuite);
        let module = module_id(scope.krate, &module_key(scope.root, path));
        self.edge(&module, GraphEdgeKind::Defines, &id);
        let mut idents = Idents(BTreeSet::new());
        idents.visit_block(block);
        self.tests.push(TestRec {
            id,
            krate: scope.krate.to_string(),
            idents: idents.0,
        });
    }

    /// Finds the file a `mod name;` declaration refers to, and the directory its own
    /// children live in.
    fn resolve_mod_file(
        &self,
        scope: &Scope,
        children_dir: &str,
        m: &syn::ItemMod,
        name: &str,
    ) -> Option<(String, String)> {
        for attr in &m.attrs {
            if !attr.path().is_ident("path") {
                continue;
            }
            let nv = attr.meta.require_name_value().ok()?;
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                let target = normalize(&join(parent_dir(scope.file), &s.value()));
                let dir = if file_name(&target) == "mod.rs" {
                    parent_dir(&target).to_string()
                } else {
                    join(
                        parent_dir(&target),
                        file_name(&target).trim_end_matches(".rs"),
                    )
                };
                return self.files.contains(&target).then_some((target, dir));
            }
            return None;
        }
        let dir = join(children_dir, name);
        [format!("{dir}.rs"), format!("{dir}/mod.rs")]
            .into_iter()
            .find(|c| self.files.contains(c))
            .map(|c| (c, dir))
    }

    #[allow(clippy::type_complexity)]
    fn finish(
        mut self,
        crates: &[CrateInfo],
    ) -> (Vec<GraphNode>, Vec<GraphEdge>, Duration, usize, usize) {
        // Names usable in source, per crate, and the crates visible to each.
        let by_package: BTreeMap<&str, &CrateInfo> = crates
            .iter()
            .rev()
            .map(|c| (c.package.as_str(), c))
            .collect();
        let mut libs: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut scope_of: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for c in crates {
            let scope = scope_of.entry(c.dir.clone()).or_default();
            scope.insert(c.dir.clone());
            if c.lib.is_some() {
                libs.insert((c.dir.clone(), c.lib_name()), c.dir.clone());
            }
            for (use_name, package) in &c.deps {
                if let Some(target) = by_package.get(package.as_str()) {
                    scope.insert(target.dir.clone());
                    if target.lib.is_some() {
                        libs.insert((c.dir.clone(), use_name.clone()), target.dir.clone());
                    }
                }
            }
        }

        let mut in_module: BTreeMap<(&str, &str, &str), &str> = BTreeMap::new();
        let mut by_name: BTreeMap<&str, Vec<&SymbolRec>> = BTreeMap::new();
        for s in &self.symbols {
            if s.kind != "method" {
                in_module
                    .entry((&s.krate, &s.module, &s.name))
                    .or_insert(&s.id);
            }
            by_name.entry(&s.name).or_default().push(s);
        }
        let unique = |name: &str, krate: &str, kinds: &[&str], any_kind: bool| -> Option<&str> {
            let scope = scope_of.get(krate)?;
            let ids: BTreeSet<&str> = by_name
                .get(name)?
                .iter()
                .filter(|s| scope.contains(&s.krate))
                .filter(|s| {
                    if any_kind {
                        s.kind != "mod"
                    } else {
                        kinds.contains(&s.kind)
                    }
                })
                .map(|s| s.id.as_str())
                .collect();
            (ids.len() == 1).then(|| *ids.iter().next().unwrap())
        };

        let mut edges = Vec::new();

        for u in &self.uses {
            let Some((krate, key, symbol)) = resolve_use(u, &libs, &self.modules, &in_module)
            else {
                continue;
            };
            let current = module_key(&u.root, &u.path);
            if (krate.as_str(), key.as_str()) != (u.krate.as_str(), current.as_str()) {
                edges.push(GraphEdge {
                    from: file_id(&u.file),
                    kind: GraphEdgeKind::Imports,
                    to: module_id(&krate, &key),
                });
            }
            if let Some(symbol) = symbol {
                edges.push(GraphEdge {
                    from: file_id(&u.file),
                    kind: GraphEdgeKind::Imports,
                    to: symbol.to_string(),
                });
            }
        }

        for i in &self.impls {
            let own: BTreeSet<&str> = by_name
                .get(i.self_name.as_str())
                .into_iter()
                .flatten()
                .filter(|s| s.krate == i.krate && ["struct", "enum", "type"].contains(&s.kind))
                .map(|s| s.id.as_str())
                .collect();
            let (Some(self_id), Some(trait_id)) = (
                (own.len() == 1).then(|| *own.iter().next().unwrap()),
                unique(&i.trait_name, &i.krate, &["trait"], false),
            ) else {
                continue;
            };
            edges.push(GraphEdge {
                from: self_id.to_string(),
                kind: GraphEdgeKind::Implements,
                to: trait_id.to_string(),
            });
        }

        for t in &self.tests {
            for ident in &t.idents {
                if let Some(target) = unique(ident, &t.krate, &[], true) {
                    edges.push(GraphEdge {
                        from: t.id.clone(),
                        kind: GraphEdgeKind::Tests,
                        to: target.to_string(),
                    });
                }
            }
        }

        self.edges.extend(edges);
        (
            self.nodes,
            self.edges,
            self.parse_time,
            self.files_scanned,
            self.unparsed,
        )
    }
}

/// Resolves a flattened `use` path to the longest module prefix it names, and the symbol
/// directly under that module if the next segment is one. `None` when the path leaves the
/// repository (std, external crates) or cannot be resolved.
fn resolve_use(
    u: &UseRec,
    libs: &BTreeMap<(String, String), String>,
    modules: &BTreeSet<(String, String)>,
    in_module: &BTreeMap<(&str, &str, &str), &str>,
) -> Option<(String, String, Option<String>)> {
    let first = u.target.first()?.as_str();
    let (krate, root, mut path, rest): (String, String, Vec<String>, &[String]) = match first {
        "crate" => (u.krate.clone(), u.root.clone(), Vec::new(), &u.target[1..]),
        "self" => (
            u.krate.clone(),
            u.root.clone(),
            u.path.clone(),
            &u.target[1..],
        ),
        "super" => {
            let mut base = u.path.clone();
            let mut rest = &u.target[..];
            while rest.first().map(String::as_str) == Some("super") {
                base.pop()?;
                rest = &rest[1..];
            }
            (u.krate.clone(), u.root.clone(), base, rest)
        }
        name => {
            if let Some(target) = libs.get(&(u.krate.clone(), name.to_string())) {
                (
                    target.clone(),
                    "crate".to_string(),
                    Vec::new(),
                    &u.target[1..],
                )
            } else {
                let mut child = u.path.clone();
                child.push(name.to_string());
                let key = module_key(&u.root, &child);
                if !modules.contains(&(u.krate.clone(), key)) {
                    return None;
                }
                (
                    u.krate.clone(),
                    u.root.clone(),
                    u.path.clone(),
                    &u.target[..],
                )
            }
        }
    };
    let mut consumed = 0;
    for segment in rest {
        path.push(segment.clone());
        if modules.contains(&(krate.clone(), module_key(&root, &path))) {
            consumed += 1;
        } else {
            path.pop();
            break;
        }
    }
    let key = module_key(&root, &path);
    let symbol = rest
        .get(consumed)
        .and_then(|name| in_module.get(&(krate.as_str(), key.as_str(), name.as_str())))
        .map(|id| id.to_string());
    Some((krate, key, symbol))
}

fn flatten(tree: &UseTree, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match tree {
        UseTree::Path(p) => {
            prefix.push(clean(&p.ident));
            flatten(&p.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(n) => leaf(&n.ident, prefix, out),
        UseTree::Rename(r) => leaf(&r.ident, prefix, out),
        UseTree::Glob(_) => out.push(prefix.clone()),
        UseTree::Group(g) => {
            for item in &g.items {
                flatten(item, prefix, out);
            }
        }
    }
}

fn leaf(ident: &proc_macro2::Ident, prefix: &[String], out: &mut Vec<Vec<String>>) {
    let mut full = prefix.to_vec();
    if ident != "self" {
        full.push(clean(ident));
    }
    out.push(full);
}

fn is_public(vis: &Visibility) -> bool {
    matches!(vis, Visibility::Public(_))
}

fn is_test_attr(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "test")
}

fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| clean(&s.ident)),
        _ => None,
    }
}

/// Collects every identifier a test body mentions, including inside macro arguments.
struct Idents(BTreeSet<String>);

impl<'ast> Visit<'ast> for Idents {
    fn visit_ident(&mut self, ident: &'ast proc_macro2::Ident) {
        self.0.insert(clean(ident));
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        collect_tokens(&mac.tokens, &mut self.0);
        syn::visit::visit_macro(self, mac);
    }
}

fn collect_tokens(tokens: &TokenStream, out: &mut BTreeSet<String>) {
    for token in tokens.clone() {
        match token {
            TokenTree::Ident(i) => {
                out.insert(clean(&i));
            }
            TokenTree::Group(g) => collect_tokens(&g.stream(), out),
            _ => {}
        }
    }
}
