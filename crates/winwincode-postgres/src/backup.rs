// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::sync::{Arc, Mutex};

use winwincode_backup::{
    BackupComponentKind, BackupComponentSnapshot, BackupSnapshotRequest, BackupSnapshotSource,
    BackupSnapshotSourceError,
};

use crate::PostgresProtocolPort;

/// One narrow secret-free backup component exported from `PostgreSQL`.
pub struct PostgresBackupSnapshotSource<P: PostgresProtocolPort> {
    protocol: Arc<Mutex<P>>,
    kind: BackupComponentKind,
}

impl<P: PostgresProtocolPort> fmt::Debug for PostgresBackupSnapshotSource<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostgresBackupSnapshotSource")
            .field("protocol", &"[BOUND]")
            .field("kind", &self.kind)
            .finish()
    }
}

impl<P: PostgresProtocolPort> PostgresBackupSnapshotSource<P> {
    pub(crate) const fn new(protocol: Arc<Mutex<P>>, kind: BackupComponentKind) -> Self {
        Self { protocol, kind }
    }
}

impl<P: PostgresProtocolPort> BackupSnapshotSource for PostgresBackupSnapshotSource<P> {
    fn kind(&self) -> BackupComponentKind {
        self.kind
    }

    fn snapshot(
        &mut self,
        request: &BackupSnapshotRequest,
    ) -> Result<BackupComponentSnapshot, BackupSnapshotSourceError> {
        let export = self
            .protocol
            .lock()
            .map_err(|_| BackupSnapshotSourceError::new())?
            .export_snapshot(self.kind, request.scope(), request.consistency_cut_digest())
            .map_err(|_| BackupSnapshotSourceError::new())?;
        BackupComponentSnapshot::try_new(
            self.kind,
            request.scope().clone(),
            request.consistency_cut_digest().clone(),
            export.checkpoint_digest().clone(),
            export.content_digest().clone(),
            export.record_count(),
            export.byte_count(),
        )
        .map_err(|_| BackupSnapshotSourceError::new())
    }
}
