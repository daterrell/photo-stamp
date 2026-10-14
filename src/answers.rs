//! Answers from the place server, remembered on this PC (`answers.sqlite` in the data folder), so
//! photos taken at a spot already asked about are stamped at once, even with no connection.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use photo_stamp_proto::{GpsPoint, Place};
use rusqlite::{Connection, OptionalExtension, params};

/// Beyond this many answers the oldest are dropped.
const MAX_ANSWERS: i64 = 200_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Cached {
    pub place: Option<Place>,
    pub data_version: String,
    pub rev: u64,
}

pub struct Answers {
    conn: Mutex<Connection>,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Answers {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS answer(
               key TEXT PRIMARY KEY, place TEXT, data_version TEXT NOT NULL, rev INTEGER NOT NULL,
               at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS answer_at ON answer(at);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn get(&self, at: GpsPoint) -> Option<Cached> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT place, data_version, rev FROM answer WHERE key = ?1", [at.cache_key()], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })
        .optional()
        .ok()
        .flatten()
        .map(|(place, data_version, rev)| Cached {
            place: place
                .and_then(|p| serde_json::from_str::<Place>(&p).ok())
                .map(|p| Place { complete: true, ..p }),
            data_version,
            rev: rev as u64,
        })
    }

    pub fn put(&self, at: GpsPoint, place: Option<&Place>, data_version: &str, rev: u64) {
        let json = place.and_then(|p| serde_json::to_string(p).ok());
        let conn = self.conn.lock().unwrap();
        let result = conn
            .execute(
                "INSERT OR REPLACE INTO answer(key, place, data_version, rev, at) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![at.cache_key(), json, data_version, rev as i64, now()],
            )
            .and_then(|_| {
                conn.execute(
                    "DELETE FROM answer WHERE key IN (SELECT key FROM answer ORDER BY at DESC LIMIT -1 OFFSET ?1)",
                    [MAX_ANSWERS],
                )
            });
        if let Err(e) = result {
            log::warn!("couldn't remember a place answer: {e}");
        }
    }

    pub fn count(&self) -> u64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT count(*) FROM answer", [], |r| r.get::<_, i64>(0)).map_or(0, |n| n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_answers_by_spot() {
        let a = Answers::in_memory().unwrap();
        let at = GpsPoint { lat: 48.85561, lon: 2.29862 };
        assert!(a.get(at).is_none());
        let place = Place {
            primary: "Champ de Mars".into(),
            near: None,
            context: Some("Paris".into()),
            complete: true,
        };
        a.put(at, Some(&place), "r1", 0);
        // A few meters away is the same spot.
        let hit = a.get(GpsPoint { lat: 48.85562, lon: 2.29863 }).unwrap();
        assert_eq!((hit.place.unwrap().primary, hit.data_version.as_str()), ("Champ de Mars".into(), "r1"));
        // "Nothing here" is an answer too.
        let sea = GpsPoint { lat: 0.0, lon: -30.0 };
        a.put(sea, None, "r1", 0);
        assert_eq!(a.get(sea).unwrap().place, None);
        assert_eq!(a.count(), 2);
    }
}
