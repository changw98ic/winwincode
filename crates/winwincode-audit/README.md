# winwincode-audit

This crate owns the Control Plane audit event and its local immutable storage
adapter. Callers provide a validated actor, exact tenant scope, request ID,
stable action and result codes, state digests, a local component or source IP,
and optional Delivery, ProductSession, Lease, and Publication references.
There is no field for a raw command body, prompt, response, credential,
provider diagnostic, or publication body.

`AuditStore` keeps one continuous sequence and SHA-256 chain per organization.
The chain header commits the event identity, exact scope, retention rule, and
canonical event payload digest. Event headers cannot be updated or deleted.
An exact `AuditEventId` replay returns the first record; reuse with changed
facts returns `RequestConflict`.

Reads require an `AuditAccess` scope already approved by the policy layer. The
store applies that scope exactly at organization, workspace, project, or
repository level and never treats this value as authentication proof.

Finite retention deletes only the canonical payload after its deadline. The
ordered header, payload digest, and immutable retention tombstone remain so the
organization chain can still be verified. Indefinite payloads are retained.
Missing or changed payloads, headers, tombstones, sequence links, or chain
heads fail closed as corruption.

Phase 3.4 delivers this typed event and storage seam. Control Plane command,
policy, Publication, and product cutover code composes this seam in the later
Phase 3 integration tasks; this crate does not provide a second HTTP or product
command API.
