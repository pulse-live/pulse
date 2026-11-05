//! Private SQLite state with bounded rollback-journal storage and durable acknowledgements.
//! Calls are serialized off the runtime; there is no database task per observation.

use crate::{
    config::Limits,
    observation::{Observation, unix_ms},
    protocol::{Command, Credentials},
};
use anyhow::{Context, Result, ensure};
use prost::Message as _;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Enrollment credentials are bound to their issuing control plane.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub server: String,
    pub credentials: Credentials,
}

/// One serialized connection, bounded file sizes, and private local state paths.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    directory: PathBuf,
    payload_limit: usize,
    record_limit: usize,
}

/// A process holds this file lock for the whole run or enrollment operation.
pub struct RunLock {
    _file: File,
}

impl Store {
    /// Open a private state directory and impose a page cap with journal headroom.
    /// # Errors
    /// Rejects public/symlink state paths, incompatible schemas, or database errors.
    pub async fn open(directory: &Path, limits: &Limits) -> Result<Self> {
        let directory = directory.to_path_buf();
        let path = directory.clone();
        let budget = usize::try_from(limits.max_spool_mb)? * 1_048_576;
        let connection = tokio::task::spawn_blocking(move || {
            let connection = open_connection(&path, true)?;
            connection.execute_batch("PRAGMA journal_mode=DELETE;")?;
            let page_size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
            let page_size = usize::try_from(page_size)?;
            ensure!(page_size > 0, "invalid database page size");
            let pages = i64::try_from(budget / 3 / page_size)?;
            connection.pragma_update(None, "max_page_count", pages)?;
            let count: i64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
            ensure!(count <= pages, "existing state exceeds configured spool budget");
            connection.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
                CREATE TABLE IF NOT EXISTS observations (id TEXT PRIMARY KEY, payload BLOB NOT NULL);
                CREATE TABLE IF NOT EXISTS commands (id TEXT PRIMARY KEY, deadline INTEGER NOT NULL, completed INTEGER NOT NULL, payload BLOB NOT NULL);
                CREATE INDEX IF NOT EXISTS command_deadline ON commands(deadline);
                CREATE TABLE IF NOT EXISTS stats (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
                INSERT OR IGNORE INTO stats(key,value) SELECT 'bytes',COALESCE(SUM(length(payload)),0) FROM observations;")?;

            connection.pragma_update(None, "user_version", 1)?;
            Ok::<_, anyhow::Error>(connection)
        }).await.context("state initialization task failed")??;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            directory,
            payload_limit: budget / 4,
            // A retained record must fit one wire batch including framing.
            record_limit: usize::try_from(limits.max_result_bytes)?.min(262_112),
        })
    }

    /// Open existing metadata without creating state or applying a caller's spool budget.
    /// Local status, opt-out, and credential recovery must work with any accepted spool size.
    pub(crate) async fn metadata(directory: &Path) -> Result<Self> {
        let directory = directory.to_path_buf();
        let path = directory.clone();
        let connection = tokio::task::spawn_blocking(move || open_connection(&path, false))
            .await
            .context("metadata initialization task failed")??;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            directory,
            // Measurement writes require the executor's explicit local limits.
            payload_limit: 0,
            record_limit: 0,
        })
    }

    /// Prevent two local executors or enrollment writers from sharing one identity.
    /// # Errors
    /// Returns an error when another agent holds the state lock.
    pub fn lock(&self) -> Result<RunLock> {
        let file = private_file(&self.directory.join("run.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("another Pulse process is using this state directory")?;
        Ok(RunLock { _file: file })
    }

    /// Inspect the process lock without interpreting a stale health snapshot as liveness.
    /// # Errors
    /// Returns file-lock errors unrelated to an active writer.
    pub fn running(&self) -> Result<bool> {
        let file = private_file(&self.directory.join("run.lock"))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    async fn access<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| anyhow::anyhow!("agent state lock poisoned"))?;
            operation(&mut connection)
        })
        .await
        .context("state operation task failed")?
    }

    /// Atomically store bounded JSON state using SQLite's durable commit.
    /// # Errors
    /// Returns serialization, capacity, or disk errors.
    pub async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(bytes.len() <= 1_048_576, "state document exceeds 1 MiB");
        let key = key.to_owned();
        self.access(move |connection| {
            connection.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, bytes])?;
            Ok(())
        }).await
    }

    /// Read typed local state without exposing corrupt input values in errors.
    /// # Errors
    /// Returns database or schema errors.
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let key = key.to_owned();
        let bytes: Option<Vec<u8>> = self
            .access(move |connection| {
                Ok(connection
                    .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
                        row.get(0)
                    })
                    .optional()?)
            })
            .await?;
        bytes
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("invalid persisted state document"))
            })
            .transpose()
    }

    /// Retain a result before delivery; overflow drops the newest record explicitly.
    /// # Errors
    /// Returns database errors other than bounded-capacity exhaustion.
    pub async fn push(&self, observation: &Observation) -> Result<bool> {
        self.require_spool_writer()?;
        let observation = observation.clone();
        let cap = self.payload_limit;
        let record = self.record_limit;
        self.access(move |connection| {
            let bytes = observation.encode_to_vec();
            let transaction =
                connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let existing: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT payload FROM observations WHERE id=?1",
                    [&observation.id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(existing) = existing {
                ensure!(
                    existing == bytes,
                    "observation ID reused with different content"
                );
                transaction.commit()?;
                return Ok(true);
            }
            let used: i64 =
                transaction.query_row("SELECT value FROM stats WHERE key='bytes'", [], |row| {
                    row.get(0)
                })?;
            if bytes.len() > record || bytes.len() > cap.saturating_sub(usize::try_from(used)?) {
                finish_command(&transaction, &observation)?;
                transaction.commit()?;
                return Ok(false);
            }
            let result = transaction.execute(
                "INSERT INTO observations(id,payload) VALUES(?1,?2)",
                params![observation.id, bytes],
            );
            match result {
                Ok(_) => {
                    transaction.execute(
                        "UPDATE stats SET value=value+?1 WHERE key='bytes'",
                        [i64::try_from(bytes.len())?],
                    )?;
                    finish_command(&transaction, &observation)?;
                    transaction.commit()?;
                    Ok(true)
                }
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DiskFull =>
                {
                    drop(transaction);
                    finish_command(connection, &observation)?;
                    Ok(false)
                }
                Err(error) => Err(error.into()),
            }
        })
        .await
    }

    /// Load at most 64 records and 256 KiB for one transport request.
    /// # Errors
    /// Rejects corrupt records rather than silently acknowledging them.
    pub async fn batch(&self) -> Result<Vec<Observation>> {
        self.access(|connection| {
            let mut statement =
                connection.prepare("SELECT payload FROM observations ORDER BY rowid LIMIT 64")?;
            let mut rows = statement.query([])?;
            let mut records = Vec::new();
            let mut bytes = 16;
            while let Some(row) = rows.next()? {
                let payload: Vec<u8> = row.get(0)?;
                if bytes + payload.len() + 16 > 262_144 {
                    break;
                }
                bytes += payload.len() + 16;
                records.push(
                    Observation::decode(payload.as_slice())
                        .context("invalid retained observation")?,
                );
            }
            Ok(records)
        })
        .await
    }

    /// Remove only IDs explicitly acknowledged as durably accepted by the destination.
    /// # Errors
    /// Returns disk or transaction errors; records remain eligible for safe retry.
    pub async fn acknowledge(&self, ids: Vec<String>) -> Result<()> {
        self.require_spool_writer()?;
        ensure!(ids.len() <= 64, "acknowledgement exceeds batch size");
        self.access(move |connection| {
            let transaction = connection.transaction()?;
            for id in ids {
                let bytes: Option<i64> = transaction
                    .query_row(
                        "SELECT length(payload) FROM observations WHERE id=?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .optional()?;
                transaction.execute("DELETE FROM observations WHERE id=?1", [id])?;
                transaction.execute(
                    "UPDATE stats SET value=value-?1 WHERE key='bytes'",
                    [bytes.unwrap_or(0)],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// Reserve an on-demand ID durably before claiming or executing its work.
    /// # Errors
    /// Returns disk/capacity errors. Existing IDs return false, including after restart.
    pub async fn reserve(&self, command: &Command, observation: &Observation) -> Result<bool> {
        self.require_spool_writer()?;
        let command = command.clone();
        let payload = observation.encode_to_vec();
        self.access(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "DELETE FROM commands WHERE deadline<=?1",
                [i64::try_from(unix_ms())?],
            )?;
            let count: i64 =
                transaction.query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))?;
            ensure!(
                count < 1024,
                "local command deduplication capacity exhausted"
            );
            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO commands(id,deadline,completed,payload) VALUES(?1,?2,0,?3)",
                params![
                    command.id,
                    i64::try_from(command.deadline_unix_ms)?,
                    payload
                ],
            )?;
            transaction.commit()?;
            Ok(inserted == 1)
        })
        .await
    }

    /// Deduplicate known command IDs before consuming rate or concurrency budgets.
    /// # Errors
    /// Returns database read errors.
    pub async fn known_command(&self, id: &str) -> Result<bool> {
        let id = id.to_owned();
        self.access(move |connection| {
            Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM commands WHERE id=?1)",
                [id],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// Recover uncertain in-flight command outcomes after obtaining the exclusive run lock.
    /// # Errors
    /// Returns corrupt state or disk errors; does not repeat an uncertain network operation.
    pub async fn recover(&self) -> Result<u64> {
        self.require_spool_writer()?;
        let records = self
            .access(|connection| {
                let mut statement = connection
                    .prepare("SELECT payload FROM commands WHERE completed=0 LIMIT 1024")?;
                let values = statement
                    .query_map([], |row| row.get::<_, Vec<u8>>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(values)
            })
            .await?;
        let mut dropped = 0;
        for payload in records {
            let mut observation = Observation::decode(payload.as_slice())?;
            observation.error = Some("execution_interrupted_outcome_unknown".into());
            observation.completed_unix_ms = unix_ms();
            if !self.push(&observation).await? {
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    /// Read constant-time retained-byte accounting for status and saturation diagnostics.
    /// # Errors
    /// Returns database errors.
    pub async fn queued_bytes(&self) -> Result<u64> {
        let value: i64 = self
            .access(|connection| {
                Ok(connection.query_row(
                    "SELECT value FROM stats WHERE key='bytes'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await?;
        Ok(u64::try_from(value)?)
    }

    /// Return durable spool count for local machine-readable status.
    /// # Errors
    /// Returns database errors.
    pub async fn queued(&self) -> Result<u64> {
        let value: i64 = self
            .access(|connection| {
                Ok(connection
                    .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))?)
            })
            .await?;
        Ok(u64::try_from(value)?)
    }

    fn require_spool_writer(&self) -> Result<()> {
        ensure!(
            self.payload_limit > 0,
            "measurement writes require configured spool limits"
        );
        Ok(())
    }
}

fn open_connection(directory: &Path, create: bool) -> Result<Connection> {
    let file = directory.join("state.sqlite3");
    ensure!(
        create || file.exists(),
        "no state exists at the selected directory"
    );
    secure_directory(directory)?;
    private_file(&file)?;
    let mut flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    if create {
        flags |= rusqlite::OpenFlags::SQLITE_OPEN_CREATE;
    }
    let connection =
        Connection::open_with_flags(file, flags).context("cannot open agent state database")?;
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(version <= 1, "state schema is newer than this binary");
    ensure!(create || version == 1, "state schema is not initialized");
    connection.busy_timeout(std::time::Duration::from_secs(1))?;
    connection.execute_batch("PRAGMA synchronous=EXTRA; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-512; PRAGMA trusted_schema=OFF;")?;
    Ok(connection)
}

fn finish_command(connection: &Connection, observation: &Observation) -> Result<()> {
    if let Some(id) = &observation.command_id {
        connection.execute("UPDATE commands SET completed=1 WHERE id=?1", [id])?;
    }
    Ok(())
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode().trailing_zeros() >= 6,
        "agent state directory must be a private directory (0700)"
    );
    Ok(())
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            ensure!(
                metadata.uid() == rustix::process::geteuid().as_raw()
                    && metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.permissions().mode().trailing_zeros() >= 6,
                "agent state files must be regular private files (0600)"
            );
            Ok(OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())?)
                .open(path)?)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<()> {
    anyhow::bail!("secure agent state currently requires Unix");
}
#[cfg(not(unix))]
fn private_file(_path: &Path) -> Result<File> {
    anyhow::bail!("secure agent state currently requires Unix");
}
