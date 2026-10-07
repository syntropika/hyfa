use serde_json::{Value, json};

#[path = "support/scoped.rs"]
mod fixture;
use fixture::Fixture;

fn seed() -> Fixture {
    let fixture = Fixture::new();
    {
        let mut remote = fixture.remote.lock().unwrap();
        remote.issues.get_mut(&1).unwrap()["title"] = json!("Manifest cache expires");
        remote.issues.get_mut(&1).unwrap()["body"] =
            json!("See #3 and other/widgets#4; preserve café data.");
        remote.issues.get_mut(&2).unwrap()["title"] = json!("Manifest cache eviction");
        remote.issues.get_mut(&2).unwrap()["assignees"] =
            json!([{"id":1,"node_id":"U_1","login":"alice"}]);
        remote.issues.get_mut(&3).unwrap()["title"] = json!("A separate fix");
        remote.issues.get_mut(&4).unwrap()["title"] = json!("Unrelated work");
        remote.issues.get_mut(&4).unwrap()["body"] =
            json!("See #12, not a reference to Issue one.");
        remote.issues.get_mut(&5).unwrap()["title"] = json!("Manifest cache history");
        remote.issues.get_mut(&5).unwrap()["state"] = json!("closed");
    }
    fixture.run(&["sync", "--repo", "acme/widgets"], false);
    fixture
}

#[test]
fn local_search_uses_all_effective_content_and_reports_filters_and_limits() {
    let fixture = seed();
    let replica = fixture.snapshot("replica.json");
    let journal = fixture.snapshot("sync-status.json");
    // An invalid API URL proves that ordinary discovery does not even initialize a client.
    let output = fixture
        .command()
        .env("HYFA_GITHUB_API_URL", "not-a-url")
        .args([
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "MANIFEST cache",
            "--limit",
            "2",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["schema_version"], "hyfa.search/v1");
    assert_eq!(result["source"], "local");
    assert_eq!(result["results"]["matched_count"], 3);
    assert_eq!(result["results"]["truncated"], true);
    assert_eq!(result["results"]["hits"][0]["number"], 1);
    assert_eq!(result["results"]["hits"][1]["number"], 2);
    let repeated = fixture.run(
        &[
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "manifest cache",
            "--limit",
            "2",
        ],
        false,
    );
    assert_eq!(repeated["results"], result["results"]);
    assert_eq!(fixture.snapshot("replica.json"), replica);
    assert_eq!(fixture.snapshot("sync-status.json"), journal);
    let comments = fixture.run(
        &[
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "recorded discussion",
            "--assignee",
            "ALICE",
        ],
        true,
    );
    assert_eq!(comments["results"]["hits"][0]["key"], "acme/widgets#2");
    assert_eq!(
        comments["results"]["hits"][0]["evidence"][0]["field"],
        "comment"
    );
    let closed = fixture.run(
        &[
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "cache",
            "--state",
            "closed",
        ],
        true,
    );
    assert_eq!(closed["results"]["matched_count"], 1);
    assert_eq!(closed["results"]["hits"][0]["number"], 5);
    let none = fixture.run(
        &[
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "cache",
            "--label",
            "missing",
        ],
        true,
    );
    assert_eq!(none["results"]["hits"], json!([]));
    let unicode = fixture.run(
        &["search", "--repo", "acme/widgets", "--query", "CAFÉ"],
        true,
    );
    assert_eq!(unicode["results"]["hits"][0]["number"], 1);
    assert!(fixture.remote.lock().unwrap().writes.is_empty());
}

#[test]
fn related_explains_references_and_title_overlap_without_changing_dependencies() {
    let fixture = seed();
    let replica = fixture.snapshot("replica.json");
    let related = fixture.run(&["related", "acme/widgets#1"], true);
    assert_eq!(related["schema_version"], "hyfa.related/v1");
    let hits = related["results"]["hits"].as_array().unwrap();
    assert_eq!(hits[0]["key"], "acme/widgets#3");
    assert_eq!(hits[0]["evidence"][0]["kind"], "subject_reference");
    assert_eq!(hits[1]["key"], "acme/widgets#2");
    assert_eq!(
        hits[1]["evidence"][0]["terms"],
        json!(["cache", "manifest"])
    );
    assert!(
        !hits
            .iter()
            .any(|hit| hit["number"] == 1 || hit["number"] == 4)
    );
    assert_eq!(fixture.snapshot("replica.json"), replica);
    assert!(fixture.remote.lock().unwrap().writes.is_empty());
}

#[test]
fn discovery_projects_pending_edits_comments_and_drafts_without_replaying_them() {
    let fixture = seed();
    let replica = fixture.snapshot("replica.json");
    fixture.run(
        &[
            "update",
            "acme/widgets#3",
            "--title",
            "Manifest cache recovery",
        ],
        true,
    );
    fixture.run(
        &[
            "comment",
            "acme/widgets#3",
            "--body",
            "Pending discussion contains café",
        ],
        true,
    );
    let draft = fixture.run(
        &[
            "create",
            "--repo",
            "acme/widgets",
            "--title",
            "Manifest cache draft",
            "--body",
            "See #1",
        ],
        true,
    );
    let key = draft["draft"]["key"].as_str().unwrap();
    let outbox = fixture.snapshot("outbox.json");
    let result = fixture.run(
        &[
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "manifest cache",
        ],
        true,
    );
    let hits = result["results"]["hits"].as_array().unwrap();
    let hit = hits.iter().find(|hit| hit["key"] == key).unwrap();
    assert!(hit.get("number").is_none());
    assert_eq!(hit["temporary_id"], draft["draft"]["temporary_id"]);
    assert_eq!(hit["pending"], true);
    let pending = fixture.run(
        &[
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "pending discussion",
        ],
        true,
    );
    assert_eq!(pending["results"]["hits"][0]["key"], "acme/widgets#3");
    assert_eq!(pending["results"]["hits"][0]["pending"], true);
    let related = fixture.run(&["related", key], true);
    assert_eq!(related["subject"], key);
    assert_eq!(related["results"]["hits"][0]["key"], "acme/widgets#1");
    assert_eq!(fixture.snapshot("outbox.json"), outbox);
    assert_eq!(fixture.snapshot("replica.json"), replica);
    assert!(fixture.remote.lock().unwrap().writes.is_empty());
}

#[test]
fn discovery_rejects_empty_queries_missing_sources_and_invalid_limits() {
    let fixture = seed();
    for args in [
        vec!["search", "--repo", "acme/widgets", "--query", "---"],
        vec!["related", "acme/widgets#999"],
        vec![
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "cache",
            "--limit",
            "0",
        ],
        vec![
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "cache",
            "--limit",
            "101",
        ],
    ] {
        let output = fixture.command().args(args).output().unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn search_bounds_unicode_evidence_and_excludes_operation_markers() {
    let fixture = Fixture::new();
    {
        let mut remote = fixture.remote.lock().unwrap();
        remote.issues.get_mut(&1).unwrap()["body"] = json!(format!(
            "{} CAFÉ tail <!-- hyfa-operation:6ba7b810-9dad-11d1-80b4-00c04fd430c8 -->",
            "λ".repeat(300)
        ));
    }
    fixture.run(&["sync", "--repo", "acme/widgets"], false);
    for _ in 0..6 {
        fixture.run(
            &[
                "comment",
                "acme/widgets#1",
                "--body",
                "Another café example",
            ],
            true,
        );
    }
    let result = fixture.run(
        &["search", "--repo", "acme/widgets", "--query", "café"],
        true,
    );
    let hit = &result["results"]["hits"][0];
    assert_eq!(hit["evidence_truncated"], true);
    assert_eq!(hit["evidence"].as_array().unwrap().len(), 5);
    for evidence in hit["evidence"].as_array().unwrap() {
        assert!(evidence["snippet"].as_str().unwrap().chars().count() <= 180);
    }
    assert!(
        hit["evidence"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("CAFÉ")
    );
    assert!(!result.to_string().contains("6ba7b810"));
    let hidden = fixture.run(
        &["search", "--repo", "acme/widgets", "--query", "6ba7b810"],
        true,
    );
    assert_eq!(hidden["results"]["matched_count"], 0);
}

#[test]
fn explicit_discovery_refresh_falls_back_without_replaying_pending_work() {
    let fixture = seed();
    fixture.run(
        &[
            "update",
            "acme/widgets#3",
            "--title",
            "Manifest cache recovery",
        ],
        true,
    );
    let replica = fixture.snapshot("replica.json");
    let outbox = fixture.snapshot("outbox.json");
    let result = fixture.run(
        &[
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "recovery",
            "--refresh",
        ],
        true,
    );
    assert_eq!(result["source"], "local_fallback");
    assert_eq!(result["results"]["hits"][0]["pending"], true);
    assert_eq!(fixture.snapshot("replica.json"), replica);
    assert_eq!(fixture.snapshot("outbox.json"), outbox);
    assert!(fixture.remote.lock().unwrap().writes.is_empty());
    let journal = fixture.snapshot("sync-status.json");
    let invalid = fixture
        .command()
        .args([
            "search",
            "--repo",
            "acme/widgets",
            "--query",
            "---",
            "--refresh",
        ])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert_eq!(fixture.snapshot("sync-status.json"), journal);
}
