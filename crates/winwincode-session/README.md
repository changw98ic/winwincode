# winwincode-session

ProductSession lifecycle state and exact execution-session identity bindings.

The crate is Control Plane code. It intentionally has no dependency on
Delivery, API DTOs, Execution Worker, Codex Core, or the Control Plane
composition crate.

Binding values are validated domain facts, not wire DTOs. The Control Plane
acceptance adapter joins a generated `session.binding` message to its one
scheduler-owned sealed lease and then creates the canonical binding values.
Lease authority is never created or restored in this crate.

Project-owned code is licensed under Apache-2.0.
