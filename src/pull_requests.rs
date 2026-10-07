use std::{fs, io};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    atomic_file::{self, AtomicFileError},
    github::GitHubClient,
    model::strip_operation_markers,
    repository::{IssueReference, Repository},
    store::{self, StoreError},
    sync_diagnostics::Failure,
};

const SCHEMA: &str = "hyfa.pull-request-cache/v1";

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct PullRequest {
    pub(crate) key: String,
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) state: String,
    pub(crate) draft: bool,
    pub(crate) url: String,
    updated_at: String,
    merged_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawPullRequest {
    number: u64,
    title: String,
    state: String,
    is_draft: bool,
    url: String,
    updated_at: String,
    merged_at: Option<String>,
    repository: RawRepository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
    name_with_owner: String,
}

impl RawPullRequest {
    pub(crate) fn normalize(self) -> Result<PullRequest, crate::github::GitHubError> {
        let repository = Repository::parse(&self.repository.name_with_owner)
            .map_err(|_| crate::github::GitHubError::InvalidPullRequestContext)?;
        let result = PullRequest {
            key: format!("{}#{}", repository.full_name(), self.number),
            number: self.number,
            title: strip_operation_markers(&self.title),
            state: self.state.to_lowercase(),
            draft: self.is_draft,
            url: self.url,
            updated_at: self.updated_at,
            merged_at: self.merged_at,
        };
        if !result.valid() {
            return Err(crate::github::GitHubError::InvalidPullRequestContext);
        }
        Ok(result)
    }
}

impl PullRequest {
    fn valid(&self) -> bool {
        let Ok(reference) = IssueReference::parse(&self.key) else {
            return false;
        };
        let Ok(url) = url::Url::parse(&self.url) else {
            return false;
        };
        let expected_path = format!(
            "/{}/pull/{}",
            reference.repository().full_name(),
            self.number
        );
        self.number > 0
            && reference.number() == self.number
            && url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path().eq_ignore_ascii_case(&expected_path)
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(self.state.as_str(), "open" | "closed" | "merged")
            && DateTime::parse_from_rfc3339(&self.updated_at).is_ok()
            && self
                .merged_at
                .as_ref()
                .is_none_or(|time| DateTime::parse_from_rfc3339(time).is_ok())
            && (self.state == "merged") == self.merged_at.is_some()
    }
}

#[derive(Deserialize, Serialize)]
struct Cache {
    schema_version: String,
    issue: String,
    observed_at: String,
    pull_requests: Vec<PullRequest>,
}

#[derive(Serialize)]
pub(crate) struct Context {
    pub(crate) source: &'static str,
    pub(crate) complete: bool,
    pub(crate) observed_at: Option<String>,
    pub(crate) pull_requests: Option<Vec<PullRequest>>,
    pub(crate) warning: Option<Failure>,
}

impl Context {
    pub(crate) fn draft() -> Self {
        Self {
            source: "draft",
            complete: true,
            observed_at: None,
            pull_requests: Some(Vec::new()),
            warning: None,
        }
    }

    pub(crate) fn print(&self) {
        println!(
            "Linked PR context: {} (observed_at {})",
            self.source,
            self.observed_at.as_deref().unwrap_or("not fetched")
        );
        if let Some(prs) = &self.pull_requests {
            if prs.is_empty() {
                println!("No linked closing PRs in this observation.");
            }
            for pr in prs {
                println!(
                    "{} {} [{}{}] {}",
                    pr.key,
                    pr.title,
                    pr.state,
                    if pr.draft { ", draft" } else { "" },
                    pr.url
                );
            }
        } else {
            println!("Linked PRs have not been fetched; their presence is unknown.");
        }
        if let Some(warning) = &self.warning {
            eprintln!("warning: {}", warning.message);
        }
    }
}

pub(crate) fn read(
    issue: &IssueReference,
    refresh: Option<Result<&GitHubClient, Failure>>,
) -> Result<Context, ContextError> {
    let path = store::repository_state_directory(issue.repository())?
        .join("pull-requests")
        .join(format!("{}.json", issue.number()));
    let mut warning = None;
    if let Some(client) = refresh {
        let fetched = match client {
            Ok(client) => client
                .fetch_linked_pull_requests(issue)
                .map_err(|error| Failure::github(&error)),
            Err(error) => Err(error),
        };
        match fetched {
            Ok(pull_requests) => {
                let cache = Cache {
                    schema_version: SCHEMA.to_owned(),
                    issue: issue.stable_key(),
                    observed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                    pull_requests,
                };
                atomic_file::publish(&path, "pull-requests", &serde_json::to_vec(&cache)?)?;
                return Ok(context(cache, "live", None));
            }
            Err(error) => warning = Some(error),
        }
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Context {
                source: "unavailable",
                complete: false,
                observed_at: None,
                pull_requests: None,
                warning,
            });
        }
        Err(error) => return Err(error.into()),
    };
    let cache: Cache = serde_json::from_slice(&bytes)?;
    if cache.schema_version != SCHEMA
        || !cache.issue.eq_ignore_ascii_case(&issue.stable_key())
        || DateTime::parse_from_rfc3339(&cache.observed_at).is_err()
        || cache.pull_requests.iter().any(|pr| !pr.valid())
        || cache
            .pull_requests
            .windows(2)
            .any(|pair| pair[0].key >= pair[1].key)
    {
        return Err(ContextError::InvalidCache);
    }
    let source = if warning.is_some() {
        "local_fallback"
    } else {
        "local"
    };
    Ok(context(cache, source, warning))
}

fn context(cache: Cache, source: &'static str, warning: Option<Failure>) -> Context {
    Context {
        source,
        complete: true,
        observed_at: Some(cache.observed_at),
        pull_requests: Some(cache.pull_requests),
        warning,
    }
}

#[derive(Debug, Error)]
pub(crate) enum ContextError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("could not read linked PR context: {0}")]
    Read(#[from] io::Error),
    #[error("could not encode or decode linked PR context: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not publish linked PR context: {0}")]
    Publish(#[from] AtomicFileError),
    #[error("invalid linked PR context cache")]
    InvalidCache,
}
