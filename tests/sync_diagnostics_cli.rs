use std::{fs, process::Command};

use mockito::{Matcher, Server};
use serde_json::{Value, json};
use tempfile::TempDir;

mod support;

fn status(state: &TempDir) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_hyfa"))
        .args(["status", "--repo", "acme/widgets", "--json"])
        .env_remove("GH_TOKEN")
        .env("HYFA_NO_KEYRING", "1")
        .env("HYFA_STATE_DIR", state.path())
        .env("HYFA_GITHUB_API_URL", "invalid-url")
        .output()
        .unwrap();
    support::assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn status_is_offline_and_distinguishes_missing_state_from_empty_successful_sync() {
    let state = TempDir::new().unwrap();
    let empty = status(&state);
    assert_eq!(empty["schema_version"], "hyfa.status/v1");
    assert!(empty["snapshot"].is_null());
    assert!(empty["last_attempt"].is_null());
    assert_eq!(empty["pending_operation_count"], 0);
    let mut github = Server::new();
    let mocks = support::mock_repository(&mut github, "acme/widgets", Vec::new(), Vec::new());
    support::assert_success(
        &support::sync_command(&state, &github.url(), "acme/widgets")
            .output()
            .unwrap(),
    );
    mocks.assert();
    let success = status(&state);
    assert_eq!(success["last_attempt"]["state"], "succeeded");
    assert_eq!(success["last_attempt"]["stage"], "publishing");
    assert_eq!(
        success["last_attempt"]["candidate_counts"]["issue_count"],
        0
    );
    assert_eq!(
        success["last_attempt"]["published_synced_at"],
        success["snapshot"]["synced_at"]
    );
    assert!(success["last_attempt"]["pages_received"].as_u64().unwrap() >= 4);
    assert!(success["last_attempt"]["finished_at"].is_string());
    assert!(success["last_attempt"]["failure"].is_null());
}

#[test]
fn rate_limited_attempt_records_observed_progress_and_preserves_the_replica() {
    let state = TempDir::new().unwrap();
    let mut github = Server::new();
    let mocks = support::mock_repository(
        &mut github,
        "acme/widgets",
        vec![support::issue(1, "open", &[], &[])],
        vec![(1, vec![])],
    );
    support::assert_success(
        &support::sync_command(&state, &github.url(), "acme/widgets")
            .output()
            .unwrap(),
    );
    mocks.assert();
    let replica = fs::read(support::replica_path(&state, "acme/widgets")).unwrap();
    let mut failure = Server::new();
    let labels = failure
        .mock("GET", "/repos/acme/widgets/labels")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body("[]")
        .create();
    let limit = failure
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(Matcher::Any)
        .with_status(403)
        .with_header("x-ratelimit-remaining", "0")
        .with_header("x-ratelimit-reset", "9999999999")
        .with_body(json!({"message":"private diagnostic automation-token"}).to_string())
        .create();
    let output = support::sync_command(&state, &failure.url(), "acme/widgets")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let diagnostic = status(&state);
    assert_eq!(diagnostic["last_attempt"]["state"], "failed");
    assert_eq!(diagnostic["last_attempt"]["stage"], "refreshing");
    assert_eq!(diagnostic["last_attempt"]["pages_received"], 1);
    assert_eq!(
        diagnostic["last_attempt"]["failure"]["code"],
        "rate_limited"
    );
    assert!(diagnostic["last_attempt"]["published_synced_at"].is_null());
    assert_eq!(
        fs::read(support::replica_path(&state, "acme/widgets")).unwrap(),
        replica
    );
    let journal = fs::read(
        state
            .path()
            .join("repositories/acme/widgets/sync-status.json"),
    )
    .unwrap();
    let text = String::from_utf8(journal).unwrap();
    assert!(
        !text.contains("automation-token")
            && !text.contains("private diagnostic")
            && !text.contains(&failure.url())
    );
    labels.assert();
    limit.assert();
}

#[test]
fn authentication_failure_is_recorded_and_explains_analysis_fallback() {
    let state = TempDir::new().unwrap();
    let mut github = Server::new();
    let mocks = support::mock_repository(
        &mut github,
        "acme/widgets",
        vec![support::issue(1, "open", &[], &[])],
        vec![(1, vec![])],
    );
    support::assert_success(
        &support::sync_command(&state, &github.url(), "acme/widgets")
            .output()
            .unwrap(),
    );
    mocks.assert();
    let output = Command::new(env!("CARGO_BIN_EXE_hyfa"))
        .args(["ready", "--repo", "acme/widgets", "--json"])
        .env_remove("GH_TOKEN")
        .env("HYFA_NO_KEYRING", "1")
        .env("HYFA_STATE_DIR", state.path())
        .env("HYFA_GITHUB_API_URL", github.url())
        .output()
        .unwrap();
    support::assert_success(&output);
    let ready: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(ready["source"], "local_fallback");
    assert!(ready["warnings"].as_array().unwrap().iter().any(|warning| {
        warning["code"] == "offline_fallback"
            && warning["message"]
                .as_str()
                .unwrap()
                .contains("authentication")
    }));
    let diagnostic = status(&state);
    assert_eq!(diagnostic["last_attempt"]["stage"], "connecting");
    assert_eq!(
        diagnostic["last_attempt"]["failure"]["code"],
        "authentication"
    );
}

#[test]
fn status_can_diagnose_an_invalid_replica_without_treating_it_as_a_valid_snapshot() {
    let state = TempDir::new().unwrap();
    let path = state.path().join("repositories/acme/widgets/replica.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "invalid JSON").unwrap();
    let diagnostic = status(&state);
    assert_eq!(diagnostic["snapshot_state"], "invalid");
    assert!(diagnostic["snapshot"].is_null());
}
