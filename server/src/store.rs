//! The server's own records, in `app.sqlite`: the family's people, their devices and device keys,
//! and (for devices with history on) the points they asked about.
//!
//! A device key looks like `psk_<device id>_<secret>`. Only a SHA-256 of the secret is stored, so
//! a copy of the database doesn't give anyone a working key.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use photo_stamp_proto::GpsPoint;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

const DEFAULT_MAX_WAIT_HOURS: u32 = 72;
/// Don't rewrite `last_seen` more often than this.
const LAST_SEEN_EVERY_SECS: i64 = 60;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS user(
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE,
  max_wait_hours INTEGER NOT NULL,
  rev INTEGER NOT NULL DEFAULT 0,
  created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS device(
  id TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES user(id),
  name TEXT NOT NULL,
  key_hash TEXT NOT NULL,
  history INTEGER NOT NULL,
  created INTEGER NOT NULL,
  last_seen INTEGER,
  revoked INTEGER,
  UNIQUE(user_id, name));
CREATE TABLE IF NOT EXISTS point(
  user_id INTEGER NOT NULL,
  device_id TEXT NOT NULL,
  lat REAL NOT NULL,
  lon REAL NOT NULL,
  at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS point_user_at ON point(user_id, at);
";

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(n: usize) -> anyhow::Result<String> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(hex(&b))
}

fn hash_secret(secret: &str) -> String {
    hex(&Sha256::digest(secret.as_bytes()))
}

/// The device a request came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Caller {
    pub user_id: i64,
    pub user: String,
    pub device_id: String,
    pub device: String,
    pub history: bool,
    pub max_wait_hours: u32,
    pub rev: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub id: String,
    pub user: String,
    pub name: String,
    pub history: bool,
    pub created: i64,
    pub last_seen: Option<i64>,
    pub revoked: bool,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn add_user(&self, name: &str) -> anyhow::Result<i64> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "a name is needed");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user(name, max_wait_hours, created) VALUES(?1, ?2, ?3)",
            params![name, DEFAULT_MAX_WAIT_HOURS, now()],
        )
        .with_context(|| format!("adding {name} (is the name taken?)"))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn users(&self) -> anyhow::Result<Vec<(String, usize)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT u.name, count(d.id) FROM user u LEFT JOIN device d ON d.user_id = u.id AND d.revoked IS NULL
             GROUP BY u.id ORDER BY u.name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Register a device for `user`; returns its key, which is shown once and never stored.
    pub fn add_device(&self, user: &str, name: &str, history: bool) -> anyhow::Result<String> {
        let conn = self.conn.lock().unwrap();
        let user_id: i64 = conn
            .query_row("SELECT id FROM user WHERE name = ?1", [user], |r| r.get(0))
            .optional()?
            .with_context(|| format!("no person called {user}; add them first"))?;
        let id = random_hex(4)?;
        let secret = random_hex(20)?;
        conn.execute(
            "INSERT INTO device(id, user_id, name, key_hash, history, created) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, user_id, name.trim(), hash_secret(&secret), history, now()],
        )
        .with_context(|| format!("adding device {name} (does {user} already have one by that name?)"))?;
        Ok(format!("psk_{id}_{secret}"))
    }

    pub fn devices(&self) -> anyhow::Result<Vec<DeviceInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT d.id, u.name, d.name, d.history, d.created, d.last_seen, d.revoked IS NOT NULL
             FROM device d JOIN user u ON u.id = d.user_id ORDER BY u.name, d.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DeviceInfo {
                id: r.get(0)?,
                user: r.get(1)?,
                name: r.get(2)?,
                history: r.get(3)?,
                created: r.get(4)?,
                last_seen: r.get(5)?,
                revoked: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Stop a device's key working. `device` is its id or "person/device".
    pub fn revoke(&self, device: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = match device.split_once('/') {
            Some((user, name)) => conn.execute(
                "UPDATE device SET revoked = ?3 WHERE name = ?2 AND revoked IS NULL
                 AND user_id = (SELECT id FROM user WHERE name = ?1)",
                params![user, name, now()],
            )?,
            None => conn.execute(
                "UPDATE device SET revoked = ?2 WHERE id = ?1 AND revoked IS NULL",
                params![device, now()],
            )?,
        };
        if changed == 0 {
            bail!("no active device {device}");
        }
        Ok(())
    }

    /// Who a key belongs to, if it's valid.
    pub fn authenticate(&self, key: &str) -> anyhow::Result<Option<Caller>> {
        let Some((id, secret)) = key.strip_prefix("psk_").and_then(|k| k.split_once('_')) else {
            return Ok(None);
        };
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT d.key_hash, d.user_id, u.name, d.name, d.history, u.max_wait_hours, u.rev, d.last_seen
                 FROM device d JOIN user u ON u.id = d.user_id WHERE d.id = ?1 AND d.revoked IS NULL",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        Caller {
                            user_id: r.get(1)?,
                            user: r.get(2)?,
                            device_id: id.to_owned(),
                            device: r.get(3)?,
                            history: r.get(4)?,
                            max_wait_hours: r.get(5)?,
                            rev: r.get::<_, i64>(6)? as u64,
                        },
                        r.get::<_, Option<i64>>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored, caller, last_seen)) = row else { return Ok(None) };
        if !photo_stamp_http::registry::same_secret(&stored, &hash_secret(secret)) {
            return Ok(None);
        }
        if last_seen.is_none_or(|t| now() - t >= LAST_SEEN_EVERY_SECS) {
            conn.execute("UPDATE device SET last_seen = ?2 WHERE id = ?1", params![id, now()])?;
        }
        Ok(Some(caller))
    }

    pub fn set_history(&self, device_id: &str, on: bool) -> anyhow::Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("UPDATE device SET history = ?2 WHERE id = ?1", params![device_id, on])?;
        Ok(())
    }

    pub fn set_max_wait(&self, user_id: i64, hours: u32) -> anyhow::Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("UPDATE user SET max_wait_hours = ?2 WHERE id = ?1", params![user_id, hours])?;
        Ok(())
    }

    /// Remember that this device asked about these points (only for devices with history on).
    pub fn record(&self, caller: &Caller, points: &[GpsPoint]) -> anyhow::Result<()> {
        if !caller.history || points.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO point(user_id, device_id, lat, lon, at) VALUES(?1, ?2, ?3, ?4, ?5)",
            )?;
            for p in points {
                stmt.execute(params![caller.user_id, caller.device_id, p.lat, p.lon, now()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn point_count(&self, user_id: i64) -> anyhow::Result<u64> {
        let n: i64 = self.conn.lock().unwrap().query_row(
            "SELECT count(*) FROM point WHERE user_id = ?1",
            [user_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_authenticate_until_revoked() {
        let s = Store::in_memory().unwrap();
        s.add_user("Dave").unwrap();
        assert!(s.add_user("dave").is_err(), "names are case-insensitive");
        let key = s.add_device("Dave", "laptop", true).unwrap();
        assert!(key.starts_with("psk_"));
        let caller = s.authenticate(&key).unwrap().unwrap();
        assert_eq!((caller.user.as_str(), caller.device.as_str(), caller.history), ("Dave", "laptop", true));
        assert_eq!(caller.max_wait_hours, DEFAULT_MAX_WAIT_HOURS);

        // A wrong secret, a malformed key and an unknown device all fail.
        let (prefix, _) = key.rsplit_once('_').unwrap();
        assert!(s.authenticate(&format!("{prefix}_00")).unwrap().is_none());
        assert!(s.authenticate("nonsense").unwrap().is_none());
        assert!(s.authenticate("psk_ffffffff_00").unwrap().is_none());

        s.revoke("Dave/laptop").unwrap();
        assert!(s.authenticate(&key).unwrap().is_none());
        assert!(s.revoke("Dave/laptop").is_err(), "already revoked");
        assert!(s.devices().unwrap()[0].revoked);
    }

    #[test]
    fn history_is_kept_only_when_on() {
        let s = Store::in_memory().unwrap();
        s.add_user("Ann").unwrap();
        let key = s.add_device("Ann", "phone", false).unwrap();
        let caller = s.authenticate(&key).unwrap().unwrap();
        let p = [GpsPoint { lat: 1.0, lon: 2.0 }];
        s.record(&caller, &p).unwrap();
        assert_eq!(s.point_count(caller.user_id).unwrap(), 0);
        s.set_history(&caller.device_id, true).unwrap();
        let caller = s.authenticate(&key).unwrap().unwrap();
        s.record(&caller, &p).unwrap();
        assert_eq!(s.point_count(caller.user_id).unwrap(), 1);
        s.set_max_wait(caller.user_id, 12).unwrap();
        assert_eq!(s.authenticate(&key).unwrap().unwrap().max_wait_hours, 12);
        assert!(s.add_device("Nobody", "x", true).is_err());
    }
}
