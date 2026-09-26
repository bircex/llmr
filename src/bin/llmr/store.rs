//! The gateway's state, in one SQLite file on the data volume.
//!
//! Providers, the models each serves, and the route sets clients ask for. Credentials are
//! sealed with the master key before they are written, and the database carries a sealed
//! marker so that starting with the wrong key fails at startup, not at the first request that
//! needs a credential.
//!
//! Every method here is synchronous. [`Db`] runs them on the blocking pool, holding the lock
//! only inside that closure, so no lock is ever held across an await.

use crate::crypto::MasterKey;
use crate::records::{Capabilities, Model, Provider, ProviderType, RouteSpec};
use crate::usage::{Cost, Grouping, Outcome, Tokens, UsageFilter, UsageRecord, UsageTotals};
use llmr::Reach;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// What a failure in the store is, for the API to map to a status.
#[derive(Debug)]
pub enum StoreError {
    /// No row by that key.
    NotFound(String),
    /// A row by that key exists already.
    Conflict(String),
    /// Anything else: the file, the query, the cipher.
    Failed(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NotFound(m) | StoreError::Conflict(m) | StoreError::Failed(m) => {
                f.write_str(m)
            }
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError::Failed(format!("database: {error}"))
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS providers (
    id              TEXT PRIMARY KEY,
    type            TEXT NOT NULL,
    base_url        TEXT,
    reach           TEXT,
    timeout_secs    INTEGER NOT NULL,
    enabled         INTEGER NOT NULL,
    credential      BLOB,
    credential_hint TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS models (
    provider_id  TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id     TEXT NOT NULL,
    enabled      INTEGER NOT NULL,
    capabilities TEXT,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (provider_id, model_id)
);
CREATE TABLE IF NOT EXISTS routes (
    name       TEXT PRIMARY KEY,
    spec       TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
"#;

/// Version 2: one row per request the client API served or refused. Never the prompt or the
/// reply; the route, the counts, the cost and the outcome.
const USAGE_V2: &str = r#"
CREATE TABLE IF NOT EXISTS usage (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    at                 INTEGER NOT NULL,
    request_id         TEXT NOT NULL,
    asked              TEXT NOT NULL,
    route              TEXT,
    provider_id        TEXT,
    model              TEXT,
    served_model       TEXT,
    stream             INTEGER NOT NULL,
    outcome            TEXT NOT NULL,
    error_code         TEXT,
    stop_reason        TEXT,
    attempts           INTEGER NOT NULL,
    fell_through       INTEGER NOT NULL,
    latency_ms         INTEGER NOT NULL,
    input_tokens       INTEGER,
    cache_read_tokens  INTEGER,
    cache_write_tokens INTEGER,
    output_tokens      INTEGER,
    cost_status        TEXT,
    cost_micros        INTEGER,
    currency           TEXT
);
CREATE INDEX IF NOT EXISTS usage_at ON usage (at);
CREATE INDEX IF NOT EXISTS usage_route ON usage (route, at);
CREATE INDEX IF NOT EXISTS usage_provider ON usage (provider_id, at);
"#;

/// The schema version this build writes. Raised with a migration, never edited in place.
const VERSION: i64 = 2;

/// What is sealed into `meta` so a wrong master key is caught at startup.
const KEY_CHECK: &[u8] = b"llmr-master-key-check";

pub struct Store {
    conn: Connection,
    key: MasterKey,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

impl Store {
    /// Opens the database at this path, creating it if it is new.
    ///
    /// # Errors
    ///
    /// When the file cannot be opened, the schema is newer than this build, or the master key
    /// is not the one the database was created with.
    pub fn open(path: &Path, key: MasterKey) -> StoreResult<Store> {
        Self::setup(Connection::open(path)?, key)
    }

    /// A database that lives only as long as the process, for tests.
    #[cfg(test)]
    pub fn in_memory(key: MasterKey) -> StoreResult<Store> {
        Self::setup(Connection::open_in_memory()?, key)
    }

    fn setup(conn: Connection, key: MasterKey) -> StoreResult<Store> {
        conn.pragma_update(None, "foreign_keys", true)?;
        // Readers do not block the writer, which matters once usage is written per request.
        // An in-memory database answers "memory", which is fine.
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > VERSION {
            return Err(StoreError::Failed(format!(
                "the database is schema version {version} and this build knows {VERSION}. \
                 It was written by a newer llmr; run that version or restore a backup"
            )));
        }
        conn.execute_batch(SCHEMA)?;
        // Each step brings a database written by an older build forward, once. A fresh one
        // (version 0) takes every step.
        if version < 2 {
            conn.execute_batch(USAGE_V2)?;
        }
        conn.pragma_update(None, "user_version", VERSION)?;

        let store = Store { conn, key };
        store.check_key()?;
        Ok(store)
    }

    fn check_key(&self) -> StoreResult<()> {
        let stored: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'key_check'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            Some(sealed) => match self.key.open(&sealed) {
                Ok(plain) if plain == KEY_CHECK => Ok(()),
                _ => Err(StoreError::Failed(
                    "LLMR_MASTER_KEY is not the key this database was created with, so no \
                     stored credential could be read. Start with the original key"
                        .into(),
                )),
            },
            None => {
                let sealed = self.key.seal(KEY_CHECK).map_err(StoreError::Failed)?;
                self.conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('key_check', ?1)",
                    params![sealed],
                )?;
                Ok(())
            }
        }
    }

    // ----- providers -----------------------------------------------------------------

    fn read_provider(row: &rusqlite::Row<'_>) -> rusqlite::Result<Provider> {
        let kind: String = row.get("type")?;
        let reach: Option<String> = row.get("reach")?;
        Ok(Provider {
            id: row.get("id")?,
            // A type this build does not know is a database from a newer llmr. Read it as
            // compatible so the row is still visible, and let `gateway` refuse to build it.
            provider_type: ProviderType::parse(&kind).unwrap_or(ProviderType::OpenaiCompatible),
            base_url: row.get("base_url")?,
            reach: reach.as_deref().and_then(Reach::parse),
            timeout_secs: u64::try_from(row.get::<_, i64>("timeout_secs")?).unwrap_or(120),
            enabled: row.get("enabled")?,
            credential: row.get("credential")?,
            credential_hint: row.get("credential_hint")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    pub fn providers(&self) -> StoreResult<Vec<Provider>> {
        let mut statement = self.conn.prepare("SELECT * FROM providers ORDER BY id")?;
        let rows = statement.query_map([], Self::read_provider)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn provider(&self, id: &str) -> StoreResult<Provider> {
        self.conn
            .query_row(
                "SELECT * FROM providers WHERE id = ?1",
                [id],
                Self::read_provider,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("no provider {id:?}")))
    }

    /// Seals a credential, and the last four characters to show in its place.
    pub fn seal_credential(&self, plain: &str) -> StoreResult<(Vec<u8>, String)> {
        let sealed = self
            .key
            .seal(plain.as_bytes())
            .map_err(StoreError::Failed)?;
        let hint: String = plain
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Ok((sealed, hint))
    }

    /// The credential in the clear, for building the provider that uses it.
    pub fn open_credential(&self, provider: &Provider) -> StoreResult<Option<String>> {
        let Some(sealed) = &provider.credential else {
            return Ok(None);
        };
        let plain = self.key.open(sealed).map_err(StoreError::Failed)?;
        String::from_utf8(plain)
            .map(Some)
            .map_err(|_| StoreError::Failed("a stored credential is not UTF-8".into()))
    }

    pub fn insert_provider(&self, provider: &Provider) -> StoreResult<()> {
        let result = self.conn.execute(
            "INSERT INTO providers (id, type, base_url, reach, timeout_secs, enabled, credential, \
             credential_hint, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                provider.id,
                provider.provider_type.as_str(),
                provider.base_url,
                provider.reach.map(Reach::as_str),
                i64::try_from(provider.timeout_secs).unwrap_or(i64::MAX),
                provider.enabled,
                provider.credential,
                provider.credential_hint,
                now(),
            ],
        );
        match result {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::Conflict(format!(
                    "a provider {:?} exists already",
                    provider.id
                )))
            }
            other => other.map(|_| ()).map_err(Into::into),
        }
    }

    pub fn update_provider(&self, provider: &Provider) -> StoreResult<()> {
        let changed = self.conn.execute(
            "UPDATE providers SET base_url = ?2, reach = ?3, timeout_secs = ?4, enabled = ?5, \
             credential = ?6, credential_hint = ?7, updated_at = ?8 WHERE id = ?1",
            params![
                provider.id,
                provider.base_url,
                provider.reach.map(Reach::as_str),
                i64::try_from(provider.timeout_secs).unwrap_or(i64::MAX),
                provider.enabled,
                provider.credential,
                provider.credential_hint,
                now(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!(
                "no provider {:?}",
                provider.id
            )));
        }
        Ok(())
    }

    /// Removes a provider and, through the foreign key, every model row under it.
    pub fn delete_provider(&self, id: &str) -> StoreResult<()> {
        if self
            .conn
            .execute("DELETE FROM providers WHERE id = ?1", [id])?
            == 0
        {
            return Err(StoreError::NotFound(format!("no provider {id:?}")));
        }
        Ok(())
    }

    // ----- models --------------------------------------------------------------------

    fn read_model(row: &rusqlite::Row<'_>) -> rusqlite::Result<Model> {
        let capabilities: Option<String> = row.get("capabilities")?;
        Ok(Model {
            provider_id: row.get("provider_id")?,
            model_id: row.get("model_id")?,
            enabled: row.get("enabled")?,
            capabilities: capabilities.and_then(|c| serde_json::from_str::<Capabilities>(&c).ok()),
            updated_at: row.get("updated_at")?,
        })
    }

    /// Every model row, or one provider's.
    pub fn models(&self, provider_id: Option<&str>) -> StoreResult<Vec<Model>> {
        let mut statement = self.conn.prepare(
            "SELECT * FROM models WHERE ?1 IS NULL OR provider_id = ?1 \
             ORDER BY provider_id, model_id",
        )?;
        let rows = statement.query_map([provider_id], Self::read_model)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn put_model(&self, model: &Model) -> StoreResult<()> {
        let capabilities = model
            .capabilities
            .map(|c| serde_json::to_string(&c))
            .transpose()
            .map_err(|e| StoreError::Failed(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO models (provider_id, model_id, enabled, capabilities, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (provider_id, model_id) DO UPDATE SET \
             enabled = excluded.enabled, capabilities = excluded.capabilities, \
             updated_at = excluded.updated_at",
            params![
                model.provider_id,
                model.model_id,
                model.enabled,
                capabilities,
                now()
            ],
        )?;
        Ok(())
    }

    pub fn delete_model(&self, provider_id: &str, model_id: &str) -> StoreResult<()> {
        let removed = self.conn.execute(
            "DELETE FROM models WHERE provider_id = ?1 AND model_id = ?2",
            [provider_id, model_id],
        )?;
        if removed == 0 {
            return Err(StoreError::NotFound(format!(
                "no model {model_id:?} under provider {provider_id:?}"
            )));
        }
        Ok(())
    }

    // ----- routes --------------------------------------------------------------------

    pub fn routes(&self) -> StoreResult<Vec<(String, RouteSpec)>> {
        let mut statement = self
            .conn
            .prepare("SELECT name, spec FROM routes ORDER BY name")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (name, spec) = row?;
            let spec: RouteSpec = serde_json::from_str(&spec).map_err(|e| {
                StoreError::Failed(format!("route set {name:?} could not be read: {e}"))
            })?;
            out.push((name, spec));
        }
        Ok(out)
    }

    pub fn route(&self, name: &str) -> StoreResult<RouteSpec> {
        let spec: String = self
            .conn
            .query_row("SELECT spec FROM routes WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("no route set {name:?}")))?;
        serde_json::from_str(&spec)
            .map_err(|e| StoreError::Failed(format!("route set {name:?} could not be read: {e}")))
    }

    pub fn put_route(&self, name: &str, spec: &RouteSpec) -> StoreResult<()> {
        let spec = serde_json::to_string(spec).map_err(|e| StoreError::Failed(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO routes (name, spec, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (name) DO UPDATE SET spec = excluded.spec, updated_at = excluded.updated_at",
            params![name, spec, now()],
        )?;
        Ok(())
    }

    pub fn delete_route(&self, name: &str) -> StoreResult<()> {
        if self
            .conn
            .execute("DELETE FROM routes WHERE name = ?1", [name])?
            == 0
        {
            return Err(StoreError::NotFound(format!("no route set {name:?}")));
        }
        Ok(())
    }
}

// ----- usage -------------------------------------------------------------------------

impl Store {
    /// Writes a batch of usage rows in one transaction.
    pub fn insert_usage(&mut self, rows: &[UsageRecord]) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO usage (at, request_id, asked, route, provider_id, model, served_model, \
                 stream, outcome, error_code, stop_reason, attempts, fell_through, latency_ms, \
                 input_tokens, cache_read_tokens, cache_write_tokens, output_tokens, cost_status, \
                 cost_micros, currency) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17, ?18, ?19, ?20, ?21)",
            )?;
            for row in rows {
                insert.execute(params![
                    row.at,
                    row.request_id,
                    row.asked,
                    row.route,
                    row.provider_id,
                    row.model,
                    row.served_model,
                    row.stream,
                    row.outcome.as_str(),
                    row.error_code,
                    row.stop_reason,
                    row.attempts,
                    row.fell_through,
                    row.latency_ms,
                    row.tokens.input,
                    row.tokens.cache_read,
                    row.tokens.cache_write,
                    row.tokens.output,
                    row.cost.status(),
                    row.cost.micros(),
                    row.cost.currency(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Totals for the rows the filter selects, grouped as asked.
    pub fn usage_summary(
        &self,
        filter: &UsageFilter,
        group: Grouping,
    ) -> StoreResult<Vec<UsageTotals>> {
        let key = group.column();
        // One row per group and currency, folded into one total per group below: a sum
        // across currencies would be a number in none of them.
        let sql = format!(
            "SELECT {key} AS key, COUNT(*), \
             SUM(outcome = 'ok'), SUM(outcome = 'refused'), SUM(outcome = 'error'), \
             SUM(outcome = 'interrupted'), \
             SUM(input_tokens), SUM(cache_read_tokens), SUM(cache_write_tokens), \
             SUM(output_tokens), \
             SUM(outcome IN ('ok', 'interrupted') AND input_tokens IS NULL \
                 AND cache_read_tokens IS NULL AND cache_write_tokens IS NULL \
                 AND output_tokens IS NULL), \
             SUM(latency_ms), \
             SUM(cost_status = 'priced'), SUM(cost_status = 'partial'), \
             SUM(cost_status = 'unpriced'), SUM(cost_status = 'free'), \
             currency, SUM(cost_micros) \
             FROM usage WHERE {where_} \
             GROUP BY key, currency ORDER BY key",
            where_ = filter.clause(),
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(filter.params().as_slice(), |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                UsageTotals {
                    key: None,
                    requests: row.get::<_, i64>(1)?,
                    ok: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    refused: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    errors: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    interrupted: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    tokens: Tokens {
                        input: row.get(6)?,
                        cache_read: row.get(7)?,
                        cache_write: row.get(8)?,
                        output: row.get(9)?,
                    },
                    usage_missing: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
                    latency_ms_total: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
                    priced: row.get::<_, Option<i64>>(12)?.unwrap_or(0),
                    partial: row.get::<_, Option<i64>>(13)?.unwrap_or(0),
                    unpriced: row.get::<_, Option<i64>>(14)?.unwrap_or(0),
                    free: row.get::<_, Option<i64>>(15)?.unwrap_or(0),
                    cost: match (
                        row.get::<_, Option<String>>(16)?,
                        row.get::<_, Option<i64>>(17)?,
                    ) {
                        (Some(currency), Some(micros)) => vec![(currency, micros)],
                        _ => Vec::new(),
                    },
                },
            ))
        })?;

        let mut folded: Vec<UsageTotals> = Vec::new();
        for row in rows {
            let (key, part) = row?;
            match folded.last_mut() {
                Some(last) if last.key == key => last.absorb(part),
                _ => folded.push(UsageTotals { key, ..part }),
            }
        }
        Ok(folded)
    }

    /// Single requests, newest first, `limit` at most, older than the row id `before`.
    pub fn usage_requests(
        &self,
        filter: &UsageFilter,
        before: Option<i64>,
        limit: i64,
    ) -> StoreResult<Vec<(i64, UsageRecord)>> {
        let mut params = filter.params();
        let before = before.unwrap_or(i64::MAX);
        params.push(&before);
        params.push(&limit);
        let sql = format!(
            "SELECT * FROM usage WHERE {} AND id < ?{} ORDER BY id DESC LIMIT ?{}",
            filter.clause(),
            params.len() - 1,
            params.len(),
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params.as_slice(), |row| {
            let outcome: String = row.get("outcome")?;
            let status: Option<String> = row.get("cost_status")?;
            Ok((
                row.get("id")?,
                UsageRecord {
                    at: row.get("at")?,
                    request_id: row.get("request_id")?,
                    asked: row.get("asked")?,
                    route: row.get("route")?,
                    provider_id: row.get("provider_id")?,
                    model: row.get("model")?,
                    served_model: row.get("served_model")?,
                    stream: row.get("stream")?,
                    outcome: Outcome::parse(&outcome),
                    error_code: row.get("error_code")?,
                    stop_reason: row.get("stop_reason")?,
                    attempts: row.get("attempts")?,
                    fell_through: row.get("fell_through")?,
                    latency_ms: row.get("latency_ms")?,
                    tokens: Tokens {
                        input: row.get("input_tokens")?,
                        cache_read: row.get("cache_read_tokens")?,
                        cache_write: row.get("cache_write_tokens")?,
                        output: row.get("output_tokens")?,
                    },
                    cost: Cost::from_row(
                        status.as_deref(),
                        row.get("cost_micros")?,
                        row.get("currency")?,
                    ),
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Removes the rows recorded before this time, and says how many.
    pub fn delete_usage(&self, before: i64) -> StoreResult<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM usage WHERE at < ?1", [before])?)
    }
}

/// The store, shareable across handlers, with every call on the blocking pool.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Store>>);

impl Db {
    pub fn new(store: Store) -> Self {
        Db(Arc::new(Mutex::new(store)))
    }

    /// Runs `work` against the store on the blocking pool, with write access to the
    /// connection, for a transaction.
    pub async fn run_mut<T, F>(&self, work: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Store) -> StoreResult<T> + Send + 'static,
    {
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = inner
                .lock()
                .map_err(|_| StoreError::Failed("the store lock was poisoned".into()))?;
            work(&mut store)
        })
        .await
        .map_err(|e| StoreError::Failed(format!("the store task failed: {e}")))?
    }

    /// Runs `work` against the store on the blocking pool.
    ///
    /// The lock is taken and released inside the closure, never across an await.
    pub async fn run<T, F>(&self, work: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> StoreResult<T> + Send + 'static,
    {
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let store = inner
                .lock()
                .map_err(|_| StoreError::Failed("the store lock was poisoned".into()))?;
            work(&store)
        })
        .await
        .map_err(|e| StoreError::Failed(format!("the store task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> MasterKey {
        MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()
    }

    fn provider(store: &Store, id: &str, credential: Option<&str>) -> Provider {
        let (credential, credential_hint) = match credential {
            Some(plain) => {
                let (sealed, hint) = store.seal_credential(plain).unwrap();
                (Some(sealed), Some(hint))
            }
            None => (None, None),
        };
        Provider {
            id: id.into(),
            provider_type: ProviderType::Anthropic,
            base_url: None,
            reach: None,
            timeout_secs: 60,
            enabled: true,
            credential,
            credential_hint,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_credential_is_stored_sealed_and_read_back_in_the_clear() {
        let store = Store::in_memory(key()).unwrap();
        store
            .insert_provider(&provider(&store, "a", Some("sk-ant-secret-1234")))
            .unwrap();

        let raw: Vec<u8> = store
            .conn
            .query_row("SELECT credential FROM providers WHERE id = 'a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !raw.windows(6).any(|w| w == b"secret"),
            "stored in the clear"
        );

        let read = store.provider("a").unwrap();
        assert_eq!(read.credential_hint.as_deref(), Some("1234"));
        assert_eq!(
            store.open_credential(&read).unwrap().as_deref(),
            Some("sk-ant-secret-1234")
        );
    }

    #[test]
    fn a_second_provider_with_the_same_id_is_a_conflict() {
        let store = Store::in_memory(key()).unwrap();
        store.insert_provider(&provider(&store, "a", None)).unwrap();
        assert!(matches!(
            store.insert_provider(&provider(&store, "a", None)),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn deleting_a_provider_takes_its_models_with_it() {
        let store = Store::in_memory(key()).unwrap();
        store.insert_provider(&provider(&store, "a", None)).unwrap();
        store
            .put_model(&Model {
                provider_id: "a".into(),
                model_id: "m".into(),
                enabled: true,
                capabilities: Some(Capabilities {
                    tools: true,
                    ..Capabilities::default()
                }),
                updated_at: 0,
            })
            .unwrap();
        assert!(
            store.models(Some("a")).unwrap()[0]
                .capabilities
                .unwrap()
                .tools
        );

        store.delete_provider("a").unwrap();
        assert!(store.models(None).unwrap().is_empty());
    }

    #[test]
    fn a_model_for_a_provider_that_does_not_exist_is_refused() {
        let store = Store::in_memory(key()).unwrap();
        let result = store.put_model(&Model {
            provider_id: "nobody".into(),
            model_id: "m".into(),
            enabled: true,
            capabilities: None,
            updated_at: 0,
        });
        assert!(result.is_err());
    }

    #[test]
    fn route_sets_round_trip() {
        let store = Store::in_memory(key()).unwrap();
        let spec: RouteSpec =
            serde_json::from_value(serde_json::json!({ "routes": ["a/m"], "on_device": true }))
                .unwrap();
        store.put_route("default", &spec).unwrap();
        assert_eq!(store.route("default").unwrap(), spec);
        assert_eq!(store.routes().unwrap().len(), 1);
        store.delete_route("default").unwrap();
        assert!(matches!(
            store.route("default"),
            Err(StoreError::NotFound(_))
        ));
    }

    fn row(
        at: i64,
        route: Option<&str>,
        outcome: crate::usage::Outcome,
        cost: crate::usage::Cost,
    ) -> UsageRecord {
        UsageRecord {
            at,
            request_id: format!("r{at}"),
            asked: "default".into(),
            route: route.map(String::from),
            provider_id: route.and_then(|r| r.split('/').next()).map(String::from),
            model: None,
            served_model: None,
            stream: false,
            outcome,
            error_code: None,
            stop_reason: None,
            attempts: 1,
            fell_through: 0,
            latency_ms: 100,
            tokens: Tokens {
                input: Some(10),
                output: Some(2),
                ..Tokens::default()
            },
            cost,
        }
    }

    #[test]
    fn usage_is_grouped_with_one_amount_per_currency() {
        use crate::usage::{Cost, Grouping, Outcome, UsageFilter};
        let mut store = Store::in_memory(key()).unwrap();
        let usd = |micros| Cost::Priced {
            micros,
            currency: "USD".into(),
            partial: false,
        };
        store
            .insert_usage(&[
                row(100, Some("a/m"), Outcome::Ok, usd(1_000)),
                row(200, Some("a/m"), Outcome::Ok, usd(2_000)),
                row(
                    300,
                    Some("b/m"),
                    Outcome::Ok,
                    Cost::Priced {
                        micros: 500,
                        currency: "EUR".into(),
                        partial: false,
                    },
                ),
                row(400, Some("c/m"), Outcome::Ok, Cost::Unpriced),
                row(500, None, Outcome::Error, Cost::None),
            ])
            .unwrap();

        let by_model = store
            .usage_summary(&UsageFilter::default(), Grouping::Model)
            .unwrap();
        let a = by_model
            .iter()
            .find(|t| t.key.as_deref() == Some("a/m"))
            .unwrap();
        assert_eq!(a.requests, 2);
        assert_eq!(a.cost, vec![("USD".to_string(), 3_000)]);
        assert_eq!(a.tokens.input, Some(20));

        let all = store
            .usage_summary(&UsageFilter::default(), Grouping::None)
            .unwrap();
        let mut total = crate::usage::UsageTotals::default();
        for part in all {
            total.absorb(part);
        }
        assert_eq!(total.requests, 5);
        assert_eq!(total.errors, 1);
        assert_eq!(total.unpriced, 1);
        assert!(total.cost.contains(&("USD".to_string(), 3_000)));
        assert!(total.cost.contains(&("EUR".to_string(), 500)));

        // A time window, from inclusive and to exclusive.
        let window = UsageFilter {
            from: Some(200),
            to: Some(400),
            ..UsageFilter::default()
        };
        let rows = store.usage_requests(&window, None, 10).unwrap();
        assert_eq!(
            rows.iter().map(|(_, r)| r.at).collect::<Vec<_>>(),
            vec![300, 200]
        );

        assert_eq!(store.delete_usage(300).unwrap(), 2);
        assert_eq!(
            store
                .usage_requests(&UsageFilter::default(), None, 10)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn a_version_one_database_is_brought_forward() {
        // A database from the build before usage existed: the providers table, version 1.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let mut store = Store::setup(conn, key()).unwrap();
        store
            .insert_usage(&[row(
                1,
                None,
                crate::usage::Outcome::Error,
                crate::usage::Cost::None,
            )])
            .unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, VERSION);
    }

    #[test]
    fn the_wrong_master_key_is_refused_at_open() {
        let dir = std::env::temp_dir().join(format!("llmr-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("llmr.db");
        let _ = std::fs::remove_file(&path);

        Store::open(&path, key()).unwrap();
        let other = MasterKey::from_base64("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").unwrap();
        let error = Store::open(&path, other).err().unwrap();
        assert!(error.to_string().contains("not the key"), "{error}");
        // And the original still opens.
        Store::open(&path, key()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
