CREATE TABLE tool_control_input_recoveries (
    input_request_sequence INTEGER PRIMARY KEY REFERENCES tool_attempts(request_sequence) ON DELETE CASCADE,
    parent_request_sequence INTEGER NOT NULL REFERENCES tool_attempts(request_sequence) ON DELETE CASCADE,
    thread_id TEXT NOT NULL,
    result_ref TEXT NOT NULL UNIQUE,
    record_json TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    page_count INTEGER NOT NULL CHECK(page_count > 0),
    sealed INTEGER NOT NULL DEFAULT 0 CHECK(sealed IN (0,1)),
    invalidated INTEGER NOT NULL DEFAULT 0 CHECK(invalidated IN (0,1))
);
CREATE INDEX tool_control_input_recovery_parent
    ON tool_control_input_recoveries(thread_id, parent_request_sequence);
CREATE TABLE tool_control_input_result_pages (
    input_request_sequence INTEGER NOT NULL REFERENCES tool_control_input_recoveries(input_request_sequence) ON DELETE CASCADE,
    cursor TEXT NOT NULL,
    page_json TEXT NOT NULL CHECK(length(CAST(page_json AS BLOB)) <= 8192),
    page_digest TEXT NOT NULL,
    PRIMARY KEY(input_request_sequence,cursor)
);
CREATE TRIGGER tool_control_input_binding_immutable
BEFORE UPDATE OF input_request_sequence,parent_request_sequence,thread_id,result_ref,metadata_json,page_count
ON tool_control_input_recoveries
BEGIN SELECT RAISE(ABORT,'control input recovery binding is immutable'); END;
CREATE TRIGGER tool_control_input_direct_delete_rejected
BEFORE DELETE ON tool_control_input_recoveries
WHEN EXISTS(SELECT 1 FROM tool_attempts WHERE request_sequence=OLD.input_request_sequence)
 AND EXISTS(SELECT 1 FROM tool_attempts WHERE request_sequence=OLD.parent_request_sequence)
BEGIN SELECT RAISE(ABORT,'control input recovery requires attempt cleanup'); END;
CREATE TRIGGER tool_control_input_replace_rejected
BEFORE INSERT ON tool_control_input_recoveries
WHEN EXISTS(SELECT 1 FROM tool_control_input_recoveries
 WHERE input_request_sequence=NEW.input_request_sequence OR result_ref=NEW.result_ref)
BEGIN SELECT RAISE(ABORT,'control input recovery cannot be replaced'); END;
CREATE TRIGGER tool_control_input_invalidation_irreversible
BEFORE UPDATE OF invalidated ON tool_control_input_recoveries
WHEN NEW.invalidated < OLD.invalidated
BEGIN SELECT RAISE(ABORT,'control input recovery invalidation is irreversible'); END;
CREATE TRIGGER tool_control_input_reseal_rejected
BEFORE UPDATE OF sealed ON tool_control_input_recoveries
WHEN NEW.sealed=1 AND NEW.invalidated=1
BEGIN SELECT RAISE(ABORT,'control input recovery cannot be resealed'); END;
CREATE TRIGGER tool_control_input_seal_revocation_invalidates
AFTER UPDATE OF sealed ON tool_control_input_recoveries
WHEN OLD.sealed=1 AND NEW.sealed=0
BEGIN UPDATE tool_control_input_recoveries SET invalidated=1 WHERE input_request_sequence=NEW.input_request_sequence; END;
CREATE TRIGGER tool_control_input_body_unseals
AFTER UPDATE OF record_json ON tool_control_input_recoveries
BEGIN UPDATE tool_control_input_recoveries SET invalidated=MAX(invalidated,sealed),sealed=0 WHERE input_request_sequence=NEW.input_request_sequence; END;
CREATE TRIGGER tool_control_input_page_insert_unseals
AFTER INSERT ON tool_control_input_result_pages
BEGIN UPDATE tool_control_input_recoveries SET invalidated=MAX(invalidated,sealed),sealed=0 WHERE input_request_sequence=NEW.input_request_sequence; END;
CREATE TRIGGER tool_control_input_page_update_unseals
AFTER UPDATE ON tool_control_input_result_pages
BEGIN
    UPDATE tool_control_input_recoveries SET invalidated=MAX(invalidated,sealed),sealed=0 WHERE input_request_sequence=OLD.input_request_sequence;
    UPDATE tool_control_input_recoveries SET invalidated=MAX(invalidated,sealed),sealed=0 WHERE input_request_sequence=NEW.input_request_sequence;
END;
CREATE TRIGGER tool_control_input_page_delete_unseals
AFTER DELETE ON tool_control_input_result_pages
BEGIN UPDATE tool_control_input_recoveries SET invalidated=MAX(invalidated,sealed),sealed=0 WHERE input_request_sequence=OLD.input_request_sequence; END;
