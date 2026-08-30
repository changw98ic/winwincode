# winwincode-audit

This crate owns the Control Plane audit event and its local immutable storage
adapter. Callers provide a validated actor, exact tenant scope, request ID,
stable action and result codes, state digests, a local component or source IP,
and optional Delivery, ProductSession, Lease, and Publication references.
There is no field for a raw command body, prompt, response, credential,
provider diagnostic, or publication body.

The closed action categories cover business, administration, commands,
approvals, Policy, Credential, Worker leases, Provider operations, model
invocations, Delivery state, and Publication operations. Credential and
Provider actions carry only stable operation names plus a canonical Credential
reference or Provider identity; secret material and remote diagnostics cannot
be represented.

`AuditStore` keeps one continuous sequence and SHA-256 chain per organization.
The chain header commits the event identity, exact scope, retention rule, and
canonical event payload digest. Event headers cannot be updated or deleted.
An exact `AuditEventId` replay returns the first record; reuse with changed
facts returns `RequestConflict`.

`verify_organization` streams the complete chain in sequence order and returns
its verified tail checkpoint. The checkpoint contains only the organization,
last sequence, and last digest, so an export can prove the exact cut without
copying event payloads.

Reads require an `AuditAccess` scope already approved by the policy layer. The
store applies that scope exactly at organization, workspace, project, or
repository level and never treats this value as authentication proof.

Finite retention deletes only the canonical payload after its deadline. The
ordered header, payload digest, and immutable retention tombstone remain so the
organization chain can still be verified. Indefinite payloads are retained.
Missing or changed payloads, headers, tombstones, sequence links, or chain
heads fail closed as corruption.

`DataGovernanceAuthority` evaluates one complete immutable policy snapshot for
classification, residency, redaction, retention, and legal holds. Its rule
digest is derived from the rule identity, version, all classification rules,
region allow-lists, retention/redaction choices, and sorted legal holds. The
authority accepts only source digests and scoped facts, never raw content.

Restricted placement receives a sealed permit only for an allowed region.
Redaction plans retain the original source digest and their rule provenance;
the decision digest is recorded in the same immutable Audit Ledger. Legal
holds and retention rules are checked before a sealed deletion permit is
created. The audit decision is durable before the explicit idempotent deletion
port is called, so Audit Ledger corruption or policy denial cannot reach
storage deletion.

`AuditStore::export_page` captures one verified organization checkpoint and
binds every continuation cursor to the exact scope, time range, subject
filter, limits, observation time, and governance redaction decision. Later
appends do not enter that snapshot. Each page walks a contiguous portion of
the organization chain, advances through at most 200 headers, and enforces a
one-megabyte encoded-record ceiling.

Matching retained events use the canonical secret-safe `AuditEvent` shape.
The export contains only digest references for state, model input/output, and
Artifact/evidence facts; it never copies Artifact bodies. A pruned payload is
represented by a sealed deletion proof that binds its deadline, deletion time,
event digest, and immutable tombstone. Subject-filtered export fails closed
when a deleted payload can no longer prove whether it matched.

`AuditExportVerifier` consumes serialized pages in order, recalculates query,
payload, header, and chain digests, validates filtering and deletion proofs,
and requires the final page to reach the captured checkpoint. This provides
offline verification without creating a second ledger.

Phase 3.4 delivers this typed event and storage seam. Control Plane command,
policy, Publication, and product cutover code composes this seam in the later
Phase 3 integration tasks; this crate does not provide a second HTTP or product
command API.
