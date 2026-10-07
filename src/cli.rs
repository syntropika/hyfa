use std::{env, path::PathBuf, time::Instant};

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use thiserror::Error;
use url::Url;

use crate::{
    auth::{AuthError, AuthToken},
    comment_create::{self, PendingCommentCreateError},
    dependency_update::{self, PendingDependencyUpdateError},
    github::{
        CreateLabelRequest, DependencyChange, DependencyIntent, GitHubClient, GitHubError,
        LabelCreation,
    },
    graph::{GraphError, PublicGraphOptions, confirm_public_repository, publish_site},
    issue_create::{self, PendingIssueCreateError},
    issue_field::{self, IssueField, IssueFieldUpdateError, IssueFieldValue, IssueStateValue},
    metadata::{GenericLabel, MetadataError, MetadataSetTarget, PendingIssueOperand},
    metadata_mutation::{self, MetadataMutationError, MetadataMutationOutcome, MetadataRequest},
    model::{DependencyPresence, LocalReplica, ReplicaError, SetPresence, TemporaryIssueId},
    operational::{ExecutionScope, PreparedRepository, analyze_ready},
    outbox::{OutboxError, OutboxStore, PendingMutation},
    plan::{DependencyLayers, PlanIssue},
    priority::{
        DeclaredPriority, LogicalPriority, PrioritySelection, PriorityState,
        missing_canonical_labels, present_canonical_labels,
    },
    priority_update::{self, PendingPriorityUpdateError, PriorityUpdateError},
    ranking::{self, NextAnalysis, PlanDecision},
    reconciliation::{self, ReconciliationError, ResolutionChoice},
    replica_sync::{self, ReplicaSyncError},
    repository::{
        IssueReferenceError, PendingIssueReference, PendingIssueReferenceError, Repository,
        RepositoryError,
    },
    store::{ReplicaStore, StoreError},
    triage::{self, TriageReport},
    working_graph::{PendingProvenance, WorkingGraph, WorkingGraphError},
};

const SYNC_SCHEMA_VERSION: &str = "hyfa.sync/v1";
const READY_SCHEMA_VERSION: &str = "hyfa.ready/v1";
const GRAPH_SCHEMA_VERSION: &str = "hyfa.graph/v1";
const DEPENDENCY_MUTATION_SCHEMA_VERSION: &str = "hyfa.dependency-mutation/v1";
const INIT_SCHEMA_VERSION: &str = "hyfa.init/v1";
const TRIAGE_SCHEMA_VERSION: &str = "hyfa.triage/v1";
const PLAN_SCHEMA_VERSION: &str = "hyfa.plan/v1";
const PRIORITY_UPDATE_SCHEMA_VERSION: &str = "hyfa.priority-update/v1";
const RESOLVE_SCHEMA_VERSION: &str = "hyfa.resolve/v1";
const ISSUE_CREATE_SCHEMA_VERSION: &str = "hyfa.issue-create/v1";
const COMMENT_CREATE_SCHEMA_VERSION: &str = "hyfa.comment-create/v1";
const ISSUE_FIELD_UPDATE_SCHEMA_VERSION: &str = "hyfa.issue-field-update/v1";
const METADATA_MUTATION_SCHEMA_VERSION: &str = "hyfa.metadata-set/v1";

#[derive(Parser)]
#[command(name = "hyfa", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Default)]
struct ScopeArguments {
    /// Select Ready work assigned to this GitHub login; otherwise require no assignee.
    #[arg(long)]
    assignee: Option<String>,
    /// Require this label on every executable step. Repeat to require all labels.
    #[arg(long = "label")]
    labels: Vec<String>,
    /// Exclude work carrying this label. Repeat to exclude any listed label.
    #[arg(long = "exclude-label")]
    exclude_labels: Vec<String>,
    /// Select direct children of this OWNER/REPO#NUMBER or draft reference.
    #[arg(long)]
    children_of: Option<String>,
}

impl ScopeArguments {
    fn has_selection(&self) -> bool {
        !self.labels.is_empty() || !self.exclude_labels.is_empty() || self.children_of.is_some()
    }

    fn parent(&self, repository: &str) -> Result<Option<String>, CliError> {
        self.children_of
            .as_deref()
            .map(|parent| {
                crate::execution_scope::parent_key(parent, repository).map_err(Into::into)
            })
            .transpose()
    }

    fn selection(
        &self,
        working: &WorkingGraph<'_>,
    ) -> Result<crate::execution_scope::Selection, CliError> {
        crate::execution_scope::Selection::new(
            &self.labels,
            &self.exclude_labels,
            self.parent(&working.replica().repository)?,
            working,
        )
        .map_err(Into::into)
    }

    fn scope<'a>(&'a self, selection: &'a crate::execution_scope::Selection) -> ExecutionScope<'a> {
        if selection.is_empty() {
            self.assignee
                .as_deref()
                .map(ExecutionScope::Assignee)
                .unwrap_or(ExecutionScope::Available)
        } else {
            ExecutionScope::Selected {
                assignee: self.assignee.as_deref(),
                selection,
            }
        }
    }
}

fn refresh_for_scope(
    repository: &Repository,
    arguments: &ScopeArguments,
) -> Result<(LocalReplica, ReplicaSource), CliError> {
    let requested: Vec<_> = arguments
        .parent(repository.full_name())?
        .into_iter()
        .collect();
    refresh_for_relationships(repository, &requested)
}

fn refresh_for_relationships(
    repository: &Repository,
    requested: &[String],
) -> Result<(LocalReplica, ReplicaSource), CliError> {
    let refresh = github_client_for_sync(repository)
        .and_then(|client| synchronize_with_relationships(repository, &client, requested));
    refresh_or_local_after(repository, refresh)
}

#[derive(Subcommand)]
enum Command {
    /// Search effective local Issue titles, bodies, and comments.
    Search {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        query: String,
        #[command(flatten)]
        filters: crate::discovery::Filters,
        /// Maximum results; all matches are counted before truncation.
        #[arg(long, default_value = "20", value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
        /// Attempt a pull-only synchronization before local discovery.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Suggest related Issues using explicit references and shared title terms.
    Related {
        /// Issue in OWNER/REPO#NUMBER or draft-reference form.
        issue: String,
        #[command(flatten)]
        filters: crate::discovery::Filters,
        #[arg(long, default_value = "20", value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect the last synchronization attempt and valid local snapshot without network access.
    Status {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        json: bool,
    },
    /// Read GitHub-linked closing PRs as separate, cached context.
    Prs {
        issue: String,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read an Issue's complete effective local content, comments, and relationships.
    View {
        /// Issue in OWNER/REPO#NUMBER or draft-reference form.
        issue: String,
        /// Read the last valid snapshot without attempting a GitHub refresh.
        #[arg(long)]
        offline: bool,
        /// Include separately observed GitHub-linked closing PR context.
        #[arg(long)]
        with_prs: bool,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Install the bundled Hyfa usage skill for coding agents.
    Skill {
        #[command(subcommand)]
        command: crate::skill::SkillCommand,
    },
    /// Sign in to GitHub, inspect authentication, or sign out.
    Auth {
        #[command(subcommand)]
        command: crate::auth::AuthCommand,
    },
    /// Create a provisional Draft Issue for later reconciliation.
    Create {
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        /// Draft Issue title.
        #[arg(long)]
        title: String,
        /// Draft Issue body in Markdown.
        #[arg(long, default_value = "")]
        body: String,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Add a recoverable comment to an Issue or Draft Issue.
    Comment {
        /// Issue in OWNER/REPO#NUMBER or OWNER/REPO#draft:TEMPORARY_ID form.
        issue: String,
        /// Comment body in Markdown.
        #[arg(long)]
        body: String,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Recommend the best executable first step under next/v1.
    Next {
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        #[command(flatten)]
        scope: ScopeArguments,
        /// Number of completions to evaluate, from one through the default three.
        #[arg(long, default_value_t = ranking::DEFAULT_HORIZON)]
        horizon: u8,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
        /// Report local ranking phase timings; Synchronization is excluded.
        #[arg(long)]
        profile: bool,
    },
    /// Explain the best rollout and structural dependency layers.
    Plan {
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        #[command(flatten)]
        scope: ScopeArguments,
        /// Number of completions to evaluate, from one through the default three.
        #[arg(long, default_value_t = ranking::DEFAULT_HORIZON)]
        horizon: u8,
        /// Capacity is intentionally unsupported by plan/v1.
        #[arg(long)]
        workers: Option<usize>,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Surface actionable operational graph problems.
    Triage {
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        /// Select work assigned to this login instead of unassigned work.
        #[arg(long)]
        assignee: Option<String>,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Update one logical Issue field.
    #[command(group(
        ArgGroup::new("issue_update")
            .required(true)
            .multiple(false)
            .args(["priority", "title", "body", "state", "assignee", "clear_assignees"])
    ))]
    Update {
        /// Issue in OWNER/REPO#NUMBER or OWNER/REPO#draft:TEMPORARY_ID form.
        issue: String,
        /// Desired logical Priority, or none to remove it.
        #[arg(long)]
        priority: Option<PrioritySelection>,
        /// Replace the Issue title.
        #[arg(long)]
        title: Option<String>,
        /// Replace the Issue body Markdown.
        #[arg(long)]
        body: Option<String>,
        /// Open or close the Issue.
        #[arg(long)]
        state: Option<IssueStateArgument>,
        /// Replace the assignee set with these logins; repeat for multiple assignees.
        #[arg(long, action = clap::ArgAction::Append)]
        assignee: Vec<String>,
        /// Remove every assignee.
        #[arg(long)]
        clear_assignees: bool,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Add or remove one non-Priority Issue label.
    #[command(group(
        ArgGroup::new("label_intent")
            .required(true)
            .multiple(false)
            .args(["add", "remove"])
    ))]
    Label {
        /// Issue in OWNER/REPO#NUMBER or OWNER/REPO#draft:TEMPORARY_ID form.
        issue: String,
        /// Add this generic label.
        #[arg(long)]
        add: Option<String>,
        /// Remove this generic label.
        #[arg(long)]
        remove: Option<String>,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Add or remove a parent/sub-Issue relationship without creating a Dependency.
    #[command(name = "sub-issue", group(
        ArgGroup::new("sub_issue_intent")
            .required(true)
            .multiple(false)
            .args(["add", "remove"])
    ))]
    SubIssue {
        /// Parent Issue reference.
        parent: String,
        /// Add this Issue as a sub-Issue.
        #[arg(long)]
        add: Option<String>,
        /// Remove this Issue as a sub-Issue.
        #[arg(long)]
        remove: Option<String>,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Generate a deterministic static Issue graph site.
    Graph {
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        /// Target directory for the complete static site.
        #[arg(long)]
        output: PathBuf,
        #[command(flatten)]
        scope: ScopeArguments,
        /// Ranking horizon embedded in the static analysis.
        #[arg(long, default_value_t = ranking::DEFAULT_HORIZON)]
        horizon: u8,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
        /// Generate a fail-closed artifact safe for deliberate public publication.
        #[arg(long)]
        public: bool,
        /// Publish labels in this explicitly allowed category prefix. Repeatable.
        #[arg(long, requires = "public")]
        public_label_prefix: Vec<String>,
        /// Publish GitHub assignee logins in the public artifact.
        #[arg(long, requires = "public")]
        public_include_assignees: bool,
    },
    /// Make one Issue blocked by another native GitHub Issue.
    Block {
        /// Issue to block in OWNER/REPO#NUMBER form.
        issue: String,
        /// Blocking Issue in OWNER/REPO#NUMBER form.
        #[arg(long)]
        by: String,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Remove a native blocked-by relationship between two Issues.
    Unblock {
        /// Issue that is currently blocked in OWNER/REPO#NUMBER form.
        issue: String,
        /// Blocking Issue in OWNER/REPO#NUMBER form.
        #[arg(long)]
        by: String,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Create any missing canonical Priority labels.
    Init {
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Synchronize one GitHub Repository into the Local replica.
    Sync {
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Enumerate the complete Executable frontier.
    Ready {
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        #[command(flatten)]
        scope: ScopeArguments,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Reconcile Pending mutations against current GitHub state.
    Reconcile {
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Resolve one Pending scalar-field conflict explicitly.
    #[command(group(
        ArgGroup::new("resolution")
            .required(true)
            .multiple(false)
            .args(["remote", "local", "priority"])
    ))]
    Resolve {
        /// Pending mutation operation ID.
        operation: String,
        /// Repository in OWNER/REPO form.
        #[arg(long)]
        repo: String,
        /// Accept GitHub's current value and retire the local intent.
        #[arg(long)]
        remote: bool,
        /// Reaffirm the local value against the last observed GitHub value.
        #[arg(long)]
        local: bool,
        /// Replace a conflicting Priority and rebase it on the last observed GitHub value.
        #[arg(long)]
        priority: Option<PrioritySelection>,
        /// Emit versioned machine-readable output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum IssueStateArgument {
    Open,
    Closed,
}

impl From<IssueStateArgument> for IssueStateValue {
    fn from(value: IssueStateArgument) -> Self {
        match value {
            IssueStateArgument::Open => Self::Open,
            IssueStateArgument::Closed => Self::Closed,
        }
    }
}

pub(crate) fn execute() -> Result<(), CliError> {
    let cli = Cli::parse();
    match cli.command {
        Command::Search {
            repo,
            query,
            filters,
            limit,
            refresh,
            json,
        } => discover(
            &Repository::parse(&repo)?,
            DiscoveryRequest::Search(&query),
            &filters,
            usize::from(limit),
            refresh,
            json,
        ),
        Command::Related {
            issue,
            filters,
            limit,
            refresh,
            json,
        } => {
            let reference = PendingIssueReference::parse(&issue)?;
            let resolved = crate::draft_identity::resolve_reference(&reference)?;
            discover(
                reference.repository(),
                DiscoveryRequest::Related(&resolved),
                &filters,
                usize::from(limit),
                refresh,
                json,
            )
        }
        Command::Status { repo, json } => sync_status(&Repository::parse(&repo)?, json),
        Command::Prs {
            issue,
            offline,
            json,
        } => linked_prs(&issue, offline, json),
        Command::View {
            issue,
            offline,
            with_prs,
            json,
        } => view_issue(&issue, offline, with_prs, json),
        Command::Auth { command } => crate::auth::execute(command).map_err(Into::into),
        Command::Skill { command } => crate::skill::execute(command).map_err(Into::into),
        Command::Create {
            repo,
            title,
            body,
            json,
        } => create_issue(&Repository::parse(&repo)?, title, body, json),
        Command::Comment { issue, body, json } => create_comment(&issue, body, json),
        Command::Graph {
            repo,
            output,
            scope,
            horizon,
            json,
            public,
            public_label_prefix,
            public_include_assignees,
        } => graph(
            &Repository::parse(&repo)?,
            &output,
            &scope,
            horizon,
            json,
            public.then_some(PublicGraphOptions {
                label_prefixes: public_label_prefix,
                include_assignees: public_include_assignees,
            }),
        ),
        Command::Next {
            repo,
            scope,
            horizon,
            json,
            profile,
        } => next(&Repository::parse(&repo)?, &scope, horizon, json, profile),
        Command::Plan {
            repo,
            scope,
            horizon,
            workers,
            json,
        } => plan(&Repository::parse(&repo)?, &scope, horizon, workers, json),
        Command::Triage {
            repo,
            assignee,
            json,
        } => triage_command(&Repository::parse(&repo)?, assignee.as_deref(), json),
        Command::Update {
            issue,
            priority,
            title,
            body,
            state,
            assignee,
            clear_assignees,
            json,
        } => {
            if let Some(priority) = priority {
                update_priority(&issue, priority, json)
            } else {
                let (field, desired) =
                    issue_field_request(title, body, state, assignee, clear_assignees);
                update_issue_field(&issue, field, desired, json)
            }
        }
        Command::Label {
            issue,
            add,
            remove,
            json,
        } => mutate_generic_label(&issue, add, remove, json),
        Command::SubIssue {
            parent,
            add,
            remove,
            json,
        } => mutate_parent_relationship(&parent, add, remove, json),
        Command::Block { issue, by, json } => {
            mutate_dependency(&issue, &by, DependencyIntent::Block, json)
        }
        Command::Unblock { issue, by, json } => {
            mutate_dependency(&issue, &by, DependencyIntent::Unblock, json)
        }
        Command::Init { repo, json } => initialize(&Repository::parse(&repo)?, json),
        Command::Sync { repo, json } => sync(&Repository::parse(&repo)?, json),
        Command::Ready { repo, scope, json } => ready(&Repository::parse(&repo)?, &scope, json),
        Command::Reconcile { repo, json } => reconcile(&Repository::parse(&repo)?, json),
        Command::Resolve {
            operation,
            repo,
            remote,
            local,
            priority,
            json,
        } => resolve_conflict(
            &Repository::parse(&repo)?,
            &operation,
            resolution_choice(remote, local, priority),
            json,
        ),
    }
}

enum DiscoveryRequest<'a> {
    Search(&'a str),
    Related(&'a PendingIssueReference),
}

fn discover(
    repository: &Repository,
    request: DiscoveryRequest<'_>,
    filters: &crate::discovery::Filters,
    limit: usize,
    refresh: bool,
    json: bool,
) -> Result<(), CliError> {
    if let DiscoveryRequest::Search(query) = &request {
        crate::discovery::validate_query(query)?;
    }
    let (replica, source) = if refresh {
        let (replica, source) = refresh_for_relationships(repository, &[])?;
        (
            replica,
            if source.is_fallback() {
                "local_fallback"
            } else {
                "live"
            },
        )
    } else {
        (
            ReplicaStore::discover(repository)?.load(repository)?,
            "local",
        )
    };
    let outbox = OutboxStore::discover(repository)?.load(repository)?;
    let working = WorkingGraph::project(&replica, &outbox)?;
    let subject_provenance = match &request {
        DiscoveryRequest::Related(reference) => {
            Some(working.provenance_for_issue(reference.local_number()))
        }
        DiscoveryRequest::Search(_) => None,
    };
    let (command, schema, method, query, subject, results) = match request {
        DiscoveryRequest::Search(query) => (
            "search",
            "hyfa.search/v1",
            "keyword/v1",
            Some(query),
            None,
            crate::discovery::search(&working, query, filters, limit)?,
        ),
        DiscoveryRequest::Related(reference) => (
            "related",
            "hyfa.related/v1",
            "references-and-title-terms/v1",
            None,
            Some(reference.stable_key()),
            crate::discovery::related(&working, reference.local_number(), filters, limit)?,
        ),
    };
    let output = serde_json::json!({
        "schema_version": schema, "command": command, "method": method,
        "repository": replica.repository, "source": source, "synced_at": replica.synced_at,
        "replica_snapshot_hash": replica.input_hash, "input_hash": working.input_hash(),
        "pending": working.is_pending(), "pending_operation_ids": working.operation_ids(),
        "query": query, "subject": subject, "subject_provenance": subject_provenance,
        "filters": filters, "results": results,
    });
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "{} in {} (source {}, synced_at {}):",
            command, replica.repository, source, replica.synced_at
        );
        for hit in &results.hits {
            hit.print();
        }
        println!(
            "{} matches; showing {}",
            results.matched_count(),
            results.hits.len()
        );
    }
    Ok(())
}

fn sync_status(repository: &Repository, json: bool) -> Result<(), CliError> {
    let store = ReplicaStore::discover(repository)?;
    let (replica, snapshot_state) = match store.load(repository) {
        Ok(replica) => (Some(replica), "valid"),
        Err(StoreError::MissingReplica) => (None, "missing"),
        Err(StoreError::Decode(_) | StoreError::InvalidReplica(_)) => (None, "invalid"),
        Err(error) => return Err(error.into()),
    };
    let attempt = crate::sync_diagnostics::load(repository)?;
    let outbox = OutboxStore::discover(repository)?.load(repository)?;
    let output = serde_json::json!({
        "schema_version": "hyfa.status/v1", "command": "status", "repository": repository.full_name(),
        "snapshot": replica.as_ref().map(snapshot_summary), "snapshot_state": snapshot_state,
        "last_attempt": attempt,
        "pending_operation_count": outbox.operations().len(),
    });
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        if let Some(replica) = &replica {
            println!(
                "{}: valid Local replica synchronized at {}",
                repository.full_name(),
                replica.synced_at
            );
            let counts = snapshot_summary(replica);
            println!(
                "{} Issues, {} comments, {} Dependencies",
                counts.issue_count, counts.comment_count, counts.dependency_count
            );
        } else {
            println!(
                "{}: no valid Local replica ({})",
                repository.full_name(),
                snapshot_state
            );
        }
        if let Some(attempt) = &attempt {
            attempt.print();
        } else {
            println!("No recorded synchronization attempt.");
        }
        println!("Pending mutations: {}", outbox.operations().len());
    }
    Ok(())
}

fn pr_context(
    reference: &PendingIssueReference,
    offline: bool,
) -> Result<crate::pull_requests::Context, CliError> {
    let Some(issue) = reference.as_github() else {
        return Ok(crate::pull_requests::Context::draft());
    };
    if offline {
        return crate::pull_requests::read(issue, None).map_err(Into::into);
    }
    let client = github_client();
    let refresh = client.as_ref().map_err(sync_failure);
    crate::pull_requests::read(issue, Some(refresh)).map_err(Into::into)
}

fn linked_prs(value: &str, offline: bool, json: bool) -> Result<(), CliError> {
    let reference = PendingIssueReference::parse(value)?;
    let resolved = crate::draft_identity::resolve_reference(&reference)?;
    let context = pr_context(&resolved, offline)?;
    if json {
        let output = serde_json::json!({
            "schema_version": "hyfa.pull-requests/v1", "command": "prs", "repository": reference.repository().full_name(),
            "issue": resolved.stable_key(), "context": context,
        });
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        context.print();
    }
    Ok(())
}

fn view_issue(value: &str, offline: bool, with_prs: bool, json: bool) -> Result<(), CliError> {
    let reference = PendingIssueReference::parse(value)?;
    let resolved = crate::draft_identity::resolve_reference(&reference)?;
    let repository = reference.repository();
    let (replica, source) = if offline {
        (
            ReplicaStore::discover(repository)?.load(repository)?,
            "local",
        )
    } else {
        let requested = vec![resolved.stable_key().to_ascii_lowercase()];
        let (replica, source) = refresh_for_relationships(repository, &requested)?;
        (
            replica,
            if source.is_fallback() {
                "local_fallback"
            } else {
                "live"
            },
        )
    };
    let outbox = OutboxStore::discover(repository)?.load(repository)?;
    let working = WorkingGraph::project(&replica, &outbox)?;
    let issue = crate::issue_read::read(&working, resolved.local_number())?;
    let prs = with_prs
        .then(|| pr_context(&resolved, offline))
        .transpose()?;
    if json {
        let mut output = serde_json::json!({
            "schema_version": "hyfa.issue-view/v1", "command": "view", "repository": replica.repository,
            "source": source, "synced_at": replica.synced_at, "replica_snapshot_hash": replica.input_hash,
            "input_hash": working.input_hash(), "pending": working.is_pending(), "pending_operation_ids": working.operation_ids(),
            "issue": issue,
        });
        if let Some(prs) = &prs {
            output["pull_request_context"] =
                serde_json::to_value(prs).map_err(CliError::EncodeOutput)?;
        }
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "{} {}\nState: {}\nSource: {} (synced_at {})",
            issue.key, issue.title, issue.state, source, replica.synced_at
        );
        if let Some(prs) = &prs {
            prs.print();
        }
        println!(
            "Assignees: {}\nLabels: {}",
            issue.assignees.join(", "),
            issue.labels.join(", ")
        );
        if working.is_pending() {
            println!(
                "Pending local changes: {}",
                working.operation_ids().join(", ")
            );
        }
        if let Some(relationships) = &issue.relationships {
            if let Some(parent) = &relationships.parent {
                println!("Parent: {parent}");
            }
            println!("Direct children: {}", relationships.children.join(", "));
        } else {
            println!(
                "Parent and child relationships: not synchronized; read this Issue online to fetch them."
            );
        }
        for blocker in &issue.blocked_by {
            println!("Blocked by: {} ({})", blocker.key, blocker.state);
        }
        if !issue.blocks.is_empty() {
            println!("Blocks: {}", issue.blocks.join(", "));
        }
        if let Some(impact) = &issue.impact {
            for line in impact.human_lines() {
                println!("{line}");
            }
        }
        println!("\n{}", issue.body);
        for comment in &issue.comments {
            println!(
                "\nComment by {} at {}\n{}",
                comment.author.as_deref().unwrap_or("unknown"),
                comment.created_at,
                comment.body
            );
        }
    }
    Ok(())
}

fn graph(
    repository: &Repository,
    output: &std::path::Path,
    arguments: &ScopeArguments,
    horizon: u8,
    json: bool,
    public_options: Option<PublicGraphOptions>,
) -> Result<(), CliError> {
    if !(ranking::MIN_HORIZON..=ranking::MAX_HORIZON).contains(&horizon) {
        return Err(CliError::UnsupportedNextHorizon(horizon));
    }
    if public_options.is_some() && arguments.has_selection() {
        return Err(CliError::PublicSelection);
    }
    let (replica, source, site) = if let Some(public_options) = public_options {
        let client = github_client()?;
        let metadata = client.fetch_repository_metadata(repository)?;
        let confirmed = confirm_public_repository(repository.full_name(), metadata)?;
        let (replica, source) = refresh_or_local_with_client(repository, &client)?;
        let site =
            crate::graph::publish_public_site(&replica, &confirmed, &public_options, output)?;
        (replica, source, site)
    } else {
        let (replica, source) = refresh_for_scope(repository, arguments)?;
        let outbox = OutboxStore::discover(repository)?.load(repository)?;
        let working = WorkingGraph::project(&replica, &outbox)?;
        let selection = arguments.selection(&working)?;
        let scope = arguments.scope(&selection);
        let site = publish_site(&working, scope, horizon, output)?;
        (replica, source, site)
    };
    let output_path = output.display().to_string();
    let result = GraphOutput {
        schema_version: GRAPH_SCHEMA_VERSION,
        command: "graph",
        repository: &replica.repository,
        source,
        synced_at: &replica.synced_at,
        input_hash: &site.input_hash,
        output: &output_path,
        artifact: GraphArtifactSummary {
            schema_version: site.schema_version,
            node_count: site.node_count,
            edge_count: site.edge_count,
            artifact_hash: &site.artifact_hash,
        },
    };
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &result).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "Generated {} nodes and {} Dependencies in {}",
            site.node_count, site.edge_count, output_path
        );
        if source.is_fallback() {
            eprintln!(
                "warning: GitHub refresh failed; generated from Local replica at {}",
                replica.synced_at
            );
        }
    }
    Ok(())
}

fn plan(
    repository: &Repository,
    arguments: &ScopeArguments,
    horizon: u8,
    workers: Option<usize>,
    json: bool,
) -> Result<(), CliError> {
    if workers.is_some() {
        return Err(CliError::UnsupportedPlanWorkers);
    }
    if !(ranking::MIN_HORIZON..=ranking::MAX_HORIZON).contains(&horizon) {
        return Err(CliError::UnsupportedNextHorizon(horizon));
    }
    let (replica, source) = refresh_for_scope(repository, arguments)?;
    let outbox = OutboxStore::discover(repository)?.load(repository)?;
    let working = WorkingGraph::project(&replica, &outbox)?;
    let selection = arguments.selection(&working)?;
    let scope = arguments.scope(&selection);
    let prepared = PreparedRepository::prepare(&working);
    let store = ReplicaStore::discover(repository)?;
    let mut cache = ranking::RankingCache::at(store.repository_directory());
    let bundle = ranking::analyze_prepared_bundle(&prepared, scope, horizon, &mut cache);
    let structural = crate::plan::analyze_with_ready(&prepared, scope, &bundle.ready);
    let decision = bundle.run.analysis.into_plan_decision();
    let parallel_now = structural.parallel_now;
    let dependency_layers = structural.dependency_layers;
    let warnings = analysis_warnings(&working, source);

    if json {
        let output = PlanOutput {
            schema_version: PLAN_SCHEMA_VERSION,
            policy_version: ranking::POLICY_VERSION,
            command: "plan",
            repository: &replica.repository,
            source,
            synced_at: &replica.synced_at,
            replica_snapshot_hash: &replica.input_hash,
            execution_scope: scope.description(),
            decision,
            parallel_now,
            dependency_layers,
            warnings,
        };
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "plan/v1 for {} (synced_at {}):",
            replica.repository, replica.synced_at
        );
        match decision.human_recommendation_summary() {
            Some(recommendation) => println!("{recommendation}"),
            None => println!("{}", decision.summary().human_empty_summary()),
        }
        println!("parallel_now:");
        for issue in &parallel_now {
            println!("{} {}", issue.display_reference(), issue.title);
        }
        println!("dependency layers (counterfactual topology):");
        for layer in dependency_layers.human_lines() {
            println!("{layer}");
        }
        if let Some(warning) = decision.truncation_warning() {
            eprintln!("warning: {warning}");
        }
        for warning in &warnings {
            print_warning(warning);
        }
    }
    Ok(())
}

fn mutate_generic_label(
    issue: &str,
    add: Option<String>,
    remove: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let issue = PendingIssueReference::parse(issue)?;
    let (label, desired) = match (add, remove) {
        (Some(label), None) => (GenericLabel::new(label)?, SetPresence::Present),
        (None, Some(label)) => (GenericLabel::new(label)?, SetPresence::Absent),
        _ => unreachable!("clap requires exactly one label intent"),
    };
    let client = optional_github_client()?;
    let result = metadata_mutation::update_or_queue(
        client.as_ref(),
        MetadataRequest::GenericLabel {
            issue: &issue,
            label,
            desired,
        },
    )?;
    print_metadata_mutation(&result, json)
}

fn mutate_parent_relationship(
    parent: &str,
    add: Option<String>,
    remove: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let parent = PendingIssueReference::parse(parent)?;
    let (child, desired) = match (add, remove) {
        (Some(child), None) => (PendingIssueReference::parse(&child)?, SetPresence::Present),
        (None, Some(child)) => (PendingIssueReference::parse(&child)?, SetPresence::Absent),
        _ => unreachable!("clap requires exactly one sub-Issue intent"),
    };
    let client = optional_github_client()?;
    let result = metadata_mutation::update_or_queue(
        client.as_ref(),
        MetadataRequest::ParentRelationship {
            parent: &parent,
            child: &child,
            desired,
        },
    )?;
    print_metadata_mutation(&result, json)
}

fn optional_github_client() -> Result<Option<GitHubClient>, CliError> {
    match github_client() {
        Ok(client) => Ok(Some(client)),
        Err(CliError::Auth(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn print_metadata_mutation(
    result: &metadata_mutation::MetadataMutationResult,
    json: bool,
) -> Result<(), CliError> {
    let (pending, result_name, operation, working_graph) = match &result.outcome {
        MetadataMutationOutcome::Synchronized { change } => {
            (false, metadata_change_name(*change), None, None)
        }
        MetadataMutationOutcome::Queued {
            operation,
            working_input_hash,
        } => (
            true,
            "pending",
            Some(MetadataOperationOutput {
                id: operation.id(),
                depends_on: operation.depends_on().to_vec(),
            }),
            Some(WorkingGraphSummary {
                input_hash: working_input_hash,
            }),
        ),
    };
    let (label, relationship) = match &result.target {
        MetadataSetTarget::GenericLabel { label, .. } => (Some(label.as_str()), None),
        MetadataSetTarget::ParentRelationship { parent, child } => (
            None,
            Some(ParentRelationshipOutput {
                parent: operand_key(&result.replica.repository, *parent),
                child: operand_key(&result.replica.repository, *child),
                kind: "parent_of",
            }),
        ),
    };
    let output = MetadataMutationOutput {
        schema_version: METADATA_MUTATION_SCHEMA_VERSION,
        command: if label.is_some() {
            "label"
        } else {
            "sub-issue"
        },
        repository: &result.replica.repository,
        result: result_name,
        pending,
        label,
        relationship,
        desired_present: result.desired.is_present(),
        operation,
        working_graph,
        snapshot: snapshot_summary(&result.replica),
    };
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else if pending {
        println!(
            "Queued {} as Pending mutation {}",
            output.command,
            output
                .operation
                .as_ref()
                .expect("Pending metadata output has an operation")
                .id
        );
    } else {
        println!("{}: {}", output.command, output.result);
    }
    Ok(())
}

fn operand_key(repository: &str, operand: PendingIssueOperand) -> String {
    operand
        .temporary_id()
        .map(|temporary_id| temporary_id.stable_node_key(repository))
        .unwrap_or_else(|| format!("{repository}#{}", operand.number()))
}

fn metadata_change_name(change: crate::github::MetadataChange) -> &'static str {
    match change {
        crate::github::MetadataChange::Added => "added",
        crate::github::MetadataChange::AlreadyPresent => "already_present",
        crate::github::MetadataChange::Removed => "removed",
        crate::github::MetadataChange::AlreadyAbsent => "already_absent",
    }
}

fn create_issue(
    repository: &Repository,
    title: String,
    body: String,
    json: bool,
) -> Result<(), CliError> {
    let result = issue_create::queue(repository, title, body)?;
    let output = IssueCreateOutput {
        schema_version: ISSUE_CREATE_SCHEMA_VERSION,
        command: "create",
        repository: repository.full_name(),
        pending: true,
        draft: DraftIssueOutput {
            temporary_id: result.temporary_id,
            stable_node_key: &result.stable_node_key,
            key: &result.key,
        },
        operation: DraftCreateOperationOutput {
            id: result.operation.id(),
            kind: "issue_create",
        },
        working_graph: WorkingGraphSummary {
            input_hash: &result.working_input_hash,
        },
        snapshot: snapshot_summary(&result.replica),
    };
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "Queued Draft Issue {} as Pending mutation {}",
            output.draft.key, output.operation.id
        );
    }
    Ok(())
}

fn create_comment(issue: &str, body: String, json: bool) -> Result<(), CliError> {
    let reference = PendingIssueReference::parse(issue)?;
    let queued = comment_create::queue(&reference, body)?;
    let operation_id = queued.operation.id().to_owned();
    let mut replica = queued.replica;
    let mut pending = true;
    let mut result_name = "pending";
    let mut remote = None;
    let mut warning = None;
    let mut working_input_hash = Some(queued.working_input_hash);

    if let Some(client) = optional_github_client()? {
        match reconciliation::reconcile(&client, reference.repository()) {
            Ok(reconciled) => {
                let operation = reconciled
                    .operations
                    .iter()
                    .find(|operation| operation.id == operation_id)
                    .expect("the queued comment participates in its immediate reconciliation");
                if let reconciliation::OperationDetails::CommentCreate { remote: created } =
                    &operation.details
                {
                    remote = created.clone();
                }
                match operation.outcome {
                    reconciliation::Outcome::Applied => {
                        pending = false;
                        result_name = "created";
                    }
                    reconciliation::Outcome::AlreadySatisfied => {
                        pending = false;
                        result_name = "recovered";
                    }
                    reconciliation::Outcome::Checkpointed => {
                        pending = false;
                        result_name = "created";
                    }
                    reconciliation::Outcome::Failed
                    | reconciliation::Outcome::Conflicting
                    | reconciliation::Outcome::TransitivelyBlocked
                    | reconciliation::Outcome::ResolvedRemote => {
                        warning.clone_from(&operation.error);
                    }
                }
                replica = reconciled.replica;
                if pending {
                    let outbox = OutboxStore::discover(reference.repository())?
                        .load(reference.repository())?;
                    working_input_hash = Some(
                        WorkingGraph::project(&replica, &outbox)?
                            .input_hash()
                            .to_owned(),
                    );
                } else {
                    working_input_hash = None;
                }
            }
            Err(error) => {
                warning = Some(format!(
                    "GitHub reconciliation was unavailable; comment remains Pending: {error}"
                ));
            }
        }
    }

    let output = CommentCreateOutput {
        schema_version: COMMENT_CREATE_SCHEMA_VERSION,
        command: "comment",
        repository: &replica.repository,
        issue: &queued.issue_key,
        body: &queued.body,
        result: result_name,
        pending,
        operation: CommentCreateOperationOutput {
            id: &operation_id,
            kind: "comment_create",
            depends_on: queued.operation.depends_on(),
        },
        remote,
        working_graph: working_input_hash
            .as_deref()
            .map(|input_hash| WorkingGraphSummary { input_hash }),
        snapshot: snapshot_summary(&replica),
        warning,
    };
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else if output.pending {
        println!(
            "Queued comment on {} as Pending mutation {}",
            output.issue, output.operation.id
        );
        if let Some(warning) = &output.warning {
            eprintln!("warning: {warning}");
        }
    } else {
        println!("Created comment on {}", output.issue);
    }
    Ok(())
}

fn issue_field_request(
    title: Option<String>,
    body: Option<String>,
    state: Option<IssueStateArgument>,
    assignees: Vec<String>,
    clear_assignees: bool,
) -> (IssueField, IssueFieldValue) {
    if let Some(title) = title {
        (IssueField::Title, IssueFieldValue::title(title))
    } else if let Some(body) = body {
        (IssueField::Body, IssueFieldValue::body(body))
    } else if let Some(state) = state {
        (IssueField::State, IssueFieldValue::state(state.into()))
    } else if !assignees.is_empty() {
        (IssueField::Assignees, IssueFieldValue::assignees(assignees))
    } else if clear_assignees {
        (
            IssueField::Assignees,
            IssueFieldValue::assignees(Vec::new()),
        )
    } else {
        unreachable!("clap requires exactly one Issue-field update")
    }
}

fn resolution_choice(
    remote: bool,
    local: bool,
    priority: Option<PrioritySelection>,
) -> ResolutionChoice {
    match (remote, local, priority) {
        (true, false, None) => ResolutionChoice::Remote,
        (false, true, None) => ResolutionChoice::Local,
        (false, false, Some(priority)) => ResolutionChoice::Replacement(priority),
        _ => unreachable!("clap requires exactly one resolution choice"),
    }
}

fn reconcile(repository: &Repository, json: bool) -> Result<(), CliError> {
    let client = github_client()?;
    let result = reconciliation::reconcile(&client, repository)?;
    print_reconciliation(&result, "reconcile", json)
}

fn resolve_conflict(
    repository: &Repository,
    operation_id: &str,
    choice: ResolutionChoice,
    json: bool,
) -> Result<(), CliError> {
    let client = github_client()?;
    let result = reconciliation::resolve(&client, repository, operation_id, choice)?;
    if json {
        let output = ResolveOutput {
            schema_version: RESOLVE_SCHEMA_VERSION,
            command: "resolve",
            repository: &result.reconciliation.repository,
            resolution: ResolutionOutput {
                operation_id: &result.operation_id,
                choice: result.choice,
            },
            operations: &result.reconciliation.operations,
            summary: &result.reconciliation.summary,
            snapshot: snapshot_summary(&result.reconciliation.replica),
        };
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "Resolved {} with --{}; reconciliation applied {}, found {} conflict(s), and left {} Pending operation(s)",
            result.operation_id,
            result.choice,
            result.reconciliation.summary.applied,
            result.reconciliation.summary.conflicting,
            result.reconciliation.summary.remaining
        );
    }
    Ok(())
}

fn print_reconciliation(
    result: &reconciliation::ReconciliationResult,
    command: &'static str,
    json: bool,
) -> Result<(), CliError> {
    if json {
        let output = ReconcileOutput {
            schema_version: reconciliation::OUTPUT_SCHEMA_VERSION,
            command,
            repository: &result.repository,
            operations: &result.operations,
            summary: &result.summary,
            snapshot: snapshot_summary(&result.replica),
        };
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "Reconciled {}: {} applied, {} already satisfied, {} conflict(s), {} transitively blocked, {} Pending",
            result.repository,
            result.summary.applied,
            result.summary.already_satisfied,
            result.summary.conflicting,
            result.summary.transitively_blocked,
            result.summary.remaining
        );
        for operation in &result.operations {
            if operation.classification == reconciliation::Classification::Conflicting {
                match &operation.details {
                    reconciliation::OperationDetails::PriorityUpdate {
                        base,
                        local,
                        remote,
                    } => println!(
                        "{} Issue #{} conflict: base {}, local {}, remote {}",
                        operation.id,
                        operation
                            .issue_number
                            .expect("Priority conflicts have a GitHub Issue number"),
                        base.to_state().display_name(),
                        local.to_state().display_name(),
                        remote
                            .as_ref()
                            .map(LogicalPriority::to_state)
                            .map(|priority| priority.display_name())
                            .unwrap_or("missing")
                    ),
                    reconciliation::OperationDetails::IssueFieldUpdate {
                        field,
                        base,
                        local,
                        remote,
                    } => println!(
                        "{} Issue #{} {} conflict: base {}, local {}, remote {}",
                        operation.id,
                        operation
                            .issue_number
                            .expect("Issue-field conflicts have an Issue number"),
                        field.name(),
                        serde_json::to_string(base).expect("Issue-field values serialize"),
                        serde_json::to_string(local).expect("Issue-field values serialize"),
                        remote
                            .as_ref()
                            .map(|value| serde_json::to_string(value)
                                .expect("Issue-field values serialize"))
                            .unwrap_or_else(|| "missing".to_owned())
                    ),
                    reconciliation::OperationDetails::IssueCreate { .. }
                    | reconciliation::OperationDetails::CommentCreate { .. }
                    | reconciliation::OperationDetails::DependencyUpdate { .. }
                    | reconciliation::OperationDetails::MetadataSetUpdate { .. } => {}
                }
            }
        }
    }
    Ok(())
}
fn initialize(repository: &Repository, json: bool) -> Result<(), CliError> {
    let client = github_client()?;
    let labels = client.fetch_labels(repository)?;
    let mut already_present: Vec<_> = present_canonical_labels(&labels)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut created_labels = Vec::new();
    for priority in DeclaredPriority::ALL {
        let spec = priority.spec();
        if labels
            .iter()
            .any(|label| label.name.eq_ignore_ascii_case(spec.name))
        {
            continue;
        }
        let request = CreateLabelRequest::new(spec.name, spec.color, spec.description);
        match client.create_label(repository, &request)? {
            LabelCreation::Created => created_labels.push(spec.name.to_owned()),
            LabelCreation::AlreadyPresent => already_present.push(spec.name.to_owned()),
        }
    }
    already_present.sort();
    let output = InitOutput {
        schema_version: INIT_SCHEMA_VERSION,
        command: "init",
        repository: repository.full_name(),
        created_labels,
        already_present,
    };
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else if output.created_labels.is_empty() {
        println!(
            "Priority labels are already initialized in {}",
            repository.full_name()
        );
    } else {
        println!(
            "Created {} in {}",
            output.created_labels.join(", "),
            repository.full_name()
        );
    }
    Ok(())
}

fn update_issue_field(
    issue: &str,
    field: IssueField,
    desired: IssueFieldValue,
    json: bool,
) -> Result<(), CliError> {
    let reference = PendingIssueReference::parse(issue)?;
    let client = match github_client() {
        Ok(client) => Some(client),
        Err(CliError::Auth(_)) => None,
        Err(error) => return Err(error),
    };
    let result = issue_field::update_or_queue(client.as_ref(), &reference, field, desired)?;
    print_issue_field_update(&result, json)
}

fn print_issue_field_update(
    result: &issue_field::IssueFieldUpdateResult,
    json: bool,
) -> Result<(), CliError> {
    let (status, pending, issue_url, operation, working_graph) = match &result.outcome {
        issue_field::IssueFieldUpdateOutcome::Synchronized { issue_url } => {
            ("synchronized", false, Some(issue_url.as_str()), None, None)
        }
        issue_field::IssueFieldUpdateOutcome::Queued {
            issue_url,
            operation,
            working_input_hash,
        } => (
            "pending",
            true,
            issue_url.as_deref(),
            Some(IssueFieldOperationOutput {
                id: operation.id(),
                kind: "issue_field_update",
                depends_on: operation.depends_on().to_vec(),
            }),
            Some(WorkingGraphSummary {
                input_hash: working_input_hash,
            }),
        ),
    };
    let output = IssueFieldUpdateOutput {
        schema_version: ISSUE_FIELD_UPDATE_SCHEMA_VERSION,
        command: "update",
        status,
        pending,
        repository: &result.replica.repository,
        issue: IssueFieldIssueOutput {
            key: &result.issue_key,
            number: issue_url.map(|_| result.issue_number),
            temporary_id: result.temporary_id,
            url: issue_url,
        },
        field: result.field,
        base: &result.base,
        local: &result.desired,
        operation,
        working_graph,
        snapshot: snapshot_summary(&result.replica),
    };
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else if pending {
        println!(
            "Queued {} {} update as Pending mutation {}",
            output.issue.key,
            output.field.name(),
            output
                .operation
                .as_ref()
                .expect("Pending field output includes an operation")
                .id
        );
    } else {
        println!("Updated {} {}", output.issue.key, output.field.name());
    }
    Ok(())
}

fn triage_command(
    repository: &Repository,
    assignee: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    let (replica, source) = refresh_for_relationships(repository, &[])?;
    let scope = assignee
        .map(ExecutionScope::Assignee)
        .unwrap_or(ExecutionScope::Available);
    let report = triage::analyze(&replica, scope);
    let warnings: Vec<_> = source.warning().into_iter().collect();
    if json {
        let output = TriageOutput {
            schema_version: TRIAGE_SCHEMA_VERSION,
            command: "triage",
            repository: &replica.repository,
            source,
            synced_at: &replica.synced_at,
            input_hash: &replica.input_hash,
            execution_scope: scope.description(),
            report,
            warnings,
        };
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "Triage diagnostics in {} (scope {}, synced_at {}):",
            replica.repository,
            execution_scope_name(assignee),
            replica.synced_at
        );
        let lines = report.human_lines();
        if lines.is_empty() {
            println!("No actionable graph problems");
        } else {
            for line in lines {
                println!("{line}");
            }
        }
        for warning in warnings {
            eprintln!("warning: {}", warning.message);
        }
    }
    Ok(())
}

fn update_priority(issue: &str, requested: PrioritySelection, json: bool) -> Result<(), CliError> {
    let reference = PendingIssueReference::parse(issue)?;
    let resolved = crate::draft_identity::resolve_reference(&reference)?;
    let Some(issue) = resolved.as_github() else {
        return queue_priority_update(
            &reference,
            requested,
            "Draft Issue requires reconciliation".to_owned(),
            json,
        );
    };
    if OutboxStore::discover(issue.repository())?
        .load(issue.repository())?
        .latest_priority_operation_for_issue(issue.number())
        .is_some()
    {
        return queue_priority_update(
            &reference,
            requested,
            "Earlier Priority intent is pending".to_owned(),
            json,
        );
    }
    let client = match github_client() {
        Ok(client) => client,
        Err(CliError::Auth(source)) => {
            return queue_priority_update(&reference, requested, source.to_string(), json);
        }
        Err(error) => return Err(error),
    };
    let result = match priority_update::update(&client, issue, requested) {
        Ok(result) => result,
        Err(source) if source.permits_offline_queue() => {
            return queue_priority_update(&reference, requested, source.to_string(), json);
        }
        Err(source) => return Err(source.into()),
    };
    let output = PriorityUpdateOutput {
        schema_version: PRIORITY_UPDATE_SCHEMA_VERSION,
        command: "update",
        status: PriorityUpdateStatus::Synchronized,
        pending: false,
        repository: &result.replica.repository,
        issue: PriorityIssueOutput {
            key: &result.issue_key,
            number: Some(result.issue_number),
            temporary_id: reference.temporary_id(),
            url: &result.issue_url,
        },
        previous_priority: result.previous_priority,
        resulting_priority: result.resulting_priority,
        operation: None,
        working_graph: None,
        snapshot: snapshot_summary(&result.replica),
    };
    print_priority_update(output, json)
}

fn queue_priority_update(
    issue: &PendingIssueReference,
    requested: PrioritySelection,
    online_failure: String,
    json: bool,
) -> Result<(), CliError> {
    let result = priority_update::queue(issue, requested).map_err(|queue| {
        CliError::OnlinePriorityUpdateAndQueueFailed {
            online: online_failure,
            queue,
        }
    })?;
    let output = PriorityUpdateOutput {
        schema_version: PRIORITY_UPDATE_SCHEMA_VERSION,
        command: "update",
        status: PriorityUpdateStatus::Pending,
        pending: true,
        repository: &result.replica.repository,
        issue: PriorityIssueOutput {
            key: &result.issue_key,
            number: result.issue_number,
            temporary_id: result.temporary_id,
            url: &result.issue_url,
        },
        previous_priority: result.previous_priority,
        resulting_priority: result.resulting_priority,
        operation: Some(PendingOperationOutput::from(&result.operation)),
        working_graph: Some(WorkingGraphSummary {
            input_hash: &result.working_input_hash,
        }),
        snapshot: snapshot_summary(&result.replica),
    };
    print_priority_update(output, json)
}

fn print_priority_update(output: PriorityUpdateOutput<'_>, json: bool) -> Result<(), CliError> {
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else if matches!(output.status, PriorityUpdateStatus::Pending) {
        println!(
            "Queued {} Priority from {} to {} as Pending mutation {}",
            output.issue.key,
            output.previous_priority.display_name(),
            output.resulting_priority.display_name(),
            output
                .operation
                .as_ref()
                .expect("Pending output includes an operation")
                .id
        );
    } else {
        println!(
            "Updated {} Priority from {} to {}",
            output.issue.key,
            output.previous_priority.display_name(),
            output.resulting_priority.display_name()
        );
    }
    Ok(())
}

fn sync(repository: &Repository, json: bool) -> Result<(), CliError> {
    let replica = synchronize(repository)?;
    print_sync_result(&replica, json)?;
    Ok(())
}

fn next(
    repository: &Repository,
    arguments: &ScopeArguments,
    horizon: u8,
    json: bool,
    profile: bool,
) -> Result<(), CliError> {
    if !(ranking::MIN_HORIZON..=ranking::MAX_HORIZON).contains(&horizon) {
        return Err(CliError::UnsupportedNextHorizon(horizon));
    }
    let (replica, source) = refresh_for_scope(repository, arguments)?;
    let outbox = OutboxStore::discover(repository)?.load(repository)?;
    let working = WorkingGraph::project(&replica, &outbox)?;
    let selection = arguments.selection(&working)?;
    let scope = arguments.scope(&selection);
    let store = ReplicaStore::discover(repository)?;
    let mut cache = ranking::RankingCache::at(store.repository_directory());
    let run = ranking::analyze_profiled(&working, scope, horizon, &mut cache);
    let analysis = run.analysis;
    let analysis_serialization = (profile && json).then(|| {
        let serialization_started = Instant::now();
        let _ = serde_json::to_vec(&analysis).expect("Next analysis is serializable");
        serialization_started.elapsed()
    });
    let performance =
        profile.then(|| PerformanceOutput::from_profile(run.profile, analysis_serialization));
    let warnings = analysis_warnings(&working, source);
    if json {
        let output = NextOutput {
            schema_version: ranking::OUTPUT_SCHEMA_VERSION,
            policy_version: ranking::POLICY_VERSION,
            command: "next",
            repository: &replica.repository,
            source,
            synced_at: &replica.synced_at,
            replica_snapshot_hash: &replica.input_hash,
            execution_scope: scope.description(),
            analysis,
            warnings,
            performance,
        };
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "next/v1 recommendation in {} (synced_at {}):",
            replica.repository, replica.synced_at
        );
        match analysis.human_recommendation_summary() {
            Some(recommendation) => println!("{recommendation}"),
            None => println!("{}", analysis.summary().human_empty_summary()),
        }
        if let Some(warning) = analysis.truncation_warning() {
            eprintln!("warning: {warning}");
        }
        for warning in &warnings {
            print_warning(warning);
        }
        if let Some(performance) = performance {
            eprintln!("{}", performance.human_summary());
        }
    }
    Ok(())
}

fn synchronize(repository: &Repository) -> Result<LocalReplica, CliError> {
    let client = github_client_for_sync(repository)?;
    synchronize_with_client(repository, &client)
}

fn synchronize_with_client(
    repository: &Repository,
    client: &GitHubClient,
) -> Result<LocalReplica, CliError> {
    synchronize_with_relationships(repository, client, &[])
}

fn synchronize_with_relationships(
    repository: &Repository,
    client: &GitHubClient,
    requested: &[String],
) -> Result<LocalReplica, CliError> {
    let journal = crate::sync_diagnostics::Journal::begin(
        repository,
        crate::sync_diagnostics::Stage::Refreshing,
    )?;
    let observer = journal.clone();
    client.observe_pages(Some(Box::new(move |items| observer.page(items))));
    let result = synchronize_observed(repository, client, requested, &journal);
    client.observe_pages(None);
    journal.finish(result.as_ref().map_err(sync_failure));
    result
}

fn synchronize_observed(
    repository: &Repository,
    client: &GitHubClient,
    requested: &[String],
    journal: &crate::sync_diagnostics::Journal,
) -> Result<LocalReplica, CliError> {
    let store = ReplicaStore::discover(repository)?;
    let previous = match store.load(repository) {
        Ok(replica) => Some(replica),
        Err(StoreError::MissingReplica | StoreError::Decode(_) | StoreError::InvalidReplica(_)) => {
            None
        }
        Err(error) => return Err(error.into()),
    };
    let replica =
        replica_sync::refresh_with_relationships(client, repository, previous.as_ref(), requested)?;
    journal.stage(crate::sync_diagnostics::Stage::Validating, Some(&replica));
    replica.validate(repository.full_name())?;
    journal.stage(crate::sync_diagnostics::Stage::Publishing, None);
    store.publish(&replica)?;
    Ok(replica)
}

fn github_client_for_sync(repository: &Repository) -> Result<GitHubClient, CliError> {
    // Reject unsafe configuration before creating any local state.
    api_base_url()?;
    let journal = crate::sync_diagnostics::Journal::begin(
        repository,
        crate::sync_diagnostics::Stage::Connecting,
    )?;
    let result = github_client();
    if let Err(error) = &result {
        journal.finish(Err(sync_failure(error)));
    }
    result
}

fn sync_failure(error: &CliError) -> crate::sync_diagnostics::Failure {
    use crate::sync_diagnostics::Failure;
    match error {
        CliError::Auth(_) => Failure::new("authentication", "GitHub credentials are unavailable"),
        CliError::GitHub(error) | CliError::ReplicaSync(ReplicaSyncError::GitHub(error)) => {
            Failure::github(error)
        }
        CliError::Store(_) | CliError::ReplicaSync(ReplicaSyncError::Store(_)) => Failure::new(
            "persistence",
            "Could not read or publish local synchronization state",
        ),
        CliError::Replica(_) | CliError::ReplicaSync(ReplicaSyncError::Replica(_)) => Failure::new(
            "invalid_replica",
            "The synchronization candidate did not pass validation",
        ),
        _ => Failure::new("configuration", "Could not initialize synchronization"),
    }
}

fn github_client() -> Result<GitHubClient, CliError> {
    let base_url = api_base_url()?;
    let hostname = env::var("HYFA_GITHUB_HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| authentication_hostname(&base_url));
    let token = AuthToken::discover(&hostname, &base_url)?;
    GitHubClient::new(base_url, &token).map_err(Into::into)
}

fn mutate_dependency(
    blocked: &str,
    blocker: &str,
    intent: DependencyIntent,
    json: bool,
) -> Result<(), CliError> {
    let blocked = PendingIssueReference::parse(blocked)?;
    let blocker = PendingIssueReference::parse(blocker)?;
    let (Some(github_blocked), Some(github_blocker)) = (blocked.as_github(), blocker.as_github())
    else {
        return queue_dependency_update(
            &blocked,
            &blocker,
            intent,
            "Draft Issue identity requires reconciliation".to_owned(),
            json,
        );
    };
    let client = match github_client() {
        Ok(client) => client,
        Err(CliError::Auth(source)) => {
            return queue_dependency_update(&blocked, &blocker, intent, source.to_string(), json);
        }
        Err(error) => return Err(error),
    };
    let result = match client.mutate_dependency(github_blocked, github_blocker, intent) {
        Ok(result) => result,
        Err(source) if source.permits_offline_queue() => {
            return queue_dependency_update(&blocked, &blocker, intent, source.to_string(), json);
        }
        Err(source) => return Err(source.into()),
    };

    let replica = replica_sync::fetch(&client, github_blocked.repository())
        .map_err(|source| CliError::MutationSynchronization { source })?;
    if replica.has_dependency(github_blocked, github_blocker) != intent.desired_present() {
        return Err(CliError::MutationReadbackMismatch {
            blocked: blocked.stable_key(),
            blocker: blocker.stable_key(),
            expected: dependency_expected_relationship(intent),
        });
    }
    ReplicaStore::discover(github_blocked.repository())
        .map_err(|source| CliError::MutationPublication { source })?
        .publish(&replica)
        .map_err(|source| CliError::MutationPublication { source })?;

    let snapshot = snapshot_summary(&replica);
    let output = DependencyMutationOutput {
        schema_version: DEPENDENCY_MUTATION_SCHEMA_VERSION,
        command: dependency_command_name(intent),
        repository: github_blocked.repository().full_name(),
        result: dependency_result_name(result),
        pending: false,
        edge: DependencyEdgeOutput {
            blocked: blocked.stable_key(),
            blocker: blocker.stable_key(),
            kind: "blocked_by",
        },
        operation: None,
        working_graph: None,
        snapshot,
    };
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!("{}", dependency_human_message(&output.edge, result));
    }
    Ok(())
}

fn queue_dependency_update(
    blocked: &PendingIssueReference,
    blocker: &PendingIssueReference,
    intent: DependencyIntent,
    online_failure: String,
    json: bool,
) -> Result<(), CliError> {
    let result = dependency_update::queue(
        blocked,
        blocker,
        DependencyPresence::from_present(intent.desired_present()),
    )
    .map_err(|queue| CliError::OnlineDependencyUpdateAndQueueFailed {
        online: online_failure,
        queue,
    })?;
    let output = DependencyMutationOutput {
        schema_version: DEPENDENCY_MUTATION_SCHEMA_VERSION,
        command: dependency_command_name(intent),
        repository: &result.replica.repository,
        result: "pending",
        pending: true,
        edge: DependencyEdgeOutput {
            blocked: blocked.stable_key(),
            blocker: blocker.stable_key(),
            kind: "blocked_by",
        },
        operation: Some(PendingDependencyOperationOutput::from(&result.operation)),
        working_graph: Some(WorkingGraphSummary {
            input_hash: &result.working_input_hash,
        }),
        snapshot: snapshot_summary(&result.replica),
    };
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "Queued {} as Pending mutation {}",
            dependency_human_pending_message(&output.edge, intent),
            output
                .operation
                .as_ref()
                .expect("Pending output includes an operation")
                .id
        );
    }
    Ok(())
}

fn print_sync_result(replica: &LocalReplica, json: bool) -> Result<(), CliError> {
    let snapshot = snapshot_summary(replica);
    if json {
        let output = SyncOutput {
            schema_version: SYNC_SCHEMA_VERSION,
            command: "sync",
            repository: &replica.repository,
            snapshot,
        };
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "Synchronized {}: {} Issues, {} comments, {} Dependencies (synced_at {})",
            replica.repository,
            snapshot.issue_count,
            snapshot.comment_count,
            snapshot.dependency_count,
            snapshot.synced_at
        );
    }
    Ok(())
}

fn snapshot_summary(replica: &LocalReplica) -> SnapshotSummary<'_> {
    let comment_count = replica
        .issues
        .iter()
        .map(|issue| issue.comments.len())
        .sum();
    SnapshotSummary {
        schema_version: &replica.schema_version,
        synced_at: &replica.synced_at,
        input_hash: &replica.input_hash,
        issue_count: replica.issues.len(),
        comment_count,
        dependency_count: replica.dependencies.len(),
    }
}

fn ready(repository: &Repository, arguments: &ScopeArguments, json: bool) -> Result<(), CliError> {
    let (replica, source) = refresh_for_scope(repository, arguments)?;
    let outbox = OutboxStore::discover(repository)?.load(repository)?;
    let working = WorkingGraph::project(&replica, &outbox)?;
    let selection = arguments.selection(&working)?;
    let scope = arguments.scope(&selection);
    let analysis = analyze_ready(working.replica(), scope);
    let warnings = analysis_warnings(&working, source);
    let issues: Vec<_> = analysis
        .executable
        .iter()
        .map(|issue| ReadyIssue {
            provenance: working.provenance_for_issue(issue.number),
            key: issue.display_key(&replica.repository),
            number: (!issue.is_draft()).then_some(issue.number),
            temporary_id: issue.temporary_id(),
            url: &issue.url,
            title: &issue.title,
            ready: true,
            available: issue.assignees.is_empty(),
            priority: working.priority(issue),
            labels: issue
                .labels
                .iter()
                .map(|label| label.name.as_str())
                .collect(),
            assignees: issue
                .assignees
                .iter()
                .map(|actor| actor.login.as_str())
                .collect(),
        })
        .collect();
    let input_hash = ranking::effective_input_hash(&working, scope);
    let output = ReadyOutput {
        schema_version: READY_SCHEMA_VERSION,
        command: "ready",
        repository: &replica.repository,
        source,
        synced_at: &replica.synced_at,
        replica_snapshot_hash: &replica.input_hash,
        input_hash: &input_hash,
        pending: working.is_pending(),
        pending_operation_ids: working.operation_ids(),
        execution_scope: scope.description(),
        issues,
        summary: ReadySummary {
            empty_reason: crate::execution_scope::empty_reason(&analysis),
            operational_issue_count: analysis.operational_issue_count,
            ready_count: analysis.ready_count,
            executable_count: analysis.executable.len(),
            assigned_ready_count: analysis.assigned_ready_count,
            blocked_count: analysis.blocked_count,
        },
        warnings,
    };
    if json {
        serde_json::to_writer(std::io::stdout().lock(), &output).map_err(CliError::EncodeOutput)?;
        println!();
    } else {
        println!(
            "Executable Issues in {} (synced_at {}):",
            replica.repository, replica.synced_at
        );
        for issue in &output.issues {
            println!(
                "{} {} [{}]",
                issue.key,
                issue.title,
                issue.priority.display_name()
            );
        }
        for warning in &output.warnings {
            print_warning(warning);
        }
    }
    Ok(())
}

fn refresh_or_local_with_client(
    repository: &Repository,
    client: &GitHubClient,
) -> Result<(LocalReplica, ReplicaSource), CliError> {
    let refresh = synchronize_with_client(repository, client);
    refresh_or_local_after(repository, refresh)
}

fn refresh_or_local_after(
    repository: &Repository,
    refresh: Result<LocalReplica, CliError>,
) -> Result<(LocalReplica, ReplicaSource), CliError> {
    match refresh {
        Ok(replica) => Ok((replica, ReplicaSource::Live)),
        Err(refresh_error) => {
            let store = ReplicaStore::discover(repository)?;
            match store.load(repository) {
                Ok(replica) => Ok((replica, ReplicaSource::LocalFallback)),
                Err(replica_error) => Err(CliError::RefreshAndReplicaUnavailable {
                    refresh: refresh_error.to_string(),
                    replica: replica_error.to_string(),
                }),
            }
        }
    }
}

fn analysis_warnings(working: &WorkingGraph<'_>, source: ReplicaSource) -> Vec<ReadyWarning> {
    let replica = working.replica();
    let mut warnings = Vec::new();
    if let Some(repository_labels) = replica.repository_labels.as_deref() {
        let missing_labels: Vec<_> = missing_canonical_labels(repository_labels)
            .into_iter()
            .map(str::to_owned)
            .collect();
        if !missing_labels.is_empty() {
            warnings.push(ReadyWarning {
                code: "missing_priority_labels",
                message: "Repository is missing canonical Priority labels".to_owned(),
                issue_number: None,
                labels: missing_labels,
            });
        }
    }
    for issue in replica
        .issues
        .iter()
        .filter(|issue| issue.state.eq_ignore_ascii_case("open"))
    {
        let priority = working.priority(issue);
        if let Some(labels) = priority.conflict_labels() {
            warnings.push(ReadyWarning {
                code: "priority_conflict",
                message: format!(
                    "Issue #{} has multiple canonical Priority labels",
                    issue.number
                ),
                issue_number: Some(issue.number),
                labels: labels.to_vec(),
            });
        }
    }
    if let Some(mut warning) = source.warning() {
        if let Ok(repository) = Repository::parse(&replica.repository)
            && let Ok(Some(attempt)) = crate::sync_diagnostics::load(&repository)
            && let Some(failure) = attempt.failure
        {
            warning
                .message
                .push_str(&format!("; {} ({})", failure.message, failure.code));
        }
        warnings.push(warning);
    }
    warnings
}

fn print_warning(warning: &ReadyWarning) {
    if warning.labels.is_empty() {
        eprintln!("warning: {}", warning.message);
    } else {
        eprintln!(
            "warning: {} ({})",
            warning.message,
            warning.labels.join(", ")
        );
    }
}

fn execution_scope_name(assignee: Option<&str>) -> String {
    assignee
        .map(|assignee| format!("assignee:{assignee}"))
        .unwrap_or_else(|| "available".to_owned())
}

fn api_base_url() -> Result<Url, CliError> {
    let raw =
        env::var("HYFA_GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com/".to_owned());
    let mut parsed = Url::parse(&raw).map_err(CliError::ParseApiBase)?;
    if parsed.cannot_be_a_base()
        || parsed.host_str().is_none()
        || !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(CliError::InvalidApiBase);
    }
    if !parsed.path().ends_with('/') {
        let path = format!("{}/", parsed.path());
        parsed.set_path(&path);
    }
    Ok(parsed)
}

fn authentication_hostname(base_url: &Url) -> String {
    match base_url
        .host_str()
        .expect("validated API base URL has a host")
    {
        host if host.eq_ignore_ascii_case("api.github.com") => "github.com".to_owned(),
        host => host.to_owned(),
    }
}

#[derive(Serialize)]
struct SyncOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    snapshot: SnapshotSummary<'a>,
}

#[derive(Serialize)]
struct DependencyMutationOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    result: &'static str,
    pending: bool,
    edge: DependencyEdgeOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<PendingDependencyOperationOutput<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_graph: Option<WorkingGraphSummary<'a>>,
    snapshot: SnapshotSummary<'a>,
}

#[derive(Serialize)]
struct PendingDependencyOperationOutput<'a> {
    id: &'a str,
    kind: &'static str,
    desired_present: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
}

impl<'a> From<&'a PendingMutation> for PendingDependencyOperationOutput<'a> {
    fn from(operation: &'a PendingMutation) -> Self {
        let (_, desired) = operation
            .dependency_values()
            .expect("Dependency output is built from a Dependency mutation");
        Self {
            id: operation.id(),
            kind: "dependency_update",
            desired_present: desired.is_present(),
            depends_on: operation.depends_on().to_vec(),
        }
    }
}

#[derive(Serialize)]
struct ReconcileOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    operations: &'a [reconciliation::OperationResult],
    summary: &'a reconciliation::ReconciliationSummary,
    snapshot: SnapshotSummary<'a>,
}

#[derive(Serialize)]
struct ResolveOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    resolution: ResolutionOutput<'a>,
    operations: &'a [reconciliation::OperationResult],
    summary: &'a reconciliation::ReconciliationSummary,
    snapshot: SnapshotSummary<'a>,
}

#[derive(Serialize)]
struct ResolutionOutput<'a> {
    operation_id: &'a str,
    choice: &'static str,
}

#[derive(Serialize)]
struct GraphOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    source: ReplicaSource,
    synced_at: &'a str,
    input_hash: &'a str,
    output: &'a str,
    artifact: GraphArtifactSummary<'a>,
}

#[derive(Serialize)]
struct GraphArtifactSummary<'a> {
    schema_version: &'static str,
    node_count: usize,
    edge_count: usize,
    artifact_hash: &'a str,
}

#[derive(Serialize)]
struct DependencyEdgeOutput {
    blocked: String,
    blocker: String,
    kind: &'static str,
}

#[derive(Serialize)]
struct InitOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    created_labels: Vec<String>,
    already_present: Vec<String>,
}

#[derive(Serialize)]
struct TriageOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    source: ReplicaSource,
    synced_at: &'a str,
    input_hash: &'a str,
    execution_scope: crate::execution_scope::ScopeDescription,
    #[serde(flatten)]
    report: TriageReport,
    warnings: Vec<ReadyWarning>,
}

#[derive(Serialize)]
struct NextOutput<'a> {
    schema_version: &'static str,
    policy_version: &'static str,
    command: &'static str,
    repository: &'a str,
    source: ReplicaSource,
    synced_at: &'a str,
    replica_snapshot_hash: &'a str,
    execution_scope: crate::execution_scope::ScopeDescription,
    #[serde(flatten)]
    analysis: NextAnalysis,
    warnings: Vec<ReadyWarning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    performance: Option<PerformanceOutput>,
}

#[derive(Serialize)]
struct PerformanceOutput {
    unit: &'static str,
    graph_preparation: u128,
    scc_detection: u128,
    readiness: u128,
    cache_lookup: u128,
    pagerank: u128,
    search: u128,
    output_assembly: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    analysis_serialization: Option<u128>,
    cache_publication: u128,
    total_before_serialization: u128,
    cache_hit: bool,
    cache_published: bool,
    synchronization_included: bool,
}

impl PerformanceOutput {
    fn from_profile(
        profile: ranking::AnalysisProfile,
        analysis_serialization: Option<std::time::Duration>,
    ) -> Self {
        Self {
            unit: "microseconds",
            graph_preparation: profile.graph_preparation.as_micros(),
            scc_detection: profile.scc_detection.as_micros(),
            readiness: profile.readiness.as_micros(),
            cache_lookup: profile.cache_lookup.as_micros(),
            pagerank: profile.pagerank.as_micros(),
            search: profile.search.as_micros(),
            output_assembly: profile.output_assembly.as_micros(),
            analysis_serialization: analysis_serialization.map(|duration| duration.as_micros()),
            cache_publication: profile.cache_publication.as_micros(),
            total_before_serialization: profile.total.as_micros(),
            cache_hit: profile.cache_hit,
            cache_published: profile.cache_published,
            synchronization_included: false,
        }
    }

    fn human_summary(&self) -> String {
        format!(
            "ranking profile (microseconds, Synchronization excluded): graph={} scc={} readiness={} cache={} pagerank={} search={} output={} cache_hit={}",
            self.graph_preparation,
            self.scc_detection,
            self.readiness,
            self.cache_lookup,
            self.pagerank,
            self.search,
            self.output_assembly,
            self.cache_hit,
        )
    }
}

#[derive(Serialize)]
struct PlanOutput<'a> {
    schema_version: &'static str,
    policy_version: &'static str,
    command: &'static str,
    repository: &'a str,
    source: ReplicaSource,
    synced_at: &'a str,
    replica_snapshot_hash: &'a str,
    execution_scope: crate::execution_scope::ScopeDescription,
    decision: PlanDecision,
    parallel_now: Vec<PlanIssue>,
    dependency_layers: DependencyLayers,
    warnings: Vec<ReadyWarning>,
}

#[derive(Serialize)]
struct PriorityUpdateOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    status: PriorityUpdateStatus,
    pending: bool,
    repository: &'a str,
    issue: PriorityIssueOutput<'a>,
    previous_priority: PriorityState,
    resulting_priority: PriorityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<PendingOperationOutput<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_graph: Option<WorkingGraphSummary<'a>>,
    snapshot: SnapshotSummary<'a>,
}

#[derive(Serialize)]
struct IssueFieldUpdateOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    status: &'static str,
    pending: bool,
    repository: &'a str,
    issue: IssueFieldIssueOutput<'a>,
    field: IssueField,
    base: &'a IssueFieldValue,
    local: &'a IssueFieldValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<IssueFieldOperationOutput<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_graph: Option<WorkingGraphSummary<'a>>,
    snapshot: SnapshotSummary<'a>,
}

#[derive(Serialize)]
struct IssueFieldIssueOutput<'a> {
    key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporary_id: Option<TemporaryIssueId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
}

#[derive(Serialize)]
struct IssueFieldOperationOutput<'a> {
    id: &'a str,
    kind: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
}

#[derive(Serialize)]
struct MetadataMutationOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    result: &'static str,
    pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    relationship: Option<ParentRelationshipOutput>,
    desired_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<MetadataOperationOutput<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_graph: Option<WorkingGraphSummary<'a>>,
    snapshot: SnapshotSummary<'a>,
}

#[derive(Serialize)]
struct ParentRelationshipOutput {
    parent: String,
    child: String,
    kind: &'static str,
}

#[derive(Serialize)]
struct MetadataOperationOutput<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum PriorityUpdateStatus {
    Synchronized,
    Pending,
}

#[derive(Serialize)]
struct PendingOperationOutput<'a> {
    id: &'a str,
    kind: &'static str,
    base: &'a LogicalPriority,
    desired: &'a LogicalPriority,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
}

impl<'a> From<&'a PendingMutation> for PendingOperationOutput<'a> {
    fn from(operation: &'a PendingMutation) -> Self {
        let (base, desired) = operation
            .priority_values()
            .expect("Priority output is built from a Priority mutation");
        Self {
            id: operation.id(),
            kind: "priority_update",
            base,
            desired,
            depends_on: operation.depends_on().to_vec(),
        }
    }
}

#[derive(Serialize)]
struct WorkingGraphSummary<'a> {
    input_hash: &'a str,
}

#[derive(Serialize)]
struct IssueCreateOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    pending: bool,
    draft: DraftIssueOutput<'a>,
    operation: DraftCreateOperationOutput<'a>,
    working_graph: WorkingGraphSummary<'a>,
    snapshot: SnapshotSummary<'a>,
}

#[derive(Serialize)]
struct DraftIssueOutput<'a> {
    temporary_id: TemporaryIssueId,
    stable_node_key: &'a crate::model::StableNodeKey,
    key: &'a str,
}

#[derive(Serialize)]
struct DraftCreateOperationOutput<'a> {
    id: &'a str,
    kind: &'static str,
}

#[derive(Serialize)]
struct CommentCreateOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    issue: &'a str,
    body: &'a str,
    result: &'static str,
    pending: bool,
    operation: CommentCreateOperationOutput<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<crate::model::CommentIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_graph: Option<WorkingGraphSummary<'a>>,
    snapshot: SnapshotSummary<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

#[derive(Serialize)]
struct CommentCreateOperationOutput<'a> {
    id: &'a str,
    kind: &'static str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    depends_on: &'a [String],
}

#[derive(Serialize)]
struct PriorityIssueOutput<'a> {
    key: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporary_id: Option<TemporaryIssueId>,
    url: &'a str,
}

#[derive(Serialize)]
struct SnapshotSummary<'a> {
    schema_version: &'a str,
    synced_at: &'a str,
    input_hash: &'a str,
    issue_count: usize,
    comment_count: usize,
    dependency_count: usize,
}

#[derive(Serialize)]
struct ReadyOutput<'a> {
    schema_version: &'static str,
    command: &'static str,
    repository: &'a str,
    source: ReplicaSource,
    synced_at: &'a str,
    replica_snapshot_hash: &'a str,
    input_hash: &'a str,
    pending: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pending_operation_ids: Vec<String>,
    execution_scope: crate::execution_scope::ScopeDescription,
    issues: Vec<ReadyIssue<'a>>,
    summary: ReadySummary,
    warnings: Vec<ReadyWarning>,
}

#[derive(Serialize)]
struct ReadyIssue<'a> {
    #[serde(flatten)]
    provenance: PendingProvenance,
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporary_id: Option<TemporaryIssueId>,
    url: &'a str,
    title: &'a str,
    ready: bool,
    available: bool,
    priority: PriorityState,
    labels: Vec<&'a str>,
    assignees: Vec<&'a str>,
}

#[derive(Serialize)]
struct ReadySummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_reason: Option<crate::execution_scope::EmptyReason>,
    operational_issue_count: usize,
    ready_count: usize,
    executable_count: usize,
    assigned_ready_count: usize,
    blocked_count: usize,
}

#[derive(Serialize)]
struct ReadyWarning {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue_number: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReplicaSource {
    Live,
    LocalFallback,
}

fn dependency_command_name(intent: DependencyIntent) -> &'static str {
    match intent {
        DependencyIntent::Block => "block",
        DependencyIntent::Unblock => "unblock",
    }
}

fn dependency_result_name(result: DependencyChange) -> &'static str {
    match result {
        DependencyChange::Created => "created",
        DependencyChange::AlreadyPresent => "already_present",
        DependencyChange::Removed => "removed",
        DependencyChange::AlreadyAbsent => "already_absent",
    }
}

fn dependency_human_pending_message(
    edge: &DependencyEdgeOutput,
    intent: DependencyIntent,
) -> String {
    match intent {
        DependencyIntent::Block => format!("{} blocked by {}", edge.blocked, edge.blocker),
        DependencyIntent::Unblock => {
            format!("{} no longer blocked by {}", edge.blocked, edge.blocker)
        }
    }
}

fn dependency_expected_relationship(intent: DependencyIntent) -> &'static str {
    if intent.desired_present() {
        "present"
    } else {
        "absent"
    }
}

fn dependency_human_message(edge: &DependencyEdgeOutput, result: DependencyChange) -> String {
    match result {
        DependencyChange::Created => {
            format!("{} is now blocked by {}", edge.blocked, edge.blocker)
        }
        DependencyChange::AlreadyPresent => {
            format!("{} was already blocked by {}", edge.blocked, edge.blocker)
        }
        DependencyChange::Removed => {
            format!("{} is no longer blocked by {}", edge.blocked, edge.blocker)
        }
        DependencyChange::AlreadyAbsent => {
            format!("{} was not blocked by {}", edge.blocked, edge.blocker)
        }
    }
}

impl ReplicaSource {
    fn is_fallback(self) -> bool {
        matches!(self, Self::LocalFallback)
    }

    fn warning(self) -> Option<ReadyWarning> {
        self.is_fallback().then_some(ReadyWarning {
            code: "offline_fallback",
            message: "GitHub refresh failed; using the latest valid Local replica".to_owned(),
            issue_number: None,
            labels: Vec::new(),
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum CliError {
    #[error(transparent)]
    Discovery(#[from] crate::discovery::DiscoveryError),
    #[error(transparent)]
    SyncDiagnostics(#[from] crate::sync_diagnostics::DiagnosticsError),
    #[error(transparent)]
    PullRequestContext(#[from] crate::pull_requests::ContextError),
    #[error(transparent)]
    IssueRead(#[from] crate::issue_read::IssueReadError),
    #[error(transparent)]
    DraftIdentity(#[from] crate::draft_identity::DraftIdentityError),
    #[error(transparent)]
    Selection(#[from] crate::execution_scope::SelectionError),
    #[error(
        "selection filters apply to the full graph explorer; the sealed public export has its own allowlisted analysis"
    )]
    PublicSelection,
    #[error(transparent)]
    Skill(#[from] crate::skill::SkillError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    IssueReference(#[from] IssueReferenceError),
    #[error(transparent)]
    PendingIssueReference(#[from] PendingIssueReferenceError),
    #[error(transparent)]
    PendingIssueCreate(#[from] PendingIssueCreateError),
    #[error(transparent)]
    PendingCommentCreate(#[from] PendingCommentCreateError),
    #[error("HYFA_GITHUB_API_URL is invalid: {0}")]
    ParseApiBase(url::ParseError),
    #[error("HYFA_GITHUB_API_URL must be a safe absolute HTTP(S) base URL")]
    InvalidApiBase,
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    GitHub(#[from] GitHubError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Replica(#[from] ReplicaError),
    #[error(transparent)]
    ReplicaSync(#[from] ReplicaSyncError),
    #[error(transparent)]
    PriorityUpdate(#[from] PriorityUpdateError),
    #[error(transparent)]
    IssueFieldUpdate(#[from] IssueFieldUpdateError),
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(transparent)]
    MetadataMutation(#[from] MetadataMutationError),
    #[error(transparent)]
    Reconciliation(#[from] ReconciliationError),
    #[error(transparent)]
    Outbox(#[from] OutboxError),
    #[error(transparent)]
    WorkingGraph(#[from] WorkingGraphError),
    #[error(
        "online Priority update was unavailable ({online}); the Pending mutation could not be queued: {queue}"
    )]
    OnlinePriorityUpdateAndQueueFailed {
        online: String,
        queue: PendingPriorityUpdateError,
    },
    #[error(
        "online Dependency update was unavailable ({online}); the Pending mutation could not be queued: {queue}"
    )]
    OnlineDependencyUpdateAndQueueFailed {
        online: String,
        queue: PendingDependencyUpdateError,
    },
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("could not encode command JSON output: {0}")]
    EncodeOutput(serde_json::Error),
    #[error("next/v1 horizon must be between 1 and 3, not {0}")]
    UnsupportedNextHorizon(u8),
    #[error("hyfa plan does not accept --workers in v1")]
    UnsupportedPlanWorkers,
    #[error("GitHub refresh failed ({refresh}); no valid Local replica is available ({replica})")]
    RefreshAndReplicaUnavailable { refresh: String, replica: String },
    #[error(
        "GitHub dependency operation completed, but synchronized readback failed; Local replica was not changed: {source}"
    )]
    MutationSynchronization {
        #[source]
        source: ReplicaSyncError,
    },
    #[error(
        "GitHub dependency operation completed and readback was verified, but Local replica publication failed: {source}"
    )]
    MutationPublication {
        #[source]
        source: StoreError,
    },
    #[error(
        "GitHub dependency operation completed, but synchronized readback did not show {blocked} blocked by {blocker} as {expected}; Local replica was not changed"
    )]
    MutationReadbackMismatch {
        blocked: String,
        blocker: String,
        expected: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::authentication_hostname;
    use url::Url;

    #[test]
    fn public_github_api_uses_the_github_dot_com_authentication_host() {
        let public_api = Url::parse("https://api.github.com/").expect("public API URL");
        let enterprise_api =
            Url::parse("https://github.example/api/v3/").expect("enterprise API URL");

        assert_eq!(authentication_hostname(&public_api), "github.com");
        assert_eq!(authentication_hostname(&enterprise_api), "github.example");
    }
}
