//! Rust Chip's capabilities executed through the generic `exec(argv, env)` transport, with the
//! worker as a separate process in its own working directory (as an external environment would run
//! it), against real files, real Git and real PAX/Cargo. The transport here is a plain local
//! process: it proves the Rust Chip side. Real Compute is exercised from the Compute repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use chip_cli::local_environment::LocalEnvironment;
use chip_cli::software_work::{CompleteWhenVerified, run_software_work_with_budget};
use chip_core::{
    EnvironmentDescription, EnvironmentId, ExecutionId, ExecutionRequest, Executor, InputValue,
    WorkEnvironment, WorkId, WorkLimits, WorkOutcome,
};
use chip_pax::PaxExecutor;
use chip_remote_env::{
    CommandOutput, CommandRunner, RemoteEnvironment, RunnerError, WorkerCommand,
};
use fx_core::{FxError, ModelProvider, ModelRequest, ModelResponse, Usage};

const OLD_LIB: &str = "pub fn payload_len(payload: &str) -> usize {\n    payload.len()\n}\n";
const RIGHT: &str = "pub fn payload_len(payload: &str) -> usize {\n    payload.len()\n}\n\npub fn canonical_fingerprint(payload: &str) -> String {\n    let mut pairs: Vec<&str> = payload.split('&').collect();\n    pairs.sort();\n    pairs.join(\"&\")\n}\n";
const TESTS: &str = "use fpfixture::canonical_fingerprint;\n\n#[test]\nfn pairs_are_sorted_and_joined() {\n    assert_eq!(canonical_fingerprint(\"b=2&a=1\"), \"a=1&b=2\");\n}\n\n#[test]\nfn a_single_pair_is_unchanged() {\n    assert_eq!(canonical_fingerprint(\"z=9\"), \"z=9\");\n}\n";

fn pax_installed() -> bool {
    std::process::Command::new("pax")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A project directory `base/project`, and `base` as the environment's working directory.
fn project(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("chip-remote-env-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fpfixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"fpfixture\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), OLD_LIB).unwrap();
    std::fs::write(root.join("tests/fingerprint.rs"), TESTS).unwrap();
    std::fs::write(root.join("README.md"), "baseline\n").unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "baseline",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
    }
    base
}

/// `exec(argv, env)` as a separate process in `cwd`, no stdin. Records every call.
struct ProcessRunner {
    cwd: PathBuf,
    calls: Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl CommandRunner for ProcessRunner {
    async fn run(
        &self,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Result<CommandOutput, RunnerError> {
        self.calls.lock().unwrap().push(argv.clone());
        let output = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .envs(env)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| RunnerError(e.to_string()))?;
        Ok(CommandOutput {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn worker() -> WorkerCommand {
    WorkerCommand {
        program: env!("CARGO_BIN_EXE_chip").to_string(),
        root: "project".to_string(),
    }
}

async fn remote(base: &Path) -> (RemoteEnvironment, Arc<ProcessRunner>) {
    let runner = Arc::new(ProcessRunner {
        cwd: base.to_path_buf(),
        calls: Mutex::new(Vec::new()),
    });
    let env = RemoteEnvironment::connect(
        EnvironmentId::new("env_remote_test"),
        runner.clone(),
        worker(),
    )
    .await
    .unwrap();
    (env, runner)
}

fn local(base: &Path) -> LocalEnvironment {
    let root = base.join("project");
    LocalEnvironment::new(
        EnvironmentId::new("env_local_test"),
        &root,
        PaxExecutor::new(&root),
        EnvironmentDescription::default(),
    )
}

fn text(v: &str) -> InputValue {
    InputValue::Text(v.to_string())
}

async fn exec(
    env: &dyn WorkEnvironment,
    n: usize,
    capability: &str,
    inputs: &[(&str, InputValue)],
) -> String {
    let set = env.capabilities();
    let id = chip_core::CapabilityId::new(capability).unwrap();
    let inputs: BTreeMap<String, InputValue> = inputs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    // Chip validates before it executes, exactly as the agent loop does.
    if let Err(e) = chip_core::CapabilityProvider::validate_inputs(set.as_ref(), &id, &inputs).await
    {
        return format!("refused|{e:?}");
    }
    let request =
        ExecutionRequest::new(ExecutionId::new(format!("x{n}")), capability).with_inputs(inputs);
    match set.execute(request).await {
        Ok(r) => format!("{:?}|{}", r.status, r.output),
        Err(e) => format!("error|{e}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_capability_means_the_same_remotely_as_locally() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let (a, b) = (project("same-local"), project("same-remote"));
    let (local, (remote, runner)) = (local(&a), remote(&b).await);
    let steps: Vec<(&str, Vec<(&str, InputValue)>)> = vec![
        ("project.list", vec![("path", text("."))]),
        ("project.read", vec![("path", text("src/lib.rs"))]),
        (
            "project.search",
            vec![("query", text("payload")), ("path", text("."))],
        ),
        (
            "project.write",
            vec![("path", text("src/lib.rs")), ("content", text(RIGHT))],
        ),
        (
            "project.write",
            vec![("path", text("src/lib.rs")), ("content", text(RIGHT))],
        ),
        (
            "project.write",
            vec![("path", text("README.md")), ("content", text("ALPHA\n"))],
        ),
        ("project.read", vec![("path", text("README.md"))]),
        ("project.git.status", vec![]),
        ("project.git.diff_stat", vec![]),
        ("project.git.diff", vec![]),
        ("project.git.log", vec![("count", InputValue::Integer(3))]),
        ("project.read", vec![("path", text("../outside"))]),
    ];
    for (n, (capability, inputs)) in steps.iter().enumerate() {
        let (l, r) = (
            exec(&local, n, capability, inputs).await,
            exec(&remote, n, capability, inputs).await,
        );
        // Git history carries a commit time; everything else must match byte for byte.
        if *capability == "project.git.log" {
            assert_eq!(l.lines().count(), r.lines().count(), "{capability}");
        } else {
            assert_eq!(l, r, "{capability} {inputs:?}");
        }
    }
    // The test runner, for real, on both.
    let (l, r) = (
        exec(&local, 90, "pax.test", &[]).await,
        exec(&remote, 90, "pax.test", &[]).await,
    );
    assert!(
        l.starts_with("Success|") && r.starts_with("Success|"),
        "{l}\n{r}"
    );
    assert!(r.contains("passed"), "{r}");
    // Every call crossed the transport as argv + env: the capability's content is not in argv.
    let calls = runner.calls.lock().unwrap();
    assert!(calls.len() >= steps.len());
    assert!(
        calls
            .iter()
            .all(|c| c.len() == 4 && c[1] == "capability-exec" && c[2] == "--root")
    );
    assert!(
        !calls
            .iter()
            .flatten()
            .any(|a| a.contains("ALPHA") || a.contains("canonical_fingerprint"))
    );
    // The work happened in the remote project, not in the local one.
    assert_eq!(
        std::fs::read_to_string(b.join("project/README.md")).unwrap(),
        "ALPHA\n"
    );
    assert_eq!(
        std::fs::read_to_string(a.join("project/README.md")).unwrap(),
        "ALPHA\n"
    );
}

/// Replies in order, then fails: the model is the only thing scripted here.
struct Script(Mutex<std::collections::VecDeque<String>>);

#[async_trait::async_trait]
impl ModelProvider for Script {
    async fn complete(&self, _r: ModelRequest) -> Result<ModelResponse, FxError> {
        let reply = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| FxError::Provider("unscripted model call".into()))?;
        Ok(ModelResponse::new("m", reply, Usage::new(3, 2)))
    }
}

fn script() -> Arc<Script> {
    let write = format!(
        r#"{{"decision":"request_capability","capability":"project.write","inputs":{{"path":"src/lib.rs","content":{}}}}}"#,
        serde_json::to_string(RIGHT).unwrap()
    );
    let test = r#"{"decision":"request_capability","capability":"pax.test"}"#.to_string();
    Arc::new(Script(Mutex::new([write, test].into())))
}

async fn work(env: &dyn WorkEnvironment) -> chip_cli::software_work::SoftwareWork {
    run_software_work_with_budget(
        WorkId::new("work"),
        script(),
        "scripted".into(),
        env,
        "Add `canonical_fingerprint` so the project's tests pass.",
        WorkLimits {
            max_turns: 12,
            max_executions: 8,
        },
        &CompleteWhenVerified,
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whole_work_is_the_same_locally_and_through_the_transport_and_verifies() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let (a, b) = (project("work-local"), project("work-remote"));
    let local_work = work(&local(&a)).await;
    let (remote_env, _) = remote(&b).await;
    let remote_work = work(&remote_env).await;

    for w in [&local_work, &remote_work] {
        assert!(
            matches!(w.report.outcome, WorkOutcome::Completed { .. }),
            "{:?}",
            w.report.outcome
        );
        assert!(w.verified, "verified from PAX's own observation");
        w.audit.assert_clean();
        assert_eq!(w.trajectory_violations, 0);
    }
    let shape = |w: &chip_cli::software_work::SoftwareWork| -> Vec<String> {
        w.report
            .events
            .iter()
            .map(|e| {
                format!("{e:?}")
                    .split([' ', '{', '('])
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect()
    };
    assert_eq!(shape(&local_work), shape(&remote_work));
    assert_eq!(
        local_work.report.summary.executions,
        remote_work.report.summary.executions
    );
    assert_eq!(
        local_work.report.summary.observations,
        remote_work.report.summary.observations
    );
    assert_eq!(local_work.paths_written, remote_work.paths_written);
    // The edit landed in the project in the environment, and in that one only.
    assert_eq!(
        std::fs::read_to_string(b.join("project/src/lib.rs")).unwrap(),
        RIGHT
    );
    assert_eq!(
        std::fs::read_to_string(a.join("project/src/lib.rs")).unwrap(),
        RIGHT
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_environment_command_is_a_failure_not_a_result() {
    if !pax_installed() {
        eprintln!("SKIPPED: PAX is not installed");
        return;
    }
    let base = project("broken");
    let (env, _) = remote(&base).await;
    // The environment stops being able to run anything useful.
    let broken = RemoteEnvironment::connect(
        EnvironmentId::new("env_broken"),
        Arc::new(ProcessRunner {
            cwd: base.clone(),
            calls: Mutex::new(Vec::new()),
        }),
        WorkerCommand {
            program: "/nonexistent/chip".into(),
            root: "project".into(),
        },
    )
    .await;
    assert!(
        broken.is_err(),
        "an environment with no worker is refused at connect"
    );
    let set = env.capabilities();
    let request = ExecutionRequest::new(ExecutionId::new("x1"), "project.read")
        .with_inputs([("path".to_string(), text("src/lib.rs"))].into());
    assert!(set.execute(request).await.is_ok());
    // Oversized request: refused whole, never truncated.
    let big = "x".repeat(chip_remote_env::MAX_REQUEST_BYTES + 1);
    let request = ExecutionRequest::new(ExecutionId::new("x2"), "project.write").with_inputs(
        [
            ("path".to_string(), text("a.txt")),
            ("content".to_string(), text(&big)),
        ]
        .into(),
    );
    let err = set.execute(request).await.unwrap_err();
    assert!(err.to_string().contains("carries at most"), "{err}");
    assert!(!base.join("project/a.txt").exists());
}
