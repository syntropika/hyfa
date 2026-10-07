---
name: hyfa
description: Use the Hyfa CLI to choose executable GitHub Issue work, inspect dependencies, manage Issues and assignments, reconcile queued changes, and generate Issue graph explorers. Use when a task involves Hyfa or dependency-aware GitHub Issue planning.
---

# Hyfa

Use `hyfa` directly; no GitHub CLI is required. Preserve the user's chosen
repository, execution scope, and existing authorization. Selecting work does
not itself assign an Issue, post a comment, or authorize unrelated changes.

Use `hyfa --version` and `hyfa COMMAND --help` to check the installed interface.
Use `--json` where supported and parse its versioned output. Pass an explicit
`--repo OWNER/REPO`; quote Issue references such as `'OWNER/REPO#42'`.
For additional options, consult the
[usage guide](https://github.com/syntropika/hyfa/blob/v0.4.0/docs/usage.md) and
[installation guide](https://github.com/syntropika/hyfa/blob/v0.4.0/docs/installation.md).

## Authentication and first use

When authentication needs checking, run `hyfa auth status --json`. Browser
login is `hyfa auth login`: GitHub.com uses Hyfa's built-in official OAuth App.
The user completes the displayed browser verification and authorization;
the agent can run the command and relay the verification instructions.
Users do not need to register an app for ordinary GitHub.com login.

A non-empty `GH_TOKEN` takes precedence over Hyfa's host-specific saved login.
Headless environments can supply it through their secret configuration.
`hyfa auth login --with-token` accepts a token piped from a trusted secret
source. Never request tokens in chat, print them, or put them in command
arguments, source files, reports, or graph artifacts. Saved login requires
the OS secure credential store. Enterprise hosts require their own app for
browser login; see the installation guide for host and API configuration.

Run `hyfa sync --repo OWNER/REPO --json` to establish the first valid Local
replica. Offline analysis and Draft creation require an existing valid replica.
GitHub is authoritative; do not edit replica or outbox files directly.

## Choose and inspect work

```bash
hyfa ready --repo OWNER/REPO --json
hyfa next --repo OWNER/REPO --json
hyfa next --repo OWNER/REPO --assignee LOGIN --horizon 3 --json
hyfa next --repo OWNER/REPO --children-of 'OWNER/REPO#10' --label ready-for-agent --exclude-label deferred --json
hyfa view 'OWNER/REPO#42' --json
hyfa plan --repo OWNER/REPO --json
hyfa triage --repo OWNER/REPO --json
```

- `ready` lists the complete executable frontier. `next` recommends one first
  step; `plan` includes the same decision, immediate parallel work, and
  dependency layers. `triage` explains blockers, cycles, and priority conflicts.
- The default scope is unassigned Ready Issues. `--assignee LOGIN` selects
  Ready work assigned to that login; it does not assign anything. Assignment
  and Dependency readiness are separate. Closed Issues may satisfy
  Dependencies but are never recommendations. Unknown blockers prevent readiness.
- `--label` requires every listed label; `--exclude-label` excludes any match.
  `--children-of` selects direct children, excludes the parent itself, and needs
  a synchronized relationship inventory for offline use. These combine with
  assignment scope on `ready`, `next`, `plan`, and the full graph.
  Labels have no implicit workflow meaning: choose filters from the task's
  instructions. Excluded blockers still block; unlocked outcomes count globally.
- Horizons are 1, 2, or 3; the default is 3. Report search restrictions from
  `truncated_by` and `search_complete` without claiming a global optimum when
  it was not established. A null recommendation is a valid result. Use
  `triage` to explain it instead of substituting blocked work.
- Dependency layers describe topology, not dates or worker schedules.
  `plan --workers` is unsupported.
- Analysis attempts a pull refresh and may use `source: local_fallback` with
  the previous `synced_at`. Surface stale input and `pending` provenance
  when they affect the answer. Analysis never replays queued remote writes.

Read the Issue's actual specification before implementation with `hyfa view
'OWNER/REPO#42' --json`. It includes the effective body, comments, labels,
assignments, and relationships, including pending local changes. `--offline`
skips the refresh; check `synced_at`, `pending`, and `relationships_complete`.
The generated explorer still excludes body and comment text.

`view` and the private explorer also explain Dependency impact: direct and
transitive open dependents, immediate Ready outcomes, remaining-blocker examples,
and downstream chain depth. Check completeness flags before treating downstream
counts as exact. Blocked subjects have no immediate-outcome claim, and chain
depth is unavailable for reachable cycles. These are structural diagnostics,
not business value, delivery estimates, or additional ranking weights.

## Discover context

Before creating an Issue, search the effective local content and inspect related
work. `search` and `related` read locally unless `--refresh` is passed; `status`
always reads locally. Online `view --with-prs` fetches Issue and PR context;
`prs --offline` reads only its saved context:

```bash
hyfa search --repo OWNER/REPO --query 'manifest cache' --json
hyfa related 'OWNER/REPO#42' --json
hyfa status --repo OWNER/REPO --json
hyfa view 'OWNER/REPO#42' --with-prs --json
hyfa prs 'OWNER/REPO#42' --offline --json
```

Search includes title, body, and comments, including pending changes and Drafts.
Check result truncation, evidence, snapshot time, and pending provenance. Related
work is a suggestion, not a confirmed duplicate or native Dependency. Discovery
may include closed or blocked Issues; choose executable work through `ready`
and `next`. Linked PR context has its own observation time and completeness;
null PR data is unknown, not proof that no PR exists. Status reports the last
sync attempt and sanitized fallback reason; received pages are not publication
evidence. These commands never replay the outbox or merge PRs.

## Make authorized changes

`update` changes exactly one logical field per invocation. Repeated
`--assignee` options replace the complete assignee set; they do not append.
Preserve existing assignees when the requested change requires it.

```bash
hyfa update 'OWNER/REPO#42' --assignee LOGIN --json
hyfa update 'OWNER/REPO#42' --clear-assignees --json
hyfa update 'OWNER/REPO#42' --title 'A clearer title' --json
hyfa update 'OWNER/REPO#42' --body 'Revised Markdown' --json
hyfa update 'OWNER/REPO#42' --state closed --json
hyfa update 'OWNER/REPO#42' --priority p1 --json
hyfa label 'OWNER/REPO#42' --add 'area:backend' --json
hyfa block 'OWNER/REPO#42' --by 'OWNER/REPO#7' --json
hyfa unblock 'OWNER/REPO#42' --by 'OWNER/REPO#7' --json
hyfa sub-issue 'OWNER/REPO#10' --add 'OWNER/REPO#42' --json
hyfa comment 'OWNER/REPO#42' --body 'Verified implementation details' --json
```

`block` means #42 is blocked by #7. A parent/sub-Issue relationship is
decomposition metadata and does not create a Dependency. `label` and
`sub-issue` also accept `--remove`. State accepts `open` or `closed`.
Priority accepts `p0` through `p4` or `none`; change canonical priority labels
only through `update --priority`. Use `hyfa init --repo OWNER/REPO --json`
when repository setup is requested to create missing canonical priority labels.

Most mutations attempt an online change and otherwise can queue a Pending
mutation. Inspect `pending`, `status` or `result`, operation IDs, and warnings.
A successful process exit does not prove GitHub accepted the change. Report
queued work as pending; claim an Issue is assigned, closed, or commented on
only when the result or a remote read confirms it.

## Drafts, reconciliation, and conflicts

```bash
hyfa create --repo OWNER/REPO --title 'Prepare the migration' --body 'Acceptance notes' --json
hyfa reconcile --repo OWNER/REPO --json
hyfa resolve OPERATION --repo OWNER/REPO --remote --json
hyfa resolve OPERATION --repo OWNER/REPO --local --json
```

`create` always creates a local Draft, even online. Keep the returned
`draft.key`, such as `OWNER/REPO#draft:TEMPORARY_ID`, and its operation ID.
Use that quoted key for Draft title, body, state, and assignee edits, comments,
generic labels, relationships, `view`, and `update --priority`. Draft priority
intents wait for creation and preserve their order during reconciliation.
Never invent a GitHub number or expose an internal synthetic number.
Reconciliation creates the GitHub Issue and returns its permanent identity.
To fulfill an authorized request to create a GitHub Issue, complete that
reconciliation and report the confirmed remote reference.

`reconcile` applies the repository's pending batch. Online `comment` and
`resolve` also invoke repository-wide reconciliation, potentially applying
other pending work. Account for this scope using the user's existing
authorization; there is no selective replay or dry-run option.

Inspect reconciliation `operations` and `summary`, including remaining,
conflicting, or blocked work. Conflicts require a deliberate choice between
the reported base, local, and remote values: `--remote` retires the intent;
`--local` reaffirms it. A priority conflict also accepts `--priority p1`
(or another priority value). Resolve according to explicit task intent;
ask for the choice only when that intent does not determine it. Reuse
reconciliation after an uncertain create/comment response instead of
issuing a duplicate create or comment. Stop blind retries when the command
reports an unresolved outcome and explain the remaining operation.

## Generate a graph

```bash
hyfa graph --repo OWNER/REPO --output /absolute/artifact/directory --json
```

Open the generated `index.html`. Keep generated files outside the checkout
unless the user requests them there. The full explorer contains private
repository information and pending intent. For requested public output use
`--public`, which requires a live confirmation that the repository is public
and generates a separate allowlisted artifact. Labels and assignees require
explicit `--public-label-prefix` and `--public-include-assignees` opt-ins.
Generate the public artifact through Hyfa; do not publish the full explorer
as its substitute. Generation itself does not deploy a site.
