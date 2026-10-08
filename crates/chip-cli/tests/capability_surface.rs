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
                ["path", "content", "query", "count", "offset", "length"]
                    .contains(&input.name.as_str()),
                "{id} takes an input named {}",
                input.name
            );
        }
    }
}

#[tokio::test]
async fn the_boundary_assigns_no_execution_id_and_keeps_the_provider_id_as_metadata() {
    // Execution identity is Chip's, assigned by the work loop; the provider's response id is only
    // correlation metadata and is never turned into an execution id.
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
    let request = |response_id: &str| match ModelDecisionBoundary
        .interpret(
            &ModelResponse::new(response_id, reply, Usage::new(1, 1)),
            &capabilities,
        )
        .unwrap()
    {
        WorkDecision::RequestCapability(r) => r,
        other => panic!("{other:?}"),
    };
    let r = request("resp-1");
    assert!(r.execution_id.is_unassigned());
    assert_eq!(r.provider_response_id.as_deref(), Some("resp-1"));
    assert_eq!(request("").provider_response_id, None);
}

/// `project.read`'s range inputs, through the validation the work loop applies to a model's request:
/// declared names and types only, bounds checked before anything executes.
#[tokio::test]
async fn the_read_range_is_a_declared_bounded_integer_input_and_nothing_else_is_accepted() {
    use chip_core::{
        Agent, CapabilityError, CapabilityId, CapabilityRequest, ExecutionId, InputValue,
    };
    use std::sync::Arc;

    struct NoModel;
    #[async_trait::async_trait]
    impl fx_core::ModelProvider for NoModel {
        async fn complete(
            &self,
            _: fx_core::ModelRequest,
        ) -> Result<fx_core::ModelResponse, fx_core::FxError> {
            Err(fx_core::FxError::Provider("not used".into()))
        }
    }
    let dir = std::env::temp_dir().join(format!("chip-surface-range-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "x").unwrap();
    let env = LocalEnvironment::new(
        opaque_id(&dir),
        &dir,
        PaxExecutor::new(&dir),
        EnvironmentDescription::default(),
    );
    let agent = Agent::new(Arc::new(NoModel)).with_capabilities(env.capabilities());
    let request = |inputs: &[(&str, InputValue)], present: bool| {
        let mut r = CapabilityRequest::new(
            ExecutionId::new("r"),
            CapabilityId::new("project.read").unwrap(),
        );
        r.inputs = inputs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        r.inputs_present = present;
        r
    };
    let path = || ("path", InputValue::Text("src/lib.rs".into()));
    use InputValue::{Bool, Integer, Text};

    // Accepted: the declared inputs, with integer values in range.
    for ok in [
        vec![path()],
        vec![path(), ("offset", Integer(0))],
        vec![path(), ("length", Integer(32_768))],
        vec![
            path(),
            ("offset", Integer(1_000_000)),
            ("length", Integer(1)),
        ],
    ] {
        let exec = agent.validate_capability_request(&request(&ok, true)).await;
        assert!(exec.is_ok(), "{ok:?}: {exec:?}");
        assert_eq!(exec.unwrap().intent, "project.read");
    }
    // Refused before any execution: wrong types, out-of-range values, an undeclared input, no path.
    for (what, bad) in [
        ("offset as text", vec![path(), ("offset", Text("0".into()))]),
        ("length as bool", vec![path(), ("length", Bool(true))]),
        ("negative offset", vec![path(), ("offset", Integer(-1))]),
        ("zero length", vec![path(), ("length", Integer(0))]),
        (
            "length over the limit",
            vec![path(), ("length", Integer(32_769))],
        ),
        ("a gigabyte", vec![path(), ("length", Integer(1 << 30))]),
        ("an undeclared input", vec![path(), ("whence", Integer(0))]),
        (
            "an input of another capability",
            vec![path(), ("query", Text("x".into()))],
        ),
        ("no path", vec![("offset", Integer(0))]),
    ] {
        let exec = agent
            .validate_capability_request(&request(&bad, true))
            .await;
        assert!(
            matches!(exec, Err(CapabilityError::InvalidInput(_))),
            "{what}: {exec:?}"
        );
    }
}
