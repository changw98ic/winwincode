# winwincode-control-plane

This crate owns the Rust Control Plane application lifecycle. It opens and
migrates storage before accepting commands, commits canonical state before
publishing events, replays pending outbox events on startup, and explicitly
closes the event publisher and storage during shutdown.

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

It does not contain an HTTP server, Delivery domain logic, or any Codex Core
dependency. Those modules arrive in later migration tasks through the accepted
Control Plane interfaces.
