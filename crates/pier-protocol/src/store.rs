use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{fs, path::Path, sync::Mutex, time::Duration};

/// Local durable state. Callers keep transactions small; no network operations
/// or builds execute while holding the database mutex.
pub struct Store(Mutex<Connection>);
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS objects (namespace TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(namespace,key));")?;
        Ok(Self(Mutex::new(connection)))
    }
    pub fn get<T: DeserializeOwned>(&self, namespace: &str, key: &str) -> Result<Option<T>> {
        let value: Option<String> = self
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM objects WHERE namespace=?1 AND key=?2",
                params![namespace, key],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn put<T: Serialize>(&self, namespace: &str, key: &str, value: &T) -> Result<()> {
        self.0.lock().unwrap().execute("INSERT INTO objects(namespace,key,value) VALUES (?1,?2,?3) ON CONFLICT(namespace,key) DO UPDATE SET value=excluded.value", params![namespace,key,serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn delete(&self, namespace: &str, key: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "DELETE FROM objects WHERE namespace=?1 AND key=?2",
            params![namespace, key],
        )?;
        Ok(())
    }
    pub fn put_pair<T: Serialize, U: Serialize>(
        &self,
        first: (&str, &str, &T),
        second: (&str, &str, &U),
    ) -> Result<()> {
        self.put_batch(&[
            (first.0, first.1, serde_json::to_value(first.2)?),
            (second.0, second.1, serde_json::to_value(second.2)?),
        ])
    }
    /// Atomically commit related objects, including first-time setup records.
    pub fn put_batch(&self, records: &[(&str, &str, serde_json::Value)]) -> Result<()> {
        let records = records
            .iter()
            .map(|(ns, key, value)| Ok((*ns, *key, serde_json::to_string(value)?)))
            .collect::<Result<Vec<_>>>()?;
        let mut connection = self.0.lock().unwrap();
        let transaction = connection.transaction()?;
        for (namespace, key, value) in records {
            transaction.execute("INSERT INTO objects(namespace,key,value) VALUES (?1,?2,?3) ON CONFLICT(namespace,key) DO UPDATE SET value=excluded.value", params![namespace,key,value])?;
        }
        transaction.commit()?;
        Ok(())
    }
    pub fn list<T: DeserializeOwned>(&self, namespace: &str) -> Result<Vec<T>> {
        let connection = self.0.lock().unwrap();
        let mut statement =
            connection.prepare("SELECT value FROM objects WHERE namespace=?1 ORDER BY key")?;
        let rows = statement.query_map([namespace], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_and_completion_commit_or_rollback_together() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("state.db")).unwrap();
        store.0.lock().unwrap().execute_batch("CREATE TRIGGER reject_result BEFORE INSERT ON objects WHEN NEW.namespace = 'completed' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        assert!(
            store
                .put_pair(
                    ("runtime", "state", &"new"),
                    ("completed", "job", &"success")
                )
                .is_err()
        );
        assert!(store.get::<String>("runtime", "state").unwrap().is_none());
        store
            .0
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_result;")
            .unwrap();
        store
            .put_pair(
                ("runtime", "state", &"new"),
                ("completed", "job", &"success"),
            )
            .unwrap();
        assert_eq!(
            store.get::<String>("runtime", "state").unwrap().as_deref(),
            Some("new")
        );
        assert_eq!(
            store.get::<String>("completed", "job").unwrap().as_deref(),
            Some("success")
        );
    }
    #[test]
    fn state_is_durable_and_process_lock_exclusive() {
        let directory = tempfile::tempdir().unwrap();
        let lock = crate::state_lock(directory.path()).unwrap();
        assert!(crate::state_lock(directory.path()).is_err());
        let database = directory.path().join("state.db");
        let store = Store::open(&database).unwrap();
        store
            .put("snapshots", "current", &vec!["api", "worker"])
            .unwrap();
        drop(store);
        assert_eq!(
            Store::open(&database)
                .unwrap()
                .get::<Vec<String>>("snapshots", "current")
                .unwrap()
                .unwrap(),
            ["api", "worker"]
        );
        drop(lock);
        assert!(crate::state_lock(directory.path()).is_ok());
    }
}
