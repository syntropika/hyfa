use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    draft_identity::{DraftIdentityError, DraftIdentityStore},
    github::{GitHubClient, GitHubError},
    model::{Actor, Issue, LocalReplica, TemporaryIssueId},
    outbox::{OutboxError, OutboxStore, OutboxTransaction, PendingMutation},
    replica_sync::{self, ReplicaSyncError},
    repository::{IssueReference, PendingIssueReference},
    store::{ReplicaStore, StoreError},
    working_graph::{WorkingGraph, WorkingGraphError},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IssueField {
    Title,
    Body,
    State,
    Assignees,
}

impl IssueField {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Body => "body",
            Self::State => "state",
            Self::Assignees => "assignees",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum IssueFieldValue {
    Title { value: String },
    Body { value: String },
    State { value: IssueStateValue },
    Assignees { logins: Vec<String> },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IssueStateValue {
    Open,
    Closed,
}

impl IssueStateValue {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

impl IssueFieldValue {
    pub(crate) fn title(value: impl Into<String>) -> Self {
        Self::Title {
            value: value.into(),
        }
    }

    pub(crate) fn body(value: impl Into<String>) -> Self {
        Self::Body {
            value: value.into(),
        }
    }

    pub(crate) fn state(value: IssueStateValue) -> Self {
        Self::State { value }
    }

    pub(crate) fn assignees(logins: impl IntoIterator<Item = String>) -> Self {
        let mut logins: Vec<_> = logins
            .into_iter()
            .map(|login| login.to_ascii_lowercase())
            .collect();
        logins.sort();
        logins.dedup();
        Self::Assignees { logins }
    }

    pub(crate) fn from_issue(field: IssueField, issue: &Issue) -> Result<Self, IssueFieldError> {
        match field {
            IssueField::Title => Ok(Self::title(issue.title.clone())),
            IssueField::Body => Ok(Self::body(issue.body.clone())),
            IssueField::State => match issue.state.to_ascii_lowercase().as_str() {
                "open" => Ok(Self::state(IssueStateValue::Open)),
                "closed" => Ok(Self::state(IssueStateValue::Closed)),
                state => Err(IssueFieldError::UnknownIssueState(state.to_owned())),
            },
            IssueField::Assignees => Ok(Self::assignees(
                issue.assignees.iter().map(|actor| actor.login.clone()),
            )),
        }
    }

    pub(crate) fn matches_field(&self, field: IssueField) -> bool {
        self.field() == field
    }

    pub(crate) fn is_canonical(&self) -> bool {
        match self {
            Self::Title { value } => !value.trim().is_empty(),
            Self::Body { .. } | Self::State { .. } => true,
            Self::Assignees { logins } => {
                logins.iter().all(|login| {
                    valid_login(login) && login.bytes().all(|byte| !byte.is_ascii_uppercase())
                }) && logins.windows(2).all(|pair| pair[0] < pair[1])
            }
        }
    }

    pub(crate) fn field(&self) -> IssueField {
        match self {
            Self::Title { .. } => IssueField::Title,
            Self::Body { .. } => IssueField::Body,
            Self::State { .. } => IssueField::State,
            Self::Assignees { .. } => IssueField::Assignees,
        }
    }

    pub(crate) fn apply_to(&self, issue: &mut Issue) {
        match self {
            Self::Title { value } => issue.title.clone_from(value),
            Self::Body { value } => issue.body.clone_from(value),
            Self::State { value } => {
                issue.state = value.as_str().to_owned();
                if *value == IssueStateValue::Open {
                    issue.closed_at = None;
                    issue.state_reason = None;
                }
            }
            Self::Assignees { logins } => {
                issue.assignees = logins
                    .iter()
                    .map(|login| Actor {
                        id: 0,
                        node_id: format!("pending:{login}"),
                        login: login.clone(),
                    })
                    .collect();
            }
        }
    }
}

pub(crate) struct IssueFieldUpdateResult {
    pub(crate) issue_key: String,
    pub(crate) issue_number: u64,
    pub(crate) temporary_id: Option<TemporaryIssueId>,
    pub(crate) field: IssueField,
    pub(crate) base: IssueFieldValue,
    pub(crate) desired: IssueFieldValue,
    pub(crate) outcome: IssueFieldUpdateOutcome,
    pub(crate) replica: LocalReplica,
}

pub(crate) enum IssueFieldUpdateOutcome {
    Synchronized {
        issue_url: String,
    },
    Queued {
        issue_url: Option<String>,
        operation: Box<PendingMutation>,
        working_input_hash: String,
    },
}

pub(crate) fn update_or_queue(
    client: Option<&GitHubClient>,
    reference: &PendingIssueReference,
    field: IssueField,
    desired: IssueFieldValue,
) -> Result<IssueFieldUpdateResult, IssueFieldUpdateError> {
    validate_desired(field, &desired)?;
    let repository = reference.repository();
    let outbox_store = OutboxStore::discover(repository)?;
    let transaction = outbox_store.begin_transaction(repository)?;
    let (issue_number, create_dependency) = match reference.temporary_id() {
        None => (reference.local_number(), None),
        Some(temporary_id) => {
            let identity_store = DraftIdentityStore::discover(repository)?;
            let identity_transaction = identity_store.begin_transaction(repository)?;
            match identity_transaction.resolve(temporary_id) {
                Some(identity) => (identity.issue_number, None),
                None => (
                    temporary_id.synthetic_number(),
                    draft_create_operation(transaction.outbox(), temporary_id).map(str::to_owned),
                ),
            }
        }
    };

    let previous = transaction
        .outbox()
        .latest_field_operation_for_issue(issue_number, field)
        .map(str::to_owned);
    let online_issue = (previous.is_none() && create_dependency.is_none())
        .then(|| reference.github_reference(issue_number));
    if let (Some(client), Some(issue)) = (client, online_issue.as_ref()) {
        match update_online(client, issue, field, desired.clone()) {
            Ok(mut result) => {
                if let Some(temporary_id) = reference.temporary_id() {
                    result.issue_key = reference.stable_key();
                    result.temporary_id = Some(temporary_id);
                }
                return Ok(result);
            }
            Err(source) if source.permits_offline_queue() => {
                return queue_locked(QueueContext {
                    reference,
                    field,
                    desired,
                    observed_base: source.observed_base().cloned(),
                    issue_number,
                    create_dependency,
                    previous,
                    transaction,
                });
            }
            Err(source) => return Err(source),
        }
    }

    queue_locked(QueueContext {
        reference,
        field,
        desired,
        observed_base: None,
        issue_number,
        create_dependency,
        previous,
        transaction,
    })
}

fn update_online(
    client: &GitHubClient,
    issue: &IssueReference,
    field: IssueField,
    desired: IssueFieldValue,
) -> Result<IssueFieldUpdateResult, IssueFieldUpdateError> {
    let before = client.fetch_issue_for_update(issue.repository(), issue.number())?;
    let base = IssueFieldValue::from_issue(field, &before)?;
    let mut remote_changed = false;
    if base != desired {
        client
            .patch_issue_field(issue.repository(), issue.number(), field, &desired)
            .map_err(|source| IssueFieldUpdateError::Mutation {
                base: base.clone(),
                source,
            })?;
        remote_changed = true;
    }
    let replica = replica_sync::fetch(client, issue.repository()).map_err(|source| {
        if remote_changed {
            IssueFieldUpdateError::SynchronizationAfterMutation(source)
        } else {
            IssueFieldUpdateError::Synchronization(source)
        }
    })?;
    let readback = replica
        .issues
        .iter()
        .find(|candidate| candidate.number == issue.number())
        .ok_or_else(|| IssueFieldUpdateError::ReadbackMissing(issue.stable_key()))?;
    let actual = IssueFieldValue::from_issue(field, readback)?;
    if actual != desired {
        return Err(IssueFieldUpdateError::ReadbackMismatch {
            issue: issue.stable_key(),
            field,
            expected: desired,
            actual,
        });
    }
    let issue_url = readback.url.clone();
    let replica = replica.publish().map_err(|source| {
        if remote_changed {
            IssueFieldUpdateError::PublicationAfterMutation(source)
        } else {
            IssueFieldUpdateError::Store(source)
        }
    })?;
    Ok(IssueFieldUpdateResult {
        issue_key: issue.stable_key(),
        issue_number: issue.number(),
        temporary_id: None,
        field,
        base,
        desired,
        outcome: IssueFieldUpdateOutcome::Synchronized { issue_url },
        replica,
    })
}

struct QueueContext<'reference, 'transaction> {
    reference: &'reference PendingIssueReference,
    field: IssueField,
    desired: IssueFieldValue,
    observed_base: Option<IssueFieldValue>,
    issue_number: u64,
    create_dependency: Option<String>,
    previous: Option<String>,
    transaction: OutboxTransaction<'transaction>,
}

fn queue_locked(
    context: QueueContext<'_, '_>,
) -> Result<IssueFieldUpdateResult, IssueFieldUpdateError> {
    let QueueContext {
        reference,
        field,
        desired,
        observed_base,
        issue_number,
        create_dependency,
        previous,
        transaction,
    } = context;
    if observed_base
        .as_ref()
        .is_some_and(|base| !base.matches_field(field) || !base.is_canonical())
    {
        return Err(IssueFieldError::MismatchedValue.into());
    }
    let repository = reference.repository();
    let replica = ReplicaStore::discover(repository)?.load(repository)?;
    let working = WorkingGraph::project(&replica, transaction.outbox())?;
    let issue = working
        .replica()
        .issues
        .iter()
        .find(|issue| issue.number == issue_number)
        .ok_or_else(|| IssueFieldUpdateError::MissingIssue(reference.stable_key()))?;
    let base = match observed_base {
        Some(base) => base,
        None => IssueFieldValue::from_issue(field, issue)?,
    };
    let issue_url = issue.url.clone();
    let mut depends_on: Vec<_> = create_dependency.into_iter().collect();
    if let Some(previous) = previous {
        depends_on.push(previous);
    }
    depends_on.sort();
    depends_on.dedup();
    let operation = PendingMutation::issue_field_update(
        repository,
        issue_number,
        reference.temporary_id(),
        field,
        base.clone(),
        desired.clone(),
        depends_on,
    );
    let next_outbox = transaction.append(repository, operation.clone())?;
    let next_working = WorkingGraph::project(&replica, &next_outbox)?;
    Ok(IssueFieldUpdateResult {
        issue_key: reference.stable_key(),
        issue_number,
        temporary_id: reference.temporary_id(),
        field,
        base,
        desired,
        outcome: IssueFieldUpdateOutcome::Queued {
            issue_url: (!issue_url.is_empty()).then_some(issue_url),
            operation: Box::new(operation),
            working_input_hash: next_working.input_hash().to_owned(),
        },
        replica,
    })
}

fn draft_create_operation(
    outbox: &crate::outbox::PendingMutationOutbox,
    temporary_id: TemporaryIssueId,
) -> Option<&str> {
    outbox.operations().iter().find_map(|operation| {
        operation
            .issue_create_view()
            .filter(|create| create.temporary_id == temporary_id)
            .map(|_| operation.id())
    })
}

fn validate_desired(field: IssueField, desired: &IssueFieldValue) -> Result<(), IssueFieldError> {
    if !desired.matches_field(field) {
        return Err(IssueFieldError::MismatchedValue);
    }
    if field == IssueField::Title
        && matches!(desired, IssueFieldValue::Title { value } if value.trim().is_empty())
    {
        return Err(IssueFieldError::EmptyTitle);
    }
    if let IssueFieldValue::Assignees { logins } = desired
        && logins.iter().any(|login| !valid_login(login))
    {
        return Err(IssueFieldError::InvalidAssignee);
    }
    Ok(())
}

fn valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && !login.starts_with('-')
        && !login.ends_with('-')
        && login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[derive(Debug, Error)]
pub(crate) enum IssueFieldError {
    #[error("Issue field value does not match its field")]
    MismatchedValue,
    #[error("Issue title cannot be empty")]
    EmptyTitle,
    #[error("assignee must be a valid GitHub login")]
    InvalidAssignee,
    #[error("GitHub returned unsupported Issue state {0:?}")]
    UnknownIssueState(String),
}

#[derive(Debug, Error)]
pub(crate) enum IssueFieldUpdateError {
    #[error(transparent)]
    Field(#[from] IssueFieldError),
    #[error(transparent)]
    GitHub(#[from] GitHubError),
    #[error("GitHub Issue-field mutation failed after observing base {base:?}: {source}")]
    Mutation {
        base: IssueFieldValue,
        #[source]
        source: GitHubError,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Outbox(#[from] OutboxError),
    #[error(transparent)]
    DraftIdentity(#[from] DraftIdentityError),
    #[error(transparent)]
    WorkingGraph(#[from] WorkingGraphError),
    #[error("Issue-field synchronization failed: {0}")]
    Synchronization(ReplicaSyncError),
    #[error(
        "GitHub accepted the Issue-field update, but synchronized readback failed; the Local replica was not changed: {0}"
    )]
    SynchronizationAfterMutation(ReplicaSyncError),
    #[error("Issue-field readback did not include {0}; Local replica was not changed")]
    ReadbackMissing(String),
    #[error(
        "Issue-field readback for {issue} did not match {field:?}: expected {expected:?}, found {actual:?}; Local replica was not changed"
    )]
    ReadbackMismatch {
        issue: String,
        field: IssueField,
        expected: IssueFieldValue,
        actual: IssueFieldValue,
    },
    #[error(
        "GitHub accepted the Issue-field update, but the Local replica could not be published: {0}"
    )]
    PublicationAfterMutation(StoreError),
    #[error("cannot queue an Issue-field update because {0} is absent from the Working graph")]
    MissingIssue(String),
}

impl IssueFieldUpdateError {
    pub(crate) fn permits_offline_queue(&self) -> bool {
        matches!(self, Self::GitHub(error) if error.permits_offline_queue())
            || matches!(self, Self::Mutation { source, .. } if source.permits_offline_queue())
    }

    pub(crate) fn observed_base(&self) -> Option<&IssueFieldValue> {
        match self {
            Self::Mutation { base, .. } => Some(base),
            _ => None,
        }
    }
}
