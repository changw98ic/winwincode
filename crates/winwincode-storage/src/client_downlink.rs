// SPDX-License-Identifier: Apache-2.0

//! Durable Server → Client downlink outbox for the `ClientControlPort`.
//!
//! Every Server-to-Client frame (`client.enrollment_accepted`,
//! `client.access.challenge`, and later occupancy and worker frames) is
//! persisted here before delivery and is delivered by the client exchange
//! under the per-client `server_to_client_ack_sequence` cursor owned by the
//! `ClientNode` registry (plan 9.2). A frame is retained until the Device
//! Client acknowledges its sequence, so a Server restart or a lost exchange
//! response never loses a downlink command.
//!
//! Sequence discipline is monotonic per client: the caller stamps each frame
//! with the next free stream position — one past the acknowledgement cursor
//! or the highest retained frame, whichever is higher — and the append
//! validates that stamp inside its transaction, so acknowledgement and
//! retention can never regress the stream and concurrent writers can never
//! interleave two frames at one position.

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;
const MAX_FRAME_BYTES: usize = 256 * 1024;

const CLIENT_DOWNLINK_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS client_downlink_frames (
    client_node_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence > 0 AND sequence <= 9007199254740991),
    message_id TEXT NOT NULL,
    frame TEXT NOT NULL CHECK (length(frame) > 0 AND length(frame) <= 262144),
    created_at TEXT NOT NULL,
    PRIMARY KEY (client_node_id, sequence),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS client_downlink_frames_by_created
    ON client_downlink_frames (client_node_id, created_at);
";

/// One durable Server → Client frame awaiting or retaining delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientDownlinkFrame {
    /// Client the frame is routed to.
    pub client_node_id: String,
    /// Contiguous per-client downlink sequence.
    pub sequence: u64,
    /// Envelope message identity of the frame.
    pub message_id: String,
    /// Canonical JSON encoding of the full `ServerToClientEnvelope`.
    pub frame: String,
    /// Instant the frame was appended.
    pub created_at: Instant,
}

/// Command that appends one downlink frame.
///
/// `sequence` must be the next free stream position
/// (`max(server_to_client_ack_sequence, highest retained sequence) + 1`);
/// the append validates it inside the same `IMMEDIATE` transaction, so
/// concurrent writers can never interleave a second frame at one position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientDownlinkAppend {
    client_node_id: String,
    message_id: String,
    sequence: u64,
    frame: String,
}

impl ClientDownlinkAppend {
    /// Builds one validated append command.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node id, an empty message id, a zero
    /// sequence, an oversized frame, or a frame that is not canonical JSON.
    pub fn try_new(
        client_node_id: impl Into<String>,
        message_id: impl Into<String>,
        sequence: u64,
        frame: impl Into<String>,
    ) -> Result<Self, ClientDownlinkError> {
        let append = Self {
            client_node_id: client_node_id.into(),
            message_id: message_id.into(),
            sequence,
            frame: frame.into(),
        };
        validate_client_node_id(&append.client_node_id)?;
        if append.message_id.is_empty() || append.message_id.len() > MAX_ID_BYTES {
            return Err(error(
                ClientDownlinkErrorKind::InvalidInput,
                "downlink message id must contain 1 to 96 bytes",
            ));
        }
        if append.sequence == 0 || append.sequence > MAX_SAFE_INTEGER {
            return Err(error(
                ClientDownlinkErrorKind::InvalidInput,
                "downlink sequence must be between 1 and the safe integer range",
            ));
        }
        if append.frame.is_empty() || append.frame.len() > MAX_FRAME_BYTES {
            return Err(error(
                ClientDownlinkErrorKind::InvalidInput,
                "downlink frame must contain 1 to 262144 bytes",
            ));
        }
        serde_json::from_slice::<serde_json::Value>(append.frame.as_bytes()).map_err(|_| {
            error(
                ClientDownlinkErrorKind::InvalidInput,
                "downlink frame is not canonical JSON",
            )
        })?;
        Ok(append)
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub fn frame(&self) -> &str {
        &self.frame
    }
}

/// Stable downlink outbox failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientDownlinkErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free downlink outbox error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientDownlinkError {
    kind: ClientDownlinkErrorKind,
    message: String,
}

impl ClientDownlinkError {
    #[must_use]
    pub const fn kind(&self) -> ClientDownlinkErrorKind {
        self.kind
    }
}

impl fmt::Display for ClientDownlinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientDownlinkError {}

/// Durable Server → Client downlink outbox over the product-state database.
pub struct ClientDownlinkOutbox<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the durable downlink outbox on this same product-state database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or an incompatible existing schema.
    pub fn client_downlink_outbox(
        &mut self,
    ) -> Result<ClientDownlinkOutbox<'_>, ClientDownlinkError> {
        ClientDownlinkOutbox::new(self)
    }
}

impl<'storage> ClientDownlinkOutbox<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, ClientDownlinkError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(CLIENT_DOWNLINK_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Appends one frame and returns the durable row.
    ///
    /// The command's `sequence` must be exactly
    /// `max(server_to_client_ack_sequence, highest retained sequence) + 1`;
    /// the check and the insert commit in one `IMMEDIATE` transaction, so
    /// concurrent appends and acknowledgements can never collide or regress
    /// the stream.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, a non-canonical command, a sequence
    /// that is not the next stream position, or storage failure.
    pub fn append(
        &mut self,
        append: &ClientDownlinkAppend,
        now: &Instant,
    ) -> Result<ClientDownlinkFrame, ClientDownlinkError> {
        validate_client_node_id(&append.client_node_id)?;
        validate_instant(now)?;
        let transaction = self.transaction()?;
        require_client_node(&transaction, &append.client_node_id)?;
        let cursor = server_to_client_ack(&transaction, &append.client_node_id)?;
        let highest = highest_sequence(&transaction, &append.client_node_id)?;
        if append.sequence != cursor.max(highest) + 1 {
            return Err(error(
                ClientDownlinkErrorKind::InvalidInput,
                "downlink sequence is not the next stream position",
            ));
        }
        transaction
            .execute(
                "INSERT INTO client_downlink_frames
                 (client_node_id, sequence, message_id, frame, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    append.client_node_id,
                    sql_integer(append.sequence)?,
                    append.message_id,
                    append.frame,
                    now.0,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        let stored = ClientDownlinkFrame {
            client_node_id: append.client_node_id.clone(),
            sequence: append.sequence,
            message_id: append.message_id.clone(),
            frame: append.frame.clone(),
            created_at: now.clone(),
        };
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(stored)
    }

    /// Returns the retained frames with a sequence above `after_sequence`,
    /// oldest first, up to `limit` frames.
    ///
    /// # Errors
    ///
    /// Rejects an invalid identity or storage failure.
    pub fn deliverable(
        &self,
        client_node_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<ClientDownlinkFrame>, ClientDownlinkError> {
        validate_client_node_id(client_node_id)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self
            .storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        let mut statement = connection
            .prepare(
                "SELECT client_node_id, sequence, message_id, frame, created_at
                 FROM client_downlink_frames
                 WHERE client_node_id = ?1 AND sequence > ?2
                 ORDER BY sequence LIMIT ?3",
            )
            .map_err(|sql| sql_error(&sql))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let frames = statement
            .query_map(
                params![client_node_id, sql_integer(after_sequence)?, limit],
                read_frame_row,
            )
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        Ok(frames)
    }

    /// Returns the highest retained sequence for one client, or zero.
    ///
    /// # Errors
    ///
    /// Rejects an invalid identity or storage failure.
    pub fn high_water(&self, client_node_id: &str) -> Result<u64, ClientDownlinkError> {
        validate_client_node_id(client_node_id)?;
        let connection = self
            .storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        highest_sequence(connection, client_node_id)
    }

    /// Deletes every retained frame at or below `ack_sequence` and returns
    /// the deleted count.
    ///
    /// # Errors
    ///
    /// Rejects an invalid identity or storage failure.
    pub fn retain_through(
        &mut self,
        client_node_id: &str,
        ack_sequence: u64,
    ) -> Result<u64, ClientDownlinkError> {
        validate_client_node_id(client_node_id)?;
        let connection = self
            .storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?;
        let deleted = connection
            .execute(
                "DELETE FROM client_downlink_frames
                 WHERE client_node_id = ?1 AND sequence <= ?2",
                params![client_node_id, sql_integer(ack_sequence)?],
            )
            .map_err(|sql| sql_error(&sql))?;
        u64::try_from(deleted).map_err(|_| {
            error(
                ClientDownlinkErrorKind::CorruptState,
                "deleted downlink frame count is negative",
            )
        })
    }

    /// Deletes every retained frame of one client; a maintenance entry point
    /// for terminal (`revoked`) identities.
    ///
    /// # Errors
    ///
    /// Rejects an invalid identity or storage failure.
    pub fn purge(&mut self, client_node_id: &str) -> Result<u64, ClientDownlinkError> {
        self.retain_through(client_node_id, MAX_SAFE_INTEGER)
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, ClientDownlinkError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }
}

fn require_client_node(
    connection: &rusqlite::Connection,
    client_node_id: &str,
) -> Result<(), ClientDownlinkError> {
    let known = connection
        .query_row(
            "SELECT 1 FROM client_nodes WHERE client_node_id = ?1",
            [client_node_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    if known.is_none() {
        return Err(error(
            ClientDownlinkErrorKind::UnknownClientNode,
            "client node does not exist",
        ));
    }
    Ok(())
}

fn server_to_client_ack(
    connection: &rusqlite::Connection,
    client_node_id: &str,
) -> Result<u64, ClientDownlinkError> {
    connection
        .query_row(
            "SELECT server_to_client_ack_sequence FROM client_exchange_cursors
             WHERE client_node_id = ?1",
            [client_node_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(|stored| from_sql_integer(stored, "server-to-client ack sequence"))
        .transpose()
        .map(|value| value.unwrap_or(0))
}

fn highest_sequence(
    connection: &rusqlite::Connection,
    client_node_id: &str,
) -> Result<u64, ClientDownlinkError> {
    connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM client_downlink_frames
             WHERE client_node_id = ?1",
            [client_node_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|sql| sql_error(&sql))
        .and_then(|stored| from_sql_integer(stored, "downlink frame sequence"))
}

fn read_frame_row(row: &rusqlite::Row<'_>) -> Result<ClientDownlinkFrame, rusqlite::Error> {
    let client_node_id = row.get::<_, String>(0)?;
    let sequence = row.get::<_, i64>(1)?;
    let message_id = row.get::<_, String>(2)?;
    let frame = row.get::<_, String>(3)?;
    let created_at = row.get::<_, String>(4)?;
    let sequence = u64::try_from(sequence).map_err(|_| {
        rusqlite::Error::InvalidColumnType(1, "sequence".to_owned(), rusqlite::types::Type::Integer)
    })?;
    Ok(ClientDownlinkFrame {
        client_node_id,
        sequence,
        message_id,
        frame,
        created_at: Instant(created_at),
    })
}

fn validate_schema(connection: &rusqlite::Connection) -> Result<(), ClientDownlinkError> {
    let pragma = "PRAGMA table_info(client_downlink_frames)";
    let mut statement = connection.prepare(pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns
        != [
            "client_node_id",
            "sequence",
            "message_id",
            "frame",
            "created_at",
        ]
    {
        return Err(error(
            ClientDownlinkErrorKind::CorruptState,
            "client downlink outbox schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_client_node_id(value: &str) -> Result<(), ClientDownlinkError> {
    let Some(suffix) = value.strip_prefix("cnd_") else {
        return Err(error(
            ClientDownlinkErrorKind::InvalidInput,
            "client node id is not canonical",
        ));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_base32)
    {
        return Err(error(
            ClientDownlinkErrorKind::InvalidInput,
            "client node id is not canonical",
        ));
    }
    Ok(())
}

/// Validates the canonical `domain.Instant` shape (`YYYY-MM-DDTHH:MM:SS.sssZ`).
fn validate_instant(value: &Instant) -> Result<(), ClientDownlinkError> {
    let bytes = value.0.as_bytes();
    let punctuation = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
    ];
    let valid = bytes.len() == 24
        && bytes[23] == b'Z'
        && punctuation
            .iter()
            .all(|(index, byte)| bytes[*index] == *byte)
        && bytes.iter().enumerate().all(|(index, byte)| {
            punctuation.iter().any(|(at, _)| at == &index) || index == 23 || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(error(
            ClientDownlinkErrorKind::InvalidInput,
            "downlink instant is not canonical",
        ))
    }
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'A'..=b'H'
            | b'J'
            | b'K'
            | b'M'
            | b'N'
            | b'P'..=b'T'
            | b'V'..=b'Z'
    )
}

fn sql_integer(value: u64) -> Result<i64, ClientDownlinkError> {
    match i64::try_from(value) {
        Ok(value) => Ok(value),
        Err(_) => Err(error(
            ClientDownlinkErrorKind::InvalidInput,
            "numeric value exceeds the SQLite integer range",
        )),
    }
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, ClientDownlinkError> {
    let value = u64::try_from(value).map_err(|_| {
        error(
            ClientDownlinkErrorKind::CorruptState,
            format!("stored {label} is negative"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            ClientDownlinkErrorKind::CorruptState,
            format!("stored {label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn storage_error(storage: &StorageError) -> ClientDownlinkError {
    error(
        ClientDownlinkErrorKind::Storage,
        format!("client downlink outbox storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> ClientDownlinkError {
    ClientDownlinkError {
        kind: ClientDownlinkErrorKind::Storage,
        message: "client downlink outbox storage operation failed".to_owned(),
    }
}

fn error(kind: ClientDownlinkErrorKind, message: impl Into<String>) -> ClientDownlinkError {
    ClientDownlinkError {
        kind,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_rejects_non_canonical_commands() {
        assert!(ClientDownlinkAppend::try_new("nope", "msg_1", 1, "{}").is_err());
        assert!(
            ClientDownlinkAppend::try_new("cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1", "", 1, "{}").is_err()
        );
        assert!(
            ClientDownlinkAppend::try_new("cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1", "msg_1", 0, "{}")
                .is_err()
        );
        assert!(
            ClientDownlinkAppend::try_new("cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1", "msg_1", 1, "not json")
                .is_err()
        );
        let valid = ClientDownlinkAppend::try_new(
            "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1",
            "msg_AAAAAAAAAAAAAAAAAAAAAAAA1",
            3,
            r#"{"kind":"x"}"#,
        )
        .expect("valid append");
        assert_eq!(valid.client_node_id(), "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1");
        assert_eq!(valid.message_id(), "msg_AAAAAAAAAAAAAAAAAAAAAAAA1");
        assert_eq!(valid.sequence(), 3);
        assert_eq!(valid.frame(), r#"{"kind":"x"}"#);
    }

    #[test]
    fn frame_text_must_be_canonical_json_within_the_byte_bound() {
        let padded = format!("\"{}\"", "x".repeat(MAX_FRAME_BYTES));
        assert!(
            ClientDownlinkAppend::try_new(
                "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1",
                "msg_AAAAAAAAAAAAAAAAAAAAAAAA1",
                1,
                padded,
            )
            .is_err(),
            "an oversized frame is rejected"
        );
    }
}
