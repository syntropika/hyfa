use thiserror::Error;

use crate::{
    github::{GitHubClient, GitHubError},
    model::LocalReplica,
    outbox::{OutboxError, OutboxStore, PendingMutation},
    priority::{DeclaredPriority, LogicalPriority, PrioritySelection, PriorityState},
    replica_sync::{self, ReplicaSyncError},
    repository::{IssueReference, PendingIssueReference},
    store::{ReplicaStore, StoreError},
    working_graph::{WorkingGraph, WorkingGraphError},
};

pub(crate) struct PriorityUpdateResult {
    pub(crate) issue_key: String,
    pub(crate) issue_number: u64,
    pub(crate) issue_url: String,
    pub(crate) previous_priority: PriorityState,
    pub(crate) resulting_priority: PriorityState,
    pub(crate) replica: LocalReplica,
}

pub(crate) struct PendingPriorityUpdateResult {
    pub(crate) issue_key: String,
    pub(crate) issue_number: Option<u64>,
    pub(crate) temporary_id: Option<crate::model::TemporaryIssueId>,
    pub(crate) issue_url: String,
    pub(crate) previous_priority: PriorityState,
    pub(crate) resulting_priority: PriorityState,
    pub(crate) operation: PendingMutation,
    pub(crate) working_input_hash: String,
    pub(crate) replica: LocalReplica,
}

pub(crate) fn update(
    client: &GitHubClient,
    issue: &IssueReference,
    selection: PrioritySelection,
) -> Result<PriorityUpdateResult, PriorityUpdateError> {
    let before = client.fetch_issue_for_update(issue.repository(), issue.number())?;
    let previous_priority = PriorityState::from_issue_labels(&before.labels);
    let desired = selection.desired();
    let mut remote_changed = false;

    if let Some(desired) = desired
        && !has_priority(&before.labels, desired)
    {
        record_mutation(
            client.add_issue_label(
                issue.repository(),
                issue.number(),
                desired.canonical_label(),
            ),
            &mut remote_changed,
        )?;
    }
    for obsolete in DeclaredPriority::ALL {
        if Some(obsolete) == desired || !has_priority(&before.labels, obsolete) {
            continue;
        }
        record_mutation(
            client.remove_issue_label(
                issue.repository(),
                issue.number(),
                obsolete.canonical_label(),
            ),
            &mut remote_changed,
        )?;
    }

    let replica = replica_sync::fetch(client, issue.repository()).map_err(|source| {
        if remote_changed {
            PriorityUpdateError::SynchronizationAfterMutation { source }
        } else {
            PriorityUpdateError::Synchronization { source }
        }
    })?;
    let readback = replica
        .issues
        .iter()
        .find(|candidate| candidate.number == issue.number())
        .ok_or_else(|| PriorityUpdateError::ReadbackMissing(issue.stable_key()))?;
    let resulting_priority = PriorityState::from_issue_labels(&readback.labels);
    if !resulting_priority.matches(desired) {
        return Err(PriorityUpdateError::ReadbackMismatch {
            issue: issue.stable_key(),
            expected: selection.logical_name(),
            actual: resulting_priority.display_name(),
        });
    }
    let missing_unrelated_labels: Vec<_> = before
        .labels
        .iter()
        .filter(|label| DeclaredPriority::parse(&label.name).is_none())
        .filter(|label| {
            !readback
                .labels
                .iter()
                .any(|candidate| candidate.name.eq_ignore_ascii_case(&label.name))
        })
        .map(|label| label.name.clone())
        .collect();
    if !missing_unrelated_labels.is_empty() {
        return Err(PriorityUpdateError::LabelsNotPreserved {
            issue: issue.stable_key(),
            labels: missing_unrelated_labels.join(", "),
        });
    }

    let issue_url = readback.url.clone();
    let issue_number = readback.number;
    let replica = replica
        .publish()
        .map_err(|source| publication_error(remote_changed, source))?;

    Ok(PriorityUpdateResult {
        issue_key: issue.stable_key(),
        issue_number,
        issue_url,
        previous_priority,
        resulting_priority,
        replica,
    })
}

pub(crate) fn queue(
    issue: &PendingIssueReference,
    selection: PrioritySelection,
) -> Result<PendingPriorityUpdateResult, PendingPriorityUpdateError> {
    let outbox_store = OutboxStore::discover(issue.repository())?;
    let transaction = outbox_store.begin_transaction(issue.repository())?;
    let resolved = crate::draft_identity::resolve_reference(issue)?;
    let number = resolved.local_number();
    let replica = ReplicaStore::discover(issue.repository())?.load(issue.repository())?;
    let current_working = WorkingGraph::project(&replica, transaction.outbox())?;
    let local_issue = current_working
        .replica()
        .issues
        .iter()
        .find(|candidate| candidate.number == number)
        .ok_or_else(|| PendingPriorityUpdateError::MissingIssue(issue.stable_key()))?;
    let previous_priority = current_working.priority(local_issue);
    let desired = LogicalPriority::from_selection(selection);
    let mut depends_on = transaction
        .outbox()
        .latest_priority_operation_for_issue(number)
        .map(|operation| vec![operation.to_owned()])
        .unwrap_or_default();
    if let Some(temporary_id) = resolved.temporary_id() {
        let create = transaction
            .outbox()
            .operations()
            .iter()
            .find(|operation| {
                operation
                    .issue_create_view()
                    .is_some_and(|create| create.temporary_id == temporary_id)
            })
            .ok_or_else(|| PendingPriorityUpdateError::MissingIssue(issue.stable_key()))?;
        depends_on.push(create.id().to_owned());
    }
    depends_on.sort();
    depends_on.dedup();
    let operation = PendingMutation::priority_update(
        issue.repository(),
        number,
        LogicalPriority::from_state(&previous_priority),
        desired.clone(),
        depends_on,
    )
    .with_priority_temporary_id(issue.temporary_id());
    let issue_number = (!local_issue.is_draft()).then_some(local_issue.number);
    let issue_url = local_issue.url.clone();
    drop(current_working);
    let next_outbox = transaction.append(issue.repository(), operation.clone())?;
    let next_working = WorkingGraph::project(&replica, &next_outbox)?;

    Ok(PendingPriorityUpdateResult {
        issue_key: issue.stable_key(),
        issue_number,
        temporary_id: issue.temporary_id(),
        issue_url,
        previous_priority,
        resulting_priority: desired.to_state(),
        operation,
        working_input_hash: next_working.input_hash().to_owned(),
        replica,
    })
}

fn has_priority(labels: &[crate::model::Label], priority: DeclaredPriority) -> bool {
    labels
        .iter()
        .any(|label| label.name.eq_ignore_ascii_case(priority.canonical_label()))
}

fn record_mutation(
    result: Result<(), GitHubError>,
    remote_changed: &mut bool,
) -> Result<(), PriorityUpdateError> {
    match result {
        Ok(()) => {
            *remote_changed = true;
            Ok(())
        }
        Err(source) if *remote_changed => Err(PriorityUpdateError::PartiallyApplied { source }),
        Err(source) => Err(PriorityUpdateError::GitHub(source)),
    }
}

fn publication_error(remote_changed: bool, source: StoreError) -> PriorityUpdateError {
    if remote_changed {
        PriorityUpdateError::PublicationAfterMutation { source }
    } else {
        PriorityUpdateError::Publication { source }
    }
}

#[derive(Debug, Error)]
pub(crate) enum PriorityUpdateError {
    #[error(transparent)]
    GitHub(#[from] GitHubError),
    #[error(
        "GitHub accepted part of the Priority update before a later write failed; the remote Priority may be provisional and the Local replica was not changed: {source}"
    )]
    PartiallyApplied { source: GitHubError },
    #[error("Priority synchronization failed: {source}")]
    Synchronization { source: ReplicaSyncError },
    #[error(
        "GitHub accepted the Priority update, but synchronized readback failed; the Local replica was not changed: {source}"
    )]
    SynchronizationAfterMutation { source: ReplicaSyncError },
    #[error("Priority readback did not include {0}; Local replica was not changed")]
    ReadbackMissing(String),
    #[error(
        "Priority readback did not match for {issue}: expected {expected}, found {actual}; Local replica was not changed"
    )]
    ReadbackMismatch {
        issue: String,
        expected: &'static str,
        actual: &'static str,
    },
    #[error(
        "Priority update for {issue} did not preserve unrelated labels ({labels}); Local replica was not changed"
    )]
    LabelsNotPreserved { issue: String, labels: String },
    #[error("verified Priority could not be published to the Local replica: {source}")]
    Publication { source: StoreError },
    #[error(
        "GitHub accepted the Priority update, but the Local replica could not be published: {source}"
    )]
    PublicationAfterMutation { source: StoreError },
}

impl PriorityUpdateError {
    pub(crate) fn permits_offline_queue(&self) -> bool {
        match self {
            Self::GitHub(source) | Self::PartiallyApplied { source } => {
                source.permits_offline_queue()
            }
            Self::Synchronization { .. }
            | Self::SynchronizationAfterMutation { .. }
            | Self::ReadbackMissing(_)
            | Self::ReadbackMismatch { .. }
            | Self::LabelsNotPreserved { .. }
            | Self::Publication { .. }
            | Self::PublicationAfterMutation { .. } => false,
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum PendingPriorityUpdateError {
    #[error(transparent)]
    DraftIdentity(#[from] crate::draft_identity::DraftIdentityError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Outbox(#[from] OutboxError),
    #[error(transparent)]
    WorkingGraph(#[from] WorkingGraphError),
    #[error("cannot queue a Priority update because {0} is absent from the Local replica")]
    MissingIssue(String),
}
