CREATE TABLE tool_diagnostics (
    thread_id TEXT NOT NULL,
    diagnostic_id TEXT NOT NULL,
    evidence_version INTEGER NOT NULL,
    anchor_request_sequence INTEGER NOT NULL REFERENCES tool_requests(sequence) ON DELETE CASCADE,
    diagnostic_json TEXT NOT NULL,
    offered_version INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(thread_id, diagnostic_id)
);
CREATE INDEX tool_diagnostics_pending ON tool_diagnostics(thread_id, evidence_version, offered_version);
CREATE TABLE tool_diagnostic_feedback (
    boundary_request_sequence INTEGER NOT NULL REFERENCES tool_requests(sequence) ON DELETE CASCADE,
    thread_id TEXT NOT NULL,
    diagnostic_id TEXT NOT NULL,
    evidence_version INTEGER NOT NULL,
    diagnostic_json TEXT NOT NULL,
    offered INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(boundary_request_sequence, diagnostic_id, evidence_version),
    FOREIGN KEY(thread_id, diagnostic_id) REFERENCES tool_diagnostics(thread_id, diagnostic_id) ON DELETE CASCADE
);
CREATE TABLE tool_diagnostic_responses (
    thread_id TEXT NOT NULL,
    diagnostic_id TEXT NOT NULL,
    evidence_version INTEGER NOT NULL,
    turn_id TEXT NOT NULL,
    response_digest TEXT NOT NULL,
    boundary_request_sequence INTEGER NOT NULL REFERENCES tool_requests(sequence) ON DELETE CASCADE,
    PRIMARY KEY(thread_id, diagnostic_id, evidence_version, turn_id, response_digest),
    FOREIGN KEY(thread_id, diagnostic_id) REFERENCES tool_diagnostics(thread_id, diagnostic_id) ON DELETE CASCADE
);
