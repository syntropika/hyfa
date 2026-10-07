use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use chrono::{DateTime, Duration, SecondsFormat};
use mockito::Matcher;
use serde_json::Value;
use tempfile::TempDir;

mod support;

#[test]
fn sync_uses_gh_token_and_reports_a_versioned_snapshot() {
    let mut github = mockito::Server::new();
    let authorization = Matcher::Exact("Bearer automation-token".into());
    let labels = mock_labels(&mut github, "acme/widgets", Some("Bearer automation-token"));

    let issues = github
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("state".into(), "all".into()),
            Matcher::UrlEncoded("sort".into(), "created".into()),
            Matcher::UrlEncoded("direction".into(), "asc".into()),
            Matcher::UrlEncoded("per_page".into(), "100".into()),
        ]))
        .match_header("authorization", authorization.clone())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(issue_inventory())
        .create();

    let comments = github
        .mock("GET", "/repos/acme/widgets/issues/comments")
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .match_header("authorization", authorization.clone())
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
        .match_header("authorization", authorization)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let events = mock_events(&mut github, "acme/widgets", Some("Bearer automation-token"));

    let state = TempDir::new().expect("temporary state directory");
    let output = sync_command(&state, &github.url(), "acme/widgets", true)
        .output()
        .expect("run hyfa");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("versioned JSON output");
    assert_eq!(document["schema_version"], "hyfa.sync/v1");
    assert_eq!(document["command"], "sync");
    assert_eq!(document["repository"], "acme/widgets");
    assert_eq!(
        document["snapshot"]["schema_version"],
        "hyfa.local-replica/v1"
    );
    assert_eq!(document["snapshot"]["issue_count"], 1);
    assert_eq!(document["snapshot"]["comment_count"], 0);
    assert_eq!(document["snapshot"]["dependency_count"], 0);
    assert!(document["snapshot"]["synced_at"].as_str().is_some());
    assert!(document["snapshot"]["input_hash"].as_str().is_some());

    let visible_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!visible_output.contains("automation-token"));

    issues.assert();
    comments.assert();
    dependencies.assert();
    labels.assert();
    events.assert();
}

#[test]
fn sync_uses_environment_token_without_external_executables() {
    let mut github = mockito::Server::new();
    let labels = mock_labels(&mut github, "acme/empty", Some("Bearer environment-token"));
    let issues = github
        .mock("GET", "/repos/acme/empty/issues")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("state".into(), "all".into()),
            Matcher::UrlEncoded("sort".into(), "created".into()),
            Matcher::UrlEncoded("direction".into(), "asc".into()),
            Matcher::UrlEncoded("per_page".into(), "100".into()),
        ]))
        .match_header("authorization", "Bearer environment-token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let comments = github
        .mock("GET", "/repos/acme/empty/issues/comments")
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .match_header("authorization", "Bearer environment-token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let events = mock_events(&mut github, "acme/empty", Some("Bearer environment-token"));

    let state = TempDir::new().expect("temporary state directory");
    let output = sync_command(&state, &github.url(), "acme/empty", true)
        .env("GH_TOKEN", "environment-token")
        .output()
        .expect("run hyfa");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    issues.assert();
    comments.assert();
    labels.assert();
    events.assert();
}

#[test]
fn sync_paginates_and_persists_only_normalized_issue_data() {
    let mut github = mockito::Server::new();
    let labels = mock_labels(&mut github, "acme/widgets", None);
    let issue_next = format!(
        "<{}/repos/acme/widgets/issues?state=all&sort=created&direction=asc&per_page=100&page=2>; rel=\"next\"",
        github.url()
    );
    let comment_next = format!(
        "<{}/repos/acme/widgets/issues/comments?per_page=100&page=2>; rel=\"next\"",
        github.url()
    );
    let dependency_next = format!(
        "<{}/repos/acme/widgets/issues/7/dependencies/blocked_by?per_page=100&page=2>; rel=\"next\"",
        github.url()
    );

    let issue_page_one = github
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(Matcher::Exact(
            "state=all&sort=created&direction=asc&per_page=100".into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_header("link", &issue_next)
        .with_body(format!("[{},{}]", issue_seven(), pull_request_eight()))
        .create();
    let issue_page_two = github
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(Matcher::Exact(
            "state=all&sort=created&direction=asc&per_page=100&page=2".into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!("[{}]", issue_nine()))
        .create();

    let comment_page_one = github
        .mock("GET", "/repos/acme/widgets/issues/comments")
        .match_query(Matcher::Exact("per_page=100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_header("link", &comment_next)
        .with_body(format!(
            "[{},{}]",
            comment(701, 7, "First Issue comment"),
            comment(801, 8, "Pull Request comment")
        ))
        .create();
    let comment_page_two = github
        .mock("GET", "/repos/acme/widgets/issues/comments")
        .match_query(Matcher::Exact("per_page=100&page=2".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!("[{}]", comment(901, 9, "Closed Issue comment")))
        .create();

    let dependency_page_one = github
        .mock("GET", "/repos/acme/widgets/issues/7/dependencies/blocked_by")
        .match_query(Matcher::Exact("per_page=100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_header("link", &dependency_next)
        .with_body("[{\"id\":900,\"node_id\":\"I_9\",\"repository_url\":\"https://api.github.com/repos/acme/widgets\",\"number\":9,\"state\":\"closed\",\"title\":\"Internal blocker title is not persisted on the edge\",\"body\":\"raw blocker body\"}]")
        .create();
    let dependency_page_two = github
        .mock("GET", "/repos/acme/widgets/issues/7/dependencies/blocked_by")
        .match_query(Matcher::Exact("per_page=100&page=2".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[{\"id\":4200,\"node_id\":\"I_42\",\"repository_url\":\"https://api.github.com/repos/partners/platform\",\"number\":42,\"state\":\"open\",\"title\":\"Sensitive external title\",\"body\":\"Sensitive external body\"}]")
        .create();
    let issue_nine_dependencies = github
        .mock(
            "GET",
            "/repos/acme/widgets/issues/9/dependencies/blocked_by",
        )
        .match_query(Matcher::Exact("per_page=100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let events = mock_events(&mut github, "acme/widgets", None);

    let state = TempDir::new().expect("temporary state directory");
    let output = sync_command(&state, &github.url(), "acme/widgets", true)
        .output()
        .expect("run hyfa");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).expect("sync summary");
    assert_eq!(summary["snapshot"]["issue_count"], 2);
    assert_eq!(summary["snapshot"]["comment_count"], 2);
    assert_eq!(summary["snapshot"]["dependency_count"], 2);

    let replica_path = state.path().join("repositories/acme/widgets/replica.json");
    let replica: Value =
        serde_json::from_slice(&fs::read(&replica_path).expect("published Local replica"))
            .expect("normalized replica JSON");
    assert_eq!(replica["issues"][0]["number"], 7);
    assert_eq!(replica["issues"][0]["title"], "Ship the widget");
    assert_eq!(replica["issues"][0]["body"], "The complete Issue body.");
    assert_eq!(replica["issues"][0]["author"]["login"], "ada");
    assert_eq!(replica["issues"][0]["assignees"][0]["login"], "grace");
    assert_eq!(replica["issues"][0]["labels"][0]["name"], "area:core");
    assert_eq!(
        replica["issues"][0]["comments"][0]["body"],
        "First Issue comment"
    );
    assert_eq!(replica["issues"][1]["number"], 9);
    assert_eq!(replica["issues"][1]["state_reason"], "completed");
    assert_eq!(
        replica["issues"][1]["comments"][0]["body"],
        "Closed Issue comment"
    );

    assert_eq!(replica["dependencies"][0]["blocked"]["number"], 7);
    assert_eq!(replica["dependencies"][0]["blocker"]["number"], 9);
    assert_eq!(replica["dependencies"][0]["blocker"]["scope"], "internal");
    assert_eq!(replica["dependencies"][0]["blocker"]["id"], 900);
    assert_eq!(
        replica["dependencies"][1]["blocker"]["repository"],
        "partners/platform"
    );
    assert_eq!(replica["dependencies"][1]["blocker"]["scope"], "external");
    assert!(replica["dependencies"][1]["blocker"].get("id").is_none());
    assert!(
        replica["dependencies"][1]["blocker"]
            .get("node_id")
            .is_none()
    );
    assert!(
        !fs::read_to_string(&replica_path)
            .expect("replica text")
            .contains("Sensitive external")
    );
    assert_eq!(
        fs::metadata(&replica_path)
            .expect("replica metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    issue_page_one.assert();
    issue_page_two.assert();
    comment_page_one.assert();
    comment_page_two.assert();
    dependency_page_one.assert();
    dependency_page_two.assert();
    issue_nine_dependencies.assert();
    labels.assert();
    events.assert();
}

#[test]
fn pagination_failure_preserves_the_previous_complete_replica() {
    let state = TempDir::new().expect("temporary state directory");
    let mut initial_github = mockito::Server::new();
    let initial_labels = mock_labels(&mut initial_github, "acme/widgets", None);
    let initial_issues = initial_github
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("state".into(), "all".into()),
            Matcher::UrlEncoded("sort".into(), "created".into()),
            Matcher::UrlEncoded("direction".into(), "asc".into()),
            Matcher::UrlEncoded("per_page".into(), "100".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(issue_inventory())
        .create();
    let initial_comments = initial_github
        .mock("GET", "/repos/acme/widgets/issues/comments")
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let initial_dependencies = initial_github
        .mock(
            "GET",
            "/repos/acme/widgets/issues/7/dependencies/blocked_by",
        )
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let initial_events = mock_events(&mut initial_github, "acme/widgets", None);

    let initial = sync_command(&state, &initial_github.url(), "acme/widgets", true)
        .output()
        .expect("initial sync");
    assert!(initial.status.success());
    initial_issues.assert();
    initial_comments.assert();
    initial_dependencies.assert();
    initial_labels.assert();
    initial_events.assert();

    let replica_path = state.path().join("repositories/acme/widgets/replica.json");
    let complete_replica = fs::read(&replica_path).expect("initial complete replica");
    let since = replica_since(&complete_replica);

    let mut failing_github = mockito::Server::new();
    let failing_labels = mock_labels(&mut failing_github, "acme/widgets", None);
    let next = format!(
        "<{}/repos/acme/widgets/issues?state=all&sort=created&direction=asc&since={since}&per_page=100&page=2>; rel=\"next\"",
        failing_github.url(),
    );
    let first_page = failing_github
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(support::issue_delta_query(&since, None))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_header("link", &next)
        .with_body(issue_inventory().replace("Ship the widget", "Incomplete update"))
        .create();
    let failed_page = failing_github
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(support::issue_delta_query(&since, Some(2)))
        .with_status(500)
        .with_header("content-type", "application/json")
        .with_body("{\"message\":\"temporary failure\"}")
        .create();

    let failed = sync_command(&state, &failing_github.url(), "acme/widgets", true)
        .output()
        .expect("failed sync");

    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("HTTP 500"));
    assert_eq!(
        fs::read(&replica_path).expect("preserved replica"),
        complete_replica
    );
    let residue: Vec<_> = fs::read_dir(replica_path.parent().expect("replica directory"))
        .expect("replica directory contents")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(residue.is_empty(), "temporary residue: {residue:?}");
    first_page.assert();
    failed_page.assert();
    failing_labels.assert();
}

#[test]
fn sync_reports_human_output_using_environment_authentication() {
    let mut github = mockito::Server::new();
    let labels = mock_labels(&mut github, "acme/empty", Some("Bearer human-token"));
    let issues = github
        .mock("GET", "/repos/acme/empty/issues")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("state".into(), "all".into()),
            Matcher::UrlEncoded("sort".into(), "created".into()),
            Matcher::UrlEncoded("direction".into(), "asc".into()),
            Matcher::UrlEncoded("per_page".into(), "100".into()),
        ]))
        .match_header("authorization", "Bearer human-token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let comments = github
        .mock("GET", "/repos/acme/empty/issues/comments")
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .match_header("authorization", "Bearer human-token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let events = mock_events(&mut github, "acme/empty", Some("Bearer human-token"));

    let state = TempDir::new().expect("temporary state directory");
    let output = sync_command(&state, &github.url(), "acme/empty", false)
        .env("GH_TOKEN", "human-token")
        .env("HYFA_GITHUB_HOST", "github.com")
        .output()
        .expect("run hyfa");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("human output");
    assert!(stdout.contains("Synchronized acme/empty"));
    assert!(stdout.contains("0 Issues, 0 comments, 0 Dependencies"));
    assert!(stdout.contains("synced_at"));
    assert!(!stdout.contains("human-token"));
    issues.assert();
    comments.assert();
    labels.assert();
    events.assert();
}

#[test]
fn authentication_failure_does_not_publish_a_replica() {
    let state = TempDir::new().expect("temporary state directory");
    let output = sync_command(&state, "http://127.0.0.1:1", "acme/widgets", true)
        .env_remove("GH_TOKEN")
        .output()
        .expect("run hyfa");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no saved Hyfa credential"));
    assert_failed_diagnostics_without_replica(&state, "authentication");
}

#[test]
fn sync_rebuilds_a_corrupt_local_replica_when_github_is_available() {
    let mut github = mockito::Server::new();
    let labels = mock_labels(&mut github, "acme/widgets", None);
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
        .with_body("[]")
        .create();
    let comments = github
        .mock("GET", "/repos/acme/widgets/issues/comments")
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();
    let events = mock_events(&mut github, "acme/widgets", None);
    let state = TempDir::new().expect("temporary state directory");
    let replica_path = state.path().join("repositories/acme/widgets/replica.json");
    fs::create_dir_all(replica_path.parent().expect("replica directory"))
        .expect("replica directory");
    fs::write(&replica_path, "{\"schema_version\":").expect("corrupt replica");

    let output = sync_command(&state, &github.url(), "acme/widgets", true)
        .output()
        .expect("repairing sync");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rebuilt: Value = serde_json::from_slice(&fs::read(replica_path).expect("rebuilt replica"))
        .expect("rebuilt replica JSON");
    assert_eq!(rebuilt["schema_version"], "hyfa.local-replica/v1");
    assert_eq!(rebuilt["issues"], serde_json::json!([]));
    issues.assert();
    comments.assert();
    labels.assert();
    events.assert();
}

#[test]
fn rate_limit_failure_is_actionable_and_does_not_publish_a_replica() {
    let mut github = mockito::Server::new();
    let labels = mock_labels(&mut github, "acme/widgets", None);
    let events = mock_events(&mut github, "acme/widgets", None);
    let limited = github
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("state".into(), "all".into()),
            Matcher::UrlEncoded("sort".into(), "created".into()),
            Matcher::UrlEncoded("direction".into(), "asc".into()),
            Matcher::UrlEncoded("per_page".into(), "100".into()),
        ]))
        .with_status(403)
        .with_header("content-type", "application/json")
        .with_header("x-ratelimit-remaining", "75")
        .with_header("x-ratelimit-reset", "1786100000")
        .with_header("retry-after", "60")
        .with_body("{\"message\":\"API rate limit exceeded\"}")
        .create();
    let state = TempDir::new().expect("temporary state directory");

    let output = sync_command(&state, &github.url(), "acme/widgets", true)
        .output()
        .expect("run hyfa");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rate limit"));
    assert!(stderr.contains("1786100000"));
    assert!(stderr.contains("60"));
    assert!(!stderr.contains("automation-token"));
    assert_failed_diagnostics_without_replica(&state, "rate_limited");
    events.assert();
    limited.assert();
    labels.assert();
}

#[test]
fn interrupted_response_does_not_publish_a_replica() {
    let mut github = mockito::Server::new();
    let labels = mock_labels(&mut github, "acme/widgets", None);
    let events = mock_events(&mut github, "acme/widgets", None);
    let interrupted = github
        .mock("GET", "/repos/acme/widgets/issues")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("state".into(), "all".into()),
            Matcher::UrlEncoded("sort".into(), "created".into()),
            Matcher::UrlEncoded("direction".into(), "asc".into()),
            Matcher::UrlEncoded("per_page".into(), "100".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[{\"id\":700")
        .create();
    let state = TempDir::new().expect("temporary state directory");

    let output = sync_command(&state, &github.url(), "acme/widgets", true)
        .output()
        .expect("run hyfa");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid JSON"));
    assert_failed_diagnostics_without_replica(&state, "invalid_response");
    events.assert();
    interrupted.assert();
    labels.assert();
}

#[test]
fn sync_rejects_an_unsafe_api_base_before_authentication() {
    for unsafe_base in [
        "ftp://api.github.example/",
        "https://embedded:secret@api.github.example/",
        "https://api.github.example/?token=secret",
        "https://api.github.example/#fragment",
    ] {
        let state = TempDir::new().expect("temporary state directory");
        let output = sync_command(&state, unsafe_base, "acme/widgets", true)
            .output()
            .expect("run hyfa");

        assert!(
            !output.status.success(),
            "unsafe base accepted: {unsafe_base}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("safe absolute HTTP(S) base URL"),
            "stderr for {unsafe_base}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!state.path().join("repositories").exists());
    }
}

fn assert_failed_diagnostics_without_replica(state: &TempDir, code: &str) {
    assert!(!support::replica_path(state, "acme/widgets").exists());
    let attempt: Value = serde_json::from_slice(
        &fs::read(
            state
                .path()
                .join("repositories/acme/widgets/sync-status.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(attempt["state"], "failed");
    assert_eq!(attempt["failure"]["code"], code);
    assert!(attempt["published_synced_at"].is_null());
}

fn mock_labels(
    github: &mut mockito::Server,
    repository: &str,
    authorization: Option<&str>,
) -> mockito::Mock {
    let path = format!("/repos/{repository}/labels");
    let mut labels = github
        .mock("GET", path.as_str())
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()));
    if let Some(authorization) = authorization {
        labels = labels.match_header("authorization", authorization);
    }
    labels
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create()
}

fn mock_events(
    github: &mut mockito::Server,
    repository: &str,
    authorization: Option<&str>,
) -> mockito::Mock {
    let path = format!("/repos/{repository}/issues/events");
    let mut events = github
        .mock("GET", path.as_str())
        .match_query(Matcher::UrlEncoded("per_page".into(), "100".into()));
    if let Some(authorization) = authorization {
        events = events.match_header("authorization", authorization);
    }
    events
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"[{"id":100,"event":"labeled","created_at":"2026-08-01T00:00:00Z","issue":null}]"#,
        )
        .create()
}

fn sync_command(state: &TempDir, api_url: &str, repository: &str, json: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hyfa"));
    command.args(["sync", "--repo", repository]);
    if json {
        command.arg("--json");
    }
    command
        .env("GH_TOKEN", "automation-token")
        .env("HYFA_GITHUB_API_URL", api_url)
        .env("HYFA_NO_KEYRING", "1")
        .env("HYFA_STATE_DIR", state.path())
        .env("PATH", "");
    command
}

fn replica_since(bytes: &[u8]) -> String {
    let replica: Value = serde_json::from_slice(bytes).expect("replica JSON");
    let watermark = replica["sync"]["ordinary_issues"]["watermark"]
        .as_str()
        .expect("ordinary-Issue watermark");
    let watermark = DateTime::parse_from_rfc3339(watermark).expect("valid watermark");
    (watermark - Duration::minutes(1)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn issue_inventory() -> &'static str {
    r#"[
      {
        "id": 700,
        "node_id": "I_kwDO_issue7",
        "number": 7,
        "title": "Ship the widget",
        "body": "The complete Issue body.",
        "state": "open",
        "state_reason": null,
        "html_url": "https://github.com/acme/widgets/issues/7",
        "user": {"login": "ada", "id": 10, "node_id": "U_ada"},
        "assignees": [],
        "labels": [{"id": 20, "node_id": "L_bug", "name": "bug", "color": "d73a4a", "description": "Something is broken"}],
        "created_at": "2026-08-01T10:00:00Z",
        "updated_at": "2026-08-02T11:00:00Z",
        "closed_at": null
      }
    ]"#
}

fn issue_seven() -> &'static str {
    r#"{
      "id":700,"node_id":"I_7","number":7,
      "title":"Ship the widget","body":"The complete Issue body.",
      "state":"open","state_reason":null,
      "html_url":"https://github.com/acme/widgets/issues/7",
      "user":{"login":"ada","id":10,"node_id":"U_ada"},
      "assignees":[{"login":"grace","id":11,"node_id":"U_grace"}],
      "labels":[{"id":21,"node_id":"L_core","name":"area:core","color":"123456","description":"Core work"}],
      "created_at":"2026-08-01T10:00:00Z","updated_at":"2026-08-02T11:00:00Z","closed_at":null
    }"#
}

fn pull_request_eight() -> &'static str {
    r#"{
      "id":800,"node_id":"PR_8","number":8,
      "title":"A pull request","body":"Must not enter the replica.",
      "state":"open","state_reason":null,
      "html_url":"https://github.com/acme/widgets/pull/8",
      "user":{"login":"linus","id":12,"node_id":"U_linus"},
      "assignees":[],"labels":[],
      "created_at":"2026-08-02T10:00:00Z","updated_at":"2026-08-02T11:00:00Z","closed_at":null,
      "pull_request":{"url":"https://api.github.com/repos/acme/widgets/pulls/8"}
    }"#
}

fn issue_nine() -> &'static str {
    r#"{
      "id":900,"node_id":"I_9","number":9,
      "title":"Completed prerequisite","body":null,
      "state":"closed","state_reason":"completed",
      "html_url":"https://github.com/acme/widgets/issues/9",
      "user":null,"assignees":[],"labels":["legacy-label"],
      "created_at":"2026-08-03T10:00:00Z","updated_at":"2026-08-04T11:00:00Z","closed_at":"2026-08-04T11:00:00Z"
    }"#
}

fn comment(id: u64, issue_number: u64, body: &str) -> String {
    format!(
        r#"{{
          "id":{id},"node_id":"IC_{id}",
          "html_url":"https://github.com/acme/widgets/issues/{issue_number}#issuecomment-{id}",
          "body":"{body}","user":{{"login":"commenter","id":13,"node_id":"U_commenter"}},
          "author_association":"CONTRIBUTOR",
          "created_at":"2026-08-05T10:00:00Z","updated_at":"2026-08-05T11:00:00Z",
          "issue_url":"https://api.github.com/repos/acme/widgets/issues/{issue_number}"
        }}"#
    )
}
