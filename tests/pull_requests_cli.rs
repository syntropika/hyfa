use std::{fs, process::Command};

use mockito::{Matcher, Server};
use serde_json::{Value, json};
use tempfile::TempDir;

#[path = "support/scoped.rs"]
mod fixture;

fn command(state: &TempDir, api: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hyfa"));
    command
        .env("HYFA_STATE_DIR", state.path())
        .env("HYFA_NO_KEYRING", "1")
        .env("GH_TOKEN", "context-token")
        .env("HYFA_GITHUB_API_URL", api);
    command
}

fn prs(state: &TempDir, api: &str, offline: bool) -> Value {
    let mut command = command(state, api);
    command.args(["prs", "acme/widgets#1", "--json"]);
    if offline {
        command.arg("--offline");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn pr(number: u64, state: &str) -> Value {
    json!({
        "number": number, "title": format!("Fix {number}"), "state": state,
        "isDraft": state == "OPEN", "url": format!("https://github.com/acme/widgets/pull/{number}"),
        "updatedAt": "2026-10-01T12:00:00Z", "mergedAt": if state == "MERGED" { Some("2026-10-01T12:00:00Z") } else { None },
        "repository": {"nameWithOwner":"acme/widgets"}
    })
}

fn page(nodes: Vec<Value>, total: usize, next: bool, cursor: Option<&str>) -> Value {
    json!({"data":{"repository":{"issue":{"number":1,"closedByPullRequestsReferences":{
        "totalCount":total,"nodes":nodes,"pageInfo":{"hasNextPage":next,"endCursor":cursor}
    }}}}})
}

fn cache_path(state: &TempDir) -> std::path::PathBuf {
    state
        .path()
        .join("repositories/acme/widgets/pull-requests/1.json")
}

#[test]
fn linked_prs_paginate_include_closed_and_merged_and_cache_complete_context() {
    let state = TempDir::new().unwrap();
    let mut github = Server::new();
    let first = github
        .mock("POST", "/graphql")
        .match_header("authorization", "Bearer context-token")
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("includeClosedPrs: true".into()),
            Matcher::PartialJson(json!({"variables":{"cursor":null,"number":1}})),
        ]))
        .with_status(200)
        .with_body(page(vec![pr(12, "OPEN")], 3, true, Some("next-page")).to_string())
        .create();
    let mut other = pr(8, "CLOSED");
    other["repository"]["nameWithOwner"] = json!("acme/other");
    other["url"] = json!("https://github.com/acme/other/pull/8");
    let second = github
        .mock("POST", "/graphql")
        .match_body(Matcher::PartialJson(
            json!({"variables":{"cursor":"next-page"}}),
        ))
        .with_status(200)
        .with_body(page(vec![pr(11, "MERGED"), other], 3, false, None).to_string())
        .create();
    let result = prs(&state, &github.url(), false);
    assert_eq!(result["schema_version"], "hyfa.pull-requests/v1");
    assert_eq!(result["context"]["source"], "live");
    assert_eq!(result["context"]["complete"], true);
    assert_eq!(result["context"]["pull_requests"][0]["key"], "acme/other#8");
    assert_eq!(result["context"]["pull_requests"][1]["state"], "merged");
    assert_eq!(result["context"]["pull_requests"][2]["draft"], true);
    first.assert();
    second.assert();
    let offline = prs(&state, "invalid-url", true);
    assert_eq!(offline["context"]["source"], "local");
    assert_eq!(
        offline["context"]["pull_requests"],
        result["context"]["pull_requests"]
    );
    assert_eq!(
        offline["context"]["observed_at"],
        result["context"]["observed_at"]
    );
    assert!(
        !fs::read_to_string(cache_path(&state))
            .unwrap()
            .contains("context-token")
    );
    assert!(
        !state
            .path()
            .join("repositories/acme/widgets/replica.json")
            .exists()
    );
}

#[test]
fn pr_context_distinguishes_unknown_from_a_complete_empty_observation() {
    let state = TempDir::new().unwrap();
    let unknown = prs(&state, "invalid-url", true);
    assert_eq!(unknown["context"]["complete"], false);
    assert!(unknown["context"]["pull_requests"].is_null());
    let mut github = Server::new();
    let empty = github
        .mock("POST", "/graphql")
        .with_status(200)
        .with_body(page(vec![], 0, false, None).to_string())
        .create();
    let result = prs(&state, &github.url(), false);
    assert_eq!(result["context"]["complete"], true);
    assert_eq!(result["context"]["pull_requests"], json!([]));
    empty.assert();
}

#[test]
fn failed_or_incomplete_pr_refresh_preserves_cached_data_and_observation_time() {
    let state = TempDir::new().unwrap();
    let mut github = Server::new();
    let seed = github
        .mock("POST", "/graphql")
        .with_status(200)
        .with_body(page(vec![pr(11, "OPEN")], 1, false, None).to_string())
        .create();
    let previous = prs(&state, &github.url(), false);
    seed.assert();
    let bytes = fs::read(cache_path(&state)).unwrap();
    for payload in [
        page(vec![pr(12, "OPEN")], 2, false, None),
        page(vec![Value::Null], 1, false, None),
        page(vec![pr(12, "OPEN")], 2, true, None),
        json!({"data":null,"errors":[{"message":"private source context-token"}]}),
        json!({"data":{"repository":{"issue":null}}}),
        page(vec![pr(12, "OPEN")], 1, true, Some("overshoot")),
        page(vec![pr(12, "OPEN"), pr(13, "OPEN")], 1, false, None),
    ] {
        let mut failure = Server::new();
        let mock = failure
            .mock("POST", "/graphql")
            .with_status(200)
            .with_body(payload.to_string())
            .create();
        let result = prs(&state, &failure.url(), false);
        assert_eq!(result["context"]["source"], "local_fallback");
        assert_eq!(
            result["context"]["observed_at"],
            previous["context"]["observed_at"]
        );
        assert_eq!(
            result["context"]["pull_requests"],
            previous["context"]["pull_requests"]
        );
        assert_eq!(fs::read(cache_path(&state)).unwrap(), bytes);
        assert!(!result.to_string().contains("context-token"));
        mock.assert();
    }
}

#[test]
fn repeated_pr_cursors_fail_without_publishing_partial_context() {
    let state = TempDir::new().unwrap();
    let mut github = Server::new();
    let first = github
        .mock("POST", "/graphql")
        .match_body(Matcher::PartialJson(json!({"variables":{"cursor":null}})))
        .with_status(200)
        .with_body(page(vec![pr(11, "OPEN")], 3, true, Some("repeated")).to_string())
        .create();
    let second = github
        .mock("POST", "/graphql")
        .match_body(Matcher::PartialJson(
            json!({"variables":{"cursor":"repeated"}}),
        ))
        .with_status(200)
        .with_body(page(vec![pr(12, "OPEN")], 3, true, Some("repeated")).to_string())
        .create();
    let result = prs(&state, &github.url(), false);
    assert_eq!(result["context"]["complete"], false);
    assert!(!cache_path(&state).exists());
    first.assert();
    second.assert();
}

#[test]
fn view_includes_opt_in_pr_context_and_drafts_never_query_github() {
    let fixture = fixture::Fixture::new();
    fixture.run(&["sync", "--repo", "acme/widgets"], false);
    let plain = fixture.run(&["view", "acme/widgets#1", "--offline"], true);
    assert!(plain.get("pull_request_context").is_none());
    let replica = fixture.snapshot("replica.json");
    let extended = fixture.run(&["view", "acme/widgets#1", "--offline", "--with-prs"], true);
    assert_eq!(extended["issue"], plain["issue"]);
    assert_eq!(extended["pull_request_context"]["complete"], false);
    assert_eq!(fixture.snapshot("replica.json"), replica);
    let draft = fixture.run(
        &[
            "create",
            "--repo",
            "acme/widgets",
            "--title",
            "Draft context",
        ],
        true,
    );
    let key = draft["draft"]["key"].as_str().unwrap();
    let output = fixture
        .command()
        .env("HYFA_GITHUB_API_URL", "invalid-url")
        .args(["prs", key, "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let draft_context: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(draft_context["issue"], key);
    assert_eq!(draft_context["context"]["source"], "draft");
    assert_eq!(draft_context["context"]["pull_requests"], json!([]));
    assert!(fixture.remote.lock().unwrap().writes.is_empty());
}
