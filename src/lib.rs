mod atomic_file;
mod auth;
mod cli;
mod comment_create;
mod dependency_events;
mod dependency_update;
mod discovery;
mod draft_identity;
mod execution_scope;
mod github;
mod graph;
mod issue_create;
mod issue_field;
mod issue_read;
mod metadata;
mod metadata_mutation;
mod model;
mod operation_marker;
mod operational;
mod outbox;
mod plan;
mod priority;
mod priority_update;
mod pull_requests;
mod ranking;
mod reconciliation;
mod replica_sync;
mod repository;
mod skill;
mod store;
mod sync_diagnostics;
mod synchronization;
mod triage;
mod working_graph;

use std::process::ExitCode;

pub fn run() -> ExitCode {
    match cli::execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
