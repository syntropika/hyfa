use chrono::{SecondsFormat, Utc};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
};
use thiserror::Error;

use crate::{
    github::{GitHubClient, GitHubError},
    model::{LocalReplica, ReplicaError},
    repository::{PendingIssueReference, Repository},
    store::{ReplicaStore, StoreError},
    sync_diagnostics::{Failure, Journal, Stage},
    synchronization::refresh_repository,
};

pub(crate) fn fetch(
    client: &GitHubClient,
    repository: &Repository,
) -> Result<RefreshedReplica, ReplicaSyncError> {
    refresh(client, repository, None)
}

pub(crate) fn refresh(
    client: &GitHubClient,
    repository: &Repository,
    previous: Option<&LocalReplica>,
) -> Result<RefreshedReplica, ReplicaSyncError> {
    refresh_with_relationships(client, repository, previous, &[])
}

pub(crate) fn refresh_with_relationships(
    client: &GitHubClient,
    repository: &Repository,
    previous: Option<&LocalReplica>,
    requested: &[String],
) -> Result<RefreshedReplica, ReplicaSyncError> {
    let journal = Journal::begin(repository, Stage::Refreshing)?;
    let observer = journal.clone();
    client.observe_pages(Some(Box::new(move |items| observer.page(items))));
    let result = acquire(client, repository, previous, requested).and_then(|replica| {
        journal.stage(Stage::Validating, Some(&replica));
        replica.validate(repository.full_name())?;
        Ok(replica)
    });
    client.observe_pages(None);
    match result {
        Ok(replica) => Ok(RefreshedReplica {
            replica: Some(replica),
            repository: repository.clone(),
            journal,
        }),
        Err(error) => {
            journal.finish(Err(Failure::synchronization(&error)));
            Err(error)
        }
    }
}

// A refresh is successful only after its validated candidate is published.
pub(crate) struct RefreshedReplica {
    replica: Option<LocalReplica>,
    repository: Repository,
    journal: Journal,
}

impl RefreshedReplica {
    pub(crate) fn publish(mut self) -> Result<LocalReplica, StoreError> {
        let replica = self
            .replica
            .take()
            .expect("unpublished synchronization candidate");
        self.journal.stage(Stage::Publishing, None);
        let result =
            ReplicaStore::discover(&self.repository).and_then(|store| store.publish(&replica));
        self.journal.finish(
            result
                .as_ref()
                .map(|()| &replica)
                .map_err(|_| Failure::persistence()),
        );
        result.map(|()| replica)
    }
}

impl Deref for RefreshedReplica {
    type Target = LocalReplica;

    fn deref(&self) -> &Self::Target {
        self.replica
            .as_ref()
            .expect("unpublished synchronization candidate")
    }
}

impl Drop for RefreshedReplica {
    fn drop(&mut self) {
        if self.replica.is_some() {
            self.journal.finish(Err(Failure::new(
                "not_published",
                "The synchronization candidate was not published",
            )));
        }
    }
}

fn acquire(
    client: &GitHubClient,
    repository: &Repository,
    previous: Option<&LocalReplica>,
    requested: &[String],
) -> Result<LocalReplica, ReplicaSyncError> {
    // Full mutation readbacks refresh the same explicitly discovered inventories.
    let stored = if previous.is_none() {
        match ReplicaStore::discover(repository)?.load(repository) {
            Ok(replica) => Some(replica),
            Err(
                StoreError::MissingReplica | StoreError::Decode(_) | StoreError::InvalidReplica(_),
            ) => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    let mut keys: BTreeSet<_> = previous
        .or(stored.as_ref())
        .into_iter()
        .flat_map(|replica| replica.relationships.keys().cloned())
        .collect();
    keys.extend(requested.iter().cloned());
    let data = refresh_repository(client, repository, previous)?;
    let issue_keys: BTreeSet<_> = data
        .issues
        .iter()
        .map(|issue| {
            issue
                .display_key(repository.full_name())
                .to_ascii_lowercase()
        })
        .collect();
    let mut relationships = BTreeMap::new();
    for key in keys {
        let reference =
            PendingIssueReference::parse(&key).map_err(|_| GitHubError::InvalidRelationship)?;
        let Some(issue) = reference.as_github() else {
            continue;
        };
        if !issue_keys.contains(&key) {
            if requested.contains(&key) {
                return Err(GitHubError::InvalidRelationship.into());
            }
            continue;
        }
        let inventory = client.fetch_issue_relationships(issue)?;
        for related in inventory.children.iter().chain(inventory.parent.iter()) {
            let reference = PendingIssueReference::parse(related)
                .map_err(|_| GitHubError::InvalidRelationship)?;
            if reference
                .repository()
                .full_name()
                .eq_ignore_ascii_case(repository.full_name())
                && !issue_keys.contains(related)
            {
                return Err(GitHubError::InvalidRelationship.into());
            }
        }
        relationships.insert(key, inventory);
    }
    LocalReplica::build_with_sync(
        repository.full_name().to_owned(),
        Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        data.sync,
        data.labels,
        data.issues,
        data.dependencies,
    )?
    .with_relationships(relationships)
    .map_err(Into::into)
}

#[derive(Debug, Error)]
pub(crate) enum ReplicaSyncError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    GitHub(#[from] GitHubError),
    #[error(transparent)]
    Replica(#[from] ReplicaError),
}
