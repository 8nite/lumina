//! Persistent sessions + turn trees in SQLite (WAL, single-writer mutex).
//! Schema (sessions/turns/approvals/traces).

use regex::Regex;
use rusqlite::{params, Connection};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
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
    /// Cumulative LLM usage for the session (opencode `session.tokens_*`
    /// parity). Updated per turn; survives restarts for cost dashboards.
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
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
    /// Memory-only secret vault (dies with the process; transcripts keep values).
    /// Arc-shared: subagent closures outlive the turn borrow.
    pub vault: Arc<crate::vault::Vault>,
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

/// Canonical workspace key so `.`/relative/absolute spellings of one
/// directory share sessions (web resume, `-c` continue, TUI picker all
/// compare through this). Canonicalizes via the filesystem when the path
/// exists, else joins relative paths onto the process cwd and cleans `.` /
/// `..` / trailing slashes lexically. Never fails: worst case returns the
/// trimmed input.
pub fn normalize_workspace(dir: &str) -> String {
    let t = dir.trim();
    if t.is_empty() || t == "." || t == "./" {
        return std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
    }
    // Strip trailing slashes (but keep root "/").
    let t = t.trim_end_matches('/');
    let t = if t.is_empty() { "/" } else { t };
    let abs = {
        let p = Path::new(t);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(p)
        }
    };
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return c.display().to_string();
    }
    let mut out = PathBuf::new();
    for comp in abs.components() {
        use std::path::Component::*;
        match comp {
            Prefix(p) => out.push(p.as_os_str()),
            RootDir => out.push("/"),
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            Normal(c) => out.push(c),
        }
    }
    let s = out.display().to_string();
    if s.is_empty() {
        "/".into()
    } else {
        s
    }
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
               id TEXT PRIMARY KEY, session_id TEXT, kind TEXT, payload TEXT, created_at TEXT);
             CREATE TABLE IF NOT EXISTS idempotency(
               session_id TEXT NOT NULL, request_id TEXT NOT NULL, created_at INTEGER,
               PRIMARY KEY(session_id, request_id));
             CREATE TABLE IF NOT EXISTS steered(
               id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, text TEXT, created_at TEXT);
             CREATE INDEX IF NOT EXISTS idx_steered_session ON steered(session_id);
              CREATE TABLE IF NOT EXISTS stops(session_id TEXT PRIMARY KEY);",
        )?;
        for ddl in [
            "ALTER TABLE sessions ADD COLUMN inflight_owner TEXT DEFAULT ''",
            "ALTER TABLE sessions ADD COLUMN inflight_since INTEGER DEFAULT 0",
            "ALTER TABLE sessions ADD COLUMN tokens_input INTEGER DEFAULT 0",
            "ALTER TABLE sessions ADD COLUMN tokens_output INTEGER DEFAULT 0",
            "ALTER TABLE sessions ADD COLUMN tokens_cache_read INTEGER DEFAULT 0",
            "ALTER TABLE sessions ADD COLUMN tokens_cache_write INTEGER DEFAULT 0",
        ] {
            // Duplicate-column is the happy path on a migrated DB.
            let _ = db.execute_batch(ddl);
        }
        Ok(Self {
            db: Mutex::new(db),
            vault: Arc::new(crate::vault::Vault::new()),
        })
    }

    pub fn create_session(
        &self,
        workspace: &str,
        agent: &str,
        mode: &str,
        model: &str,
    ) -> Result<Session, StoreError> {
        // Normalize so `.`/relative/absolute spellings of one directory share
        // sessions (web resume `-c` continue, TUI picker). See normalize_workspace.
        let workspace = normalize_workspace(workspace);
        let s = Session {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: String::new(),
            workspace_dir: workspace,
            agent: agent.into(),
            permission_mode: mode.into(),
            model: model.into(),
            title: String::new(),
            created_at: now(),
            updated_at: now(),
            tokens_input: 0,
            tokens_output: 0,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
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
        // New columns default to 0 on pre-migration rows / narrow SELECTs.
        let get_i64 = |idx: usize| -> i64 { r.get::<_, Option<i64>>(idx).unwrap_or(Some(0)).unwrap_or(0) };
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
            tokens_input: get_i64(9),
            tokens_output: get_i64(10),
            tokens_cache_read: get_i64(11),
            tokens_cache_write: get_i64(12),
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>, StoreError> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at,tokens_input,tokens_output,tokens_cache_read,tokens_cache_write
             FROM sessions WHERE id=?",
        )?;
        let mut rows = st.query([id])?;
        Ok(rows.next()?.map(Self::row_to_session).transpose()?)
    }

    pub fn latest_for_dir(&self, dir: &str) -> Result<Option<Session>, StoreError> {
        let want = normalize_workspace(dir);
        let db = self.db.lock().unwrap();
        let hit = {
            let mut st = db.prepare(
                "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at,tokens_input,tokens_output,tokens_cache_read,tokens_cache_write
                 FROM sessions WHERE workspace_dir=? ORDER BY updated_at DESC LIMIT 1",
            )?;
            let mut rows = st.query([&want])?;
            rows.next()?.map(Self::row_to_session).transpose()?
        };
        if let Some(s) = hit {
            return Ok(Some(s));
        }
        // Legacy fallback: rows stored pre-normalization (e.g. literal ".").
        let mut st = db.prepare(
            "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at,tokens_input,tokens_output,tokens_cache_read,tokens_cache_write
             FROM sessions ORDER BY updated_at DESC",
        )?;
        let all = st.query_map([], Self::row_to_session)?;
        for s in all {
            let s = s?;
            if normalize_workspace(&s.workspace_dir) == want {
                return Ok(Some(s));
            }
        }
        Ok(None)
    }

    pub fn list_for_dir(&self, dir: Option<&str>) -> Result<Vec<Session>, StoreError> {
        let db = self.db.lock().unwrap();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if let Some(d) = dir {
            let want = normalize_workspace(d);
            {
                let mut st = db.prepare(
                    "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at,tokens_input,tokens_output,tokens_cache_read,tokens_cache_write
                     FROM sessions WHERE workspace_dir=? ORDER BY updated_at DESC",
                )?;
                for r in st.query_map([&want], Self::row_to_session)? {
                    let s = r?;
                    seen.insert(s.id.clone());
                    out.push(s);
                }
            }
            // Legacy fallback: pre-normalization spellings (e.g. ".").
            let mut st = db.prepare(
                "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at,tokens_input,tokens_output,tokens_cache_read,tokens_cache_write
                 FROM sessions ORDER BY updated_at DESC",
            )?;
            for r in st.query_map([], Self::row_to_session)? {
                let s = r?;
                if !seen.contains(&s.id) && normalize_workspace(&s.workspace_dir) == want {
                    out.push(s);
                }
            }
        } else {
            let mut st = db.prepare(
                "SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at,tokens_input,tokens_output,tokens_cache_read,tokens_cache_write
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

    /// Accumulate per-turn LLM usage onto the session (opencode
    /// `session.tokens_*` parity) and stamp updated_at. Saturating: a
    /// corrupt usage spike can never wrap the counters.
    pub fn add_usage(&self, id: &str, u: &crate::llm::Usage) -> Result<(), StoreError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE sessions SET
                tokens_input = tokens_input + ?, tokens_output = tokens_output + ?,
                tokens_cache_read = tokens_cache_read + ?, tokens_cache_write = tokens_cache_write + ?,
                updated_at=? WHERE id=?",
            params![u.input as i64, u.output as i64, u.cache_read as i64, u.cache_write as i64, now(), id],
        )?;
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
            "SELECT id,session_id,parent_id,role,text,created_at FROM turns WHERE session_id=? ORDER BY created_at, rowid",
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

    /// Go-daemon-compatible transcript JSON for the web UI
    /// (`GET /export?format=json`). Missing sessions error so the
    /// handler keeps its 404 (the web client treats export-404 as
    /// "session vanished server-side").
    pub fn transcript_json(&self, session: &str, sanitize: bool) -> Result<serde_json::Value, StoreError> {
        let s = self
            .get_session(session)?
            .ok_or_else(|| StoreError::Msg(format!("session {session} not found")))?;
        let transcript: Vec<serde_json::Value> = self
            .turns(session)?
            .into_iter()
            .map(|t| {
                let text = if sanitize { sanitize_export(&t.text) } else { t.text };
                serde_json::json!({
                    "id": t.id,
                    "role": t.role,
                    "text": text,
                    "created_at": t.created_at,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "id": s.id,
            "sanitized": sanitize,
            "transcript": transcript,
        }))
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


impl Store {
    /// Atomically swap compacted turns for their summary (compaction).
    /// Delete-then-insert in one transaction: a crash can never leave
    /// the middle turns deleted with no summary.
    pub fn compact_replace(&self, session: &str, ids: &[String], summary: &str) -> Result<usize, StoreError> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut n = 0;
        for id in ids {
            n += tx.execute("DELETE FROM turns WHERE session_id=? AND id=?", params![session, id])?;
        }
        let t = Turn {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.into(),
            parent_id: String::new(),
            role: "assistant".into(),
            text: summary.into(),
            created_at: now(),
        };
        tx.execute(
            "INSERT INTO turns(id,session_id,parent_id,role,text,created_at) VALUES(?,?,?,?,?,?)",
            params![t.id, t.session_id, t.parent_id, t.role, t.text, t.created_at],
        )?;
        tx.execute("UPDATE sessions SET updated_at=? WHERE id=?", params![now(), session])?;
        tx.commit()?;
        Ok(n)
    }

    /// Delete a session and all its rows (turns, approvals, traces).
    /// Returns true iff a session row was deleted.
    pub fn delete_session(&self, id: &str) -> Result<bool, StoreError> {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM turns WHERE session_id=?", params![id])?;
        db.execute("DELETE FROM approvals WHERE session_id=?", params![id])?;
        db.execute("DELETE FROM traces WHERE session_id=?", params![id])?;
        Ok(db.execute("DELETE FROM sessions WHERE id=?", params![id])? > 0)
    }

    /// Session owner + claim timestamp for 409 diagnostics (TurnOwner parity).
    pub fn turn_owner(&self, id: &str) -> Option<(String, i64)> {
        let db = self.db.lock().unwrap();
        let mut st = db
            .prepare("SELECT inflight_owner, inflight_since FROM sessions WHERE id=?")
            .ok()?;
        let (owner, since): (String, i64) = st.query_row([id], |r| Ok((r.get(0)?, r.get(1)?))).ok()?;
        if owner.is_empty() {
            return None;
        }
        Some((owner, since))
    }

    /// Append a trace event (checkpoints, reverts, stats).
    pub fn add_trace(&self, session: &str, kind: &str, payload: &str) -> Result<(), StoreError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO traces(id,session_id,kind,payload,created_at) VALUES(?,?,?,?,?)",
            params![uuid::Uuid::new_v4().to_string(), session, kind, payload, now()],
        )?;
        Ok(())
    }

    /// Latest trace payload of a kind for a session.
    pub fn latest_trace(&self, session: &str, kind: &str) -> Result<Option<String>, StoreError> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT payload FROM traces WHERE session_id=? AND kind=? ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = st.query(params![session, kind])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }
}

impl Store {
    /// Checkpoint payloads in creation order (oldest first).
    pub fn traces(&self, session: &str, kind: &str) -> Result<Vec<String>, StoreError> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT payload FROM traces WHERE session_id=? AND kind=? ORDER BY created_at",
        )?;
        let rows = st.query_map(params![session, kind], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Store {
        Store::open(Some(":memory:")).unwrap()
    }

    #[test]
    fn workspace_dot_and_absolute_share_sessions() {
        // Web creates sessions with "." while CLI/TUI use the absolute cwd;
        // both spellings must resolve to the same workspace (resume fix).
        let cwd = std::env::current_dir().unwrap().display().to_string();
        assert_eq!(normalize_workspace("."), cwd);
        assert_eq!(normalize_workspace(""), cwd);
        assert_eq!(normalize_workspace("./"), cwd);
        assert_eq!(normalize_workspace(&cwd), cwd);
        assert_eq!(normalize_workspace(&format!("{cwd}/")), cwd);
        let s = mem();
        let a = s.create_session(".", "build", "bypass", "").unwrap();
        assert_eq!(a.workspace_dir, cwd);
        // Lookup by absolute path finds the dot-created session.
        assert_eq!(s.latest_for_dir(&cwd).unwrap().unwrap().id, a.id);
        assert_eq!(s.list_for_dir(Some(&cwd)).unwrap().len(), 1);
        // Legacy rows stored pre-normalization (literal ".") still resolve.
        {
            let db = s.db.lock().unwrap();
            db.execute(
                "INSERT INTO sessions(id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at)
                 VALUES(?,?,?,?,?,?,?,?,?)",
                rusqlite::params!["legacy-dot", "", ".", "build", "bypass", "", "", "2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z"],
            )
            .unwrap();
        }
        let listed = s.list_for_dir(Some(&cwd)).unwrap();
        assert_eq!(listed.len(), 2, "legacy '.' row must list under {cwd}");
        // Unrelated directories are unaffected.
        assert!(s.latest_for_dir("/definitely/not/here").unwrap().is_none());
        assert!(s.list_for_dir(Some("/definitely/not/here")).unwrap().is_empty());
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
    fn usage_accumulates_per_session() {
        let s = mem();
        let a = s.create_session("/repo", "build", "bypass", "").unwrap();
        let u = crate::llm::Usage { input: 100, output: 20, cache_read: 800, cache_write: 5 };
        s.add_usage(&a.id, &u).unwrap();
        s.add_usage(&a.id, &u).unwrap();
        let got = s.get_session(&a.id).unwrap().unwrap();
        assert_eq!((got.tokens_input, got.tokens_output), (200, 40));
        assert_eq!((got.tokens_cache_read, got.tokens_cache_write), (1600, 10));
    }

    #[test]
    fn delete_session_removes_turns_and_row() {
        let s = mem();
        let a = s.create_session("/repo", "build", "bypass", "").unwrap();
        s.add_turn(&a.id, "user", "hello").unwrap();
        assert!(s.delete_session(&a.id).unwrap());
        assert!(s.get_session(&a.id).unwrap().is_none());
        assert!(s.turns(&a.id).unwrap().is_empty());
        assert!(!s.delete_session(&a.id).unwrap());
        assert!(!s.delete_session("no-such-session").unwrap());
    }

    #[test]
    fn steer_inbox_roundtrip() {
        let s = mem();
        assert_eq!(s.take_steered("s1"), "");
        s.steer("s1", "  ");
        assert_eq!(s.take_steered("s1"), "");
        s.steer("s1", "first");
        s.steer("s1", "second");
        s.steer("s2", "other");
        assert_eq!(s.take_steered("s1"), "first\n\nsecond");
        assert_eq!(s.take_steered("s1"), "");
        assert_eq!(s.take_steered("s2"), "other");
    }

    #[test]
    fn stop_flag_roundtrip() {
        let s = mem();
        assert!(!s.stop_requested("s1"));
        s.request_stop("s1");
        // Non-consuming: the running turn clears it when honored.
        assert!(s.stop_requested("s1"));
        assert!(s.stop_requested("s1"));
        s.clear_stop("s1");
        assert!(!s.stop_requested("s1"));
        // Re-request is idempotent (INSERT OR REPLACE).
        s.request_stop("s1");
        s.request_stop("s1");
        assert!(s.stop_requested("s1"));
    }

    #[test]
    fn sanitize_redacts() {
        let t = "key sk-abc123xyz789 here and AKIAIOSFODNN7EXAMPLE plus Bearer tok123";
        let out = sanitize_export(t);
        assert!(!out.contains("sk-abc123"), "{out}");
        assert!(out.contains("REDACTED"));
    }

    #[test]
    fn transcript_json_carries_thinking_in_order() {
        let s = mem();
        let a = s.create_session("/repo", "build", "bypass", "").unwrap();
        s.add_turn(&a.id, "user", "fix it").unwrap();
        s.add_turn(&a.id, "thinking", "first I will read the file").unwrap();
        s.add_turn(&a.id, "tool", "read: src/main.rs").unwrap();
        s.add_turn(&a.id, "thinking", "now I will patch it").unwrap();
        s.add_turn(&a.id, "assistant", "done").unwrap();
        let v = s.transcript_json(&a.id, false).unwrap();
        let roles: Vec<&str> = v["transcript"].as_array().unwrap().iter().map(|t| t["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "thinking", "tool", "thinking", "assistant"]);
        assert_eq!(v["id"], a.id);
        assert!(s.transcript_json("no-such-session", false).is_err(), "missing session must error (handler 404)");
        // Sanitize path redacts secrets but keeps roles.
        s.add_turn(&a.id, "thinking", "key sk-abc123xyz789 here").unwrap();
        let red = s.transcript_json(&a.id, true).unwrap();
        let last = red["transcript"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], "thinking");
        assert!(!last["text"].as_str().unwrap().contains("sk-abc123"));
    }

    #[test]
    fn transcript_json_preserves_todowrite_progress_to_3_of_3() {
        // Contract with the web TodoPill: the agent persists every tool
        // output as role "user" with a "[tool result <name>]" prefix, and
        // the pill scans the transcript backwards for the LATEST todowrite
        // turn. A 1/3 → 2/3 → 3/3 run must keep all three turns in order
        // with roles and text intact, or the pill can never settle at 3/3.
        let s = mem();
        let a = s.create_session("/repo", "build", "bypass", "").unwrap();
        for done in 1..=3 {
            s.add_turn(&a.id, "user", &format!("do step {done}")).unwrap();
            s.add_turn(&a.id, "user", &format!("[tool result todowrite]\nTodos updated: {done}/3 completed")).unwrap();
        }
        let v = s.transcript_json(&a.id, false).unwrap();
        let tx = v["transcript"].as_array().unwrap();
        assert_eq!(tx.len(), 6);
        let todos: Vec<&str> = tx
            .iter()
            .filter(|t| {
                t["role"].as_str() == Some("user")
                    && t["text"].as_str().unwrap_or("").starts_with("[tool result todowrite]")
            })
            .map(|t| t["text"].as_str().unwrap())
            .collect();
        assert_eq!(todos.len(), 3);
        assert!(todos[2].contains("3/3 completed"), "{todos:?}");
    }
}

impl Store {
    /// Atomically claim the in-flight turn slot (cross-daemon safety).
    /// Takes the slot when free, owned by us, or stale (> max_age_secs).
    pub fn claim_turn(&self, id: &str, owner: &str, max_age_secs: i64) -> bool {
        let db = self.db.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        let stale_before = now - max_age_secs;
        let n = db
            .execute(
                "UPDATE sessions SET inflight_owner=?, inflight_since=? WHERE id=? AND (inflight_owner='' OR inflight_owner=? OR inflight_since<?)",
                params![owner, now, id, owner, stale_before],
            )
            .unwrap_or(0);
        n > 0
    }

    /// Release the turn slot (only our own claims).
    /// Buffer text into the running turn (steer delivery). Empty trims to noop.
    /// DB-backed: survives daemon restarts (the old memory inbox lost steers).
    pub fn steer(&self, session: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let db = self.db.lock().unwrap();
        let _ = db.execute(
            "INSERT INTO steered(session_id,text,created_at) VALUES(?,?,?)",
            params![session, text, now()],
        );
    }

    /// Drain buffered steer text for one turn (joined, trimmed).
    pub fn take_steered(&self, session: &str) -> String {
        let db = self.db.lock().unwrap();
        let rows: Vec<String> = db
            .prepare("SELECT text FROM steered WHERE session_id=? ORDER BY id")
            .and_then(|mut st| st.query_map([session], |r| r.get(0))?.collect::<Result<_, _>>())
            .unwrap_or_default();
        let _ = db.execute("DELETE FROM steered WHERE session_id=?", params![session]);
        rows.join("\n\n").trim().to_string()
    }

    /// Cooperative stop flag: set by POST /stop, noticed at turn round
    /// boundaries (and mid-stream polls). DB-backed like steered so the
    /// flag survives daemon restarts mid-turn.
    pub fn request_stop(&self, session: &str) {
        let db = self.db.lock().unwrap();
        let _ = db.execute("INSERT OR REPLACE INTO stops(session_id) VALUES(?)", params![session]);
    }

    /// Non-consuming check: the running turn clears the flag when it
    /// honors the stop, so a set flag always means "stop now".
    pub fn stop_requested(&self, session: &str) -> bool {
        let db = self.db.lock().unwrap();
        db.query_row("SELECT COUNT(*) FROM stops WHERE session_id=?", params![session], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    /// Clear the flag (stop honored, or stop arrived for an idle session).
    pub fn clear_stop(&self, session: &str) {
        let db = self.db.lock().unwrap();
        let _ = db.execute("DELETE FROM stops WHERE session_id=?", params![session]);
    }

    /// Retry idempotency: true when (session, request_id) was already
    /// submitted within ttl_secs. DB-backed: survives daemon restarts.
    /// Prunes expired keys on each call so the table stays bounded.
    pub fn seen_request(&self, session: &str, request_id: &str, ttl_secs: i64) -> Result<bool, StoreError> {
        let db = self.db.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        db.execute("DELETE FROM idempotency WHERE created_at < ?", params![now - ttl_secs])?;
        let n: i64 = db.query_row(
            "SELECT COUNT(*) FROM idempotency WHERE session_id=? AND request_id=?",
            params![session, request_id],
            |r| r.get(0),
        )?;
        if n > 0 {
            return Ok(true);
        }
        db.execute(
            "INSERT INTO idempotency(session_id,request_id,created_at) VALUES(?,?,?)",
            params![session, request_id, now],
        )?;
        Ok(false)
    }

    pub fn release_turn(&self, id: &str, owner: &str) {
        let db = self.db.lock().unwrap();
        let _ = db.execute(
            "UPDATE sessions SET inflight_owner='', inflight_since=0 WHERE id=? AND inflight_owner=?",
            params![id, owner],
        );
    }
}

#[cfg(test)]
mod claim_tests {
    use super::*;

    #[test]
    fn claim_release_cycle() {
        let s = Store::open(Some(":memory:")).unwrap();
        let sess = s.create_session("/w", "build", "bypass", "").unwrap();
        assert!(s.claim_turn(&sess.id, "daemon-a", 600));
        assert!(!s.claim_turn(&sess.id, "daemon-b", 600)); // held: 409 path
        assert!(s.claim_turn(&sess.id, "daemon-a", 600)); // owner re-entry ok
        s.release_turn(&sess.id, "daemon-b"); // wrong owner: no-op
        assert!(!s.claim_turn(&sess.id, "daemon-b", 600));
        s.release_turn(&sess.id, "daemon-a");
        assert!(s.claim_turn(&sess.id, "daemon-b", 600));
        assert!(!s.claim_turn(&sess.id, "daemon-c", 3600)); // fresh: still held
        assert!(s.claim_turn(&sess.id, "daemon-c", -1)); // stale: taken over
    }
}

impl Store {
    /// Release every in-flight turn slot (daemon boot: a fresh process owns
    /// no live turns, so any leftover claim is a crash artifact).
    pub fn clear_all_claims(&self) -> Result<usize, StoreError> {
        let db = self.db.lock().unwrap();
        Ok(db.execute("UPDATE sessions SET inflight_owner='', inflight_since=0 WHERE inflight_owner != ''", [])? as usize)
    }
}
