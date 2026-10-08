//! The crate inventory in `docs/product/crates.md` stays synchronised with the workspace.
//!
//! Small and deterministic on purpose: it reads the root manifest, the inventory table and each
//! crate's manifest, and checks the claims the document makes about them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const CLASSES: &[&str] = &[
    "PRODUCT",
    "SUPPORTING",
    "INTEGRATION",
    "EXPERIMENT",
    "PROOF",
    "DEPRECATED",
    "UNPROVEN",
];
const SHIPPING_CLASSES: &[&str] = &["PRODUCT", "SUPPORTING", "INTEGRATION"];

struct Row {
    class: String,
    disposition: String,
    ci_level: String,
    reported_level: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn workspace_members() -> Vec<String> {
    let text = fs::read_to_string(repo_root().join("Cargo.toml")).unwrap();
    let members = text
        .split("members")
        .nth(1)
        .and_then(|rest| rest.split('[').nth(1))
        .and_then(|rest| rest.split(']').next())
        .expect("workspace members");
    members
        .split(',')
        .map(|m| m.trim().trim_matches('"'))
        .filter(|m| !m.is_empty())
        .map(|m| m.strip_prefix("crates/").unwrap_or(m).to_string())
        .collect()
}

fn inventory() -> Vec<(String, Row)> {
    let text = fs::read_to_string(repo_root().join("docs/product/crates.md")).unwrap();
    let table = text
        .split("<!-- inventory:start -->")
        .nth(1)
        .and_then(|rest| rest.split("<!-- inventory:end -->").next())
        .expect("inventory markers");
    let mut rows = Vec::new();
    for line in table.lines().map(str::trim).filter(|l| l.starts_with('|')) {
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        assert_eq!(cells.len(), 5, "malformed inventory row: {line}");
        if cells[0] == "Crate" || cells[0].starts_with("---") {
            continue;
        }
        let name = cells[0].trim_matches('`').to_string();
        rows.push((
            name,
            Row {
                class: cells[1].to_string(),
                disposition: cells[2].to_string(),
                ci_level: cells[3].to_string(),
                reported_level: cells[4].to_string(),
            },
        ));
    }
    rows
}

fn is_level(s: &str) -> bool {
    matches!(s, "L0" | "L1" | "L2" | "L3" | "L4" | "L5")
}

#[test]
fn every_workspace_crate_is_in_the_inventory_exactly_once_and_nothing_else_is() {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (name, _) in inventory() {
        *counts.entry(name).or_default() += 1;
    }
    for member in workspace_members() {
        assert_eq!(
            counts.remove(&member),
            Some(1),
            "{member} must appear exactly once in docs/product/crates.md"
        );
    }
    assert!(
        counts.is_empty(),
        "inventory lists crates that are not workspace members: {:?}",
        counts.keys().collect::<Vec<_>>()
    );
}

#[test]
fn every_inventory_crate_exists() {
    for (name, _) in inventory() {
        let manifest = repo_root().join("crates").join(&name).join("Cargo.toml");
        let text = fs::read_to_string(&manifest)
            .unwrap_or_else(|_| panic!("{name}: {} does not exist", manifest.display()));
        assert!(
            text.contains(&format!("name = \"{name}\"")),
            "{name}: package name differs from its directory"
        );
    }
}

#[test]
fn every_crate_has_exactly_one_known_classification_and_disposition() {
    for (name, row) in inventory() {
        assert!(
            CLASSES.contains(&row.class.as_str()),
            "{name}: unknown classification {:?}",
            row.class
        );
        let allowed: &[&str] = if SHIPPING_CLASSES.contains(&row.class.as_str()) {
            &["KEEP", "FROZEN"]
        } else {
            &["FROZEN", "REJECTED"]
        };
        assert!(
            allowed.contains(&row.disposition.as_str()),
            "{name}: a {} crate must be {:?}, not {:?}",
            row.class,
            allowed,
            row.disposition
        );
    }
}

#[test]
fn every_crate_has_a_documented_validation_level() {
    for (name, row) in inventory() {
        assert!(
            is_level(&row.ci_level),
            "{name}: CI level {:?}",
            row.ci_level
        );
        assert!(
            is_level(&row.reported_level) || row.reported_level == "-",
            "{name}: reported level {:?}",
            row.reported_level
        );
    }
}

#[test]
fn only_chip_cli_links_crates_that_are_not_product_integration_or_supporting() {
    let rows: BTreeMap<String, Row> = inventory().into_iter().collect();
    for (name, row) in &rows {
        if !SHIPPING_CLASSES.contains(&row.class.as_str()) || name == "chip-cli" {
            continue;
        }
        let manifest =
            fs::read_to_string(repo_root().join("crates").join(name).join("Cargo.toml")).unwrap();
        // Only normal dependencies; dev-dependencies are test fixtures.
        let deps = manifest
            .split("[dependencies]")
            .nth(1)
            .map(|rest| rest.split("\n[").next().unwrap())
            .unwrap_or("");
        for line in deps.lines() {
            let dep = line.split('=').next().unwrap().trim();
            if let Some(other) = rows.get(dep) {
                assert!(
                    SHIPPING_CLASSES.contains(&other.class.as_str()),
                    "{name} ({}) depends on {dep} ({}), which has not earned product status",
                    row.class,
                    other.class
                );
            }
        }
    }
}
