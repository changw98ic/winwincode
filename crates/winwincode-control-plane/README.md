# winwincode-control-plane

This crate owns the Rust Control Plane application lifecycle. It opens and
migrates storage before accepting commands, commits canonical state before
publishing events, replays pending outbox events on startup, and explicitly
closes the event publisher and storage during shutdown.

Its public commit seam accepts the generated canonical `CommandEnvelope` plus
one `StateChange`. The adapter validates the full actor/scope identity, hashes
the semantic command, and only then calls the lower storage port. Storage-only
receipt key constructors are not re-exported as Control Plane command inputs.

Each running instance creates a unique temporary root with an ownership marker.
Shutdown removes only the directory whose marker still matches that instance;
failed startup and failed outbox flush follow the same close-and-release path.

Phase 2.1 does not proactively delete temporary roots left by a crashed process.
Startup creates a new unique root and never enumerates or removes earlier roots,
because a PID or an old marker alone is not proof that an ownership lease is
stale. Durable database and outbox recovery is implemented independently of
temporary-root reclamation. Reclaiming crash-left roots is deferred until the
Control Plane has a renewable lease with an explicit stale threshold; graceful
and failed lifecycle cleanup remains limited to the current instance's exact
marker and path.

`delivery_execution` maps one validated Delivery stage effect to the generated
`ExecutionJob` and returns a `PendingDeliveryExecution`. Its transaction port
must commit the Delivery journal, command receipt, and job outbox intent before
dispatch. The durable receipt carries the original event and job; the adapter
validates that job again, dispatches that exact value, and only then marks the
outbox event published. Commit, dispatch, and acknowledgement failures remain
distinct so restart can replay pending work without inventing a new job.

The crate does not contain an HTTP server, Codex scheduling state, or any Codex
Core dependency. Delivery transitions remain in `winwincode-delivery`; this
crate only composes them with generated Control Plane and ExecutionPort types.
