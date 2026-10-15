// SPDX-License-Identifier: Apache-2.0

use crate::{ClientNodeRegistration, SqliteStorage, StorageError, sql_error};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS device_pairing_tokens (
 token_digest TEXT PRIMARY KEY NOT NULL, owner_user_id TEXT NOT NULL,
 expires_at TEXT NOT NULL, consumed_request_id TEXT
);
CREATE TABLE IF NOT EXISTS device_identity_bindings (
 client_node_id TEXT PRIMARY KEY NOT NULL REFERENCES client_nodes(client_node_id),
 public_key TEXT UNIQUE NOT NULL, owner_user_id TEXT NOT NULL,
 request_id TEXT UNIQUE NOT NULL, intent_digest TEXT NOT NULL,
 token_digest TEXT NOT NULL, response_json TEXT NOT NULL,
 bound_at TEXT NOT NULL, revoked_at TEXT
);
CREATE TABLE IF NOT EXISTS device_auth_challenges (
 challenge_id TEXT PRIMARY KEY NOT NULL, public_key TEXT NOT NULL,
 challenge TEXT NOT NULL, expires_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS device_auth_events (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT, client_node_id TEXT NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN ('device.bound','device.revoked')), occurred_at TEXT NOT NULL
);
";

pub(crate) fn create_schema(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    transaction.execute_batch(SCHEMA).map_err(sql_error)
}

pub(crate) fn validate_schema(connection: &rusqlite::Connection) -> Result<(), StorageError> {
    for query in [
        "SELECT token_digest,owner_user_id,expires_at,consumed_request_id FROM device_pairing_tokens LIMIT 0",
        "SELECT client_node_id,public_key,owner_user_id,request_id,intent_digest,token_digest,response_json,bound_at,revoked_at FROM device_identity_bindings LIMIT 0",
        "SELECT challenge_id,public_key,challenge,expires_at FROM device_auth_challenges LIMIT 0",
        "SELECT sequence,client_node_id,kind,occurred_at FROM device_auth_events LIMIT 0",
    ] {
        connection.prepare(query).map_err(sql_error)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAuthStoreError {
    Unavailable,
    Authentication,
    InvalidRequest,
    Conflict,
    Revoked,
    NotFound,
}

impl From<rusqlite::Error> for DeviceAuthStoreError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Unavailable
    }
}
impl From<StorageError> for DeviceAuthStoreError {
    fn from(_: StorageError) -> Self {
        Self::Unavailable
    }
}

pub struct DeviceBindingWrite<'a> {
    pub request_id: &'a str,
    pub public_key: &'a str,
    pub token_digest: &'a str,
    pub intent_digest: &'a str,
    pub registration: &'a ClientNodeRegistration,
    pub access_grant_id: &'a str,
    pub now: &'a str,
}

impl SqliteStorage {
    pub fn device_identity_owned_by(
        &self,
        node: &str,
        owner: &str,
    ) -> Result<bool, DeviceAuthStoreError> {
        self.connection()?.query_row("SELECT EXISTS(SELECT 1 FROM device_identity_bindings WHERE client_node_id=?1 AND owner_user_id=?2 AND revoked_at IS NULL)",params![node,owner],|row|row.get(0)).map_err(Into::into)
    }

    pub fn active_remote_worker_instances(
        &mut self,
        now: &str,
    ) -> Result<Vec<(String, String)>, DeviceAuthStoreError> {
        self.client_node_registry()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        self.worker_launch_grant_ledger()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        self.client_occupancy_ledger()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        let mut statement=self.connection()?.prepare("SELECT DISTINCT g.worker_id,g.worker_instance_id FROM worker_launch_grants g JOIN client_nodes n ON n.client_node_id=g.client_node_id JOIN client_occupancy_leases o ON o.occupancy_lease_id=g.occupancy_lease_id WHERE g.state IN ('issued','consumed') AND g.ended_at IS NULL AND (g.state='consumed' OR g.expires_at>?1) AND n.presence_state!='revoked' AND n.current_instance_id=g.client_instance_id AND o.state IN ('occupied','draining','recovery_pending') AND o.client_node_id=g.client_node_id AND o.fencing_token=g.occupancy_fencing_token AND NOT EXISTS(SELECT 1 FROM worker_launch_grant_exits e WHERE e.worker_launch_grant_id=g.worker_launch_grant_id) AND NOT EXISTS(SELECT 1 FROM worker_launch_process_closures c WHERE c.grant_id=g.worker_launch_grant_id AND c.reporting_client_instance_id=g.client_instance_id)")?;
        statement
            .query_map([now], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn initialize_device_auth(&mut self) -> Result<(), DeviceAuthStoreError> {
        self.client_node_registry()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        self.client_connect_ledger()
            .map_err(|_| DeviceAuthStoreError::Unavailable)?;
        self.connection()?.execute_batch(SCHEMA)?;
        Ok(())
    }

    pub fn publish_device_pairing(
        &mut self,
        digest: &str,
        owner: &str,
        expiry: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        self.connection()?.execute("INSERT INTO device_pairing_tokens(token_digest,owner_user_id,expires_at) VALUES(?1,?2,?3)",params![digest,owner,expiry])?;
        Ok(())
    }

    pub fn publish_device_challenge(
        &mut self,
        id: &str,
        key: &str,
        challenge: &str,
        expiry: &str,
        now: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        let tx = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM device_auth_challenges WHERE expires_at<=?1",
            [now],
        )?;
        let count: i64 = tx.query_row("SELECT count(*) FROM device_auth_challenges", [], |r| {
            r.get(0)
        })?;
        if count >= 4096 {
            return Err(DeviceAuthStoreError::Unavailable);
        }
        tx.execute(
            "INSERT INTO device_auth_challenges VALUES(?1,?2,?3,?4)",
            params![id, key, challenge, expiry],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn device_challenge(
        &mut self,
        id: &str,
        key: &str,
        now: &str,
    ) -> Result<String, DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        self.connection()?.query_row("SELECT challenge FROM device_auth_challenges WHERE challenge_id=?1 AND public_key=?2 AND expires_at>?3",params![id,key,now],|r|r.get(0)).optional()?.ok_or(DeviceAuthStoreError::Authentication)
    }

    pub fn consume_device_challenge(
        &mut self,
        id: &str,
        key: &str,
        challenge: &str,
        now: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        if self.connection()?.execute("DELETE FROM device_auth_challenges WHERE challenge_id=?1 AND public_key=?2 AND challenge=?3 AND expires_at>?4",params![id,key,challenge,now])? !=1 { return Err(DeviceAuthStoreError::Authentication); }
        Ok(())
    }

    pub fn authenticate_device_binding(
        &mut self,
        node: &str,
        key: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        let record:Option<(Option<String>,String)>=self.connection()?.query_row("SELECT b.revoked_at,n.presence_state FROM device_identity_bindings b JOIN client_nodes n USING(client_node_id) WHERE b.client_node_id=?1 AND b.public_key=?2",params![node,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        match record {
            Some((None, state)) if state != "revoked" => Ok(()),
            Some(_) => Err(DeviceAuthStoreError::Revoked),
            None => Err(DeviceAuthStoreError::Authentication),
        }
    }

    pub fn bind_device_identity(
        &mut self,
        write: &DeviceBindingWrite<'_>,
    ) -> Result<String, DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        let tx = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior:Option<(String,String,String,String,Option<String>,String)>=tx.query_row("SELECT b.public_key,b.intent_digest,b.token_digest,b.response_json,b.revoked_at,n.presence_state FROM device_identity_bindings b JOIN client_nodes n USING(client_node_id) WHERE request_id=?1",[write.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        if let Some((key, intent, token, response, revoked, presence)) = prior {
            if key != write.public_key
                || intent != write.intent_digest
                || token != write.token_digest
            {
                return Err(DeviceAuthStoreError::Conflict);
            }
            if revoked.is_some() || presence == "revoked" {
                return Err(DeviceAuthStoreError::Revoked);
            }
            return Ok(response);
        }
        let owner:String=tx.query_row("SELECT owner_user_id FROM device_pairing_tokens WHERE token_digest=?1 AND consumed_request_id IS NULL AND expires_at>?2",params![write.token_digest,write.now],|r|r.get(0)).optional()?.ok_or(DeviceAuthStoreError::Authentication)?;
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM device_identity_bindings WHERE public_key=?1)",
            [write.public_key],
            |r| r.get(0),
        )?;
        if existing {
            return Err(DeviceAuthStoreError::Conflict);
        }
        let r = write.registration;
        tx.execute("INSERT INTO client_nodes(client_node_id,public_client_id,display_name,platform,architecture,client_version,current_instance_id,created_at,revision) VALUES(?1,?2,?3,?4,?5,?6,NULL,?7,1)",params![r.client_node_id(),r.public_client_id(),r.display_name(),r.platform(),r.architecture(),r.client_version(),write.now])?;
        tx.execute(
            "INSERT INTO client_exchange_cursors(client_node_id) VALUES(?1)",
            [r.client_node_id()],
        )?;
        let response=serde_json::json!({"schemaVersion":"winwincode/v1","requestId":write.request_id,"clientNodeId":r.client_node_id(),"publicClientId":r.public_client_id(),"ownerUserId":owner,"boundAt":write.now}).to_string();
        tx.execute(
            "INSERT INTO device_identity_bindings VALUES(?1,?2,?3,?4,?5,?6,?7,?8,NULL)",
            params![
                r.client_node_id(),
                write.public_key,
                owner,
                write.request_id,
                write.intent_digest,
                write.token_digest,
                response,
                write.now
            ],
        )?;
        tx.execute("INSERT INTO client_access_grants VALUES(?1,?2,?3,'use+manage+share','trusted','active','administrator',?3,NULL,?4,1)",params![write.access_grant_id,r.client_node_id(),owner,write.now])?;
        tx.execute(
            "UPDATE device_pairing_tokens SET consumed_request_id=?2 WHERE token_digest=?1",
            params![write.token_digest, write.request_id],
        )?;
        tx.execute("INSERT INTO device_auth_events(client_node_id,kind,occurred_at) VALUES(?1,'device.bound',?2)",params![r.client_node_id(),write.now])?;
        tx.commit()?;
        Ok(response)
    }

    pub fn revoke_device_identity(
        &mut self,
        node: &str,
        owner: &str,
        now: &str,
    ) -> Result<(), DeviceAuthStoreError> {
        self.initialize_device_auth()?;
        let tx = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorized:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM device_identity_bindings WHERE client_node_id=?1 AND owner_user_id=?2)",params![node,owner],|r|r.get(0))?;
        if !authorized {
            return Err(DeviceAuthStoreError::NotFound);
        }
        let newly_revoked=tx.execute("UPDATE device_identity_bindings SET revoked_at=?2 WHERE client_node_id=?1 AND revoked_at IS NULL",params![node,now])?;
        if newly_revoked == 0 {
            tx.commit()?;
            return Ok(());
        }
        tx.execute("UPDATE client_nodes SET presence_state='revoked',accepting_connections=0,revision=revision+1 WHERE client_node_id=?1 AND presence_state!='revoked'",[node])?;
        tx.execute("UPDATE client_access_grants SET state='revoked',revision=revision+1 WHERE client_node_id=?1 AND state='active'",[node])?;
        for (table, sql) in [
            (
                "client_occupancy_leases",
                "UPDATE client_occupancy_leases SET state='released',release_reason='force_released',idle_expires_at=NULL,recovery_deadline_at=NULL,revision=revision+1 WHERE client_node_id=?1 AND state IN ('reserving','occupied','draining','recovery_pending')",
            ),
            (
                "worker_launch_grants",
                "UPDATE worker_launch_grants SET state='revoked',ended_at=?2,revision=revision+1 WHERE client_node_id=?1 AND state IN ('issued','consumed')",
            ),
            (
                "device_execution_bindings",
                "UPDATE device_execution_bindings SET state='released',released_at=?2,revision=revision+1 WHERE client_node_id=?1 AND state='bound'",
            ),
            (
                "device_scheduler_reservations",
                "UPDATE device_scheduler_reservations SET state='released',release_reason='expired',updated_at=?2,revision=revision+1 WHERE client_node_id=?1 AND state!='released'",
            ),
        ] {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |r| r.get(0),
            )?;
            if exists {
                let mut statement = tx.prepare(sql)?;
                if statement.parameter_count() == 2 {
                    statement.execute(params![node, now])?;
                } else {
                    statement.execute([node])?;
                }
            }
        }
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='execution_leases')",[],|r|r.get(0))?;
        let grants:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='worker_launch_grants')",[],|r|r.get(0))?;
        if exists && grants {
            tx.execute("UPDATE execution_leases SET expires_at=?2 WHERE EXISTS(SELECT 1 FROM worker_launch_grants g WHERE g.client_node_id=?1 AND g.worker_id=execution_leases.worker_id AND g.worker_instance_id=execution_leases.worker_instance_id)",params![node,now])?;
        }
        tx.execute("INSERT INTO device_auth_events(client_node_id,kind,occurred_at) VALUES(?1,'device.revoked',?2)",params![node,now])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_binding_access_and_receipt_roll_back_together() {
        let directory =
            std::env::temp_dir().join(format!("wwc-binding-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let mut storage = SqliteStorage::open(&directory).unwrap();
        storage
            .publish_device_pairing(
                "token-digest",
                "usr_00000000000000000000000001",
                "2027-01-01T00:00:00.000Z",
            )
            .unwrap();
        storage.connection().unwrap().execute_batch("CREATE TRIGGER binding_fault BEFORE INSERT ON device_auth_events BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        let registration = ClientNodeRegistration::try_new(
            "cnd_00000000000000000000000001",
            "123456789012",
            "Device",
            "aarch64-apple-darwin",
            "aarch64",
            "0.2.0",
            None,
            0,
        )
        .unwrap();
        let mut write = DeviceBindingWrite {
            request_id: "req_00000000000000000000000001",
            public_key: "key",
            token_digest: "token-digest",
            intent_digest: "intent",
            registration: &registration,
            access_grant_id: "cag_00000000000000000000000001",
            now: "2026-01-01T00:00:00.000Z",
        };
        assert_eq!(
            storage.bind_device_identity(&write),
            Err(DeviceAuthStoreError::Unavailable)
        );
        let count: i64 = storage
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM client_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        for table in [
            "device_identity_bindings",
            "client_access_grants",
            "device_auth_events",
        ] {
            let count: i64 = storage
                .connection()
                .unwrap()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
        let consumed: Option<String> = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT consumed_request_id FROM device_pairing_tokens",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(consumed, None);
        storage
            .connection()
            .unwrap()
            .execute_batch("DROP TRIGGER binding_fault")
            .unwrap();
        let receipt = storage.bind_device_identity(&write).unwrap();
        write.now = "2028-01-01T00:00:00.000Z";
        assert_eq!(storage.bind_device_identity(&write).unwrap(), receipt);
        let permissions: String = storage
            .connection()
            .unwrap()
            .query_row("SELECT permissions FROM client_access_grants", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(permissions, "use+manage+share");
        storage
            .revoke_device_identity(
                registration.client_node_id(),
                "usr_00000000000000000000000001",
                write.now,
            )
            .unwrap();
        assert_eq!(
            storage.bind_device_identity(&write),
            Err(DeviceAuthStoreError::Revoked)
        );
        drop(storage);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
