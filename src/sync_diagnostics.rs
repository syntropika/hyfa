use std::{cell::RefCell, fs, io, path::PathBuf, rc::Rc};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{atomic_file, github::GitHubError, model::LocalReplica, repository::Repository, store};

const SCHEMA: &str = "hyfa.sync-attempt/v1";

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Counts {
    issue_count: usize,
    comment_count: usize,
    dependency_count: usize,
}

impl Counts {
    pub(crate) fn from_replica(replica: &LocalReplica) -> Self {
        Self {
            issue_count: replica.issues.len(),
            comment_count: replica
                .issues
                .iter()
                .map(|issue| issue.comments.len())
                .sum(),
            dependency_count: replica.dependencies.len(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Failure {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl Failure {
    pub(crate) fn github(error: &GitHubError) -> Self {
        let (code, message) = match error {
            GitHubError::RateLimited { .. } => ("rate_limited", "GitHub rate limit reached"),
            GitHubError::Authentication(_) | GitHubError::InvalidToken => {
                ("authentication", "GitHub authentication failed")
            }
            GitHubError::Request(_) | GitHubError::BuildClient(_) => {
                ("connection", "Could not reach GitHub")
            }
            GitHubError::Status(_) => ("github_status", "GitHub returned an unsuccessful response"),
            _ => (
                "invalid_response",
                "GitHub returned invalid or incomplete synchronization data",
            ),
        };
        Self::new(code, message)
    }

    pub(crate) fn persistence() -> Self {
        Self::new(
            "persistence",
            "Could not read or publish local synchronization state",
        )
    }

    pub(crate) fn synchronization(error: &crate::replica_sync::ReplicaSyncError) -> Self {
        use crate::replica_sync::ReplicaSyncError;
        match error {
            ReplicaSyncError::GitHub(error) => Self::github(error),
            ReplicaSyncError::Store(_) => Self::persistence(),
            ReplicaSyncError::Replica(_) => Self::new(
                "invalid_replica",
                "The synchronization candidate did not pass validation",
            ),
        }
    }

    pub(crate) fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stage {
    Connecting,
    Refreshing,
    Validating,
    Publishing,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Attempt {
    schema_version: String,
    repository: String,
    started_at: String,
    observed_at: String,
    finished_at: Option<String>,
    state: State,
    stage: Stage,
    pages_received: usize,
    items_received: usize,
    candidate_counts: Option<Counts>,
    published_synced_at: Option<String>,
    pub(crate) failure: Option<Failure>,
}

impl Attempt {
    pub(crate) fn print(&self) {
        let state = match self.state {
            State::Running => "running (last reported)",
            State::Succeeded => "succeeded",
            State::Failed => "failed",
        };
        let stage = match self.stage {
            Stage::Connecting => "connecting",
            Stage::Refreshing => "refreshing",
            Stage::Validating => "validating",
            Stage::Publishing => "publishing",
        };
        println!(
            "Last synchronization: {} at {} (stage {}, {} pages, {} received items)",
            state, self.observed_at, stage, self.pages_received, self.items_received
        );
        if let Some(failure) = &self.failure {
            println!("Failure: {} ({})", failure.message, failure.code);
        }
    }
}

#[derive(Clone)]
pub(crate) struct Journal {
    path: PathBuf,
    attempt: Rc<RefCell<Attempt>>,
    write_failed: Rc<RefCell<bool>>,
}

impl Journal {
    pub(crate) fn begin(repository: &Repository, stage: Stage) -> Result<Self, store::StoreError> {
        let started_at = now();
        let journal = Self {
            path: path(repository)?,
            attempt: Rc::new(RefCell::new(Attempt {
                schema_version: SCHEMA.to_owned(),
                repository: repository.full_name().to_owned(),
                started_at: started_at.clone(),
                observed_at: started_at,
                finished_at: None,
                state: State::Running,
                stage,
                pages_received: 0,
                items_received: 0,
                candidate_counts: None,
                published_synced_at: None,
                failure: None,
            })),
            write_failed: Rc::new(RefCell::new(false)),
        };
        journal.save();
        Ok(journal)
    }

    pub(crate) fn page(&self, items: usize) {
        {
            let mut attempt = self.attempt.borrow_mut();
            attempt.pages_received += 1;
            attempt.items_received += items;
        }
        self.save();
    }

    pub(crate) fn stage(&self, stage: Stage, replica: Option<&LocalReplica>) {
        {
            let mut attempt = self.attempt.borrow_mut();
            attempt.stage = stage;
            if let Some(replica) = replica {
                attempt.candidate_counts = Some(Counts::from_replica(replica));
            }
        }
        self.save();
    }

    pub(crate) fn finish(&self, result: Result<&LocalReplica, Failure>) {
        {
            let mut attempt = self.attempt.borrow_mut();
            attempt.finished_at = Some(now());
            match result {
                Ok(replica) => {
                    attempt.state = State::Succeeded;
                    attempt.published_synced_at = Some(replica.synced_at.clone());
                }
                Err(failure) => {
                    attempt.state = State::Failed;
                    attempt.failure = Some(failure);
                }
            }
        }
        self.save();
    }

    fn save(&self) {
        if *self.write_failed.borrow() {
            return;
        }
        let mut attempt = self.attempt.borrow_mut();
        attempt.observed_at = now();
        let bytes =
            serde_json::to_vec(&*attempt).expect("synchronization diagnostics are serializable");
        if atomic_file::publish(&self.path, "sync-status", &bytes).is_err() {
            *self.write_failed.borrow_mut() = true;
            eprintln!("warning: could not persist synchronization diagnostics");
        }
    }
}

pub(crate) fn load(repository: &Repository) -> Result<Option<Attempt>, DiagnosticsError> {
    let bytes = match fs::read(path(repository)?) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let attempt: Attempt = serde_json::from_slice(&bytes)?;
    if attempt.schema_version != SCHEMA
        || !attempt
            .repository
            .eq_ignore_ascii_case(repository.full_name())
    {
        return Err(DiagnosticsError::Invalid);
    }
    Ok(Some(attempt))
}

fn path(repository: &Repository) -> Result<PathBuf, store::StoreError> {
    Ok(store::repository_state_directory(repository)?.join("sync-status.json"))
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Debug, Error)]
pub(crate) enum DiagnosticsError {
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error("could not read synchronization diagnostics: {0}")]
    Read(#[from] io::Error),
    #[error("could not decode synchronization diagnostics: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("invalid synchronization diagnostics schema or repository")]
    Invalid,
}
