//! The capability surface `chip work` offers is exactly the one `docs/product/capabilities.md`
//! documents, and the authority classes in that document are the ones the audit found.
//!
//! This is a product-boundary test, not a framework: adding, removing or re-classifying a
//! capability must be a visible, documented decision. It runs without PAX (declaring a capability
//! starts nothing).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chip_cli::local_environment::{LocalEnvironment, opaque_id};
use chip_core::{CapabilityProvider, EnvironmentDescription, WorkEnvironment};
use chip_pax::PaxExecutor;

const CLASSES: &[&str] = &["CORE", "INTEGRATION"];
const AUTHORITIES: &[&str] = &["read", "read-git", "write", "exec-via-tooling"];

struct Row {
    class: String,
    authority: String,
    level: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn documented() -> BTreeMap<String, Row> {
    let text = std::fs::read_to_string(repo_root().join("docs/product/capabilities.md")).unwrap();
    let table = text
        .split("<!-- capabilities:start -->")
        .nth(1)
        .and_then(|rest| rest.split("<!-- capabilities:end -->").next())
        .expect("capability table markers");
    let mut rows = BTreeMap::new();
    for line in table.lines().map(str::trim).filter(|l| l.starts_with('|')) {
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        assert_eq!(cells.len(), 5, "malformed row: {line}");
        if cells[0] == "Capability" || cells[0].starts_with("---") {
            continue;
        }
        let id = cells[0].trim_matches('`').to_string();
        let row = Row {
            class: cells[1].into(),
            authority: cells[2].into(),
            level: cells[4].into(),
        };
        assert!(rows.insert(id.clone(), row).is_none(), "{id} appears twice");
    }
    rows
}

async fn declared() -> Vec<chip_core::CapabilityDescriptor> {
    let dir = std::env::temp_dir().join(format!("chip-surface-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let env = LocalEnvironment::new(
        opaque_id(&dir),
        &dir,
        PaxExecutor::new(&dir),
        EnvironmentDescription::default(),
    );
    env.capabilities().capabilities().await.unwrap()
}

#[tokio::test]
async fn the_declared_surface_is_exactly_the_documented_one() {
    let documented = documented();
    let declared = declared().await;
    let mut ids: Vec<String> = declared.iter().map(|d| d.id.to_string()).collect();
    ids.sort();
    let mut docs: Vec<String> = documented.keys().cloned().collect();
    docs.sort();
    assert_eq!(
        ids, docs,
        "a capability was added, removed or renamed without updating docs/product/capabilities.md"
    );
    for (id, row) in &documented {
        assert!(
            CLASSES.contains(&row.class.as_str()),
            "{id}: class {}",
            row.class
        );
        assert!(
            AUTHORITIES.contains(&row.authority.as_str()),
            "{id}: authority {}",
            row.authority
        );
        assert!(
            matches!(row.level.as_str(), "L0" | "L1" | "L2" | "L3" | "L4" | "L5"),
            "{id}: level {}",
            row.level
        );
    }
}

#[tokio::test]
async fn exactly_one_capability_writes_and_exactly_one_runs_project_tooling() {
    let documented = documented();
    let with = |authority: &str| -> Vec<&String> {
        documented
            .iter()
            .filter(|(_, r)| r.authority == authority)
            .map(|(id, _)| id)
            .collect()
    };
    assert_eq!(
        with("write"),
        ["project.write"],
        "the only capability that changes the project"
    );
    assert_eq!(
        with("exec-via-tooling"),
        ["pax.test"],
        "the only one that runs the project's own tooling"
    );
}

#[tokio::test]
async fn no_declared_capability_is_a_shell_a_process_a_network_or_a_git_mutation() {
    for d in declared().await {
        let id = d.id.to_string();
        for banned in [
            "shell", "exec", "process", "command", "bash", "http", "fetch", "net", "browser",
            "sql", "db", "secret", "commit", "push", "checkout", "reset", "branch", "delete",
            "remove", "rename", "mkdir",
        ] {
            assert!(
                !id.contains(banned),
                "{id}: a higher-authority capability needs an explicit design"
            );
        }
        // Everything the product offers depends on state that changes between requests, so none is
        // ever answered from remembered evidence.
        assert!(!d.reuse_evidence, "{id} may be answered from memory");
        // The model may supply only these input names: never a command, an executable, an
        // argument vector, a working directory, a root, a status, an observation or a receipt.
        for input in &d.inputs {
            assert!(
                ["path", "content", "query", "count"].contains(&input.name.as_str()),
                "{id} takes an input named {}",
                input.name
            );
        }
    }
}

#[tokio::test]
async fn execution_ids_are_chips_and_derive_from_the_providers_response_id() {
    // Chip builds the id ("model-" plus the sanitised provider response id); a model cannot supply
    // one. A provider that repeats a response id repeats the id, and nothing in the audit objects:
    // recorded in docs/product/capabilities.md as an evidence-identity limit.
    use chip_core::{Capability, ModelDecisionBoundary, WorkDecision, WorkDecisionBoundary};
    use fx_core::{ModelResponse, Usage};
    let capabilities: Vec<Capability> = {
        let dir = std::env::temp_dir().join(format!("chip-surface-{}", std::process::id()));
        let env = LocalEnvironment::new(
            opaque_id(&dir),
            &dir,
            PaxExecutor::new(&dir),
            EnvironmentDescription::default(),
        );
        let set = env.capabilities();
        let mut out = Vec::new();
        for d in set.capabilities().await.unwrap() {
            let availability = set.availability(&d.id).await;
            out.push(Capability {
                descriptor: d,
                availability,
            });
        }
        out
    };
    let reply = r#"{"decision":"request_capability","capability":"project.list"}"#;
    let id_of = |response_id: &str| match ModelDecisionBoundary
        .interpret(
            &ModelResponse::new(response_id, reply, Usage::new(1, 1)),
            &capabilities,
        )
        .unwrap()
    {
        WorkDecision::RequestCapability(r) => r.execution_id.0,
        other => panic!("{other:?}"),
    };
    assert_eq!(id_of("resp-1"), "model-resp-1");
    assert_eq!(
        id_of("resp-1"),
        id_of("resp-1"),
        "a repeated provider id repeats the execution id"
    );
    assert_eq!(
        id_of("a b/../c"),
        "model-abc",
        "the provider's id is sanitised, never trusted as written"
    );
}
