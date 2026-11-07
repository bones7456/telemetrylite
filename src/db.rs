use rusqlite::{Connection, params};
use std::thread::JoinHandle;
use tokio::sync::mpsc;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS app (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL,
  retention_days INTEGER NOT NULL DEFAULT 90
);
CREATE TABLE IF NOT EXISTS api_key (
  id TEXT PRIMARY KEY, app_id TEXT NOT NULL REFERENCES app(id) ON DELETE CASCADE,
  label TEXT NOT NULL, prefix TEXT NOT NULL, key_hash TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL, revoked_at INTEGER
);
CREATE TABLE IF NOT EXISTS signal (
  id INTEGER PRIMARY KEY,
  app_id TEXT NOT NULL, ts INTEGER NOT NULL, received_at INTEGER NOT NULL,
  type TEXT NOT NULL, user_hash TEXT, session_id TEXT, float_value REAL,
  is_test INTEGER NOT NULL DEFAULT 0,
  app_version TEXT, build TEXT, os TEXT, os_version TEXT, device_model TEXT,
  locale TEXT, sdk TEXT, country TEXT, region TEXT, city TEXT,
  payload TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS signal_app_ts ON signal(app_id, ts);
CREATE INDEX IF NOT EXISTS signal_app_type ON signal(app_id, type, ts);
";

pub fn open(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
    )?;
    Ok(conn)
}

/// Database schema version, stored in `PRAGMA user_version`.
/// Version 1 is the schema of every release so far. To change the schema, bump this and append a
/// step to `MIGRATIONS` (step N upgrades version N to N+1); existing databases are migrated on start.
const SCHEMA_VERSION: i64 = 1;
const MIGRATIONS: &[&str] = &[];

/// Creates missing tables and applies pending migrations. Refuses a database written by a newer
/// version, so an older binary (e.g. after a rollback) never works on a schema it does not know.
pub fn init(conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let have: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if have > SCHEMA_VERSION {
        return Err(format!("database schema version {have} is newer than this binary supports ({SCHEMA_VERSION}); upgrade telemetrylite").into());
    }
    conn.execute_batch(SCHEMA)?;
    // A database from before versioning has user_version 0 and the version-1 schema.
    for (i, step) in MIGRATIONS.iter().enumerate().skip(have.max(1) as usize - 1) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(step)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 2))?;
        tx.commit()?;
    }
    if have < SCHEMA_VERSION {
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    }
    Ok(())
}

pub fn meta_get(conn: &Connection, k: &str) -> Option<String> {
    conn.query_row("SELECT v FROM meta WHERE k=?", [k], |r| r.get(0)).ok()
}

pub fn meta_set(conn: &Connection, k: &str, v: &str) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO meta(k,v) VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v", [k, v])?;
    Ok(())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[derive(Default, Debug)]
pub struct Signal {
    pub app_id: String,
    pub ts: i64,
    pub received_at: i64,
    pub kind: String,
    pub user_hash: Option<String>,
    pub session_id: Option<String>,
    pub float_value: Option<f64>,
    pub is_test: bool,
    pub app_version: Option<String>,
    pub build: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub device_model: Option<String>,
    pub locale: Option<String>,
    pub sdk: Option<String>,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub payload: String,
}

/// 单线程批量写入:队列里有多少就合并成一个事务,空闲时延迟最低,繁忙时自然批量。
pub fn spawn_writer(path: String) -> (mpsc::Sender<Vec<Signal>>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<Vec<Signal>>(1024);
    let handle = std::thread::spawn(move || {
        let mut conn = open(&path).expect("writer: open db");
        while let Some(mut batch) = rx.blocking_recv() {
            while batch.len() < 2000 {
                match rx.try_recv() {
                    Ok(mut more) => batch.append(&mut more),
                    Err(_) => break,
                }
            }
            if let Err(e) = insert(&mut conn, &batch) {
                crate::i18n::log_tr!(error, "write failed, dropped {} events: {e}", "写入失败,丢弃 {} 条: {e}", batch.len());
            }
        }
    });
    (tx, handle)
}

fn insert(conn: &mut Connection, batch: &[Signal]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut st = tx.prepare_cached(
            "INSERT INTO signal(app_id,ts,received_at,type,user_hash,session_id,float_value,is_test,
              app_version,build,os,os_version,device_model,locale,sdk,country,region,city,payload)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )?;
        for s in batch {
            st.execute(params![
                s.app_id, s.ts, s.received_at, s.kind, s.user_hash, s.session_id, s.float_value,
                s.is_test, s.app_version, s.build, s.os, s.os_version, s.device_model, s.locale,
                s.sdk, s.country, s.region, s.city, s.payload
            ])?;
        }
    }
    tx.commit()
}

/// 按各 App 的保留期分批清理过期事件。
pub fn purge_expired(conn: &Connection) -> rusqlite::Result<usize> {
    let mut total = 0;
    loop {
        let n = conn.execute(
            "DELETE FROM signal WHERE id IN (
               SELECT s.id FROM signal s JOIN app a ON a.id = s.app_id
               WHERE s.ts < ?1 - a.retention_days * 86400000 LIMIT 10000)",
            [now_ms()],
        )?;
        total += n;
        if n < 10000 {return Ok(total);}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(c: &Connection) -> i64 {
        c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn fresh_database_gets_current_version() {
        let c = Connection::open_in_memory().unwrap();
        init(&c).unwrap();
        assert_eq!(version(&c), SCHEMA_VERSION);
        init(&c).unwrap(); // idempotent
    }

    #[test]
    fn unversioned_database_from_older_release_is_accepted() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap(); // what v0.1.x created: tables, user_version 0
        c.execute("INSERT INTO app(id,name,created_at) VALUES('a','n',1)", []).unwrap();
        init(&c).unwrap();
        assert_eq!(version(&c), SCHEMA_VERSION);
        let n: i64 = c.query_row("SELECT COUNT(*) FROM app", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn newer_database_is_refused() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1)).unwrap();
        let err = init(&c).unwrap_err().to_string();
        assert!(err.contains("newer than this binary"), "{err}");
    }
}

