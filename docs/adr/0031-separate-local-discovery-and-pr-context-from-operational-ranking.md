# Separate local discovery and PR context from operational ranking

Hyfa will expose local Issue-content search and evidence-based related-Issue
suggestions over the Working graph. Discovery reads the latest valid replica
and ordered pending intent without a network refresh by default; an explicit
`--refresh` requests the existing pull-only refresh and fallback behavior.
Discovery includes closed and blocked Issues for context, preserves Draft
identities and pending provenance, and reports result and evidence limits.
Its versioned keyword and reference/title-term methods never infer native
Dependencies, confirm duplicates, or change readiness and `next/v1` ranking.

GitHub-linked closing PRs will be fetched on demand as separately timestamped
private context. A complete, validated connection replaces its cache atomically;
failed or incomplete acquisition preserves the preceding observation, and
missing context remains explicitly unknown. PR context does not enter the Issue
graph, replica hash, pending outbox, ranking cache, or sealed public export.
Issue reading may opt into this context without changing its default envelope.

All replica refreshes, including mutation readbacks and reconciliation, will
report sanitized phase and acquisition diagnostics in an independent private
sidecar. Diagnostic activity does not prove publication,
advance cursors or `synced_at`, or authorize remote writes. The shared refresh
and publication lifecycle reports success only after verified publication;
a discarded candidate is recorded as not published. The valid replica
remains the only analysis input after a failure. Status reads diagnostics and
snapshot metadata offline; diagnostic persistence failures remain observable
warnings rather than failures of an otherwise valid synchronization.
