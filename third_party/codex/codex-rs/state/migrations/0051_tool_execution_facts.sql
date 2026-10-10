-- Execution receipts outlive result bodies. Thread cleanup removes both.
CREATE TABLE tool_requests (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    logical_id TEXT NOT NULL,
    binding TEXT NOT NULL,
    resolution TEXT NOT NULL DEFAULT 'observed' CHECK(resolution IN ('observed', 'denied')),
    request_json TEXT NOT NULL,
    UNIQUE(thread_id, logical_id)
);
CREATE TABLE tool_attempts (
    request_sequence INTEGER PRIMARY KEY REFERENCES tool_requests(sequence) ON DELETE CASCADE,
    attempt_id TEXT NOT NULL UNIQUE,
    owner_id TEXT NOT NULL,
    operation_digest TEXT NOT NULL,
    effective_input TEXT NOT NULL,
    execution TEXT NOT NULL CHECK(execution IN ('running', 'completed', 'uncertain')),
    disposition TEXT NOT NULL DEFAULT 'pending' CHECK(disposition IN ('pending', 'accepted', 'rejected')),
    delivery TEXT NOT NULL DEFAULT 'pending' CHECK(delivery IN ('pending', 'offered')),
    execution_result TEXT,
    accepted_result TEXT,
    rejection TEXT,
    revision INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE tool_fact_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    request_sequence INTEGER NOT NULL REFERENCES tool_requests(sequence) ON DELETE CASCADE,
    thread_id TEXT NOT NULL,
    fact_json TEXT NOT NULL
);
CREATE INDEX tool_fact_events_thread_sequence ON tool_fact_events(thread_id, sequence);

-- Older binaries ignore newer migration versions. Their thread deletes must
-- still remove private execution data created by a newer binary.
CREATE TRIGGER threads_delete_tool_facts AFTER DELETE ON threads BEGIN
    DELETE FROM tool_requests WHERE thread_id = OLD.id;
END;
