CREATE TABLE tool_input_bindings (
    request_sequence INTEGER PRIMARY KEY REFERENCES tool_attempts(request_sequence) ON DELETE CASCADE,
    thread_id TEXT NOT NULL,
    operation_digest TEXT NOT NULL,
    snapshot_json TEXT NOT NULL,
    validation TEXT NOT NULL DEFAULT 'pending' CHECK(validation IN ('pending','verified','mismatch','unknown')),
    validation_json TEXT
);
CREATE INDEX tool_input_reuse ON tool_input_bindings(thread_id, operation_digest, validation);
CREATE TABLE tool_progress_receipts (
    thread_id TEXT NOT NULL,
    evidence_digest TEXT NOT NULL,
    request_sequence INTEGER NOT NULL REFERENCES tool_requests(sequence) ON DELETE CASCADE,
    PRIMARY KEY(thread_id, evidence_digest)
);
