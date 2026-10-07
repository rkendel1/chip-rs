//! `chip-cli init` and `chip-cli graph`: build and show the architecture graph snapshot.
//!
//! The graph is a projection of source. These commands only read source (for `init`) and
//! read or write the snapshot cache under `.chip/graph/`; they never run anything.

use std::path::{Path, PathBuf};
use std::time::Instant;

use chip_graph::{
    ArchitectureGraph, CapabilityCatalog, GraphEdgeKind, GraphNodeKind, StoreError, analyze_impact,
    analyze_with_stats, find_repository_root, read_latest, write_snapshot,
};

fn parse_root(args: &[String]) -> Result<PathBuf, String> {
    match args.iter().position(|a| a == "--root") {
        Some(i) => args
            .get(i + 1)
            .map(PathBuf::from)
            .ok_or_else(|| "--root needs a path".to_string()),
        None => {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            Ok(find_repository_root(&cwd).unwrap_or(cwd))
        }
    }
}

fn count(graph: &ArchitectureGraph, kind: GraphNodeKind) -> usize {
    graph.count_nodes(kind)
}

fn display_name(root: &Path) -> String {
    std::fs::canonicalize(root)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".to_string())
}

/// Returns the process exit code.
pub fn init(args: &[String]) -> i32 {
    let benchmark = args.iter().any(|a| a == "--benchmark");
    let root = match parse_root(args) {
        Ok(root) => root,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let total = Instant::now();
    let (graph, stats) = match analyze_with_stats(&root) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("chip init failed: {e}");
            return 1;
        }
    };
    let serialize = Instant::now();
    let snapshot_size = graph.to_snapshot_json().len();
    let serialize_time = serialize.elapsed();
    let written = match write_snapshot(&root, &graph) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("chip init failed: {e}");
            return 1;
        }
    };
    let total_time = total.elapsed();
    let relative = written.strip_prefix(&root).unwrap_or(&written);

    println!("Chip Init");
    println!("=========");
    println!();
    println!("Repository: {}", display_name(&root));
    println!("Crates: {}", count(&graph, GraphNodeKind::Crate));
    println!("Modules: {}", count(&graph, GraphNodeKind::Module));
    println!("Files: {}", count(&graph, GraphNodeKind::File));
    println!("Symbols: {}", count(&graph, GraphNodeKind::Symbol));
    println!("Tests: {}", count(&graph, GraphNodeKind::TestSuite));
    println!("Binaries: {}", count(&graph, GraphNodeKind::BinaryTarget));
    println!("Config: {}", count(&graph, GraphNodeKind::ConfigSurface));
    println!();
    println!("Nodes: {}", graph.nodes.len());
    println!("Edges: {}", graph.edges.len());
    println!();
    println!("Snapshot: {}", graph.snapshot_id);
    println!("Written: {}", relative.display());
    println!();
    println!("The graph is a projection of source, not authority. Source, tests and evidence win.");
    if stats.unparsed_files > 0 {
        println!(
            "Note: {} Rust file(s) could not be parsed and contribute no symbols.",
            stats.unparsed_files
        );
    }
    if benchmark {
        println!();
        println!("Benchmark");
        println!("---------");
        println!("Files scanned: {}", stats.files_scanned);
        println!("Nodes: {}", graph.nodes.len());
        println!("Edges: {}", graph.edges.len());
        println!(
            "Parse time: {:.3} ms",
            stats.parse_time.as_secs_f64() * 1000.0
        );
        println!(
            "Graph construction time: {:.3} ms",
            stats.graph_time.as_secs_f64() * 1000.0
        );
        println!(
            "Serialization time: {:.3} ms",
            serialize_time.as_secs_f64() * 1000.0
        );
        println!("Total: {:.3} ms", total_time.as_secs_f64() * 1000.0);
        println!("Snapshot size: {snapshot_size} bytes");
    }
    0
}

/// Returns the process exit code. Reads the stored snapshot; never rebuilds the graph.
pub fn graph(args: &[String]) -> i32 {
    let root = match parse_root(args) {
        Ok(root) => root,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let graph = match read_latest(&root) {
        Ok(graph) => graph,
        Err(StoreError::NotFound) => {
            println!("No architecture snapshot found.");
            println!("Run `chip init`.");
            return 1;
        }
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!("Architecture Graph");
    println!("==================");
    println!();
    println!("Snapshot: {}", graph.snapshot_id);
    println!();
    println!("Nodes");
    println!("  Crates: {}", count(&graph, GraphNodeKind::Crate));
    println!("  Modules: {}", count(&graph, GraphNodeKind::Module));
    println!("  Files: {}", count(&graph, GraphNodeKind::File));
    println!("  Symbols: {}", count(&graph, GraphNodeKind::Symbol));
    println!("  Tests: {}", count(&graph, GraphNodeKind::TestSuite));
    println!("  Binaries: {}", count(&graph, GraphNodeKind::BinaryTarget));
    println!("  Config: {}", count(&graph, GraphNodeKind::ConfigSurface));
    println!();
    println!("Edges");
    let edge = |kind: GraphEdgeKind| graph.count_edges(kind);
    println!("  Contains: {}", edge(GraphEdgeKind::Contains));
    println!("  Imports: {}", edge(GraphEdgeKind::Imports));
    println!("  Defines: {}", edge(GraphEdgeKind::Defines));
    println!("  Implements: {}", edge(GraphEdgeKind::Implements));
    println!("  Tests: {}", edge(GraphEdgeKind::Tests));
    println!("  Targets: {}", edge(GraphEdgeKind::Targets));
    0
}

/// `chip-cli impact --capabilities FILE [--root PATH] <path>...`
///
/// Reads the stored snapshot and an external capability catalog; never runs `init`, never
/// infers or invents a catalog. Returns the process exit code.
pub fn impact(args: &[String]) -> i32 {
    let mut catalog_path: Option<String> = None;
    let mut paths = Vec::new();
    let mut rest = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--capabilities" => catalog_path = iter.next().cloned(),
            "--root" => {
                rest.push(arg.clone());
                rest.extend(iter.next().cloned());
            }
            _ => paths.push(arg.clone()),
        }
    }
    let Some(catalog_path) = catalog_path else {
        eprintln!("usage: chip-cli impact --capabilities FILE [--root PATH] <changed path>...");
        return 2;
    };
    if paths.is_empty() {
        eprintln!("usage: chip-cli impact --capabilities FILE [--root PATH] <changed path>...");
        return 2;
    }
    let root = match parse_root(&rest) {
        Ok(root) => root,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let graph = match read_latest(&root) {
        Ok(graph) => graph,
        Err(StoreError::NotFound) => {
            println!("No architecture snapshot found.");
            println!("Run `chip init`.");
            return 1;
        }
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let text = match std::fs::read_to_string(&catalog_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("Capability catalog not found:");
            println!("  {catalog_path}");
            return 1;
        }
        Err(e) => {
            eprintln!("cannot read {catalog_path}: {e}");
            return 1;
        }
    };
    let catalog = match CapabilityCatalog::from_json(&text, &graph) {
        Ok(catalog) => catalog,
        Err(e) => {
            eprintln!("{catalog_path}: {e}");
            return 1;
        }
    };
    print!("{}", analyze_impact(&graph, &catalog, &paths).render());
    0
}
