mod support;

use std::{
    fs,
    process::Command,
    sync::{Arc, Mutex},
    thread,
};

use mockito::{Matcher, Server};
use serde_json::{Value, json};
use tempfile::TempDir;

#[test]
fn offline_text_edits_project_immediately_without_changing_the_replica() {
    let state = TempDir::new().expect("state directory");
    let mut github = Server::new();
    seed_replica(&mut github, &state);
    let replica_path = state.path().join("repositories/acme/widgets/replica.json");
    let synchronized = fs::read(&replica_path).expect("synchronized replica");
    let unavailable = Server::new();

    let title = update_offline(
        &state,
        &unavailable.url(),
        &["--title", "Locally edited title"],
    );
    assert_eq!(title["pending"], true);
    assert_eq!(title["field"], "title");
    assert_eq!(
        title["base"],
        json!({"type": "title", "value": "Original title"})
    );
    assert_eq!(
        title["local"],
        json!({"type": "title", "value": "Locally edited title"})
    );

    let ranked = hyfa(&state, &unavailable.url())
        .env_remove("GH_TOKEN")
        .args(["next", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("rank projected title");
    assert_success(&ranked);
    let ranked: Value = serde_json::from_slice(&ranked.stdout).expect("next JSON");
    assert_eq!(
        ranked["recommendation"]["first_issue"]["title"],
        "Locally edited title"
    );
    assert_eq!(ranked["recommendation"]["first_issue"]["pending"], true);

    let body = update_offline(&state, &unavailable.url(), &["--body", "Local Markdown"]);
    assert_eq!(body["field"], "body");
    assert_eq!(
        fs::read(&replica_path).expect("unchanged Local replica"),
        synchronized
    );
    let outbox: Value = serde_json::from_slice(
        &fs::read(state.path().join("repositories/acme/widgets/outbox.json")).expect("outbox"),
    )
    .expect("outbox JSON");
    assert_eq!(
        outbox["operations"].as_array().expect("operations").len(),
        2
    );
}

#[test]
fn offline_assignment_edit_changes_the_execution_scope_immediately() {
    let state = TempDir::new().expect("state directory");
    let mut github = Server::new();
    seed_replica(&mut github, &state);
    let unavailable = Server::new();

    let assignment = update_offline(&state, &unavailable.url(), &["--assignee", "alice"]);
    assert_eq!(assignment["field"], "assignees");
    let assigned = hyfa(&state, &unavailable.url())
        .env_remove("GH_TOKEN")
        .args([
            "ready",
            "--repo",
            "acme/widgets",
            "--assignee",
            "alice",
            "--json",
        ])
        .output()
        .expect("enumerate assigned projected Issue");
    assert_success(&assigned);
    let assigned: Value = serde_json::from_slice(&assigned.stdout).expect("ready JSON");
    assert_eq!(assigned["issues"][0]["assignees"], json!(["alice"]));
}

#[test]
fn offline_state_edit_changes_the_operational_scope_immediately() {
    let state = TempDir::new().expect("state directory");
    let mut github = Server::new();
    seed_replica(&mut github, &state);
    let unavailable = Server::new();

    let state_update = update_offline(&state, &unavailable.url(), &["--state", "closed"]);
    assert_eq!(state_update["field"], "state");
    let after_close = hyfa(&state, &unavailable.url())
        .env_remove("GH_TOKEN")
        .args(["ready", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("enumerate after projected close");
    assert_success(&after_close);
    let after_close: Value =
        serde_json::from_slice(&after_close.stdout).expect("ready JSON after close");
    assert_eq!(after_close["summary"]["executable_count"], 0);
}

#[test]
fn reconciliation_contains_text_conflicts_and_applies_independent_fields() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);
    let unavailable = Server::new();
    update_offline(&state, &unavailable.url(), &["--title", "Local title"]);
    update_offline(&state, &unavailable.url(), &["--body", "Local body"]);
    update_offline(&state, &unavailable.url(), &["--state", "closed"]);
    update_offline(&state, &unavailable.url(), &["--assignee", "alice"]);

    let mut remote_issue = issue();
    remote_issue["title"] = json!("Remote title");
    remote_issue["body"] = json!("Remote body");
    let remote = Arc::new(Mutex::new(RemoteRepository {
        issues: vec![remote_issue],
        ..RemoteRepository::default()
    }));
    let mut github = Server::new();
    let inventory = mock_inventory(&mut github, Arc::clone(&remote), 2, 2);
    let patches = mock_field_patches(&mut github, Arc::clone(&remote), 2);
    let reconciled = hyfa(&state, &github.url())
        .args(["reconcile", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("reconcile independent field mutations");
    assert_success(&reconciled);
    let reconciled: Value = serde_json::from_slice(&reconciled.stdout).expect("reconcile JSON");
    assert_eq!(reconciled["summary"]["conflicting"], 2, "{reconciled}");
    assert_eq!(reconciled["summary"]["applied"], 2);
    assert_eq!(reconciled["summary"]["remaining"], 2);
    assert_eq!(reconciled["operations"][0]["field"], "title");
    assert_eq!(
        reconciled["operations"][0]["base"],
        json!({"type": "title", "value": "Original title"})
    );
    assert_eq!(
        reconciled["operations"][0]["local"],
        json!({"type": "title", "value": "Local title"})
    );
    assert_eq!(
        reconciled["operations"][0]["remote"],
        json!({"type": "title", "value": "Remote title"})
    );
    assert_eq!(reconciled["operations"][1]["field"], "body");
    assert_eq!(reconciled["operations"][2]["outcome"], "applied");
    assert_eq!(reconciled["operations"][3]["outcome"], "applied");
    assert_eq!(
        remote.lock().expect("remote lock").patch_requests,
        vec![json!({"state": "closed"}), json!({"assignees": ["alice"]})]
    );
    inventory.assert();
    patches.assert();

    let replica: Value = serde_json::from_slice(
        &fs::read(state.path().join("repositories/acme/widgets/replica.json")).expect("replica"),
    )
    .expect("replica JSON");
    assert_eq!(replica["issues"][0]["title"], "Remote title");
    assert_eq!(replica["issues"][0]["body"], "Remote body");
    assert_eq!(replica["issues"][0]["state"], "closed");
    assert_eq!(replica["issues"][0]["assignees"][0]["login"], "alice");
}

#[test]
fn conflict_resolution_revalidates_the_remote_before_local_or_remote_choice() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);
    let unavailable = Server::new();
    let title = update_offline(&state, &unavailable.url(), &["--title", "Local title"]);
    let operation = title["operation"]["id"].as_str().expect("title operation");

    let mut remote_issue = issue();
    remote_issue["title"] = json!("Remote title");
    let remote = Arc::new(Mutex::new(RemoteRepository {
        issues: vec![remote_issue],
        ..RemoteRepository::default()
    }));
    let mut github = Server::new();
    let inventory = mock_inventory(&mut github, Arc::clone(&remote), 1, 1);
    let conflict = hyfa(&state, &github.url())
        .args(["reconcile", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("record initial conflict");
    assert_success(&conflict);
    inventory.assert();

    remote.lock().expect("remote lock").issues[0]["title"] = json!("Remote title v2");
    let inventory = mock_inventory(&mut github, Arc::clone(&remote), 1, 1);
    let retried = hyfa(&state, &github.url())
        .args([
            "resolve",
            operation,
            "--repo",
            "acme/widgets",
            "--local",
            "--json",
        ])
        .output()
        .expect("revalidate local resolution");
    assert_success(&retried);
    let retried: Value = serde_json::from_slice(&retried.stdout).expect("resolve JSON");
    assert_eq!(retried["summary"]["conflicting"], 1);
    assert_eq!(
        retried["operations"][0]["remote"],
        json!({"type": "title", "value": "Remote title v2"})
    );
    assert!(
        remote
            .lock()
            .expect("remote lock")
            .patch_requests
            .is_empty()
    );
    inventory.assert();

    let inventory = mock_inventory(&mut github, Arc::clone(&remote), 1, 1);
    let accepted = hyfa(&state, &github.url())
        .args([
            "resolve",
            operation,
            "--repo",
            "acme/widgets",
            "--remote",
            "--json",
        ])
        .output()
        .expect("accept current remote title");
    assert_success(&accepted);
    let accepted: Value = serde_json::from_slice(&accepted.stdout).expect("resolve JSON");
    assert_eq!(accepted["summary"]["remaining"], 0);
    inventory.assert();
}

#[test]
fn online_field_update_publishes_only_verified_readback() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);

    let remote = Arc::new(Mutex::new(RemoteRepository {
        issues: vec![issue()],
        ..RemoteRepository::default()
    }));
    let mut github = Server::new();
    let fetch = mock_fetch_issue(&mut github, Arc::clone(&remote), 1);
    let patches = mock_field_patches(&mut github, Arc::clone(&remote), 1);
    let inventory = mock_inventory(&mut github, Arc::clone(&remote), 1, 1);
    let updated = hyfa(&state, &github.url())
        .args([
            "update",
            "acme/widgets#1",
            "--title",
            "Online title",
            "--json",
        ])
        .output()
        .expect("update title online");
    assert_success(&updated);
    let updated: Value = serde_json::from_slice(&updated.stdout).expect("update JSON");
    assert_eq!(updated["status"], "synchronized");
    assert_eq!(updated["pending"], false);
    assert_eq!(
        updated["base"],
        json!({"type": "title", "value": "Original title"})
    );
    assert_eq!(
        updated["local"],
        json!({"type": "title", "value": "Online title"})
    );
    fetch.assert();
    patches.assert();
    inventory.assert();

    let replica: Value = serde_json::from_slice(
        &fs::read(state.path().join("repositories/acme/widgets/replica.json")).expect("replica"),
    )
    .expect("replica JSON");
    assert_eq!(replica["issues"][0]["title"], "Online title");
    let diagnostic = support::synchronization_status(&state, "acme/widgets");
    assert_eq!(diagnostic["last_attempt"]["state"], "succeeded");
    assert_eq!(
        diagnostic["last_attempt"]["published_synced_at"],
        replica["synced_at"]
    );
}

#[test]
fn native_online_update_does_not_depend_on_the_draft_identity_store() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);
    fs::write(
        state
            .path()
            .join("repositories/acme/widgets/draft-identities.json"),
        b"not valid JSON",
    )
    .expect("corrupt unrelated Draft identities");

    let remote = Arc::new(Mutex::new(RemoteRepository {
        issues: vec![issue()],
        ..RemoteRepository::default()
    }));
    let mut github = Server::new();
    let fetch = mock_fetch_issue(&mut github, Arc::clone(&remote), 1);
    let patches = mock_field_patches(&mut github, Arc::clone(&remote), 1);
    let inventory = mock_inventory(&mut github, remote, 1, 1);
    let updated = hyfa(&state, &github.url())
        .args([
            "update",
            "acme/widgets#1",
            "--title",
            "Independent native edit",
            "--json",
        ])
        .output()
        .expect("update native Issue");
    assert_success(&updated);
    let updated: Value = serde_json::from_slice(&updated.stdout).expect("update JSON");
    assert_eq!(updated["status"], "synchronized");
    fetch.assert();
    patches.assert();
    inventory.assert();
}

#[test]
fn online_field_update_preserves_the_replica_when_readback_does_not_verify_the_write() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);
    let replica_path = state.path().join("repositories/acme/widgets/replica.json");
    let synchronized = fs::read(&replica_path).expect("synchronized replica");

    let remote = Arc::new(Mutex::new(RemoteRepository {
        issues: vec![issue()],
        ..RemoteRepository::default()
    }));
    let mut github = Server::new();
    let fetch = mock_fetch_issue(&mut github, Arc::clone(&remote), 1);
    let patch = github
        .mock("PATCH", "/repos/acme/widgets/issues/1")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(issue().to_string())
        .expect(1)
        .create();
    let inventory = mock_inventory(&mut github, remote, 1, 1);
    let updated = hyfa(&state, &github.url())
        .args([
            "update",
            "acme/widgets#1",
            "--title",
            "Unverified title",
            "--json",
        ])
        .output()
        .expect("reject unverified online update");
    assert!(!updated.status.success());
    assert!(
        String::from_utf8_lossy(&updated.stderr).contains("did not match"),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    assert_eq!(
        fs::read(replica_path).expect("unchanged replica"),
        synchronized
    );
    fetch.assert();
    patch.assert();
    inventory.assert();
    let diagnostic = support::synchronization_status(&state, "acme/widgets");
    assert_eq!(diagnostic["last_attempt"]["state"], "failed");
    assert_eq!(diagnostic["last_attempt"]["stage"], "validating");
    assert_eq!(
        diagnostic["last_attempt"]["failure"]["code"],
        "not_published"
    );
    assert!(diagnostic["last_attempt"]["published_synced_at"].is_null());
}

#[test]
fn draft_field_edit_waits_for_creation_then_rewrites_its_identity() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_empty_replica(&mut seed, &state);
    let draft = hyfa(&state, &seed.url())
        .args([
            "create",
            "--repo",
            "acme/widgets",
            "--title",
            "Provisional title",
            "--body",
            "Draft body",
            "--json",
        ])
        .output()
        .expect("queue Draft Issue");
    assert_success(&draft);
    let draft: Value = serde_json::from_slice(&draft.stdout).expect("Draft JSON");
    let draft_key = draft["draft"]["key"].as_str().expect("Draft key");
    let temporary_id = draft["draft"]["temporary_id"].clone();
    let edited = hyfa(&state, &seed.url())
        .args(["update", draft_key, "--title", "Final title", "--json"])
        .output()
        .expect("queue Draft title edit");
    assert_success(&edited);
    let edited: Value = serde_json::from_slice(&edited.stdout).expect("edit JSON");
    assert_eq!(edited["pending"], true);
    assert_eq!(edited["issue"]["temporary_id"], temporary_id);
    assert_eq!(
        edited["operation"]["depends_on"].as_array().unwrap().len(),
        1
    );

    let remote = Arc::new(Mutex::new(RemoteRepository::default()));
    let mut github = Server::new();
    let inventory = mock_inventory(&mut github, Arc::clone(&remote), 2, 1);
    let create = mock_issue_create(&mut github, Arc::clone(&remote), 1);
    let patches = mock_field_patches(&mut github, Arc::clone(&remote), 1);
    let reconciled = hyfa(&state, &github.url())
        .args(["reconcile", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("reconcile Draft and title");
    assert_success(&reconciled);
    let reconciled: Value = serde_json::from_slice(&reconciled.stdout).expect("reconcile JSON");
    assert_eq!(reconciled["summary"]["applied"], 2, "{reconciled}");
    assert_eq!(reconciled["summary"]["remaining"], 0);
    assert_eq!(reconciled["operations"][1]["issue_number"], 1);
    assert_eq!(reconciled["operations"][1]["temporary_id"], temporary_id);
    assert_eq!(
        remote.lock().expect("remote lock").issues[0]["title"],
        "Final title"
    );
    inventory.assert();
    create.assert();
    patches.assert();
}

#[test]
fn reconciliation_treats_the_desired_remote_value_as_already_satisfied() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);
    let unavailable = Server::new();
    update_offline(&state, &unavailable.url(), &["--title", "Converged title"]);

    let mut desired = issue();
    desired["title"] = json!("Converged title");
    let remote = Arc::new(Mutex::new(RemoteRepository {
        issues: vec![desired],
        ..RemoteRepository::default()
    }));
    let mut github = Server::new();
    let inventory = mock_inventory(&mut github, Arc::clone(&remote), 1, 1);
    let reconciled = hyfa(&state, &github.url())
        .args(["reconcile", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("reconcile already-satisfied title");
    assert_success(&reconciled);
    let reconciled: Value = serde_json::from_slice(&reconciled.stdout).expect("reconcile JSON");
    assert_eq!(reconciled["summary"]["already_satisfied"], 1);
    assert_eq!(reconciled["summary"]["remaining"], 0);
    assert!(
        remote
            .lock()
            .expect("remote lock")
            .patch_requests
            .is_empty()
    );
    inventory.assert();
}

#[test]
fn mapped_temporary_id_uses_the_online_update_path_and_keeps_its_alias() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_empty_replica(&mut seed, &state);
    let draft = hyfa(&state, &seed.url())
        .args([
            "create",
            "--repo",
            "acme/widgets",
            "--title",
            "Mapped title",
            "--json",
        ])
        .output()
        .expect("queue Draft Issue");
    assert_success(&draft);
    let draft: Value = serde_json::from_slice(&draft.stdout).expect("Draft JSON");
    let draft_key = draft["draft"]["key"].as_str().expect("Draft key");
    let temporary_id = draft["draft"]["temporary_id"].clone();

    let remote = Arc::new(Mutex::new(RemoteRepository::default()));
    let mut github = Server::new();
    let inventory = mock_inventory(&mut github, Arc::clone(&remote), 2, 1);
    let create = mock_issue_create(&mut github, Arc::clone(&remote), 1);
    let reconciled = hyfa(&state, &github.url())
        .args(["reconcile", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("map Draft Issue");
    assert_success(&reconciled);
    inventory.assert();
    create.assert();

    let fetch = mock_fetch_issue(&mut github, Arc::clone(&remote), 1);
    let patches = mock_field_patches(&mut github, Arc::clone(&remote), 1);
    let inventory = mock_inventory(&mut github, Arc::clone(&remote), 1, 1);
    let updated = hyfa(&state, &github.url())
        .args([
            "update",
            draft_key,
            "--title",
            "Online alias edit",
            "--json",
        ])
        .output()
        .expect("update mapped alias online");
    assert_success(&updated);
    let updated: Value = serde_json::from_slice(&updated.stdout).expect("update JSON");
    assert_eq!(updated["status"], "synchronized");
    assert_eq!(updated["issue"]["key"], draft_key);
    assert_eq!(updated["issue"]["temporary_id"], temporary_id);
    assert_eq!(updated["issue"]["number"], 1);
    fetch.assert();
    patches.assert();
    inventory.assert();
}

#[test]
fn uncertain_online_patch_queues_against_the_observed_remote_base() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);

    let mut fresh = issue();
    fresh["title"] = json!("Fresh remote title");
    let remote = Arc::new(Mutex::new(RemoteRepository {
        issues: vec![fresh],
        ..RemoteRepository::default()
    }));
    let mut github = Server::new();
    let fetch = mock_fetch_issue(&mut github, remote, 1);
    let patch = github
        .mock("PATCH", "/repos/acme/widgets/issues/1")
        .with_status(500)
        .with_body("server failed after receiving the write")
        .expect(1)
        .create();
    let updated = hyfa(&state, &github.url())
        .args([
            "update",
            "acme/widgets#1",
            "--title",
            "Desired title",
            "--json",
        ])
        .output()
        .expect("queue ambiguous field write");
    assert_success(&updated);
    let updated: Value = serde_json::from_slice(&updated.stdout).expect("update JSON");
    assert_eq!(updated["pending"], true);
    assert_eq!(
        updated["base"],
        json!({"type": "title", "value": "Fresh remote title"})
    );
    fetch.assert();
    patch.assert();
}

#[test]
fn an_existing_same_field_intent_serializes_a_later_online_request_without_bypassing_it() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);
    let unavailable = Server::new();
    let first = update_offline(&state, &unavailable.url(), &["--title", "Pending A"]);

    let second = hyfa(&state, &unavailable.url())
        .args(["update", "acme/widgets#1", "--title", "Pending B", "--json"])
        .output()
        .expect("serialize later field request");
    assert_success(&second);
    let second: Value = serde_json::from_slice(&second.stdout).expect("second update JSON");
    assert_eq!(second["pending"], true);
    assert_eq!(second["base"], first["local"]);
    assert_eq!(
        second["operation"]["depends_on"],
        json!([first["operation"]["id"].clone()])
    );
}

#[test]
fn invalid_noncanonical_persisted_field_values_fail_closed() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);
    let unavailable = Server::new();
    update_offline(&state, &unavailable.url(), &["--assignee", "alice"]);

    let outbox_path = state.path().join("repositories/acme/widgets/outbox.json");
    let mut outbox: Value =
        serde_json::from_slice(&fs::read(&outbox_path).expect("outbox")).expect("outbox JSON");
    outbox["operations"][0]["desired"]["logins"] = json!(["Alice", "alice"]);
    fs::write(
        &outbox_path,
        serde_json::to_vec_pretty(&outbox).expect("encode corrupt outbox"),
    )
    .expect("write corrupt outbox");

    let output = hyfa(&state, &unavailable.url())
        .args(["ready", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("reject invalid persisted scalar");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid Issue-field update"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn concurrent_offline_field_updates_all_survive_restart() {
    let state = TempDir::new().expect("state directory");
    let mut seed = Server::new();
    seed_replica(&mut seed, &state);
    let unavailable = Server::new();
    let state_path = state.path().to_path_buf();
    let api_url = unavailable.url();

    let workers: Vec<_> = (0..8)
        .map(|index| {
            let state_path = state_path.clone();
            let api_url = api_url.clone();
            thread::spawn(move || {
                Command::new(env!("CARGO_BIN_EXE_hyfa"))
                    .env("HYFA_NO_KEYRING", "1")
                    .env("HYFA_STATE_DIR", state_path)
                    .env("HYFA_GITHUB_API_URL", api_url)
                    .env_remove("GH_TOKEN")
                    .args([
                        "update",
                        "acme/widgets#1",
                        "--title",
                        &format!("Concurrent title {index}"),
                        "--json",
                    ])
                    .output()
                    .expect("queue concurrent field update")
            })
        })
        .collect();
    for worker in workers {
        assert_success(&worker.join().expect("join concurrent update"));
    }

    let outbox: Value = serde_json::from_slice(
        &fs::read(state.path().join("repositories/acme/widgets/outbox.json")).expect("outbox"),
    )
    .expect("outbox JSON");
    let operations = outbox["operations"].as_array().expect("operations");
    assert_eq!(operations.len(), 8);
    for window in operations.windows(2) {
        assert_eq!(window[1]["depends_on"], json!([window[0]["id"].clone()]));
        assert_eq!(window[1]["base"], window[0]["desired"]);
    }
}

fn update_offline(state: &TempDir, api_url: &str, field_args: &[&str]) -> Value {
    let mut command = hyfa(state, api_url);
    command
        .env_remove("GH_TOKEN")
        .args(["update", "acme/widgets#1"])
        .args(field_args)
        .arg("--json");
    let output = command.output().expect("queue field update");
    assert_success(&output);
    serde_json::from_slice(&output.stdout).expect("field update JSON")
}

#[derive(Default)]
struct RemoteRepository {
    issues: Vec<Value>,
    patch_requests: Vec<Value>,
}

struct RepositoryMocks {
    mocks: Vec<mockito::Mock>,
}

impl RepositoryMocks {
    fn assert(self) {
        for mock in self.mocks {
            mock.assert();
        }
    }
}

fn mock_inventory(
    github: &mut Server,
    remote: Arc<Mutex<RemoteRepository>>,
    inventories: usize,
    dependency_inventories: usize,
) -> RepositoryMocks {
    let mut mocks = Vec::new();
    mocks.push(
        github
            .mock("GET", "/repos/acme/widgets/labels")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect(inventories)
            .create(),
    );
    mocks.push(
        github
            .mock("GET", "/repos/acme/widgets/issues/events")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect(inventories)
            .create(),
    );
    let issue_inventory = Arc::clone(&remote);
    mocks.push(
        github
            .mock("GET", "/repos/acme/widgets/issues")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("state".into(), "all".into()),
                Matcher::UrlEncoded("sort".into(), "created".into()),
                Matcher::UrlEncoded("direction".into(), "asc".into()),
                Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body_from_request(move |_| {
                json!(issue_inventory.lock().expect("remote lock").issues)
                    .to_string()
                    .into_bytes()
            })
            .expect(inventories)
            .create(),
    );
    mocks.push(
        github
            .mock("GET", "/repos/acme/widgets/issues/comments")
            .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect(inventories)
            .create(),
    );
    if dependency_inventories > 0 {
        mocks.push(
            github
                .mock(
                    "GET",
                    Matcher::Regex(
                        r"^/repos/acme/widgets/issues/[0-9]+/dependencies/blocked_by$".into(),
                    ),
                )
                .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body("[]")
                .expect(dependency_inventories)
                .create(),
        );
    }
    RepositoryMocks { mocks }
}

fn mock_field_patches(
    github: &mut Server,
    remote: Arc<Mutex<RemoteRepository>>,
    expected: usize,
) -> RepositoryMocks {
    let patch_remote = Arc::clone(&remote);
    let patch = github
        .mock("PATCH", "/repos/acme/widgets/issues/1")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |request| {
            let patch: Value =
                serde_json::from_slice(request.body().expect("PATCH body")).expect("PATCH JSON");
            let mut remote = patch_remote.lock().expect("remote lock");
            remote.patch_requests.push(patch.clone());
            apply_patch(&mut remote.issues[0], &patch);
            remote.issues[0].to_string().into_bytes()
        })
        .expect(expected)
        .create();
    RepositoryMocks { mocks: vec![patch] }
}

fn mock_fetch_issue(
    github: &mut Server,
    remote: Arc<Mutex<RemoteRepository>>,
    expected: usize,
) -> RepositoryMocks {
    let fetch_remote = Arc::clone(&remote);
    let fetch = github
        .mock("GET", "/repos/acme/widgets/issues/1")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |_| {
            fetch_remote.lock().expect("remote lock").issues[0]
                .to_string()
                .into_bytes()
        })
        .expect(expected)
        .create();
    RepositoryMocks { mocks: vec![fetch] }
}

fn mock_issue_create(
    github: &mut Server,
    remote: Arc<Mutex<RemoteRepository>>,
    expected: usize,
) -> RepositoryMocks {
    let create_remote = Arc::clone(&remote);
    let create = github
        .mock("POST", "/repos/acme/widgets/issues")
        .with_status(201)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |request| {
            let request: Value =
                serde_json::from_slice(request.body().expect("create body")).expect("create JSON");
            let mut remote = create_remote.lock().expect("remote lock");
            let number = remote.issues.len() as u64 + 1;
            let issue = issue_with(number, &request["title"], &request["body"]);
            remote.issues.push(issue.clone());
            issue.to_string().into_bytes()
        })
        .expect(expected)
        .create();
    RepositoryMocks {
        mocks: vec![create],
    }
}

fn apply_patch(issue: &mut Value, patch: &Value) {
    if let Some(title) = patch.get("title") {
        issue["title"] = title.clone();
    }
    if let Some(body) = patch.get("body") {
        issue["body"] = body.clone();
    }
    if let Some(state) = patch.get("state") {
        issue["state"] = state.clone();
    }
    if let Some(assignees) = patch.get("assignees").and_then(Value::as_array) {
        issue["assignees"] = Value::Array(
            assignees
                .iter()
                .enumerate()
                .map(|(index, login)| {
                    json!({
                        "id": index as u64 + 1,
                        "node_id": format!("U_{}", index + 1),
                        "login": login
                    })
                })
                .collect(),
        );
    }
}

fn seed_replica(github: &mut Server, state: &TempDir) {
    let events = github
        .mock("GET", "/repos/acme/widgets/issues/events")
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let labels = github
        .mock("GET", "/repos/acme/widgets/labels")
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let issues = github
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("state".into(), "all".into()),
            Matcher::UrlEncoded("sort".into(), "created".into()),
            Matcher::UrlEncoded("direction".into(), "asc".into()),
            Matcher::UrlEncoded("per_page".into(), "100".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(json!([issue()]).to_string())
        .create();
    let comments = github
        .mock("GET", "/repos/acme/widgets/issues/comments")
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let dependencies = github
        .mock(
            "GET",
            "/repos/acme/widgets/issues/1/dependencies/blocked_by",
        )
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let output = hyfa(state, &github.url())
        .args(["sync", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("seed Local replica");
    assert_success(&output);
    labels.assert();
    events.assert();
    issues.assert();
    comments.assert();
    dependencies.assert();
}

fn seed_empty_replica(github: &mut Server, state: &TempDir) {
    let remote = Arc::new(Mutex::new(RemoteRepository::default()));
    let mocks = mock_inventory(github, remote, 1, 0);
    let output = hyfa(state, &github.url())
        .args(["sync", "--repo", "acme/widgets", "--json"])
        .output()
        .expect("seed empty Local replica");
    assert_success(&output);
    mocks.assert();
}

fn issue() -> Value {
    issue_with(1, &json!("Original title"), &json!("Original body"))
}

fn issue_with(number: u64, title: &Value, body: &Value) -> Value {
    json!({
        "id": number * 100,
        "node_id": format!("I_{number}"),
        "number": number,
        "title": title,
        "body": body,
        "state": "open",
        "state_reason": null,
        "html_url": format!("https://github.com/acme/widgets/issues/{number}"),
        "user": null,
        "assignees": [],
        "labels": [],
        "created_at": "2026-08-07T00:00:00Z",
        "updated_at": "2026-08-07T00:00:00Z",
        "closed_at": null,
        "pull_request": null
    })
}

fn hyfa(state: &TempDir, api_url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hyfa"));
    command
        .env("HYFA_NO_KEYRING", "1")
        .env("HYFA_STATE_DIR", state.path())
        .env("HYFA_GITHUB_API_URL", api_url)
        .env("GH_TOKEN", "test-token");
    command
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
