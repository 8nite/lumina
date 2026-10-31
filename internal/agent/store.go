// Package agent owns the turn loop, persistent sessions, shadow-Git
// checkpoints (/undo /redo), auto-compaction, and subagent runs.
package agent

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/google/uuid"
	_ "modernc.org/sqlite"
)

// Turn is one user/assistant exchange branch node.
type Turn struct {
	ID        string
	SessionID string
	ParentID  string
	Role      string
	Text      string
	CreatedAt string
}

// Session mirrors the API view plus parent linkage.
type Session struct {
	ID             string
	ParentID       string
	WorkspaceDir   string
	Agent          string
	PermissionMode string
	Model          string
	Title          string
	CreatedAt      string
	UpdatedAt      string
}

// Store persists sessions + turn trees in pure-Go SQLite (WAL).
// The secret vault (vault.go) is memory-only: per-session ref→value maps
// that die with the process.
type Store struct {
	db   *sql.DB
	path string

	vaultMu      sync.Mutex
	vault        map[string]map[string]string // sessionID -> ref -> value
	vaultByValue map[string]map[string]string // sessionID -> value -> ref
}

// Open creates ~/. dirs as needed and migrates schema.
func Open(path string) (*Store, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, err
	}
	db, err := sql.Open("sqlite", path+"?cache=shared")
	if err != nil {
		return nil, err
	}
	// Single-connection pool: one daemon owns this DB, so every statement
	// serializes through one SQLite connection. Without this, database/sql
	// opens extra pooled connections that never saw the busy_timeout below,
	// and concurrent writers fail fast with SQLITE_BUSY (the parallel-prompt
	// turn loss). WAL + busy_timeout cover the rest; exec() retries the
	// cross-process remainder (e.g. headless `lumina run` on one LUMINA_DB
	// while the daemon writes).
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(`PRAGMA journal_mode=WAL`); err != nil {
		return nil, err
	}
	// Concurrent turns (steer bursts, queued chains) must wait on the
	// writer lock instead of failing with SQLITE_BUSY.
	if _, err := db.Exec(`PRAGMA busy_timeout=5000`); err != nil {
		return nil, err
	}
	if _, err := db.Exec(`PRAGMA synchronous=NORMAL`); err != nil {
		return nil, err
	}
	s := &Store{db: db, path: path}
	if err := s.migrate(); err != nil {
		return nil, err
	}
	return s, nil
}

func (s *Store) migrate() error {
	_, err := s.db.Exec(`
CREATE TABLE IF NOT EXISTS sessions(
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
`)
	if err != nil {
		return err
	}
	// Cross-daemon turn claim (see ClaimTurn): pre-existing rows predate
	// the columns, so ALTER tolerantly — duplicate-column is the happy
	// path on an already-migrated DB.
	for _, ddl := range []string{
		`ALTER TABLE sessions ADD COLUMN inflight_owner TEXT DEFAULT ''`,
		`ALTER TABLE sessions ADD COLUMN inflight_since INTEGER DEFAULT 0`,
	} {
		if _, err := s.db.Exec(ddl); err != nil && !isBusy(err) &&
			!strings.Contains(strings.ToLower(err.Error()), "duplicate column") {
			return err
		}
	}
	return nil
}

// Close releases the DB.
func (s *Store) Close() error { return s.db.Close() }

func nowStr() string { return time.Now().UTC().Format(time.RFC3339Nano) }

// exec runs a write, retrying SQLITE_BUSY with backoff. In-process writers
// never collide (single-connection pool above), but a second process on the
// same DB file can hold the write lock briefly; losing a turn over that
// would be silent data loss, so wait it out instead.
func (s *Store) exec(query string, args ...any) (sql.Result, error) {
	var res sql.Result
	var err error
	for attempt := 0; attempt < 5; attempt++ {
		res, err = s.db.Exec(query, args...)
		if err == nil || !isBusy(err) {
			return res, err
		}
		time.Sleep(time.Duration(25<<attempt) * time.Millisecond)
	}
	return res, err
}

// isBusy reports lock-contention errors worth retrying (never logic errors).
func isBusy(err error) bool {
	if err == nil {
		return false
	}
	msg := strings.ToLower(err.Error())
	return strings.Contains(msg, "locked") || strings.Contains(msg, "busy")
}

// CreateSession inserts a workspace-attributed session.
func (s *Store) CreateSession(workspaceDir, agent, perm, model, title string) (*Session, error) {
	if agent == "" {
		agent = "build"
	}
	if perm == "" {
		perm = "bypass"
	}
	sess := &Session{
		ID: uuid.NewString()[:8], WorkspaceDir: workspaceDir,
		Agent: agent, PermissionMode: perm, Model: model, Title: title,
		CreatedAt: nowStr(), UpdatedAt: nowStr(),
	}
	_, err := s.exec(`INSERT INTO sessions(id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at)
 VALUES(?,?,?,?,?,?,?,?,?)`, sess.ID, "", sess.WorkspaceDir, sess.Agent, sess.PermissionMode, sess.Model, sess.Title, sess.CreatedAt, sess.UpdatedAt)
	return sess, err
}

// InsertSession stores a session with a pre-assigned ID (daemon API ids).
func (s *Store) InsertSession(sess *Session) error {
	if sess.CreatedAt == "" {
		sess.CreatedAt = nowStr()
	}
	if sess.UpdatedAt == "" {
		sess.UpdatedAt = sess.CreatedAt
	}
	_, err := s.exec(`INSERT INTO sessions(id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at)
 VALUES(?,?,?,?,?,?,?,?,?)`, sess.ID, sess.ParentID, sess.WorkspaceDir, sess.Agent, sess.PermissionMode, sess.Model, sess.Title, sess.CreatedAt, sess.UpdatedAt)
	return err
}

// GetSession fetches one session.
func (s *Store) GetSession(id string) (*Session, error) {
	row := s.db.QueryRow(`SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at FROM sessions WHERE id=?`, id)
	var x Session
	if err := row.Scan(&x.ID, &x.ParentID, &x.WorkspaceDir, &x.Agent, &x.PermissionMode, &x.Model, &x.Title, &x.CreatedAt, &x.UpdatedAt); err != nil {
		return nil, err
	}
	return &x, nil
}

// ListSessions lists sessions, optionally filtered by workspace dir.
func (s *Store) ListSessions(directory string) ([]*Session, error) {
	q := `SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at FROM sessions ORDER BY updated_at DESC`
	var rows *sql.Rows
	var err error
	if directory != "" {
		q = `SELECT id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at FROM sessions WHERE workspace_dir=? ORDER BY updated_at DESC`
		rows, err = s.db.Query(q, directory)
	} else {
		rows, err = s.db.Query(q)
	}
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*Session
	for rows.Next() {
		var x Session
		if err := rows.Scan(&x.ID, &x.ParentID, &x.WorkspaceDir, &x.Agent, &x.PermissionMode, &x.Model, &x.Title, &x.CreatedAt, &x.UpdatedAt); err != nil {
			return nil, err
		}
		out = append(out, &x)
	}
	return out, rows.Err()
}

// LatestSession returns the most recent session for a workspace (-c flag).
func (s *Store) LatestSession(directory string) (*Session, error) {
	list, err := s.ListSessions(directory)
	if err != nil || len(list) == 0 {
		return nil, fmt.Errorf("no sessions for %s", directory)
	}
	return list[0], nil
}

// UpdateSession patches agent/permission/model/title in place.
func (s *Store) UpdateSession(id string, agent, perm, model, title *string) (*Session, error) {
	sess, err := s.GetSession(id)
	if err != nil {
		return nil, err
	}
	if agent != nil {
		sess.Agent = *agent
	}
	if perm != nil {
		sess.PermissionMode = *perm
	}
	if model != nil {
		sess.Model = *model
	}
	if title != nil {
		sess.Title = *title
	}
	sess.UpdatedAt = nowStr()
	_, err = s.exec(`UPDATE sessions SET agent=?,permission_mode=?,model=?,title=?,updated_at=? WHERE id=?`,
		sess.Agent, sess.PermissionMode, sess.Model, sess.Title, sess.UpdatedAt, id)
	return sess, err
}

// ForkSession branches a session from a turn (parent linkage preserved).
func (s *Store) ForkSession(id, turnID string) (*Session, error) {
	src, err := s.GetSession(id)
	if err != nil {
		return nil, err
	}
	fork := &Session{
		ID: uuid.NewString()[:8], ParentID: src.ID, WorkspaceDir: src.WorkspaceDir,
		Agent: src.Agent, PermissionMode: src.PermissionMode,
		Model: src.Model, Title: src.Title + " (fork)",
		CreatedAt: nowStr(), UpdatedAt: nowStr(),
	}
	_, err = s.exec(`INSERT INTO sessions(id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at)
 VALUES(?,?,?,?,?,?,?,?,?)`, fork.ID, fork.ParentID, fork.WorkspaceDir, fork.Agent, fork.PermissionMode, fork.Model, fork.Title, fork.CreatedAt, fork.UpdatedAt)
	if err != nil {
		return nil, err
	}
	// Copy ancestor turns up to turnID (or all when empty).
	turns, err := s.ListTurns(id)
	if err != nil {
		return nil, err
	}
	for _, t := range turns {
		_, err = s.exec(`INSERT INTO turns(id,session_id,parent_id,role,text,created_at) VALUES(?,?,?,?,?,?)`,
			uuid.NewString()[:8], fork.ID, t.ParentID, t.Role, t.Text, t.CreatedAt)
		if err != nil {
			return nil, err
		}
		if turnID != "" && t.ID == turnID {
			break
		}
	}
	return fork, nil
}

// AddTurn appends a turn node.
func (s *Store) AddTurn(sessionID, parentID, role, text string) (*Turn, error) {
	t := &Turn{ID: uuid.NewString()[:8], SessionID: sessionID, ParentID: parentID, Role: role, Text: text, CreatedAt: nowStr()}
	_, err := s.exec(`INSERT INTO turns(id,session_id,parent_id,role,text,created_at) VALUES(?,?,?,?,?,?)`,
		t.ID, t.SessionID, t.ParentID, t.Role, t.Text, t.CreatedAt)
	if err != nil {
		return nil, err
	}
	_, _ = s.exec(`UPDATE sessions SET updated_at=? WHERE id=?`, t.CreatedAt, sessionID)
	return t, nil
}

// ListTurns returns a session's turns in insertion order.
func (s *Store) ListTurns(sessionID string) ([]*Turn, error) {
	rows, err := s.db.Query(`SELECT id,session_id,parent_id,role,text,created_at FROM turns WHERE session_id=? ORDER BY created_at`, sessionID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*Turn
	for rows.Next() {
		var t Turn
		if err := rows.Scan(&t.ID, &t.SessionID, &t.ParentID, &t.Role, &t.Text, &t.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, &t)
	}
	return out, rows.Err()
}

// ClaimTurn atomically claims the in-flight turn slot for a session.
// Only one holder across ALL processes sharing this DB file can own the
// slot: without this, two daemons on one DB (e.g. :4469 + :4089) silently
// double-drive one session, interleaving transcripts and crossing streams.
// A claim by the same owner re-claims (chain-safe); a claim older than
// maxAge is stale (owner died mid-turn — turn ctx caps at 10min) and may be
// taken over. Returns true when this owner now holds the slot.
func (s *Store) ClaimTurn(sessionID, owner string, maxAge time.Duration) bool {
	cutoff := time.Now().Add(-maxAge).UnixNano()
	res, err := s.exec(`UPDATE sessions SET inflight_owner=?, inflight_since=?
  WHERE id=? AND (inflight_owner='' OR inflight_owner=? OR inflight_since<?)`,
		owner, time.Now().UnixNano(), sessionID, owner, cutoff)
	if err != nil {
		return false
	}
	n, err := res.RowsAffected()
	return err == nil && n == 1
}

// ReleaseTurn frees the slot, but only for its current owner (never steal
// another daemon's live claim on a delayed completion).
func (s *Store) ReleaseTurn(sessionID, owner string) {
	_, _ = s.exec(`UPDATE sessions SET inflight_owner='', inflight_since=0
  WHERE id=? AND inflight_owner=?`, sessionID, owner)
}

// TurnOwner reports the current slot holder ("", 0 when idle).
func (s *Store) TurnOwner(sessionID string) (string, int64) {
	var owner string
	var since int64
	if err := s.db.QueryRow(`SELECT inflight_owner, inflight_since FROM sessions WHERE id=?`,
		sessionID).Scan(&owner, &since); err != nil {
		return "", 0
	}
	return owner, since
}

var sanitizePatterns = []*regexp.Regexp{
	regexp.MustCompile(`(?i)(api[_-]?key\s*[:=]\s*)['"]?[^\s'"]+`),
	regexp.MustCompile(`(?i)(secret\s*[:=]\s*)['"]?[^\s'"]+`),
	regexp.MustCompile(`(?i)(private[_-]?key\s*[:=]\s*)['"]?[^\s'"]+`),
	regexp.MustCompile(`sk-[A-Za-z0-9-_]{8,}`),
	regexp.MustCompile(`AKIA[0-9A-Z]{16}`),
	regexp.MustCompile(`(?i)bearer\s+[A-Za-z0-9\-._~+/]+=*`),
	regexp.MustCompile(`(?s)-----BEGIN [^-]*PRIVATE KEY[^-]*-----.*?-----END [^-]*PRIVATE KEY[^-]*-----`),
	regexp.MustCompile(`(?i)(sin|ssn|social[_ \-]?security(?:[_ \-]?number)?|social[_ \-]?insurance(?:[_ \-]?number)?\s*[:=]?\s*)(\d{3}[- ]?\d{3}[- ]?\d{3}|\d{3}[- ]\d{2}[- ]\d{4})`),
	regexp.MustCompile(`\b\d{3}[- ]\d{3}[- ]\d{3}\b`),
	regexp.MustCompile(`\b\d{3}[- ]\d{2}[- ]\d{4}\b`),
}

// SanitizeExport redacts secret-shaped values from transcripts.
func SanitizeExport(text string) string {
	for _, re := range sanitizePatterns {
		text = re.ReplaceAllString(text, "${1}[REDACTED]")
	}
	// Fix group-less patterns that captured the whole token.
	text = regexp.MustCompile(`sk-\[REDACTED\]`).ReplaceAllString(text, "[REDACTED]")
	return text
}
