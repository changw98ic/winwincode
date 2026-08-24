# winwincode-control-plane

This crate owns the Rust Control Plane application lifecycle. It opens and
migrates storage before accepting commands, commits canonical state before
publishing events, replays pending outbox events on startup, and explicitly
closes the event publisher and storage during shutdown.

It does not contain an HTTP server, Delivery domain logic, or any Codex Core
dependency. Those modules arrive in later migration tasks through the accepted
Control Plane interfaces.
