use std::{fs, process::Command};

use chrono::{DateTime, Duration, SecondsFormat};
use mockito::{Matcher, Mock, Server};
use serde_json::Value;
use tempfile::TempDir;

mod support;

#[test]
fn ready_separates_readiness_from_default_and_assignee_execution_scopes() {
    let mut github = Server::new();
    let dependencies = [1_u64, 2, 3, 4, 5]
        .into_iter()
        .map(|number| {
            let body = match number {
                3 => blocker(1, "open"),
                4 => blocker(5, "closed"),
                _ => "[]".to_owned(),
            };
            (number, body)
        })
        .collect();
    let mocks = mock_repository(
        &mut github,
        "acme/widgets",
        issue_inventory().to_owned(),
        dependencies,
        1,
    );
    let state = TempDir::new().expect("temporary state directory");
    let default = ready_command(&state, &github.url(), None)
        .output()
        .expect("run hyfa ready");
    assert!(
        default.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&default.stderr)
    );
    let default: Value = serde_json::from_slice(&default.stdout).expect("ready JSON");
    assert_eq!(default["schema_version"], "hyfa.ready/v1");
    assert_eq!(default["command"], "ready");
    assert_eq!(default["repository"], "acme/widgets");
    assert_eq!(default["source"], "live");
    assert_eq!(default["execution_scope"]["mode"], "available");
    assert_eq!(issue_numbers(&default), vec![1, 4]);
    assert_eq!(default["issues"][0]["ready"], true);
    assert_eq!(default["issues"][0]["available"], true);
    assert_eq!(default["summary"]["operational_issue_count"], 4);
    assert_eq!(default["summary"]["ready_count"], 3);
    assert_eq!(default["summary"]["executable_count"], 2);
    assert_eq!(default["summary"]["assigned_ready_count"], 1);
    assert_eq!(default["summary"]["blocked_count"], 1);
    assert_eq!(default["warnings"], serde_json::json!([]));
    mocks.assert();

    let delta_mocks = mock_unchanged_delta(
        &mut github,
        "acme/widgets",
        &replica_since(&state, "acme/widgets"),
        issue_inventory().to_owned(),
        5,
    );

    let assigned = ready_command(&state, &github.url(), Some("alice"))
        .output()
        .expect("run hyfa ready for assignee");
    assert!(
        assigned.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&assigned.stderr)
    );
    let assigned: Value = serde_json::from_slice(&assigned.stdout).expect("assigned ready JSON");
    assert_eq!(assigned["execution_scope"]["mode"], "assignee");
    assert_eq!(assigned["execution_scope"]["assignee"], "alice");
    assert_eq!(issue_numbers(&assigned), vec![2]);
    assert_eq!(assigned["issues"][0]["ready"], true);
    assert_eq!(assigned["issues"][0]["available"], false);
    assert_eq!(assigned["issues"][0]["assignees"][0], "alice");

    delta_mocks.assert();
}

#[test]
fn ready_falls_back_to_the_latest_valid_replica_without_advancing_synced_at() {
    let mut github = Server::new();
    let mocks = mock_repository(
        &mut github,
        "acme/widgets",
        single_issue_inventory().to_owned(),
        vec![(1, "[]".to_owned())],
        1,
    );
    let api_url = github.url();
    let state = TempDir::new().expect("temporary state directory");

    let online = ready_command(&state, &api_url, None)
        .output()
        .expect("online ready");
    assert!(online.status.success());
    let online: Value = serde_json::from_slice(&online.stdout).expect("online ready JSON");
    let synced_at = online["synced_at"].clone();
    let input_hash = online["input_hash"].clone();
    mocks.assert();
    drop(github);

    let offline = ready_command(&state, &api_url, None)
        .env_remove("GH_TOKEN")
        .output()
        .expect("offline ready");
    assert!(
        offline.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&offline.stderr)
    );
    let offline: Value = serde_json::from_slice(&offline.stdout).expect("offline ready JSON");
    assert_eq!(offline["source"], "local_fallback");
    assert_eq!(offline["synced_at"], synced_at);
    assert_eq!(offline["input_hash"], input_hash);
    assert_eq!(issue_numbers(&offline), vec![1]);
    assert_eq!(offline["warnings"][0]["code"], "offline_fallback");
    assert_eq!(
        offline["warnings"][0]["message"],
        "GitHub refresh failed; using the latest valid Local replica; GitHub credentials are unavailable (authentication)"
    );
}

#[test]
fn ready_respects_cycles_and_and_dependencies_and_every_external_state() {
    let mut github = Server::new();
    let dependencies = (1_u64..=9)
        .map(|number| (number, graph_dependencies(number)))
        .collect();
    let mocks = mock_repository(
        &mut github,
        "acme/graph",
        graph_issue_inventory(),
        dependencies,
        1,
    );
    let state = TempDir::new().expect("temporary state directory");

    let output = ready_command_for(&state, &github.url(), "acme/graph", None)
        .output()
        .expect("run hyfa ready");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output: Value = serde_json::from_slice(&output.stdout).expect("ready JSON");
    assert_eq!(issue_numbers(&output), vec![1, 5]);
    assert_eq!(output["summary"]["operational_issue_count"], 8);
    assert_eq!(output["summary"]["ready_count"], 2);
    assert_eq!(output["summary"]["blocked_count"], 6);

    mocks.assert();
}

#[test]
fn ready_fails_clearly_without_github_and_a_valid_replica() {
    let state = TempDir::new().expect("temporary state directory");
    let missing = ready_command(&state, "http://127.0.0.1:1", None)
        .output()
        .expect("ready without GitHub");
    assert!(!missing.status.success());
    let missing_error = String::from_utf8_lossy(&missing.stderr);
    assert!(missing_error.contains("GitHub refresh failed"));
    assert!(missing_error.contains("no valid Local replica"));
    assert!(missing_error.contains("no Local replica exists"));

    let replica_dir = state.path().join("repositories/acme/widgets");
    fs::create_dir_all(&replica_dir).expect("replica directory");
    fs::write(replica_dir.join("replica.json"), "{\"schema_version\":").expect("corrupt replica");
    let corrupt = ready_command(&state, "http://127.0.0.1:1", None)
        .output()
        .expect("ready with corrupt replica");
    assert!(!corrupt.status.success());
    let corrupt_error = String::from_utf8_lossy(&corrupt.stderr);
    assert!(corrupt_error.contains("GitHub refresh failed"));
    assert!(corrupt_error.contains("no valid Local replica"));
    assert!(corrupt_error.contains("could not decode the Local replica"));
}

fn ready_command(state: &TempDir, api_url: &str, assignee: Option<&str>) -> Command {
    ready_command_for(state, api_url, "acme/widgets", assignee)
}

struct RepositoryMocks {
    labels: Mock,
    issues: Mock,
    comments: Mock,
    dependencies: Vec<Mock>,
    events: Mock,
}

struct DeltaMocks {
    labels: Mock,
    issues: Mock,
    comments: Mock,
    events: Mock,
    count: Mock,
}

impl DeltaMocks {
    fn assert(self) {
        self.labels.assert();
        self.issues.assert();
        self.comments.assert();
        self.events.assert();
        self.count.assert();
    }
}

impl RepositoryMocks {
    fn assert(self) {
        self.labels.assert();
        self.issues.assert();
        self.comments.assert();
        for dependency in self.dependencies {
            dependency.assert();
        }
        self.events.assert();
    }
}

fn mock_repository(
    github: &mut Server,
    repository: &str,
    issue_inventory: String,
    dependencies: Vec<(u64, String)>,
    expected_calls: usize,
) -> RepositoryMocks {
    let labels_path = format!("/repos/{repository}/labels");
    let labels = github
        .mock("GET", labels_path.as_str())
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(canonical_labels())
        .expect(expected_calls)
        .create();
    let issues_path = format!("/repos/{repository}/issues");
    let issues = github
        .mock("GET", issues_path.as_str())
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("state".into(), "all".into()),
            Matcher::UrlEncoded("sort".into(), "created".into()),
            Matcher::UrlEncoded("direction".into(), "asc".into()),
            Matcher::UrlEncoded("per_page".into(), "100".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(issue_inventory)
        .expect(expected_calls)
        .create();

    let comments_path = format!("/repos/{repository}/issues/comments");
    let comments = github
        .mock("GET", comments_path.as_str())
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .expect(expected_calls)
        .create();

    let dependencies = dependencies
        .into_iter()
        .map(|(number, body)| {
            let path = format!("/repos/{repository}/issues/{number}/dependencies/blocked_by");
            github
                .mock("GET", path.as_str())
                .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(body)
                .expect(expected_calls)
                .create()
        })
        .collect();
    let events_path = format!("/repos/{repository}/issues/events");
    let events = github
        .mock("GET", events_path.as_str())
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(checkpoint_event())
        .expect(expected_calls)
        .create();

    RepositoryMocks {
        labels,
        issues,
        comments,
        dependencies,
        events,
    }
}

fn canonical_labels() -> String {
    serde_json::to_string(
        &(0_u64..=4)
            .map(|priority| {
                serde_json::json!({
                    "id": priority + 100,
                    "node_id": format!("L_{priority}"),
                    "name": format!("priority:p{priority}"),
                    "color": "123456",
                    "description": null
                })
            })
            .collect::<Vec<_>>(),
    )
    .expect("canonical labels")
}

fn mock_unchanged_delta(
    github: &mut Server,
    repository: &str,
    since: &str,
    issue_inventory: String,
    issue_count: u64,
) -> DeltaMocks {
    let labels_path = format!("/repos/{repository}/labels");
    let labels = github
        .mock("GET", labels_path.as_str())
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(canonical_labels())
        .create();
    let issues_path = format!("/repos/{repository}/issues");
    let issues = github
        .mock("GET", issues_path.as_str())
        .match_query(support::issue_delta_query(since, None))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(issue_inventory)
        .create();

    let comments_path = format!("/repos/{repository}/issues/comments");
    let comments = github
        .mock("GET", comments_path.as_str())
        .match_query(support::comment_delta_query(since, None))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();

    let events_path = format!("/repos/{repository}/issues/events");
    let events = github
        .mock("GET", events_path.as_str())
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(checkpoint_event())
        .create();
    let count = support::mock_issue_count(github, issue_count);

    DeltaMocks {
        labels,
        issues,
        comments,
        events,
        count,
    }
}

fn checkpoint_event() -> &'static str {
    r#"[{"id":100,"event":"labeled","created_at":"2026-08-01T00:00:00Z","issue":null}]"#
}

fn ready_command_for(
    state: &TempDir,
    api_url: &str,
    repository: &str,
    assignee: Option<&str>,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hyfa"));
    command.args(["ready", "--repo", repository, "--json"]);
    if let Some(assignee) = assignee {
        command.args(["--assignee", assignee]);
    }
    command
        .env("GH_TOKEN", "automation-token")
        .env("HYFA_GITHUB_API_URL", api_url)
        .env("HYFA_NO_KEYRING", "1")
        .env("HYFA_STATE_DIR", state.path())
        .env("PATH", "");
    command
}

fn replica_since(state: &TempDir, repository: &str) -> String {
    let replica_path = state
        .path()
        .join("repositories")
        .join(repository)
        .join("replica.json");
    let replica: Value = serde_json::from_slice(&fs::read(replica_path).expect("Local replica"))
        .expect("replica JSON");
    let watermark = replica["sync"]["ordinary_issues"]["watermark"]
        .as_str()
        .expect("ordinary-Issue watermark");
    let watermark = DateTime::parse_from_rfc3339(watermark).expect("valid watermark");
    (watermark - Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn issue_numbers(document: &Value) -> Vec<u64> {
    document["issues"]
        .as_array()
        .expect("issues array")
        .iter()
        .map(|issue| issue["number"].as_u64().expect("Issue number"))
        .collect()
}

fn blocker(number: u64, state: &str) -> String {
    format!(
        "[{{\"id\":{0}00,\"node_id\":\"I_{0}\",\"repository_url\":\"https://api.github.com/repos/acme/widgets\",\"number\":{0},\"state\":\"{1}\"}}]",
        number, state
    )
}

fn issue_inventory() -> &'static str {
    r#"[
      {
        "id":100,"node_id":"I_1","number":1,"title":"Available root","body":"",
        "state":"open","state_reason":null,"html_url":"https://github.com/acme/widgets/issues/1",
        "user":null,"assignees":[],"labels":[],
        "created_at":"2026-08-01T00:00:00Z","updated_at":"2026-08-01T00:00:00Z","closed_at":null
      },
      {
        "id":200,"node_id":"I_2","number":2,"title":"Alice owns this","body":"",
        "state":"open","state_reason":null,"html_url":"https://github.com/acme/widgets/issues/2",
        "user":null,"assignees":[{"login":"alice","id":20,"node_id":"U_alice"}],"labels":[],
        "created_at":"2026-08-01T00:00:00Z","updated_at":"2026-08-01T00:00:00Z","closed_at":null
      },
      {
        "id":300,"node_id":"I_3","number":3,"title":"Blocked by open root","body":"",
        "state":"open","state_reason":null,"html_url":"https://github.com/acme/widgets/issues/3",
        "user":null,"assignees":[],"labels":[],
        "created_at":"2026-08-01T00:00:00Z","updated_at":"2026-08-01T00:00:00Z","closed_at":null
      },
      {
        "id":400,"node_id":"I_4","number":4,"title":"Closed blocker is satisfied","body":"",
        "state":"open","state_reason":null,"html_url":"https://github.com/acme/widgets/issues/4",
        "user":null,"assignees":[],"labels":[],
        "created_at":"2026-08-01T00:00:00Z","updated_at":"2026-08-01T00:00:00Z","closed_at":null
      },
      {
        "id":500,"node_id":"I_5","number":5,"title":"Historical closed blocker","body":"",
        "state":"closed","state_reason":"completed","html_url":"https://github.com/acme/widgets/issues/5",
        "user":null,"assignees":[],"labels":[],
        "created_at":"2026-08-01T00:00:00Z","updated_at":"2026-08-01T00:00:00Z","closed_at":"2026-08-02T00:00:00Z"
      }
    ]"#
}

fn single_issue_inventory() -> &'static str {
    r#"[
      {
        "id":100,"node_id":"I_1","number":1,"title":"Available root","body":"",
        "state":"open","state_reason":null,"html_url":"https://github.com/acme/widgets/issues/1",
        "user":null,"assignees":[],"labels":[],
        "created_at":"2026-08-01T00:00:00Z","updated_at":"2026-08-01T00:00:00Z","closed_at":null
      }
    ]"#
}

fn graph_issue_inventory() -> String {
    let mut issues: Vec<_> = (1_u64..=9)
        .map(|number| {
            let closed = number == 9;
            serde_json::json!({
                "id": number * 100,
                "node_id": format!("I_{number}"),
                "number": number,
                "title": format!("Graph Issue {number}"),
                "body": "",
                "state": if closed { "closed" } else { "open" },
                "state_reason": if closed { Some("completed") } else { None },
                "html_url": format!("https://github.com/acme/graph/issues/{number}"),
                "user": null,
                "assignees": [],
                "labels": [],
                "created_at": "2026-08-01T00:00:00Z",
                "updated_at": "2026-08-01T00:00:00Z",
                "closed_at": if closed { Some("2026-08-02T00:00:00Z") } else { None }
            })
        })
        .collect();
    issues.sort_by_key(|issue| issue["number"].as_u64());
    serde_json::to_string(&issues).expect("graph Issue inventory")
}

fn graph_dependencies(number: u64) -> String {
    let internal = |blocker: u64, state: &str| {
        serde_json::json!({
            "id": blocker * 100,
            "node_id": format!("I_{blocker}"),
            "repository_url": "https://api.github.com/repos/acme/graph",
            "number": blocker,
            "state": state
        })
    };
    let external = |blocker: u64, state: &str| {
        serde_json::json!({
            "id": blocker * 100,
            "node_id": format!("E_{blocker}"),
            "repository_url": "https://api.github.com/repos/partners/platform",
            "number": blocker,
            "state": state
        })
    };
    let dependencies = match number {
        2 => vec![internal(3, "open")],
        3 => vec![internal(2, "open")],
        4 => vec![internal(2, "open")],
        5 => vec![external(50, "closed")],
        6 => vec![external(60, "open")],
        7 => vec![external(70, "unknown")],
        8 => vec![
            internal(1, "open"),
            internal(1, "open"),
            internal(9, "closed"),
        ],
        _ => Vec::new(),
    };
    serde_json::to_string(&dependencies).expect("graph Dependencies")
}
