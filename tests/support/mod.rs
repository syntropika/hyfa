#![allow(dead_code)]

pub(crate) mod browser;

use std::{fs, process::Command};

use chrono::{DateTime, Duration, SecondsFormat};
use mockito::{Matcher, Mock, Server};
use serde_json::{Value, json};
use tempfile::TempDir;

pub(crate) fn issue_delta_query(since: &str, page: Option<u64>) -> Matcher {
    let mut fields = vec![
        Matcher::UrlEncoded("state".into(), "all".into()),
        Matcher::UrlEncoded("sort".into(), "created".into()),
        Matcher::UrlEncoded("direction".into(), "asc".into()),
        Matcher::UrlEncoded("since".into(), since.into()),
        Matcher::UrlEncoded("per_page".into(), "100".into()),
    ];
    if let Some(page) = page {
        fields.push(Matcher::UrlEncoded("page".into(), page.to_string()));
    }
    Matcher::AllOf(fields)
}

#[allow(dead_code)]
pub(crate) fn comment_delta_query(since: &str, page: Option<u64>) -> Matcher {
    let mut fields = vec![
        Matcher::UrlEncoded("sort".into(), "created".into()),
        Matcher::UrlEncoded("direction".into(), "asc".into()),
        Matcher::UrlEncoded("since".into(), since.into()),
        Matcher::UrlEncoded("per_page".into(), "100".into()),
    ];
    if let Some(page) = page {
        fields.push(Matcher::UrlEncoded("page".into(), page.to_string()));
    }
    Matcher::AllOf(fields)
}

pub(crate) fn sync_command(state: &TempDir, api_url: &str, repository: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hyfa"));
    command.args(["sync", "--repo", repository, "--json"]);
    command
        .env("GH_TOKEN", "automation-token")
        .env("HYFA_GITHUB_API_URL", api_url)
        .env("HYFA_NO_KEYRING", "1")
        .env("HYFA_STATE_DIR", state.path())
        .env("PATH", "");
    command
}

pub(crate) fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn synchronization_status(state: &TempDir, repository: &str) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_hyfa"))
        .args(["status", "--repo", repository, "--json"])
        .env_remove("GH_TOKEN")
        .env("HYFA_NO_KEYRING", "1")
        .env("HYFA_STATE_DIR", state.path())
        .env("HYFA_GITHUB_API_URL", "invalid-url")
        .output()
        .expect("offline synchronization status");
    assert_success(&output);
    serde_json::from_slice(&output.stdout).expect("status JSON")
}

pub(crate) fn replica_path(state: &TempDir, repository: &str) -> std::path::PathBuf {
    state
        .path()
        .join("repositories")
        .join(repository)
        .join("replica.json")
}

pub(crate) fn load_replica(state: &TempDir, repository: &str) -> Value {
    serde_json::from_slice(&fs::read(replica_path(state, repository)).expect("Local replica"))
        .expect("replica JSON")
}

pub(crate) fn replica_since(state: &TempDir, repository: &str) -> String {
    let replica = load_replica(state, repository);
    let watermark = DateTime::parse_from_rfc3339(
        replica["sync"]["ordinary_issues"]["watermark"]
            .as_str()
            .expect("ordinary-Issue watermark"),
    )
    .expect("valid watermark");
    (watermark - Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub(crate) fn mock_issue_count(github: &mut Server, count: u64) -> Mock {
    github
        .mock("POST", "/graphql")
        .match_body(Matcher::Regex("IssueInventoryCount".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "data": { "repository": { "issues": { "totalCount": count } } }
            })
            .to_string(),
        )
        .create()
}

pub struct RepositoryMocks {
    events: Mock,
    labels: Mock,
    issues: Mock,
    comments: Mock,
    dependencies: Vec<Mock>,
}

impl RepositoryMocks {
    pub fn assert(self) {
        self.events.assert();
        self.labels.assert();
        self.issues.assert();
        self.comments.assert();
        for dependency in self.dependencies {
            dependency.assert();
        }
    }
}

pub fn mock_repository(
    github: &mut Server,
    repository: &str,
    issues: Vec<Value>,
    dependencies: Vec<(u64, Vec<Value>)>,
) -> RepositoryMocks {
    mock_repository_with_calls(github, repository, issues, dependencies, 1)
}

pub fn mock_repository_with_calls(
    github: &mut Server,
    repository: &str,
    issues: Vec<Value>,
    dependencies: Vec<(u64, Vec<Value>)>,
    expected_calls: usize,
) -> RepositoryMocks {
    let labels = github
        .mock("GET", format!("/repos/{repository}/labels").as_str())
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(canonical_labels().to_string())
        .expect(expected_calls)
        .create();
    let issues = issues
        .into_iter()
        .map(|mut issue| {
            if issue["html_url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://github.com/acme/placeholder/issues/"))
            {
                issue["html_url"] = Value::String(format!(
                    "https://github.com/{repository}/issues/{}",
                    issue["number"]
                        .as_u64()
                        .expect("fixture Issue has a number")
                ));
            }
            issue
        })
        .collect();
    let issues = github
        .mock("GET", format!("/repos/{repository}/issues").as_str())
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("state".into(), "all".into()),
            Matcher::UrlEncoded("sort".into(), "created".into()),
            Matcher::UrlEncoded("direction".into(), "asc".into()),
            Matcher::UrlEncoded("per_page".into(), "100".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(Value::Array(issues).to_string())
        .expect(expected_calls)
        .create();
    let comments = github
        .mock(
            "GET",
            format!("/repos/{repository}/issues/comments").as_str(),
        )
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .expect(expected_calls)
        .create();
    let dependencies = dependencies
        .into_iter()
        .map(|(number, blockers)| {
            let blockers = blockers
                .into_iter()
                .map(|mut blocker| {
                    if blocker["repository_url"] == "https://api.github.com/repos/acme/placeholder"
                    {
                        blocker["repository_url"] =
                            Value::String(format!("https://api.github.com/repos/{repository}"));
                    }
                    blocker
                })
                .collect();
            github
                .mock(
                    "GET",
                    format!("/repos/{repository}/issues/{number}/dependencies/blocked_by").as_str(),
                )
                .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(Value::Array(blockers).to_string())
                .expect(expected_calls)
                .create()
        })
        .collect();
    let events = github
        .mock("GET", format!("/repos/{repository}/issues/events").as_str())
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"[{"id":100,"event":"labeled","created_at":"2026-08-01T00:00:00Z","issue":null}]"#,
        )
        .expect(expected_calls)
        .create();
    RepositoryMocks {
        events,
        labels,
        issues,
        comments,
        dependencies,
    }
}

pub fn issue(number: u64, state: &str, priority_labels: &[&str], assignees: &[&str]) -> Value {
    issue_for(
        "acme/placeholder",
        number,
        state,
        priority_labels,
        assignees,
    )
}

pub fn issue_for(
    repository: &str,
    number: u64,
    state: &str,
    priority_labels: &[&str],
    assignees: &[&str],
) -> Value {
    json!({
        "id": number * 100,
        "node_id": format!("I_{number}"),
        "number": number,
        "title": format!("Issue {number}"),
        "body": "",
        "state": state,
        "state_reason": if state == "closed" { Some("completed") } else { None },
        "html_url": format!("https://github.com/{repository}/issues/{number}"),
        "user": null,
        "assignees": assignees
            .iter()
            .enumerate()
            .map(|(index, login)| actor(number * 1000 + index as u64, login))
            .collect::<Vec<_>>(),
        "labels": priority_labels
            .iter()
            .enumerate()
            .map(|(index, name)| label(number * 10 + index as u64, name))
            .collect::<Vec<_>>(),
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": "2026-08-01T00:00:00Z",
        "closed_at": if state == "closed" { Some("2026-08-02T00:00:00Z") } else { None }
    })
}

pub fn internal_blocker(number: u64, state: &str) -> Value {
    internal_blocker_for("acme/placeholder", number, state)
}

pub fn internal_blocker_for(repository: &str, number: u64, state: &str) -> Value {
    blocker(repository, number, state)
}

pub fn external_blocker(repository: &str, number: u64, state: &str) -> Value {
    blocker(repository, number, state)
}

fn blocker(repository: &str, number: u64, state: &str) -> Value {
    json!({
        "id": number * 100,
        "node_id": format!("I_{number}"),
        "repository_url": format!("https://api.github.com/repos/{repository}"),
        "number": number,
        "state": state
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

fn actor(id: u64, login: &str) -> Value {
    json!({"id": id, "node_id": format!("U_{id}"), "login": login})
}
