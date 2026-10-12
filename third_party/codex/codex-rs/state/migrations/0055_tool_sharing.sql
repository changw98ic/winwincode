CREATE TABLE tool_sharing (
    request_sequence INTEGER PRIMARY KEY REFERENCES tool_requests(sequence) ON DELETE CASCADE,
    thread_id TEXT NOT NULL,
    source_request_sequence INTEGER NOT NULL REFERENCES tool_attempts(request_sequence) ON DELETE CASCADE,
    fact_json TEXT NOT NULL,
    accepted_result TEXT,
    rejection TEXT
);
CREATE TABLE tool_waiter_cancellations (
    request_sequence INTEGER PRIMARY KEY REFERENCES tool_requests(sequence) ON DELETE CASCADE
);
