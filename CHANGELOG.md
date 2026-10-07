# Changelog

## Unreleased

- Search effective Issue titles, bodies, and comments locally with `hyfa search`,
  including pending edits and Drafts, with filters, evidence snippets, and JSON.
- Suggest related Issues with `hyfa related` using explicit references and shared
  title terms, without changing native Dependencies or operational ranking.
- Inspect offline synchronization status, phase progress, received-page counters,
  and sanitized failure reasons with `hyfa status`. Failed refreshes retain the
  preceding valid replica and synchronization time.
- Inspect GitHub-linked closing PRs through `hyfa prs` or `hyfa view --with-prs`,
  including open, closed, merged, and Draft state. Complete observations are
  cached privately and remain readable offline with their own observation time.

## 0.4.0

- Explain Dependency impact in `hyfa view` and the private graph's Issue details:
  direct dependents, distinct open downstream work, immediate Ready outcomes,
  and examples of work that still needs other blockers resolved.
- Show the longest downstream chain in dependency steps, with an unavailable
  result for reachable cycles. Closed history and parent relationships do not
  inflate impact; ownership and execution filters do not hide downstream work.
- Share precomputed impact between offline reading and the explorer, including
  pending Draft identities and effective changes. Bounded traversal reports
  lower bounds explicitly, and examples have declared limits.
- Advance the strict private graph contract to `hyfa.graph-artifact/v4` and add
  `issue.impact` to Issue-view JSON. Ranking and the sealed public graph retain
  their existing contracts.

## 0.3.0

- Select executable work with repeatable `--label` and `--exclude-label` filters
  and `--children-of OWNER/REPO#NUMBER` across `ready`, `next`, `plan`,
  and the full graph explorer. Every simulated step respects the same scope.
- Preserve all Dependencies and global unlocked-outcome scoring. Synchronize
  requested parent/child inventories for offline selection; unknown membership
  produces an explicit error instead of an invented empty scope.
- Read full Issue bodies, comments, labels, assignees, Dependencies, and parent
  relationships with `hyfa view ISSUE`; use `--offline` to read the effective
  local view without contacting GitHub. Pending changes and snapshot time remain visible.
- Set and clear Draft Issue priorities before publication. Reconciliation applies
  their ordered intents after creation and retains the temporary identity alias.
- Add optional scope and empty-result details to CLI JSON. The strict private
  graph contract advances to `hyfa.graph-artifact/v3`; consumers must use the
  accompanying schema. Existing replicas and outboxes remain readable.
  The sealed public export keeps its separate allowlisted contract.

## 0.2.1

- Name the source package `hyfa`, matching the executable and agent skill.
- Add crates.io distribution so installation uses `cargo install hyfa --locked`.
  GitHub binary archives remain available for Linux and macOS.

## 0.2.0

- Rename the CLI to `hyfa` and the repository to [`syntropika/hyfa`](https://github.com/syntropika/hyfa).
- Ship `hyfa-v0.2.0-TARGET.tar.gz` archives containing the `hyfa` executable.
  Update the graph explorers, bundled agent skill, documentation, and workflows.
- Use `HYFA_*` environment variables, `hyfa.*` JSON schema identifiers, a
  `hyfa` local state directory, and a separate Hyfa credential-store entry.
  Scripts and integrations must adopt these names before upgrading.
- Finish reconciling pending changes with the previous CLI before upgrading,
  then run `hyfa auth login` and `hyfa sync --repo OWNER/REPO`. Existing state
  and saved credentials are preserved but are not migrated automatically.
- Continue recognizing historical operation markers in GitHub content so they
  remain hidden and ambiguous writes can still be identified.

## 0.1.1

- Use the official OAuth App by default for browser login on GitHub.com,
  without requiring users to supply a Client ID.
- Keep custom app configuration through `--client-id` and the environment.
  GitHub Enterprise hosts require their own app.
- Update the installation guide and quickstart for the configured login.
- Bundle a usage skill and install it offline with the `skill install`
  subcommand, using the native `skillinstaller` library for provider destinations.
- Prepare source packaging with an organization-prefixed package name.
  Include the agent skill in source and binary distributions; registry
  publication is deferred.

## 0.1.0

First public release of the CLI, for choosing executable work from GitHub Issues
and understanding what it can unblock.

- Sign in using browser device authorization or a token
  supplied on standard input. Store credentials in the operating system's
  secure store; inspect the active account and remove the saved login.
- Synchronize native Issue dependencies into a validated local replica.
  Analyze the latest successful snapshot when GitHub is unavailable.
- List ready work, recommend a next Issue with bounded rollout evidence,
  inspect dependency layers, and diagnose cycles and priority conflicts.
- Create Draft Issues and queue edits, comments, labels, parent relationships,
  and Dependencies offline. Project pending work into analysis and reconcile
  it explicitly with GitHub.
- Generate a self-contained graph explorer with a spatial network view,
  dependency layers, pan/zoom/fit controls, search, an accessible Issue table,
  and precomputed recommendation evidence.
  Visible outcome counts and a completed-Issue list distinguish completed
  work from Issues closed as not planned or without a known reason.
- Generate a separate allowlisted public graph for public repositories,
  with a sealed publication boundary and a GitHub Pages workflow.
- Provide versioned JSON outputs for scripts and agents.
- Distribute native Linux and macOS archives for x86_64 and ARM64, with
  checksums, build metadata, documentation, and an MIT license. Every release
  target passes tests and an extracted-binary workflow before publication.

See the [installation guide](docs/installation.md) for supported system
versions and the [usage guide](docs/usage.md) for command contracts.
