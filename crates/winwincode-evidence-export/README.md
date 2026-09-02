# winwincode-evidence-export

Builds byte-stable, offline-verifiable evidence packages from explicit
Control Plane records and content-addressed local files. The exporter never
discovers evidence from chat text or repository scans. It preflights hashes,
secret markers, and the caller-provided disk budget before creating output.

Each package contains canonical trace JSONL, patch/diff, verdict, merge guide,
referenced artifacts, and a manifest binding every file to its SHA-256 digest.
An optional deterministic `.wwcevidence` archive contains the same files.
