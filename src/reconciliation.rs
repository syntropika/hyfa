use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use thiserror::Error;

mod comment_create;
mod dependency;
mod field;
mod issue_create;
mod metadata_set;
mod priority;

use crate::{
    draft_identity::{
        DraftIdentity, DraftIdentityError, DraftIdentityStore, DraftIdentityTransaction,
    },
    github::{DependencyIntent, GitHubClient, GitHubError},
    issue_field::{IssueField, IssueFieldValue},
    model::{
        CommentIdentity, DependencyEdgeKey, DependencyPresence, Issue, LocalReplica, SetPresence,
    },
    outbox::{
        CommentCreateState, DependencyMutationState, IssueCreateState, IssueFieldMutationState,
        MutationKind, MutationStateUpdate, OutboxError, OutboxStore, PendingMutation,
        PriorityMutationState, PriorityWrite,
    },
    priority::{DeclaredPriority, LogicalPriority, PrioritySelection, PriorityState},
    replica_sync::{self, ReplicaSyncError},
    repository::{IssueReference, Repository},
    store::StoreError,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScalarClassification {
    Applicable,
    AlreadySatisfied,
    Conflicting,
}

fn classify_scalar<T: Eq>(base: &T, desired: &T, remote: &T) -> ScalarClassification {
    if remote == desired {
        ScalarClassification::AlreadySatisfied
    } else if remote == base {
        ScalarClassification::Applicable
    } else {
        ScalarClassification::Conflicting
    }
}

pub(crate) const OUTPUT_SCHEMA_VERSION: &str = "hyfa.reconcile/v1";

#[derive(Clone, Copy)]
pub(crate) enum ResolutionChoice {
    Remote,
    Local,
    Replacement(PrioritySelection),
}

pub(crate) struct ResolutionResult {
    pub(crate) operation_id: String,
    pub(crate) choice: &'static str,
    pub(crate) reconciliation: ReconciliationResult,
}

#[derive(Serialize)]
pub(crate) struct ReconciliationResult {
    pub(crate) repository: String,
    pub(crate) operations: Vec<OperationResult>,
    pub(crate) summary: ReconciliationSummary,
    #[serde(skip)]
    pub(crate) replica: LocalReplica,
}

#[derive(Clone, Serialize)]
pub(crate) struct OperationResult {
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) issue_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temporary_id: Option<crate::model::TemporaryIssueId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) depends_on: Vec<String>,
    pub(crate) classification: Classification,
    pub(crate) outcome: Outcome,
    #[serde(flatten)]
    pub(crate) details: OperationDetails,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) blocked_by: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum OperationDetails {
    IssueCreate {
        #[serde(skip_serializing_if = "Option::is_none")]
        remote: Option<DraftIssueResult>,
    },
    CommentCreate {
        #[serde(skip_serializing_if = "Option::is_none")]
        remote: Option<CommentIdentity>,
    },
    PriorityUpdate {
        base: LogicalPriority,
        local: LogicalPriority,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote: Option<LogicalPriority>,
    },
    DependencyUpdate {
        edge: DependencyEdgeResult,
        desired_present: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote_present: Option<bool>,
    },
    IssueFieldUpdate {
        field: IssueField,
        base: IssueFieldValue,
        local: IssueFieldValue,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote: Option<IssueFieldValue>,
    },
    MetadataSetUpdate {
        target: crate::metadata::MetadataSetTarget,
        desired_present: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote_present: Option<bool>,
    },
}

#[derive(Clone, Serialize)]
pub(crate) struct DraftIssueResult {
    pub(crate) issue_id: u64,
    pub(crate) issue_node_id: String,
    pub(crate) issue_number: u64,
    pub(crate) issue_url: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct DependencyEdgeResult {
    pub(crate) blocked_number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blocked_temporary_id: Option<crate::model::TemporaryIssueId>,
    pub(crate) blocker_repository: String,
    pub(crate) blocker_number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blocker_temporary_id: Option<crate::model::TemporaryIssueId>,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Classification {
    Applicable,
    AlreadySatisfied,
    Conflicting,
    TransitivelyBlocked,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Applied,
    AlreadySatisfied,
    Conflicting,
    TransitivelyBlocked,
    Checkpointed,
    Failed,
    ResolvedRemote,
}

#[derive(Default, Serialize)]
pub(crate) struct ReconciliationSummary {
    pub(crate) applicable: usize,
    pub(crate) already_satisfied: usize,
    pub(crate) conflicting: usize,
    pub(crate) transitively_blocked: usize,
    pub(crate) applied: usize,
    pub(crate) checkpointed: usize,
    pub(crate) failed: usize,
    pub(crate) remaining: usize,
}

impl ReconciliationSummary {
    fn from_operations(operations: &[OperationResult], remaining: usize) -> Self {
        let mut summary = Self {
            remaining,
            ..Self::default()
        };
        for operation in operations {
            match operation.classification {
                Classification::Applicable => summary.applicable += 1,
                Classification::AlreadySatisfied => summary.already_satisfied += 1,
                Classification::Conflicting => summary.conflicting += 1,
                Classification::TransitivelyBlocked => summary.transitively_blocked += 1,
            }
            match operation.outcome {
                Outcome::Applied => summary.applied += 1,
                Outcome::Checkpointed => summary.checkpointed += 1,
                Outcome::Failed => summary.failed += 1,
                Outcome::AlreadySatisfied
                | Outcome::Conflicting
                | Outcome::TransitivelyBlocked
                | Outcome::ResolvedRemote => {}
            }
        }
        summary
    }
}

#[derive(Clone)]
struct RemotePriority {
    logical: LogicalPriority,
    canonical_labels: Vec<String>,
}

pub(crate) fn reconcile(
    client: &GitHubClient,
    repository: &Repository,
) -> Result<ReconciliationResult, ReconciliationError> {
    let outbox_store = OutboxStore::discover(repository)?;
    let mut transaction = outbox_store.begin_transaction(repository)?;
    let preflight = replica_sync::fetch(client, repository)?;
    let identity_store = DraftIdentityStore::discover(repository)?;
    let mut identity_transaction = identity_store.begin_transaction(repository)?;
    let remote_issues: BTreeMap<_, _> = preflight
        .issues
        .iter()
        .cloned()
        .map(|issue| (issue.number, issue))
        .collect();
    let markers_to_recover: BTreeSet<_> = transaction
        .operations()
        .iter()
        .filter(|operation| {
            matches!(
                operation.issue_create_state(),
                Some(IssueCreateState::AwaitingMarker { .. })
            )
        })
        .filter_map(|operation| {
            operation
                .issue_create_view()
                .map(|create| create.marker.to_owned())
        })
        .collect();
    let marker_matches = client.find_issues_with_markers(repository, &markers_to_recover)?;
    let comment_markers_to_recover: BTreeSet<_> = transaction
        .operations()
        .iter()
        .filter_map(|operation| {
            operation
                .comment_create_view()
                .filter(|comment| {
                    matches!(comment.state, CommentCreateState::AwaitingMarker { .. })
                })
                .map(|comment| comment.marker.to_owned())
        })
        .collect();
    let comment_marker_matches =
        client.find_comments_with_markers(repository, &comment_markers_to_recover)?;
    let pass = ReconciliationPass {
        client,
        repository,
        transaction: &mut transaction,
        remote: priority::remote_priorities(&preflight),
        remote_dependencies: dependency::remote_dependencies(&preflight),
        remote_issues,
        metadata_presence: BTreeMap::new(),
        remote_sub_issues: BTreeMap::new(),
        identity_transaction: &mut identity_transaction,
        marker_matches,
        comment_marker_matches,
        results: Vec::new(),
        requires_final_refresh: false,
    }
    .run()?;
    let mut results = pass.operations;

    let final_replica = if pass.requires_final_refresh {
        // Finish the discarded preflight before starting the publication attempt.
        drop(preflight);
        replica_sync::fetch(client, repository)
            .map_err(ReconciliationError::FinalSynchronization)?
    } else {
        preflight
    };
    let final_replica = final_replica
        .publish()
        .map_err(ReconciliationError::FinalPublication)?;

    retire_verified_operations(
        client,
        repository,
        &mut transaction,
        &final_replica,
        &mut results,
    )?;
    let remaining = transaction.operations().len();
    let summary = ReconciliationSummary::from_operations(&results, remaining);

    Ok(ReconciliationResult {
        repository: repository.full_name().to_owned(),
        operations: results,
        summary,
        replica: final_replica,
    })
}

struct ReconciliationPass<'client, 'transaction, 'store, 'identity> {
    client: &'client GitHubClient,
    repository: &'client Repository,
    transaction: &'transaction mut crate::outbox::OutboxTransaction<'store>,
    remote: BTreeMap<u64, RemotePriority>,
    remote_dependencies: BTreeSet<DependencyEdgeKey>,
    remote_issues: BTreeMap<u64, Issue>,
    metadata_presence: BTreeMap<crate::metadata::MetadataSetTarget, bool>,
    remote_sub_issues: BTreeMap<u64, BTreeSet<u64>>,
    identity_transaction: &'identity mut DraftIdentityTransaction<'identity>,
    marker_matches: BTreeMap<String, Vec<crate::github::CreatedIssueIdentity>>,
    comment_marker_matches: BTreeMap<String, Vec<CommentIdentity>>,
    results: Vec<OperationResult>,
    requires_final_refresh: bool,
}

impl ReconciliationPass<'_, '_, '_, '_> {
    fn run(mut self) -> Result<PassResult, ReconciliationError> {
        let operation_ids: Vec<_> = self
            .transaction
            .operations()
            .iter()
            .map(|operation| operation.id().to_owned())
            .collect();
        self.results.reserve(operation_ids.len());
        for operation_id in operation_ids {
            self.reconcile_operation(&operation_id)?;
        }
        Ok(PassResult {
            operations: self.results,
            requires_final_refresh: self.requires_final_refresh,
        })
    }

    fn reconcile_operation(&mut self, operation_id: &str) -> Result<(), ReconciliationError> {
        let operation = self
            .transaction
            .operation(operation_id)
            .expect("operation ID came from this transaction")
            .clone();
        let blocked_by = self.blocking_dependencies(&operation);
        match operation.kind() {
            MutationKind::IssueCreate => {
                self.reconcile_issue_create_operation(&operation, blocked_by)
            }
            MutationKind::CommentCreate => {
                self.reconcile_comment_create_operation(&operation, blocked_by)
            }
            MutationKind::PriorityUpdate => {
                self.reconcile_priority_operation(&operation, blocked_by)
            }
            MutationKind::DependencyUpdate => {
                let (edge, desired) = operation
                    .dependency_values()
                    .expect("Dependency-update kind has Dependency values");
                self.reconcile_dependency_operation(&operation, edge.clone(), desired, blocked_by)
            }
            MutationKind::IssueFieldUpdate => {
                self.reconcile_issue_field_operation(&operation, blocked_by)
            }
            MutationKind::MetadataSetUpdate => {
                self.reconcile_metadata_set_operation(&operation, blocked_by)
            }
        }
    }

    fn blocking_dependencies(&self, operation: &PendingMutation) -> Vec<String> {
        operation
            .depends_on()
            .iter()
            .filter(|dependency| {
                self.transaction
                    .operation(dependency)
                    .is_some_and(|dependency| !dependency.permits_dependents())
            })
            .cloned()
            .collect()
    }
}
struct PassResult {
    operations: Vec<OperationResult>,
    requires_final_refresh: bool,
}

fn retire_verified_operations(
    client: &GitHubClient,
    repository: &Repository,
    transaction: &mut crate::outbox::OutboxTransaction<'_>,
    final_replica: &LocalReplica,
    results: &mut [OperationResult],
) -> Result<(), ReconciliationError> {
    let final_priorities = priority::remote_priorities(final_replica);
    let final_dependencies = dependency::remote_dependencies(final_replica);
    let terminal_ids: Vec<_> = transaction
        .operations()
        .iter()
        .filter(|operation| operation.is_successfully_terminal())
        .map(|operation| operation.id().to_owned())
        .collect();
    let terminal_set: BTreeSet<_> = terminal_ids.iter().cloned().collect();
    let superseded = superseded_terminal_ids(transaction.operations(), &terminal_set);
    let mut retired = BTreeSet::new();
    let mut state_updates = BTreeMap::new();
    let mut final_sub_issues = BTreeMap::new();
    for operation_id in terminal_ids.into_iter().rev() {
        let operation = transaction
            .operation(&operation_id)
            .expect("known terminal operation")
            .clone();
        if superseded.contains(&operation_id)
            || matches!(
                operation.priority_state(),
                Some(PriorityMutationState::ResolvedRemote { .. })
            )
        {
            retired.insert(operation_id);
            continue;
        }
        let verified = match operation.kind() {
            MutationKind::IssueCreate => issue_create::verify_terminal(
                &operation,
                final_replica,
                results,
                &mut state_updates,
            ),
            MutationKind::CommentCreate => comment_create::verify_terminal(
                &operation,
                final_replica,
                results,
                &mut state_updates,
            ),
            MutationKind::DependencyUpdate => dependency::verify_terminal(
                &operation,
                &final_dependencies,
                results,
                &mut state_updates,
            ),
            MutationKind::PriorityUpdate => priority::verify_terminal(
                &operation,
                &final_priorities,
                results,
                &mut state_updates,
            ),
            MutationKind::IssueFieldUpdate => {
                field::verify_terminal(&operation, final_replica, results, &mut state_updates)
            }
            MutationKind::MetadataSetUpdate => metadata_set::verify_terminal(
                client,
                repository,
                &operation,
                final_replica,
                results,
                &mut state_updates,
                &mut final_sub_issues,
            )?,
        };
        if verified {
            retired.insert(operation_id);
        }
    }
    transaction.finalize(repository, &retired, state_updates)?;
    Ok(())
}

fn superseded_terminal_ids(
    operations: &[PendingMutation],
    terminal: &BTreeSet<String>,
) -> BTreeSet<String> {
    let target_by_id: BTreeMap<_, _> = operations
        .iter()
        .map(|operation| (operation.id(), mutation_target(operation)))
        .collect();
    operations
        .iter()
        .filter(|operation| terminal.contains(operation.id()))
        .flat_map(|operation| {
            operation.depends_on().iter().filter(|dependency| {
                target_by_id
                    .get(dependency.as_str())
                    .is_some_and(|target| *target == mutation_target(operation))
            })
        })
        .cloned()
        .collect()
}

#[derive(Clone, Eq, PartialEq)]
enum MutationTarget {
    IssueCreate(crate::model::TemporaryIssueId),
    CommentCreate(String),
    Priority(u64),
    Dependency(DependencyEdgeKey),
    IssueField(u64, IssueField),
    MetadataSet(crate::metadata::MetadataSetTarget),
}

fn mutation_target(operation: &PendingMutation) -> MutationTarget {
    match operation.kind() {
        MutationKind::IssueCreate => MutationTarget::IssueCreate(
            operation
                .issue_create_view()
                .expect("Issue-create kind has Issue-create values")
                .temporary_id,
        ),
        MutationKind::CommentCreate => MutationTarget::CommentCreate(operation.id().to_owned()),
        MutationKind::PriorityUpdate => MutationTarget::Priority(operation.issue_number()),
        MutationKind::DependencyUpdate => MutationTarget::Dependency(
            operation
                .dependency_values()
                .expect("Dependency-update kind has Dependency values")
                .0
                .clone(),
        ),
        MutationKind::IssueFieldUpdate => {
            let update = operation
                .issue_field_update_view()
                .expect("Issue-field kind has Issue-field values");
            MutationTarget::IssueField(update.issue_number, update.field)
        }
        MutationKind::MetadataSetUpdate => MutationTarget::MetadataSet(
            operation
                .metadata_set_update_view()
                .expect("metadata kind has metadata values")
                .target
                .clone(),
        ),
    }
}

pub(crate) fn resolve(
    client: &GitHubClient,
    repository: &Repository,
    operation_id: &str,
    choice: ResolutionChoice,
) -> Result<ResolutionResult, ReconciliationError> {
    let store = OutboxStore::discover(repository)?;
    let mut transaction = store.begin_transaction(repository)?;
    let operation = transaction
        .operation(operation_id)
        .ok_or_else(|| ReconciliationError::UnknownOperation(operation_id.to_owned()))?;
    enum ConflictValue {
        Priority(LogicalPriority),
        IssueField(IssueFieldValue),
    }
    let conflict = if let Some(PriorityMutationState::Conflicting { remote }) =
        operation.priority_state().cloned()
    {
        Some(ConflictValue::Priority(remote))
    } else if let Some(update) = operation.issue_field_update_view() {
        match update.state {
            IssueFieldMutationState::Conflicting { remote } => {
                Some(ConflictValue::IssueField(remote.clone()))
            }
            _ => None,
        }
    } else {
        None
    };
    let Some(conflict) = conflict else {
        return Err(ReconciliationError::OperationNotConflicting(
            operation_id.to_owned(),
        ));
    };
    let choice_name = match choice {
        ResolutionChoice::Remote => "remote",
        ResolutionChoice::Local => "local",
        ResolutionChoice::Replacement(_) => "priority",
    };
    match (choice, conflict) {
        (ResolutionChoice::Remote, ConflictValue::Priority(remote)) => {
            transaction.resolve_remote(repository, operation_id, remote)?;
        }
        (ResolutionChoice::Local, ConflictValue::Priority(remote)) => {
            transaction.rebase_priority(repository, operation_id, remote, None)?;
        }
        (ResolutionChoice::Replacement(selection), ConflictValue::Priority(remote)) => {
            transaction.rebase_priority(
                repository,
                operation_id,
                remote,
                Some(LogicalPriority::from_selection(selection)),
            )?;
        }
        (ResolutionChoice::Remote, ConflictValue::IssueField(remote)) => {
            transaction.resolve_issue_field_remote(repository, operation_id, remote)?;
        }
        (ResolutionChoice::Local, ConflictValue::IssueField(remote)) => {
            transaction.rebase_issue_field(repository, operation_id, remote)?;
        }
        (ResolutionChoice::Replacement(_), ConflictValue::IssueField(_)) => {
            return Err(ReconciliationError::PriorityReplacementForIssueField(
                operation_id.to_owned(),
            ));
        }
    }
    drop(transaction);

    Ok(ResolutionResult {
        operation_id: operation_id.to_owned(),
        choice: choice_name,
        reconciliation: reconcile(client, repository)?,
    })
}

#[derive(Debug, Error)]
pub(crate) enum ReconciliationError {
    #[error("could not refresh GitHub before Mutation reconciliation: {0}")]
    Preflight(#[from] ReplicaSyncError),
    #[error(transparent)]
    Outbox(#[from] OutboxError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    DraftIdentity(#[from] DraftIdentityError),
    #[error(transparent)]
    GitHub(#[from] GitHubError),
    #[error(transparent)]
    IssueField(#[from] crate::issue_field::IssueFieldError),
    #[error("final Synchronization after Mutation reconciliation failed: {0}")]
    FinalSynchronization(ReplicaSyncError),
    #[error("accepted GitHub state could not be published to the Local replica: {0}")]
    FinalPublication(StoreError),
    #[error("Pending mutation operation {0:?} does not exist")]
    UnknownOperation(String),
    #[error("Pending mutation operation {0:?} is not a resolvable field conflict")]
    OperationNotConflicting(String),
    #[error("Pending mutation operation {0:?} is not a Priority conflict")]
    PriorityReplacementForIssueField(String),
    #[error("GitHub Issue #{0} is missing during metadata reconciliation")]
    MissingMetadataIssue(u64),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{ScalarClassification, classify_scalar, superseded_terminal_ids};
    use crate::{
        outbox::{PendingMutation, PriorityWrite},
        priority::{DeclaredPriority, LogicalPriority},
        repository::Repository,
    };

    #[test]
    fn three_way_priority_classification_uses_the_current_remote_value() {
        let base = declared(DeclaredPriority::P1);
        let desired = declared(DeclaredPriority::P0);

        assert_eq!(
            classify_scalar(&base, &desired, &desired),
            ScalarClassification::AlreadySatisfied
        );
        assert_eq!(
            classify_scalar(&base, &desired, &base),
            ScalarClassification::Applicable
        );
        assert_eq!(
            classify_scalar(&base, &desired, &declared(DeclaredPriority::P3)),
            ScalarClassification::Conflicting
        );
    }

    #[test]
    fn priority_write_plan_adds_desired_before_removing_every_obsolete_value() {
        assert_eq!(
            PriorityWrite::canonical_plan(
                &["priority:p1".to_owned(), "priority:p3".to_owned()],
                &declared(DeclaredPriority::P0),
            )
            .expect("canonical plan"),
            vec![
                PriorityWrite::Add {
                    label: "priority:p0".to_owned()
                },
                PriorityWrite::Remove {
                    label: "priority:p1".to_owned()
                },
                PriorityWrite::Remove {
                    label: "priority:p3".to_owned()
                }
            ]
        );
    }

    #[test]
    fn superseded_terminal_lookup_scales_as_one_precomputed_chain_walk() {
        let repository = Repository::parse("acme/reconcile").expect("repository");
        let mut operations = Vec::with_capacity(5_000);
        let mut dependency = None;
        for _ in 0..5_000 {
            let operation = PendingMutation::priority_update(
                &repository,
                1,
                declared(DeclaredPriority::P1),
                declared(DeclaredPriority::P0),
                dependency.into_iter().collect(),
            );
            dependency = Some(operation.id().to_owned());
            operations.push(operation);
        }
        let terminal: BTreeSet<_> = operations
            .iter()
            .map(|operation| operation.id().to_owned())
            .collect();

        assert_eq!(
            superseded_terminal_ids(&operations, &terminal).len(),
            operations.len() - 1
        );
    }

    fn declared(value: DeclaredPriority) -> LogicalPriority {
        LogicalPriority::Declared { value }
    }
}
