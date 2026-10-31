//! Persistent sessions + turn trees in SQLite (WAL, single-writer mutex).
//! Mirrors `internal/agent/store.go` schema (sessions/turns/approvals/traces).

use regex::Regex;
use rusqlite::{params, Connection};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store: {0}")]
    Msg(String),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub parent_id: String,
    pub workspace_dir: String,
    pub agent: String,
    pub permission_mode: String,
    pub model: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub id: String,
    pub session_id: String,
    pub parent_id: String,
    pub role: String,
    pub text: String,
    pub created_at: String,
}

pub struct Store {
    db: Mutex<Connection>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn db_path(explicit: Option<&str>) -> PathBuf {
    if let Some(p) = explicit {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("LUMINA_DB") {
        return PathBuf::from(p);
    }
    Path::new(".lumina/lumina.db").to_path_buf()
}

impl Store {
    pub fn open(explicit: Option<&str>) -> Result<Self, StoreError> {
        let path = db_path(explicit);
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let db = Connection::open(&path).map_err(|e| StoreError::Msg(format!("{path:?}: {e}")))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "busy_timeout", 5000)?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions(
               id TEXT PRIMARY KEY, parent_id TEXT DEFAULT '',
               workspace_dir TEXT NOT NULL, agent TEXT DEFAULT 'build',
               permission_mode TEXT DEFAULT 'bypass', model TEXT DEFAULT '', title TEXT DEFAULT '',
               created_at TEXT, updated_at TEXT);
             CREATE TABLE IF NOT EXISTS turns(
               id TEXT PRIMARY KEY, session_id TEXT NOT NULL, parent_id TEXT DEFAULT '',
               role TEXT, text TEXT, created_at TEXT);
             CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);
             CREATE TABLE IF NOT EXISTS approvals(
               req_id TEXT PRIMARY KEY, session_id TEXT, tool TEXT, target TEXT,
               decision TEXT, created_at TEXT);
             CREATE TABLE IF NOT EXISTS traces(
               id TEXT PRIMARY KEY, session_id TEXT, kind TEXT, payload TEXT, created_at TEXT);",
        )?;
        for ddl in [
            "ALTER TABLE sessions ADD COLUMN inflight_owner TEXT DEFAULT ''",
            "ALTER TABLE sessions ADD COLUMN inflight_since INTEGER DEFAULT 0",
        ] {
            // Duplicate-column is the happy path on a migrated DB.
            let _ = db.execute_batch(ddl);
        }
        Ok(Self { db: Mutex::new(db) })
    }

    pub fn create_session(
        &self,
        workspace: &str,
        agent: &str,
        mode: &str,
        model: &str,
    ) -> Result<Session, StoreError> {
        let s = Session {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: String::new(),
            workspace_dir: workspace.into(),
            agent: agent.into(),
            permission_mode: mode.into(),
            model: model.into(),
            title: String::new(),
            created_at: now(),
            updated_at: now(),
        };
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO sessions(id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at)
             VALUES(?,?,?,?,?,?,?,?,?)",
            params![s.id, s.parent_id, s.workspace_dir, s.agent, s.permission_mode, s.model, s.title, s.created_at, s.updated_at],
        )?;
        Ok(s)
    }

    fn row_to_session(r: &rusqlite::Row) -> rusqlite::Result<Session> {
        Ok(Session {
            id: r.get(0)?,
            parent_id: r.get(1)?,
            workspace_dir: r.get(2)?,
            agent: r.get(3)?,
            permission_mode: r.get(4)?,
            model: r.get(5)?,
            title: r.get(6)?,
            created_at: r.get(7)?,
            updated_at: r.get(8)?,
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>, StoreError> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at
             FROM sessions WHERE id=?",
        )?;
        let mut rows = st.query([id])?;
        Ok(rows.next()?.map(Self::row_to_session).transpose()?)
    }

    pub fn latest_for_dir(&self, dir: &str) -> Result<Option<Session>, StoreError> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at
             FROM sessions WHERE workspace_dir=? ORDER BY updated_at DESC LIMIT 1",
        )?;
        let mut rows = st.query([dir])?;
        Ok(rows.next()?.map(Self::row_to_session).transpose()?)
    }

    pub fn list_for_dir(&self, dir: Option<&str>) -> Result<Vec<Session>, StoreError> {
        let db = self.db.lock().unwrap();
        let mut out = Vec::new();
        if let Some(d) = dir {
            let mut st = db.prepare(
                "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at
                 FROM sessions WHERE workspace_dir=? ORDER BY updated_at DESC",
            )?;
            for r in st.query_map([d], Self::row_to_session)? {
                out.push(r?);
            }
        } else {
            let mut st = db.prepare(
                "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at
                 FROM sessions ORDER BY updated_at DESC",
            )?;
            for r in st.query_map([], Self::row_to_session)? {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn touch(&self, id: &str) -> Result<(), StoreError> {
        let db = self.db.lock().unwrap();
        db.execute("UPDATE sessions SET updated_at=? WHERE id=?", params![now(), id])?;
        Ok(())
    }

    pub fn update_session(
        &self,
        id: &str,
        agent: Option<&str>,
        mode: Option<&str>,
        model: Option<&str>,
        title: Option<&str>,
    ) -> Result<Option<Session>, StoreError> {
        {
            let db = self.db.lock().unwrap();
            if let Some(a) = agent {
                db.execute("UPDATE sessions SET agent=?, updated_at=? WHERE id=?", params![a, now(), id])?;
            }
            if let Some(m) = mode {
                db.execute("UPDATE sessions SET permission_mode=?, updated_at=? WHERE id=?", params![m, now(), id])?;
            }
            if let Some(m) = model {
                db.execute("UPDATE sessions SET model=?, updated_at=? WHERE id=?", params![m, now(), id])?;
            }
            if let Some(t) = title {
                db.execute("UPDATE sessions SET title=?, updated_at=? WHERE id=?", params![t, now(), id])?;
            }
        }
        self.get_session(id)
    }

    pub fn fork(&self, id: &str, at_turn: Option<&str>) -> Result<Session, StoreError> {
        let Some(src) = self.get_session(id)? else {
            return Err(StoreError::Msg(format!("session {id} not found")));
        };
        let mut child = self.create_session(&src.workspace_dir.clone(), &src.agent, &src.permission_mode, &src.model)?;
        child.parent_id = src.id.clone();
        child.title = format!("fork of {}", &src.id[..8.min(src.id.len())]);
        {
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE sessions SET parent_id=?, title=? WHERE id=?",
                params![child.parent_id, child.title, child.id],
            )?;
            // Copy turns up to (and including) the fork point.
            let turns: Vec<Turn> = {
                let mut st = db.prepare(
                    "SELECT id,session_id,parent_id,role,text,created_at FROM turns WHERE session_id=? ORDER BY created_at",
                )?;
                let rows = st.query_map([&src.id], |r| {
                    Ok(Turn {
                        id: r.get(0)?,
                        session_id: r.get(1)?,
                        parent_id: r.get(2)?,
                        role: r.get(3)?,
                        text: r.get(4)?,
                        created_at: r.get(5)?,
                    })
                })?;
                rows.collect::<Result<_, _>>()?
            };
            for t in turns {
                db.execute(
                    "INSERT INTO turns(id,session_id,parent_id,role,text,created_at) VALUES(?,?,?,?,?,?)",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        child.id,
                        t.parent_id,
                        t.role,
                        t.text,
                        t.created_at
                    ],
                )?;
                if Some(t.id.as_str()) == at_turn {
                    break;
                }
            }
        }
        self.get_session(&child.id).map(|s| s.unwrap())
    }

    pub fn add_turn(&self, session: &str, role: &str, text: &str) -> Result<Turn, StoreError> {
        let t = Turn {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.into(),
            parent_id: String::new(),
            role: role.into(),
            text: text.into(),
            created_at: now(),
        };
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO turns(id,session_id,parent_id,role,text,created_at) VALUES(?,?,?,?,?,?)",
            params![t.id, t.session_id, t.parent_id, t.role, t.text, t.created_at],
        )?;
        db.execute("UPDATE sessions SET updated_at=? WHERE id=?", params![now(), session])?;
        Ok(t)
    }

    pub fn turns(&self, session: &str) -> Result<Vec<Turn>, StoreError> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id,session_id,parent_id,role,text,created_at FROM turns WHERE session_id=? ORDER BY created_at",
        )?;
        let rows = st.query_map([session], |r| {
            Ok(Turn {
                id: r.get(0)?,
                session_id: r.get(1)?,
                parent_id: r.get(2)?,
                role: r.get(3)?,
                text: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn export_markdown(&self, session: &str, sanitize: bool) -> Result<String, StoreError> {
        let s = self
            .get_session(session)?
            .ok_or_else(|| StoreError::Msg(format!("session {session} not found")))?;
        let turns = self.turns(session)?;
        let mut out = format!("# Session {}\n\n- workspace: {}\n- agent: {}\n\n", s.id, s.workspace_dir, s.agent);
        for t in turns {
            let text = if sanitize { sanitize_export(&t.text) } else { t.text.clone() };
            out.push_str(&format!("## {}\n\n{}\n\n", t.role, text));
        }
        Ok(out)
    }
}

fn patterns() -> &'static Vec<Regex> {
    static ONCE: OnceLock<Vec<Regex>> = OnceLock::new();
    ONCE.get_or_init(|| {
        [
            r"sk-[A-Za-z0-9_-]{8,}",
            r"AKIA[0-9A-Z]{16}",
            r"Bearer\s+[A-Za-z0-9._~+/-]+=*",
            r#"(?i)(api[_-]?key\s*[:=]\s*)(['"]?)[^\s'"]+"#,
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    })
}

/// Redact secret-shaped values from transcripts (mirrors Go `SanitizeExport`).
pub fn sanitize_export(text: &str) -> String {
    let mut out = text.to_string();
    out = patterns()[0].replace_all(&out, "sk-REDACTED").into_owned();
    out = patterns()[1].replace_all(&out, "AKIA-REDACTED").into_owned();
    out = patterns()[2].replace_all(&out, "Bearer REDACTED").into_owned();
    out = patterns()[3].replace_all(&out, "${1}${2}REDACTED").into_owned();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Store {
        let db = Connection::open_in_memory().unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, parent_id TEXT DEFAULT '',
             workspace_dir TEXT NOT NULL, agent TEXT DEFAULT 'build',
             permission_mode TEXT DEFAULT 'bypass', model TEXT DEFAULT '', title TEXT DEFAULT '',
             created_at TEXT, updated_at TEXT);
             CREATE TABLE turns(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, parent_id TEXT DEFAULT '',
             role TEXT, text TEXT, created_at TEXT);
             CREATE INDEX idx_turns_session ON turns(session_id);
             CREATE TABLE approvals(req_id TEXT PRIMARY KEY, session_id TEXT, tool TEXT, target TEXT,
             decision TEXT, created_at TEXT);
             CREATE TABLE traces(id TEXT PRIMARY KEY, session_id TEXT, kind TEXT, payload TEXT, created_at TEXT);",
        )
        .unwrap();
        Store { db: Mutex::new(db) }
    }

    #[test]
    fn roundtrip_and_fork() {
        let s = mem();
        let a = s.create_session("/repo", "build", "bypass", "o/m").unwrap();
        s.add_turn(&a.id, "user", "hello").unwrap();
        s.add_turn(&a.id, "assistant", "hi").unwrap();
        let latest = s.latest_for_dir("/repo").unwrap().unwrap();
        assert_eq!(latest.id, a.id);
        let child = s.fork(&a.id, None).unwrap();
        assert_eq!(child.parent_id, a.id);
        assert_eq!(s.turns(&child.id).unwrap().len(), 2);
    }

    #[test]
    fn sanitize_redacts() {
        let t = "key sk-abc123xyz789 here and AKIAIOSFODNN7EXAMPLE plus Bearer tok123";
        let out = sanitize_export(t);
        assert!(!out.contains("sk-abc123"), "{out}");
        assert!(out.contains("REDACTED"));
    }
}
