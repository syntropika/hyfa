mod support;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    process::{Command, Output},
    sync::{Arc, Mutex},
};

use mockito::{Matcher, Mock, Request, Server, ServerGuard};
use serde_json::{Value, json};
use tempfile::TempDir;

#[test]
fn reconciliation_contains_conflicts_and_advances_independent_branches() {
    let state = TempDir::new().expect("temporary state directory");
    let remote = Arc::new(Mutex::new(RemoteRepository::new([
        (1, vec!["priority:p1"]),
        (2, vec!["priority:p2"]),
        (3, vec!["priority:p3"]),
    ])));
    let mut github = Server::new();
    let mocks = mock_stateful_repository(&mut github, Arc::clone(&remote));

    assert_success(&run(
        &state,
        &github.url(),
        &["sync", "--repo", "acme/reconcile", "--json"],
    ));
    remote.lock().expect("remote lock").online = false;

    let first = queue(&state, &github.url(), 1, "p0");
    let second = queue(&state, &github.url(), 1, "p4");
    let third = queue(&state, &github.url(), 2, "p0");
    let fourth = queue(&state, &github.url(), 3, "p0");
    let first_id = operation_id(&first);
    let second_id = operation_id(&second);
    let third_id = operation_id(&third);
    let fourth_id = operation_id(&fourth);

    {
        let mut remote = remote.lock().expect("remote lock");
        remote.online = true;
        remote.labels.insert(1, vec!["priority:p3".to_owned()]);
        remote.labels.insert(2, vec!["priority:p0".to_owned()]);
    }

    let reconciled = run(
        &state,
        &github.url(),
        &["reconcile", "--repo", "acme/reconcile", "--json"],
    );
    assert_success(&reconciled);
    let reconciled: Value = serde_json::from_slice(&reconciled.stdout).expect("reconcile JSON");
    assert_eq!(reconciled["schema_version"], "hyfa.reconcile/v1");
    assert_eq!(reconciled["command"], "reconcile");
    assert_eq!(reconciled["summary"]["conflicting"], 1);
    assert_eq!(reconciled["summary"]["transitively_blocked"], 1);
    assert_eq!(reconciled["summary"]["already_satisfied"], 1);
    assert_eq!(reconciled["summary"]["applied"], 1);

    let operations = reconciled["operations"].as_array().expect("operations");
    let conflict = find_operation(operations, &first_id);
    assert_eq!(conflict["classification"], "conflicting");
    assert_eq!(
        conflict["base"],
        json!({"state": "declared", "value": "p1"})
    );
    assert_eq!(
        conflict["local"],
        json!({"state": "declared", "value": "p0"})
    );
    assert_eq!(
        conflict["remote"],
        json!({"state": "declared", "value": "p3"})
    );

    let blocked = find_operation(operations, &second_id);
    assert_eq!(blocked["classification"], "transitively_blocked");
    assert_eq!(blocked["blocked_by"], json!([first_id]));
    assert_eq!(
        find_operation(operations, &third_id)["classification"],
        "already_satisfied"
    );
    assert_eq!(
        find_operation(operations, &fourth_id)["classification"],
        "applicable"
    );
    assert_eq!(find_operation(operations, &fourth_id)["outcome"], "applied");

    let remote = remote.lock().expect("remote lock");
    assert_eq!(remote.labels[&1], vec!["priority:p3"]);
    assert_eq!(remote.labels[&2], vec!["priority:p0"]);
    assert_eq!(remote.labels[&3], vec!["priority:p0"]);
    assert_eq!(
        remote.accepted_writes,
        vec!["add #3 priority:p0", "remove #3 priority:p3"]
    );
    drop(remote);

    let outbox: Value =
        serde_json::from_slice(&fs::read(outbox_path(&state)).expect("remaining outbox"))
            .expect("outbox JSON");
    let remaining = outbox["operations"]
        .as_array()
        .expect("remaining operations");
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0]["id"], first_id);
    assert_eq!(remaining[1]["id"], second_id);
    assert_eq!(remaining[1]["depends_on"], json!([first_id]));

    let replica: Value =
        serde_json::from_slice(&fs::read(replica_path(&state)).expect("synchronized replica"))
            .expect("replica JSON");
    assert_eq!(issue_priority(&replica, 2), "priority:p0");
    assert_eq!(issue_priority(&replica, 3), "priority:p0");
    mocks.assert();
}

#[test]
fn final_verification_retires_every_accepted_step_in_a_superseding_chain() {
    let state = TempDir::new().expect("temporary state directory");
    let remote = Arc::new(Mutex::new(RemoteRepository::new([(
        1,
        vec!["priority:p1"],
    )])));
    let mut github = Server::new();
    let mocks = mock_stateful_repository(&mut github, Arc::clone(&remote));
    assert_success(&run(
        &state,
        &github.url(),
        &["sync", "--repo", "acme/reconcile", "--json"],
    ));
    remote.lock().expect("remote lock").online = false;
    queue(&state, &github.url(), 1, "p0");
    queue(&state, &github.url(), 1, "p4");
    remote.lock().expect("remote lock").online = true;

    let reconciled = run(
        &state,
        &github.url(),
        &["reconcile", "--repo", "acme/reconcile", "--json"],
    );
    assert_success(&reconciled);
    let reconciled: Value = serde_json::from_slice(&reconciled.stdout).expect("reconcile JSON");
    assert_eq!(reconciled["summary"]["applied"], 2);
    assert_eq!(reconciled["summary"]["conflicting"], 0);
    assert_eq!(reconciled["summary"]["remaining"], 0);
    assert_eq!(
        remote.lock().expect("remote lock").labels[&1],
        vec!["priority:p4"]
    );
    let outbox: Value =
        serde_json::from_slice(&fs::read(outbox_path(&state)).expect("retired outbox"))
            .expect("outbox JSON");
    assert!(
        outbox["operations"]
            .as_array()
            .expect("operations")
            .is_empty()
    );
    mocks.assert();
}

#[test]
fn a_persisted_conflict_is_reclassified_when_github_now_satisfies_the_intent() {
    let fixture = conflict_fixture();
    fixture
        .remote
        .lock()
        .expect("remote lock")
        .labels
        .insert(1, vec!["priority:p0".to_owned()]);

    let reconciled = run(
        &fixture.state,
        &fixture.github.url(),
        &["reconcile", "--repo", "acme/reconcile", "--json"],
    );
    assert_success(&reconciled);
    let reconciled: Value = serde_json::from_slice(&reconciled.stdout).expect("reconcile JSON");
    assert_eq!(
        reconciled["operations"][0]["classification"],
        "already_satisfied"
    );
    assert_eq!(reconciled["summary"]["remaining"], 0);
    assert!(
        fixture
            .remote
            .lock()
            .expect("remote lock")
            .accepted_writes
            .is_empty()
    );
    fixture.mocks.assert();
}

#[test]
fn remote_resolution_retires_the_intent_without_a_github_write() {
    let fixture = conflict_fixture();
    fixture.remote.lock().expect("remote lock").online = false;
    let pending = ranked_output(&fixture, "next");
    let cached_pending = ranked_output(&fixture, "next");
    assert_eq!(pending["pending"], true);
    assert_eq!(
        pending["recommendation"]["first_issue"]["priority"]["value"],
        "p0"
    );
    assert_eq!(cached_pending["performance"]["cache_hit"], true);
    assert_eq!(pending["recommendation"], cached_pending["recommendation"]);
    fixture.remote.lock().expect("remote lock").online = true;

    let resolved = run(
        &fixture.state,
        &fixture.github.url(),
        &[
            "resolve",
            &fixture.operation_id,
            "--repo",
            "acme/reconcile",
            "--remote",
            "--json",
        ],
    );
    assert_success(&resolved);
    let resolved: Value = serde_json::from_slice(&resolved.stdout).expect("resolve JSON");
    assert_eq!(resolved["schema_version"], "hyfa.resolve/v1");
    assert_eq!(resolved["resolution"]["choice"], "remote");
    assert_eq!(resolved["summary"]["remaining"], 0);
    assert!(
        fixture
            .remote
            .lock()
            .expect("remote lock")
            .accepted_writes
            .is_empty()
    );
    assert!(
        serde_json::from_slice::<Value>(&fs::read(outbox_path(&fixture.state)).expect("outbox"))
            .expect("outbox JSON")["operations"]
            .as_array()
            .expect("operations")
            .is_empty()
    );
    fixture.remote.lock().expect("remote lock").online = false;
    let reconciled = ranked_output(&fixture, "next");
    let plan = ranked_output(&fixture, "plan");
    let cached_reconciled = ranked_output(&fixture, "next");
    assert_eq!(reconciled["performance"]["cache_hit"], false);
    assert_eq!(cached_reconciled["performance"]["cache_hit"], true);
    assert_ne!(pending["input_hash"], reconciled["input_hash"]);
    assert_eq!(reconciled["pending"], false);
    assert!(reconciled.get("pending_operation_ids").is_none());
    assert_eq!(
        reconciled["recommendation"]["first_issue"]["priority"]["value"],
        "p3"
    );
    assert_eq!(plan["parallel_now"][0]["priority"]["value"], "p3");
    assert_eq!(plan["parallel_now"][0]["pending"], false);
    for (field, value) in plan["decision"].as_object().expect("plan decision") {
        if field == "parameters" {
            for (parameter, expected) in value.as_object().expect("plan parameters") {
                assert_eq!(expected, &reconciled[field][parameter]);
            }
        } else {
            assert_eq!(value, &reconciled[field], "shared {field}");
            assert_eq!(value, &cached_reconciled[field], "cached shared {field}");
        }
    }
    fixture.mocks.assert();
}

#[test]
fn local_resolution_rebases_then_applies_the_original_value() {
    let fixture = conflict_fixture();

    let resolved = run(
        &fixture.state,
        &fixture.github.url(),
        &[
            "resolve",
            &fixture.operation_id,
            "--repo",
            "acme/reconcile",
            "--local",
            "--json",
        ],
    );
    assert_success(&resolved);
    let resolved: Value = serde_json::from_slice(&resolved.stdout).expect("resolve JSON");
    assert_eq!(resolved["resolution"]["choice"], "local");
    assert_eq!(resolved["operations"][0]["classification"], "applicable");
    assert_eq!(resolved["operations"][0]["outcome"], "applied");
    let remote = fixture.remote.lock().expect("remote lock");
    assert_eq!(remote.labels[&1], vec!["priority:p0"]);
    assert_eq!(
        remote.accepted_writes,
        vec!["add #1 priority:p0", "remove #1 priority:p3"]
    );
    drop(remote);
    fixture.mocks.assert();
}

#[test]
fn replacement_resolution_rebases_and_applies_the_explicit_priority() {
    let fixture = conflict_fixture();

    let resolved = run(
        &fixture.state,
        &fixture.github.url(),
        &[
            "resolve",
            &fixture.operation_id,
            "--repo",
            "acme/reconcile",
            "--priority",
            "p4",
            "--json",
        ],
    );
    assert_success(&resolved);
    let resolved: Value = serde_json::from_slice(&resolved.stdout).expect("resolve JSON");
    assert_eq!(resolved["resolution"]["choice"], "priority");
    assert_eq!(
        resolved["operations"][0]["local"],
        json!({"state": "declared", "value": "p4"})
    );
    assert_eq!(
        fixture.remote.lock().expect("remote lock").labels[&1],
        vec!["priority:p4"]
    );
    fixture.mocks.assert();
}

#[test]
fn replacement_resolution_accepts_none() {
    let fixture = conflict_fixture();

    let resolved = run(
        &fixture.state,
        &fixture.github.url(),
        &[
            "resolve",
            &fixture.operation_id,
            "--repo",
            "acme/reconcile",
            "--priority",
            "none",
            "--json",
        ],
    );
    assert_success(&resolved);
    let resolved: Value = serde_json::from_slice(&resolved.stdout).expect("resolve JSON");
    assert_eq!(
        resolved["operations"][0]["local"],
        json!({"state": "unspecified"})
    );
    assert!(fixture.remote.lock().expect("remote lock").labels[&1].is_empty());
    fixture.mocks.assert();
}

#[test]
fn local_resolution_conflicts_again_when_remote_changed_after_observation() {
    let fixture = conflict_fixture();
    fixture
        .remote
        .lock()
        .expect("remote lock")
        .labels
        .insert(1, vec!["priority:p2".to_owned()]);

    let resolved = run(
        &fixture.state,
        &fixture.github.url(),
        &[
            "resolve",
            &fixture.operation_id,
            "--repo",
            "acme/reconcile",
            "--local",
            "--json",
        ],
    );
    assert_success(&resolved);
    let resolved: Value = serde_json::from_slice(&resolved.stdout).expect("resolve JSON");
    assert_eq!(resolved["operations"][0]["classification"], "conflicting");
    assert_eq!(
        resolved["operations"][0]["base"],
        json!({"state": "declared", "value": "p3"})
    );
    assert_eq!(
        resolved["operations"][0]["remote"],
        json!({"state": "declared", "value": "p2"})
    );
    assert!(
        fixture
            .remote
            .lock()
            .expect("remote lock")
            .accepted_writes
            .is_empty()
    );
    fixture.mocks.assert();
}

#[test]
fn accepted_writes_are_checkpointed_and_not_replayed_after_final_sync_failure() {
    let state = TempDir::new().expect("temporary state directory");
    let remote = Arc::new(Mutex::new(RemoteRepository::new([(
        1,
        vec!["priority:p1"],
    )])));
    let mut github = Server::new();
    let mocks = mock_stateful_repository(&mut github, Arc::clone(&remote));
    assert_success(&run(
        &state,
        &github.url(),
        &["sync", "--repo", "acme/reconcile", "--json"],
    ));
    remote.lock().expect("remote lock").online = false;
    queue(&state, &github.url(), 1, "p0");
    {
        let mut remote = remote.lock().expect("remote lock");
        remote.online = true;
        remote.fail_refresh_after_write = true;
    }

    let interrupted = run(
        &state,
        &github.url(),
        &["reconcile", "--repo", "acme/reconcile", "--json"],
    );
    assert!(!interrupted.status.success());
    let diagnostic = support::synchronization_status(&state, "acme/reconcile");
    assert_eq!(diagnostic["last_attempt"]["state"], "failed");
    assert_eq!(diagnostic["last_attempt"]["stage"], "refreshing");
    assert_eq!(
        diagnostic["last_attempt"]["failure"]["code"],
        "github_status"
    );
    assert!(diagnostic["last_attempt"]["published_synced_at"].is_null());

    let outbox: Value =
        serde_json::from_slice(&fs::read(outbox_path(&state)).expect("checkpointed outbox"))
            .expect("outbox JSON");
    assert_eq!(outbox["operations"][0]["state"]["status"], "applied");
    let writes_after_failure = remote.lock().expect("remote lock").accepted_writes.clone();
    assert_eq!(
        writes_after_failure,
        vec!["add #1 priority:p0", "remove #1 priority:p1"]
    );

    remote.lock().expect("remote lock").fail_refresh_after_write = false;
    let resumed = run(
        &state,
        &github.url(),
        &["reconcile", "--repo", "acme/reconcile", "--json"],
    );
    assert_success(&resumed);
    let resumed: Value = serde_json::from_slice(&resumed.stdout).expect("resumed JSON");
    assert_eq!(resumed["operations"][0]["outcome"], "checkpointed");
    assert_eq!(resumed["summary"]["remaining"], 0);
    assert_eq!(
        remote.lock().expect("remote lock").accepted_writes,
        writes_after_failure
    );
    mocks.assert();
}

#[test]
fn each_accepted_label_write_is_checkpointed_before_the_next_write() {
    let state = TempDir::new().expect("temporary state directory");
    let remote = Arc::new(Mutex::new(RemoteRepository::new([(
        1,
        vec!["priority:p1"],
    )])));
    let mut github = Server::new();
    let mocks = mock_stateful_repository(&mut github, Arc::clone(&remote));
    assert_success(&run(
        &state,
        &github.url(),
        &["sync", "--repo", "acme/reconcile", "--json"],
    ));
    remote.lock().expect("remote lock").online = false;
    queue(&state, &github.url(), 1, "p0");
    {
        let mut remote = remote.lock().expect("remote lock");
        remote.online = true;
        remote.reject_removals = true;
    }

    let partial = run(
        &state,
        &github.url(),
        &["reconcile", "--repo", "acme/reconcile", "--json"],
    );
    assert_success(&partial);
    let partial: Value = serde_json::from_slice(&partial.stdout).expect("partial JSON");
    assert_eq!(partial["operations"][0]["outcome"], "failed");
    let outbox: Value =
        serde_json::from_slice(&fs::read(outbox_path(&state)).expect("checkpointed outbox"))
            .expect("outbox JSON");
    assert_eq!(outbox["operations"][0]["state"]["status"], "applying");
    assert_eq!(
        outbox["operations"][0]["state"]["expected_labels"],
        json!(["priority:p0", "priority:p1"])
    );
    assert_eq!(
        outbox["operations"][0]["state"]["remaining_writes"],
        json!([{"action": "remove", "label": "priority:p1"}])
    );
    assert_eq!(
        remote.lock().expect("remote lock").accepted_writes,
        vec!["add #1 priority:p0"]
    );

    remote.lock().expect("remote lock").reject_removals = false;
    let resumed = run(
        &state,
        &github.url(),
        &["reconcile", "--repo", "acme/reconcile", "--json"],
    );
    assert_success(&resumed);
    assert_eq!(
        remote.lock().expect("remote lock").accepted_writes,
        vec!["add #1 priority:p0", "remove #1 priority:p1"]
    );
    mocks.assert();
}

#[test]
fn an_uncertain_first_write_forces_a_complete_final_synchronization() {
    let state = TempDir::new().expect("temporary state directory");
    let remote = Arc::new(Mutex::new(RemoteRepository::new([(
        1,
        vec!["priority:p1"],
    )])));
    let mut github = Server::new();
    let mocks = mock_stateful_repository(&mut github, Arc::clone(&remote));
    assert_success(&run(
        &state,
        &github.url(),
        &["sync", "--repo", "acme/reconcile", "--json"],
    ));
    remote.lock().expect("remote lock").online = false;
    queue(&state, &github.url(), 1, "p0");
    {
        let mut remote = remote.lock().expect("remote lock");
        remote.online = true;
        remote.reject_additions = true;
        remote.apply_rejected_additions = true;
    }

    let reconciled = run(
        &state,
        &github.url(),
        &["reconcile", "--repo", "acme/reconcile", "--json"],
    );
    assert_success(&reconciled);
    let reconciled: Value = serde_json::from_slice(&reconciled.stdout).expect("reconcile JSON");
    assert_eq!(reconciled["operations"][0]["outcome"], "failed");
    let replica: Value = serde_json::from_slice(
        &fs::read(replica_path(&state)).expect("final synchronized replica"),
    )
    .expect("replica JSON");
    let labels: BTreeSet<_> = replica["issues"][0]["labels"]
        .as_array()
        .expect("Issue labels")
        .iter()
        .map(|label| label["name"].as_str().expect("label name"))
        .collect();
    assert!(labels.contains("priority:p0"));
    assert!(labels.contains("priority:p1"));
    assert_eq!(
        remote.lock().expect("remote lock").accepted_writes,
        vec!["add #1 priority:p0"]
    );
    mocks.assert();
}

struct ConflictFixture {
    state: TempDir,
    github: ServerGuard,
    remote: Arc<Mutex<RemoteRepository>>,
    mocks: StatefulMocks,
    operation_id: String,
}

fn conflict_fixture() -> ConflictFixture {
    let state = TempDir::new().expect("temporary state directory");
    let remote = Arc::new(Mutex::new(RemoteRepository::new([(
        1,
        vec!["priority:p1"],
    )])));
    let mut github = Server::new();
    let mocks = mock_stateful_repository(&mut github, Arc::clone(&remote));
    assert_success(&run(
        &state,
        &github.url(),
        &["sync", "--repo", "acme/reconcile", "--json"],
    ));
    remote.lock().expect("remote lock").online = false;
    let operation_id = operation_id(&queue(&state, &github.url(), 1, "p0"));
    {
        let mut remote = remote.lock().expect("remote lock");
        remote.online = true;
        remote.labels.insert(1, vec!["priority:p3".to_owned()]);
    }
    let conflict = run(
        &state,
        &github.url(),
        &["reconcile", "--repo", "acme/reconcile", "--json"],
    );
    assert_success(&conflict);
    let conflict: Value = serde_json::from_slice(&conflict.stdout).expect("conflict JSON");
    assert_eq!(conflict["operations"][0]["classification"], "conflicting");
    ConflictFixture {
        state,
        github,
        remote,
        mocks,
        operation_id,
    }
}

fn ranked_output(fixture: &ConflictFixture, command: &str) -> Value {
    let mut args = vec![command, "--repo", "acme/reconcile", "--json"];
    if command == "next" {
        args.push("--profile");
    }
    let output = run(&fixture.state, &fixture.github.url(), &args);
    assert_success(&output);
    serde_json::from_slice(&output.stdout).expect("ranking JSON")
}

fn queue(state: &TempDir, api_url: &str, number: u64, priority: &str) -> Value {
    let output = run(
        state,
        api_url,
        &[
            "update",
            &format!("acme/reconcile#{number}"),
            "--priority",
            priority,
            "--json",
        ],
    );
    assert_success(&output);
    serde_json::from_slice(&output.stdout).expect("queued update JSON")
}

fn operation_id(output: &Value) -> String {
    output["operation"]["id"]
        .as_str()
        .expect("operation ID")
        .to_owned()
}

fn find_operation<'a>(operations: &'a [Value], id: &str) -> &'a Value {
    operations
        .iter()
        .find(|operation| operation["id"] == id)
        .expect("operation result")
}

fn issue_priority(replica: &Value, number: u64) -> &str {
    replica["issues"]
        .as_array()
        .expect("replica issues")
        .iter()
        .find(|issue| issue["number"] == number)
        .expect("replica issue")["labels"][0]["name"]
        .as_str()
        .expect("priority label")
}

fn outbox_path(state: &TempDir) -> std::path::PathBuf {
    state.path().join("repositories/acme/reconcile/outbox.json")
}

fn replica_path(state: &TempDir) -> std::path::PathBuf {
    state
        .path()
        .join("repositories/acme/reconcile/replica.json")
}

fn run(state: &TempDir, api_url: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hyfa"))
        .args(args)
        .env("GH_TOKEN", "automation-token")
        .env("HYFA_GITHUB_API_URL", api_url)
        .env("HYFA_NO_KEYRING", "1")
        .env("HYFA_STATE_DIR", state.path())
        .env("PATH", "")
        .output()
        .expect("run hyfa")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[derive(Debug)]
struct RemoteRepository {
    online: bool,
    fail_refresh_after_write: bool,
    reject_removals: bool,
    reject_additions: bool,
    apply_rejected_additions: bool,
    labels: BTreeMap<u64, Vec<String>>,
    accepted_writes: Vec<String>,
}

impl RemoteRepository {
    fn new<'a>(issues: impl IntoIterator<Item = (u64, Vec<&'a str>)>) -> Self {
        Self {
            online: true,
            fail_refresh_after_write: false,
            reject_removals: false,
            reject_additions: false,
            apply_rejected_additions: false,
            labels: issues
                .into_iter()
                .map(|(number, labels)| (number, labels.into_iter().map(str::to_owned).collect()))
                .collect(),
            accepted_writes: Vec::new(),
        }
    }
}

struct StatefulMocks {
    mocks: Vec<Mock>,
}

impl StatefulMocks {
    fn assert(self) {
        for mock in self.mocks {
            mock.assert();
        }
    }
}

fn mock_stateful_repository(
    github: &mut Server,
    remote: Arc<Mutex<RemoteRepository>>,
) -> StatefulMocks {
    let mut mocks = Vec::new();
    mocks.push(
        github
            .mock("GET", "/repos/acme/reconcile/issues/events")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_status_code_from_request(status(Arc::clone(&remote)))
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect_at_least(1)
            .create(),
    );
    mocks.push(
        github
            .mock("GET", "/repos/acme/reconcile/labels")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_status_code_from_request(status(Arc::clone(&remote)))
            .with_header("content-type", "application/json")
            .with_body(canonical_labels().to_string())
            .expect_at_least(3)
            .create(),
    );
    let issue_inventory = Arc::clone(&remote);
    mocks.push(
        github
            .mock("GET", "/repos/acme/reconcile/issues")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("state".into(), "all".into()),
                Matcher::UrlEncoded("sort".into(), "created".into()),
                Matcher::UrlEncoded("direction".into(), "asc".into()),
                Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .with_status_code_from_request(status(Arc::clone(&remote)))
            .with_header("content-type", "application/json")
            .with_body_from_request(move |_| {
                let remote = issue_inventory.lock().expect("remote lock");
                Value::Array(
                    remote
                        .labels
                        .iter()
                        .map(|(number, labels)| issue(*number, labels))
                        .collect(),
                )
                .to_string()
                .into_bytes()
            })
            .expect_at_least(3)
            .create(),
    );
    mocks.push(
        github
            .mock("GET", "/repos/acme/reconcile/issues/comments")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_status_code_from_request(status(Arc::clone(&remote)))
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect_at_least(3)
            .create(),
    );
    mocks.push(
        github
            .mock(
                "GET",
                Matcher::Regex(
                    r"^/repos/acme/reconcile/issues/[0-9]+/dependencies/blocked_by$".into(),
                ),
            )
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_status_code_from_request(status(Arc::clone(&remote)))
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect_at_least(1)
            .create(),
    );
    let individual = Arc::clone(&remote);
    mocks.push(
        github
            .mock(
                "GET",
                Matcher::Regex(r"^/repos/acme/reconcile/issues/[0-9]+$".into()),
            )
            .with_status_code_from_request(status(Arc::clone(&remote)))
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let number = path_number(request);
                let remote = individual.lock().expect("remote lock");
                issue(number, &remote.labels[&number])
                    .to_string()
                    .into_bytes()
            })
            .expect_at_least(1)
            .create(),
    );
    let additions = Arc::clone(&remote);
    let addition_status = Arc::clone(&remote);
    mocks.push(
        github
            .mock(
                "POST",
                Matcher::Regex(r"^/repos/acme/reconcile/issues/[0-9]+/labels$".into()),
            )
            .with_status_code_from_request(move |_| {
                if addition_status
                    .lock()
                    .expect("remote lock")
                    .reject_additions
                {
                    500
                } else {
                    200
                }
            })
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                let number = path_number(request);
                let body: Value = serde_json::from_slice(request.body().expect("request body"))
                    .expect("label request JSON");
                let label = body["labels"][0].as_str().expect("requested label");
                let mut remote = additions.lock().expect("remote lock");
                if remote.reject_additions && !remote.apply_rejected_additions {
                    return b"[]".to_vec();
                }
                let labels = remote.labels.get_mut(&number).expect("remote issue");
                if !labels.iter().any(|candidate| candidate == label) {
                    labels.push(label.to_owned());
                }
                remote
                    .accepted_writes
                    .push(format!("add #{number} {label}"));
                b"[]".to_vec()
            })
            .expect_at_most(8)
            .create(),
    );
    let removals = Arc::clone(&remote);
    let removal_status = Arc::clone(&remote);
    mocks.push(
        github
            .mock(
                "DELETE",
                Matcher::Regex(
                    r"^/repos/acme/reconcile/issues/[0-9]+/labels/priority:[pP][0-4]$".into(),
                ),
            )
            .with_status_code_from_request(move |_| {
                if removal_status.lock().expect("remote lock").reject_removals {
                    500
                } else {
                    200
                }
            })
            .with_body_from_request(move |request| {
                let number = path_number(request);
                let label = request.path().rsplit('/').next().expect("label path");
                let mut remote = removals.lock().expect("remote lock");
                if remote.reject_removals {
                    return Vec::new();
                }
                remote
                    .labels
                    .get_mut(&number)
                    .expect("remote issue")
                    .retain(|candidate| candidate != label);
                remote
                    .accepted_writes
                    .push(format!("remove #{number} {label}"));
                Vec::new()
            })
            .expect_at_most(8)
            .create(),
    );
    StatefulMocks { mocks }
}

fn status(
    remote: Arc<Mutex<RemoteRepository>>,
) -> impl Fn(&Request) -> usize + Send + Sync + 'static {
    move |request| {
        let remote = remote.lock().expect("remote lock");
        if remote.online
            && !(remote.fail_refresh_after_write
                && !remote.accepted_writes.is_empty()
                && request.method() == "GET")
        {
            200
        } else {
            503
        }
    }
}

fn path_number(request: &Request) -> u64 {
    request
        .path()
        .split('/')
        .nth(5)
        .expect("Issue number path segment")
        .parse()
        .expect("Issue number")
}

fn issue(number: u64, priority_labels: &[String]) -> Value {
    json!({
        "id": number * 100,
        "node_id": format!("I_{number}"),
        "number": number,
        "title": format!("Issue {number}"),
        "body": "",
        "state": "open",
        "state_reason": null,
        "html_url": format!("https://github.com/acme/reconcile/issues/{number}"),
        "user": null,
        "assignees": [],
        "labels": priority_labels
            .iter()
            .enumerate()
            .map(|(index, name)| label(number * 10 + index as u64, name))
            .collect::<Vec<_>>(),
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": "2026-08-01T00:00:00Z",
        "closed_at": null
    })
}

fn canonical_labels() -> Value {
    json!([
        label(1, "priority:p0"),
        label(2, "priority:p1"),
        label(3, "priority:p2"),
        label(4, "priority:p3"),
        label(5, "priority:p4")
    ])
}

fn label(id: u64, name: &str) -> Value {
    json!({
        "id": id,
        "node_id": format!("L_{id}"),
        "name": name,
        "color": "123456",
        "description": null
    })
}
