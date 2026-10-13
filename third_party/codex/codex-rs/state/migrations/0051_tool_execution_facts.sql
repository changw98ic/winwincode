-- Execution receipts outlive result bodies. Thread cleanup removes both.
CREATE TABLE tool_requests (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    logical_id TEXT NOT NULL,
    binding TEXT NOT NULL,
    resolution TEXT NOT NULL DEFAULT 'observed' CHECK(resolution IN ('observed', 'denied')),
    request_json TEXT NOT NULL,
    request_owner TEXT NOT NULL,
    original_input TEXT,
    call_status TEXT NOT NULL DEFAULT 'pending' CHECK(call_status IN ('pending','succeeded','failed','cancelled','forgotten')),
    call_result TEXT,
    CHECK((call_status='pending' AND original_input IS NOT NULL AND call_result IS NULL) OR
          (call_status IN ('succeeded','failed','cancelled') AND original_input IS NOT NULL AND call_result IS NOT NULL) OR
          (call_status='forgotten' AND original_input IS NULL AND call_result IS NULL)),
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

-- Thread cleanup removes private execution data and its receipts together.
CREATE TRIGGER threads_delete_tool_facts AFTER DELETE ON threads BEGIN
    DELETE FROM tool_requests WHERE thread_id = OLD.id;
END;

CREATE TRIGGER tool_call_terminal_immutable BEFORE UPDATE ON tool_requests
WHEN OLD.call_status IN ('succeeded','failed','cancelled') AND NEW.call_status!='forgotten' AND
    (NEW.original_input IS NOT OLD.original_input OR NEW.call_result IS NOT OLD.call_result OR NEW.call_status!=OLD.call_status)
BEGIN SELECT RAISE(ABORT,'tool call is already closed'); END;
