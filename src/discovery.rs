use std::collections::BTreeSet;

use clap::{Args, ValueEnum};
use serde::Serialize;
use thiserror::Error;

use crate::{
    model::{Issue, TemporaryIssueId, strip_operation_markers},
    working_graph::{PendingProvenance, WorkingGraph},
};

const EVIDENCE_LIMIT: usize = 5;
const SNIPPET_CHARS: usize = 180;

#[derive(Clone, Copy, Default, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StateFilter {
    Open,
    Closed,
    #[default]
    All,
}

#[derive(Args, Default, Serialize)]
pub(crate) struct Filters {
    /// Include open, closed, or all Issues; discovery does not imply readiness.
    #[arg(long, value_enum, default_value = "all")]
    state: StateFilter,
    /// Require every specified label.
    #[arg(long = "label")]
    labels: Vec<String>,
    /// Select Issues assigned to this login.
    #[arg(long)]
    assignee: Option<String>,
}

impl Filters {
    fn includes(&self, issue: &Issue) -> bool {
        let state = match self.state {
            StateFilter::All => true,
            StateFilter::Open => issue.state.eq_ignore_ascii_case("open"),
            StateFilter::Closed => issue.state.eq_ignore_ascii_case("closed"),
        };
        state
            && self.labels.iter().all(|name| {
                issue
                    .labels
                    .iter()
                    .any(|label| label.name.eq_ignore_ascii_case(name))
            })
            && self.assignee.as_ref().is_none_or(|login| {
                issue
                    .assignees
                    .iter()
                    .any(|actor| actor.login.eq_ignore_ascii_case(login))
            })
    }
}

#[derive(Serialize)]
pub(crate) struct Evidence {
    kind: &'static str,
    field: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment_index: Option<usize>,
    terms: Vec<String>,
    snippet: String,
}

#[derive(Serialize)]
pub(crate) struct Hit {
    pub(crate) key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporary_id: Option<TemporaryIssueId>,
    pub(crate) title: String,
    state: String,
    url: String,
    labels: Vec<String>,
    assignees: Vec<String>,
    #[serde(flatten)]
    provenance: PendingProvenance,
    pub(crate) evidence: Vec<Evidence>,
    evidence_truncated: bool,
}

impl Hit {
    pub(crate) fn print(&self) {
        println!(
            "{} {} [{}{}]",
            self.key,
            self.title,
            self.state,
            if self.provenance.is_pending() {
                ", pending"
            } else {
                ""
            }
        );
        for evidence in &self.evidence {
            println!(
                "  {} ({}): {}",
                evidence.kind, evidence.field, evidence.snippet
            );
        }
    }

    fn new(working: &WorkingGraph<'_>, issue: &Issue, mut evidence: Vec<Evidence>) -> Self {
        let evidence_truncated = evidence.len() > EVIDENCE_LIMIT;
        evidence.truncate(EVIDENCE_LIMIT);
        Self {
            key: issue.display_key(&working.replica().repository),
            number: (!issue.is_draft()).then_some(issue.number),
            temporary_id: issue.temporary_id(),
            title: strip_operation_markers(&issue.title),
            state: issue.state.clone(),
            url: issue.url.clone(),
            labels: issue
                .labels
                .iter()
                .map(|label| label.name.clone())
                .collect(),
            assignees: issue
                .assignees
                .iter()
                .map(|actor| actor.login.clone())
                .collect(),
            provenance: working.provenance_for_issue(issue.number),
            evidence,
            evidence_truncated,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct Results {
    pub(crate) hits: Vec<Hit>,
    matched_count: usize,
    limit: usize,
    truncated: bool,
    evidence_limit: usize,
    snippet_chars: usize,
}

impl Results {
    pub(crate) fn matched_count(&self) -> usize {
        self.matched_count
    }
}

fn results(mut ranked: Vec<(usize, &Issue, Hit)>, limit: usize) -> Results {
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.stable_node_key().cmp(&b.1.stable_node_key()))
    });
    let matched_count = ranked.len();
    Results {
        hits: ranked
            .into_iter()
            .take(limit)
            .map(|(_, _, hit)| hit)
            .collect(),
        matched_count,
        limit,
        truncated: matched_count > limit,
        evidence_limit: EVIDENCE_LIMIT,
        snippet_chars: SNIPPET_CHARS,
    }
}

pub(crate) fn search(
    working: &WorkingGraph<'_>,
    query: &str,
    filters: &Filters,
    limit: usize,
) -> Result<Results, DiscoveryError> {
    validate_query(query)?;
    let terms = tokens(query);
    let mut ranked = Vec::new();
    for issue in working
        .replica()
        .issues
        .iter()
        .filter(|issue| filters.includes(issue))
    {
        let mut found = BTreeSet::new();
        let mut evidence = Vec::new();
        let mut score = 0;
        for (field, comment_index, text) in documents(issue) {
            let text = strip_operation_markers(text);
            let matched: BTreeSet<_> = tokens(&text).intersection(&terms).cloned().collect();
            if matched.is_empty() {
                continue;
            }
            score += matched.len()
                * match field {
                    "title" => 3,
                    "body" => 2,
                    _ => 1,
                };
            found.extend(matched.iter().cloned());
            evidence.push(Evidence {
                kind: "keyword",
                field,
                comment_index,
                snippet: snippet(&text, &matched),
                terms: matched.into_iter().collect(),
            });
        }
        if found == terms {
            ranked.push((score, issue, Hit::new(working, issue, evidence)));
        }
    }
    Ok(results(ranked, limit))
}

pub(crate) fn validate_query(query: &str) -> Result<(), DiscoveryError> {
    if tokens(query).is_empty() {
        Err(DiscoveryError::EmptyQuery)
    } else {
        Ok(())
    }
}

pub(crate) fn related(
    working: &WorkingGraph<'_>,
    number: u64,
    filters: &Filters,
    limit: usize,
) -> Result<Results, DiscoveryError> {
    let subject = working
        .replica()
        .issues
        .iter()
        .find(|issue| issue.number == number)
        .ok_or(DiscoveryError::MissingIssue)?;
    let subject_terms = informative_title(&subject.title);
    let repository = &working.replica().repository;
    let mut ranked = Vec::new();
    for issue in working
        .replica()
        .issues
        .iter()
        .filter(|issue| issue.number != number && filters.includes(issue))
    {
        let shared: BTreeSet<_> = informative_title(&issue.title)
            .intersection(&subject_terms)
            .cloned()
            .collect();
        let candidate_refs = reference_evidence(issue, subject, repository);
        let subject_refs = reference_evidence(subject, issue, repository);
        let has_reference = !candidate_refs.is_empty() || !subject_refs.is_empty();
        if !has_reference && shared.len() < 2 {
            continue;
        }
        let mut evidence = candidate_refs;
        // This evidence is explicitly located in the subject, not the candidate.
        evidence.extend(subject_refs.into_iter().map(|mut evidence| {
            evidence.kind = "subject_reference";
            evidence
        }));
        if !shared.is_empty() {
            evidence.push(Evidence {
                kind: "shared_title_terms",
                field: "title",
                comment_index: None,
                snippet: snippet(&strip_operation_markers(&issue.title), &shared),
                terms: shared.iter().cloned().collect(),
            });
        }
        // Explicit references precede any title-only candidate, without altering operational ranking.
        let score = usize::from(has_reference) * (subject_terms.len() + 1) + shared.len();
        ranked.push((score, issue, Hit::new(working, issue, evidence)));
    }
    Ok(results(ranked, limit))
}

fn documents(issue: &Issue) -> impl Iterator<Item = (&'static str, Option<usize>, &str)> {
    [
        ("title", None, issue.title.as_str()),
        ("body", None, issue.body.as_str()),
    ]
    .into_iter()
    .chain(
        issue
            .comments
            .iter()
            .enumerate()
            .map(|(index, comment)| ("comment", Some(index), comment.body.as_str())),
    )
}

fn reference_evidence(from: &Issue, to: &Issue, repository: &str) -> Vec<Evidence> {
    let key = to.display_key(repository).to_lowercase();
    let short = (!to.is_draft()).then(|| format!("#{}", to.number));
    documents(from)
        .filter_map(|(field, comment_index, text)| {
            let text = strip_operation_markers(text);
            let matched = text
                .split(|c: char| {
                    c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '<' | '>' | '`')
                })
                .map(|word| {
                    word.trim_matches(|c: char| matches!(c, ',' | '.' | ':' | ';' | '!' | '?'))
                        .to_lowercase()
                })
                .find(|word| {
                    word == &key
                        || short.as_ref() == Some(word)
                        || (!to.url.is_empty() && word == &to.url.to_lowercase())
                });
            matched.map(|term| Evidence {
                kind: "candidate_reference",
                field,
                comment_index,
                snippet: reference_snippet(&text, &term),
                terms: vec![term],
            })
        })
        .collect()
}

fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn informative_title(title: &str) -> BTreeSet<String> {
    tokens(&strip_operation_markers(title))
        .into_iter()
        .filter(|word| {
            word.chars().count() >= 4
                && !matches!(
                    word.as_str(),
                    "this"
                        | "that"
                        | "with"
                        | "from"
                        | "when"
                        | "into"
                        | "have"
                        | "does"
                        | "issue"
                        | "issues"
                        | "should"
                        | "would"
                        | "could"
                        | "after"
                        | "before"
                        | "support"
                        | "please"
                        | "error"
                        | "cannot"
                )
        })
        .collect()
}

fn snippet(text: &str, terms: &BTreeSet<String>) -> String {
    let position = text
        .split_inclusive(|c: char| !c.is_alphanumeric())
        .scan(0usize, |offset, part| {
            let start = *offset;
            *offset += part.chars().count();
            Some((
                start,
                part.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase(),
            ))
        })
        .find(|(_, word)| terms.contains(word))
        .map(|(start, _)| start)
        .unwrap_or(0);
    window(text, position)
}

fn reference_snippet(text: &str, term: &str) -> String {
    // Work in character offsets; lowercasing can change UTF-8 byte lengths.
    let chars: Vec<_> = text.chars().collect();
    let position = (0..chars.len())
        .find(|&index| {
            chars[index..]
                .iter()
                .take(term.chars().count())
                .collect::<String>()
                .to_lowercase()
                == term
        })
        .unwrap_or(0);
    window(text, position)
}

fn window(text: &str, position: usize) -> String {
    let start = position.saturating_sub(40);
    text.chars().skip(start).take(SNIPPET_CHARS).collect()
}

#[derive(Debug, Error)]
pub(crate) enum DiscoveryError {
    #[error("the search query must contain at least one alphanumeric term")]
    EmptyQuery,
    #[error("the source Issue is not present in the Working graph")]
    MissingIssue,
}
