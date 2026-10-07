use thiserror::Error;

use crate::{
    draft_identity::{DraftIdentityError, DraftIdentityStore, DraftIdentityTransaction},
    github::{GitHubClient, GitHubError, MetadataChange},
    metadata::{GenericLabel, MetadataError, MetadataSetTarget, PendingIssueOperand},
    model::{LocalReplica, SetPresence, TemporaryIssueId},
    outbox::{OutboxError, OutboxStore, OutboxTransaction, PendingMutation},
    replica_sync::{self, ReplicaSyncError},
    repository::{IssueReference, PendingIssueReference, Repository},
    store::{ReplicaStore, StoreError},
    working_graph::{WorkingGraph, WorkingGraphError},
};

pub(crate) enum MetadataRequest<'a> {
    GenericLabel {
        issue: &'a PendingIssueReference,
        label: GenericLabel,
        desired: SetPresence,
    },
    ParentRelationship {
        parent: &'a PendingIssueReference,
        child: &'a PendingIssueReference,
        desired: SetPresence,
    },
}

pub(crate) struct MetadataMutationResult {
    pub(crate) target: MetadataSetTarget,
    pub(crate) desired: SetPresence,
    pub(crate) outcome: MetadataMutationOutcome,
    pub(crate) replica: LocalReplica,
}

pub(crate) enum MetadataMutationOutcome {
    Synchronized {
        change: MetadataChange,
    },
    Queued {
        operation: Box<PendingMutation>,
        working_input_hash: String,
    },
}

pub(crate) fn update_or_queue(
    client: Option<&GitHubClient>,
    request: MetadataRequest<'_>,
) -> Result<MetadataMutationResult, MetadataMutationError> {
    let repository = request.repository()?.clone();
    let outbox_store = OutboxStore::discover(&repository)?;
    let transaction = outbox_store.begin_transaction(&repository)?;
    let has_temporary_references = request.has_temporary_references();
    let resolved = if has_temporary_references {
        let identity_store = DraftIdentityStore::discover(&repository)?;
        let identity_transaction = identity_store.begin_transaction(&repository)?;
        request.resolve(Some(&identity_transaction), transaction.outbox())?
    } else {
        request.resolve(None, transaction.outbox())?
    };
    let target = resolved.target();
    let previous = transaction
        .outbox()
        .latest_metadata_operation_for_target(&target)
        .map(str::to_owned);

    if previous.is_none()
        && resolved.create_dependencies().is_empty()
        && target.is_remote_resolved()
        && let Some(client) = client
    {
        match apply_online(client, &resolved) {
            Ok(change) => {
                let replica = synchronize_and_verify(client, &repository, &resolved)?.publish()?;
                return Ok(MetadataMutationResult {
                    target,
                    desired: resolved.desired(),
                    outcome: MetadataMutationOutcome::Synchronized { change },
                    replica,
                });
            }
            Err(source) if source.permits_offline_queue() => {}
            Err(source) => return Err(source.into()),
        }
    }

    queue_locked(&repository, transaction, resolved, previous)
}

enum ResolvedMetadataRequest {
    GenericLabel {
        issue: ResolvedOperand,
        label: GenericLabel,
        desired: SetPresence,
    },
    ParentRelationship {
        parent: ResolvedOperand,
        child: ResolvedOperand,
        desired: SetPresence,
    },
}

impl ResolvedMetadataRequest {
    fn target(&self) -> MetadataSetTarget {
        match self {
            Self::GenericLabel { issue, label, .. } => MetadataSetTarget::GenericLabel {
                issue: issue.operand,
                label: label.clone(),
            },
            Self::ParentRelationship { parent, child, .. } => {
                MetadataSetTarget::ParentRelationship {
                    parent: parent.operand,
                    child: child.operand,
                }
            }
        }
    }

    fn desired(&self) -> SetPresence {
        match self {
            Self::GenericLabel { desired, .. } | Self::ParentRelationship { desired, .. } => {
                *desired
            }
        }
    }

    fn create_dependencies(&self) -> Vec<String> {
        let mut dependencies: Vec<_> = match self {
            Self::GenericLabel { issue, .. } => issue.create_dependency.iter().cloned().collect(),
            Self::ParentRelationship { parent, child, .. } => parent
                .create_dependency
                .iter()
                .chain(child.create_dependency.iter())
                .cloned()
                .collect(),
        };
        dependencies.sort();
        dependencies.dedup();
        dependencies
    }
}

impl MetadataRequest<'_> {
    fn repository(&self) -> Result<&Repository, MetadataMutationError> {
        match self {
            Self::GenericLabel { issue, .. } => Ok(issue.repository()),
            Self::ParentRelationship { parent, child, .. } => {
                if !parent
                    .repository()
                    .full_name()
                    .eq_ignore_ascii_case(child.repository().full_name())
                {
                    return Err(MetadataMutationError::CrossRepositoryParent);
                }
                Ok(parent.repository())
            }
        }
    }

    fn has_temporary_references(&self) -> bool {
        match self {
            Self::GenericLabel { issue, .. } => issue.temporary_id().is_some(),
            Self::ParentRelationship { parent, child, .. } => {
                parent.temporary_id().is_some() || child.temporary_id().is_some()
            }
        }
    }

    fn resolve(
        self,
        identities: Option<&DraftIdentityTransaction<'_>>,
        outbox: &crate::outbox::PendingMutationOutbox,
    ) -> Result<ResolvedMetadataRequest, MetadataMutationError> {
        match self {
            Self::GenericLabel {
                issue,
                label,
                desired,
            } => {
                let issue = resolve_operand(issue, identities, outbox)?;
                Ok(ResolvedMetadataRequest::GenericLabel {
                    issue,
                    label,
                    desired,
                })
            }
            Self::ParentRelationship {
                parent,
                child,
                desired,
            } => {
                let parent = resolve_operand(parent, identities, outbox)?;
                let child = resolve_operand(child, identities, outbox)?;
                Ok(ResolvedMetadataRequest::ParentRelationship {
                    parent,
                    child,
                    desired,
                })
            }
        }
    }
}

struct ResolvedOperand {
    operand: PendingIssueOperand,
    github: IssueReference,
    create_dependency: Option<String>,
}

fn resolve_operand(
    reference: &PendingIssueReference,
    identities: Option<&DraftIdentityTransaction<'_>>,
    outbox: &crate::outbox::PendingMutationOutbox,
) -> Result<ResolvedOperand, MetadataMutationError> {
    let (number, create_dependency) = match reference.temporary_id() {
        None => (reference.local_number(), None),
        Some(temporary_id) => match identities.and_then(|store| store.resolve(temporary_id)) {
            Some(identity) => (identity.issue_number, None),
            None => (
                temporary_id.synthetic_number(),
                draft_create_operation(outbox, temporary_id).map(str::to_owned),
            ),
        },
    };
    Ok(ResolvedOperand {
        operand: PendingIssueOperand::new(number, reference.temporary_id())?,
        github: reference.github_reference(number),
        create_dependency,
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

fn apply_online(
    client: &GitHubClient,
    request: &ResolvedMetadataRequest,
) -> Result<MetadataChange, GitHubError> {
    match request {
        ResolvedMetadataRequest::GenericLabel {
            issue,
            label,
            desired,
        } => client.mutate_generic_label(&issue.github, label, *desired),
        ResolvedMetadataRequest::ParentRelationship {
            parent,
            child,
            desired,
        } => client.mutate_parent_relationship(&parent.github, &child.github, *desired),
    }
}

fn synchronize_and_verify(
    client: &GitHubClient,
    repository: &Repository,
    request: &ResolvedMetadataRequest,
) -> Result<replica_sync::RefreshedReplica, MetadataMutationError> {
    let replica = replica_sync::fetch(client, repository)?;
    let verified = match request {
        ResolvedMetadataRequest::GenericLabel {
            issue,
            label,
            desired,
        } => replica
            .issues
            .iter()
            .find(|candidate| candidate.number == issue.operand.number())
            .is_some_and(|issue| {
                issue
                    .labels
                    .iter()
                    .any(|candidate| label.matches(&candidate.name))
                    == desired.is_present()
            }),
        ResolvedMetadataRequest::ParentRelationship {
            parent,
            child,
            desired,
        } => {
            let child_id = replica
                .issues
                .iter()
                .find(|issue| issue.number == child.operand.number())
                .map(|issue| issue.id)
                .ok_or(MetadataMutationError::ReadbackMissing(
                    child.github.stable_key(),
                ))?;
            client.sub_issue_exists(&parent.github, child_id)? == desired.is_present()
        }
    };
    if !verified {
        return Err(MetadataMutationError::ReadbackMismatch);
    }
    Ok(replica)
}

fn queue_locked(
    repository: &Repository,
    transaction: OutboxTransaction<'_>,
    request: ResolvedMetadataRequest,
    previous: Option<String>,
) -> Result<MetadataMutationResult, MetadataMutationError> {
    let replica = ReplicaStore::discover(repository)?.load(repository)?;
    WorkingGraph::validate_references(&replica, transaction.outbox())?;
    let target = request.target();
    let desired = request.desired();
    let mut depends_on = request.create_dependencies();
    depends_on.extend(previous);
    depends_on.sort();
    depends_on.dedup();
    let operation =
        PendingMutation::metadata_set_update(repository, target.clone(), desired, depends_on);
    let outbox = transaction.append(repository, operation.clone())?;
    let working = WorkingGraph::project(&replica, &outbox)?;
    Ok(MetadataMutationResult {
        target,
        desired,
        outcome: MetadataMutationOutcome::Queued {
            operation: Box::new(operation),
            working_input_hash: working.input_hash().to_owned(),
        },
        replica,
    })
}

#[derive(Debug, Error)]
pub(crate) enum MetadataMutationError {
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(transparent)]
    GitHub(#[from] GitHubError),
    #[error(transparent)]
    Outbox(#[from] OutboxError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    DraftIdentity(#[from] DraftIdentityError),
    #[error(transparent)]
    WorkingGraph(#[from] WorkingGraphError),
    #[error("metadata synchronization failed: {0}")]
    Synchronization(#[from] ReplicaSyncError),
    #[error("parent and sub-Issue references must belong to the same Repository")]
    CrossRepositoryParent,
    #[error("metadata readback did not include {0}")]
    ReadbackMissing(String),
    #[error("GitHub synchronized readback did not verify the intended metadata set state")]
    ReadbackMismatch,
}
