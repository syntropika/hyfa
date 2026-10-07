mod support;

use std::{fs, path::Path, process::Command};

use mockito::{Matcher, Mock, Server};
use serde_json::{Value, json};
use tempfile::TempDir;

#[test]
fn update_repairs_a_conflict_and_preserves_concurrently_added_non_priority_labels() {
    let mut github = Server::new();
    let state = TempDir::new().expect("temporary state directory");
    let before = issue(
        7,
        vec![
            label(10, "area:core"),
            label(20, "Priority:P0"),
            label(24, "priority:p4"),
            label(11, "bug"),
        ],
    );
    let after = issue(
        7,
        vec![
            label(10, "area:core"),
            label(11, "bug"),
            label(13, "help wanted"),
            label(21, "priority:p1"),
        ],
    );
    let current = mock_current_issue(&mut github, before);
    let addition = mock_priority_addition(&mut github, "priority:p1", 200);
    let remove_p0 = mock_priority_removal(&mut github, "priority:p0", 200);
    let remove_p4 = mock_priority_removal(&mut github, "priority:p4", 200);
    let synchronization = mock_synchronization(&mut github, after);

    let output = update_command(&state, &github.url(), "p1", true)
        .output()
        .expect("priority update");
    assert_success(&output);
    let output: Value = serde_json::from_slice(&output.stdout).expect("update JSON");
    assert_eq!(output["schema_version"], "hyfa.priority-update/v1");
    assert_eq!(output["command"], "update");
    assert_eq!(output["repository"], "acme/widgets");
    assert_eq!(output["issue"]["key"], "acme/widgets#7");
    assert_eq!(
        output["previous_priority"],
        json!({
            "state": "conflict",
            "comparison": "neutral",
            "labels": ["priority:p0", "priority:p4"]
        })
    );
    assert_eq!(
        output["resulting_priority"],
        json!({"state": "declared", "comparison": "p1", "value": "p1"})
    );
    assert!(output["snapshot"]["synced_at"].as_str().is_some());
    assert!(output["snapshot"]["input_hash"].as_str().is_some());

    let replica: Value = serde_json::from_slice(
        &fs::read(state.path().join("repositories/acme/widgets/replica.json"))
            .expect("published replica"),
    )
    .expect("replica JSON");
    assert_eq!(
        replica["issues"][0]["labels"]
            .as_array()
            .expect("labels")
            .iter()
            .map(|label| label["name"].as_str().expect("label name"))
            .collect::<Vec<_>>(),
        vec!["area:core", "bug", "help wanted", "priority:p1"]
    );
    current.assert();
    addition.assert();
    remove_p0.assert();
    remove_p4.assert();
    synchronization.assert();
}

#[test]
fn update_none_removes_priority_and_reports_the_logical_transition_to_humans() {
    let mut github = Server::new();
    let state = TempDir::new().expect("temporary state directory");
    let before = issue(
        7,
        vec![label(12, "documentation"), label(22, "priority:p2")],
    );
    let after = issue(7, vec![label(12, "documentation")]);
    let current = mock_current_issue(&mut github, before);
    let removal = mock_priority_removal(&mut github, "priority:p2", 200);
    let synchronization = mock_synchronization(&mut github, after);

    let output = update_command(&state, &github.url(), "none", false)
        .output()
        .expect("Priority removal");
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).expect("human output"),
        "Updated acme/widgets#7 Priority from p2 to unspecified\n"
    );
    current.assert();
    removal.assert();
    synchronization.assert();
}

#[test]
fn update_requires_synchronized_readback_before_publishing_the_replica() {
    let mut github = Server::new();
    let state = TempDir::new().expect("temporary state directory");
    let before = issue(7, vec![label(10, "area:core")]);
    let stale = before.clone();
    let current = mock_current_issue(&mut github, before.clone());
    let addition = mock_priority_addition(&mut github, "priority:p3", 200);
    let synchronization = mock_synchronization(&mut github, stale);

    let output = update_command(&state, &github.url(), "p3", true)
        .output()
        .expect("Priority update with stale readback");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Priority readback did not match"));
    assert!(
        !state
            .path()
            .join("repositories/acme/widgets/replica.json")
            .exists()
    );
    current.assert();
    addition.assert();
    synchronization.assert();
}

#[test]
fn update_reports_when_github_changed_but_synchronization_failed() {
    let mut github = Server::new();
    let state = TempDir::new().expect("temporary state directory");
    let before = issue(7, vec![label(10, "area:core")]);
    let current = mock_current_issue(&mut github, before);
    let addition = mock_priority_addition(&mut github, "priority:p3", 200);
    let failed_sync = github
        .mock("GET", "/repos/acme/widgets/labels")
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(500)
        .create();

    let output = update_command(&state, &github.url(), "p3", true)
        .output()
        .expect("Priority update with failed synchronization");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("GitHub accepted the Priority update, but synchronized readback failed")
    );
    assert!(
        !state
            .path()
            .join("repositories/acme/widgets/replica.json")
            .exists()
    );
    current.assert();
    addition.assert();
    failed_sync.assert();
    let diagnostic = support::synchronization_status(&state, "acme/widgets");
    assert_eq!(diagnostic["last_attempt"]["state"], "failed");
    assert_eq!(diagnostic["last_attempt"]["stage"], "refreshing");
    assert_eq!(
        diagnostic["last_attempt"]["failure"]["code"],
        "github_status"
    );
    assert!(diagnostic["last_attempt"]["published_synced_at"].is_null());
}

#[test]
fn update_reports_when_github_changed_but_replica_publication_failed() {
    let mut github = Server::new();
    let state = TempDir::new().expect("temporary state directory");
    let before = issue(7, vec![label(10, "area:core")]);
    let after = issue(7, vec![label(10, "area:core"), label(23, "priority:p3")]);
    let current = mock_current_issue(&mut github, before);
    let addition = mock_priority_addition(&mut github, "priority:p3", 200);
    let synchronization =
        mock_synchronization_with_failure(&mut github, after, Some(state.path().to_path_buf()));

    let output = update_command(&state, &github.url(), "p3", true)
        .output()
        .expect("Priority update with failed publication");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "GitHub accepted the Priority update, but the Local replica could not be published"
    ));
    current.assert();
    addition.assert();
    synchronization.assert();
    let diagnostic = support::synchronization_status(&state, "acme/widgets");
    assert_eq!(diagnostic["last_attempt"]["state"], "failed");
    assert_eq!(diagnostic["last_attempt"]["stage"], "publishing");
    assert_eq!(diagnostic["last_attempt"]["failure"]["code"], "persistence");
    assert!(diagnostic["last_attempt"]["published_synced_at"].is_null());
}

#[test]
fn update_reports_a_partial_remote_change_when_a_later_label_write_fails() {
    let mut github = Server::new();
    let state = TempDir::new().expect("temporary state directory");
    let before = issue(7, vec![label(10, "area:core"), label(20, "priority:p0")]);
    let current = mock_current_issue(&mut github, before);
    let addition = mock_priority_addition(&mut github, "priority:p1", 200);
    let failed_removal = mock_priority_removal(&mut github, "priority:p0", 403);

    let output = update_command(&state, &github.url(), "p1", true)
        .output()
        .expect("partially applied Priority update");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("GitHub accepted part of the Priority update")
    );
    assert!(
        !state
            .path()
            .join("repositories/acme/widgets/replica.json")
            .exists()
    );
    current.assert();
    addition.assert();
    failed_removal.assert();
}

struct SynchronizationMocks {
    events: Mock,
    labels: Mock,
    issues: Mock,
    comments: Mock,
    dependencies: Mock,
}

impl SynchronizationMocks {
    fn assert(self) {
        self.events.assert();
        self.labels.assert();
        self.issues.assert();
        self.comments.assert();
        self.dependencies.assert();
    }
}

fn mock_current_issue(github: &mut Server, issue: Value) -> Mock {
    github
        .mock("GET", "/repos/acme/widgets/issues/7")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(issue.to_string())
        .create()
}

fn mock_priority_addition(github: &mut Server, label: &str, status: usize) -> Mock {
    github
        .mock("POST", "/repos/acme/widgets/issues/7/labels")
        .match_body(Matcher::Json(json!({"labels": [label]})))
        .with_status(status)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create()
}

fn mock_priority_removal(github: &mut Server, label: &str, status: usize) -> Mock {
    github
        .mock(
            "DELETE",
            format!("/repos/acme/widgets/issues/7/labels/{label}").as_str(),
        )
        .with_status(status)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create()
}

fn mock_synchronization(github: &mut Server, issue: Value) -> SynchronizationMocks {
    mock_synchronization_with_failure(github, issue, None)
}

fn mock_synchronization_with_failure(
    github: &mut Server,
    issue: Value,
    fail_publication: Option<std::path::PathBuf>,
) -> SynchronizationMocks {
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
        .with_body(
            json!([
                label(10, "area:core"),
                label(11, "bug"),
                label(12, "documentation"),
                label(13, "help wanted"),
                label(20, "priority:p0"),
                label(21, "priority:p1"),
                label(22, "priority:p2"),
                label(23, "priority:p3"),
                label(24, "priority:p4")
            ])
            .to_string(),
        )
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
        .with_body(json!([issue]).to_string())
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
            "/repos/acme/widgets/issues/7/dependencies/blocked_by",
        )
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |_| {
            if let Some(path) = &fail_publication {
                fs::create_dir_all(path.join("repositories/acme/widgets/replica.json"))
                    .expect("block replica publication while leaving diagnostics writable");
            }
            b"[]".to_vec()
        })
        .create();
    SynchronizationMocks {
        events,
        labels,
        issues,
        comments,
        dependencies,
    }
}

fn update_command(state: &TempDir, api_url: &str, priority: &str, json: bool) -> Command {
    update_command_at(state.path(), api_url, priority, json)
}

fn update_command_at(state: &Path, api_url: &str, priority: &str, json: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hyfa"));
    command.args(["update", "acme/widgets#7", "--priority", priority]);
    if json {
        command.arg("--json");
    }
    command
        .env("GH_TOKEN", "automation-token")
        .env("HYFA_GITHUB_API_URL", api_url)
        .env("HYFA_NO_KEYRING", "1")
        .env("HYFA_STATE_DIR", state)
        .env("PATH", "");
    command
}

fn issue(number: u64, labels: Vec<Value>) -> Value {
    json!({
        "id": number * 100,
        "node_id": format!("I_{number}"),
        "number": number,
        "title": format!("Issue {number}"),
        "body": "Keep this body",
        "state": "open",
        "state_reason": null,
        "html_url": format!("https://github.com/acme/widgets/issues/{number}"),
        "user": null,
        "assignees": [],
        "labels": labels,
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": "2026-08-01T00:00:00Z",
        "closed_at": null
    })
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

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
