-- One ordered Core fact stream covers requests, cells and trusted wait edges.
UPDATE tool_fact_events SET fact_json = json_object('kind', 'request', 'fact', json(fact_json));
CREATE TABLE tool_runtime_cells (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    parent_request_sequence INTEGER NOT NULL REFERENCES tool_requests(sequence) ON DELETE CASCADE,
    cell_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    lifecycle TEXT NOT NULL CHECK(lifecycle IN ('live', 'closed')),
    revision INTEGER NOT NULL DEFAULT 1,
    UNIQUE(thread_id, scope_id, cell_id)
);
CREATE TABLE tool_runtime_waits (
    waiter_request_sequence INTEGER PRIMARY KEY REFERENCES tool_requests(sequence) ON DELETE CASCADE,
    target_cell_sequence INTEGER NOT NULL REFERENCES tool_runtime_cells(sequence) ON DELETE CASCADE,
    owner_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('waiting', 'settled')),
    revision INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX tool_runtime_cells_thread ON tool_runtime_cells(thread_id, sequence);
