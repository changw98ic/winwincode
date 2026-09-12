# Community containers

The Compose deployment runs only the Community Web and Backend. The Device Client remains a native process on the code machine and connects to the Backend's TLS endpoint.

1. Build the Client and Server images with `docker compose build`.
2. Copy `deploy/compose.env.example` to a file outside version control and replace every example identity, revision, proof, repository, and expiry.
3. Point the three Compose secret file variables at `server.crt`, `server.key`, and `remote-worker.credential`. The credential must be non-empty and at most 16 KiB. File-backed Compose secrets are mounted read-only but retain source-file ownership and permissions, so restrict each source file to the operator and container UID `10001`, then verify the effective container permissions before exposing the service.
4. Populate the persistent repository volume with the exact repository/revision configured for the Server.
5. Run `docker compose --env-file /path/to/community.env up -d` and verify both health checks.

Backend state, writable model credentials, and the repository mirror use three separate named volumes. The credential store must remain outside the Backend data root because the Server rejects overlapping secret and data directories. `docker compose down` preserves all three; `docker compose down --volumes` is destructive and is intentionally not part of the normal workflow.
