// Package agent owns the turn loop, persistent sessions, shadow-Git
// checkpoints (/undo /redo), auto-compaction, and subagent runs.
package agent

import (
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
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
type Store struct {
	db   *sql.DB
	path string
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
	if _, err := db.Exec(`PRAGMA journal_mode=WAL`); err != nil {
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
 permission_mode TEXT DEFAULT 'ask', model TEXT DEFAULT '', title TEXT DEFAULT '',
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
	return err
}

// Close releases the DB.
func (s *Store) Close() error { return s.db.Close() }

func nowStr() string { return time.Now().UTC().Format(time.RFC3339Nano) }

// CreateSession inserts a workspace-attributed session.
func (s *Store) CreateSession(workspaceDir, agent, perm, model, title string) (*Session, error) {
	if agent == "" {
		agent = "build"
	}
	if perm == "" {
		perm = "ask"
	}
	sess := &Session{
		ID: uuid.NewString()[:8], WorkspaceDir: workspaceDir,
		Agent: agent, PermissionMode: perm, Model: model, Title: title,
		CreatedAt: nowStr(), UpdatedAt: nowStr(),
	}
	_, err := s.db.Exec(`INSERT INTO sessions(id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at)
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
	_, err := s.db.Exec(`INSERT INTO sessions(id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at)
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
	_, err = s.db.Exec(`UPDATE sessions SET agent=?,permission_mode=?,model=?,title=?,updated_at=? WHERE id=?`,
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
		Agent: src.PermissionMode, PermissionMode: src.PermissionMode,
		Model: src.Model, Title: src.Title + " (fork)",
		CreatedAt: nowStr(), UpdatedAt: nowStr(),
	}
	fork.Agent = src.Agent
	_, err = s.db.Exec(`INSERT INTO sessions(id,parent_id,workspace_dir,agent,permission_mode,model,title,created_at,updated_at)
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
		_, err = s.db.Exec(`INSERT INTO turns(id,session_id,parent_id,role,text,created_at) VALUES(?,?,?,?,?,?)`,
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
	_, err := s.db.Exec(`INSERT INTO turns(id,session_id,parent_id,role,text,created_at) VALUES(?,?,?,?,?,?)`,
		t.ID, t.SessionID, t.ParentID, t.Role, t.Text, t.CreatedAt)
	if err != nil {
		return nil, err
	}
	_, _ = s.db.Exec(`UPDATE sessions SET updated_at=? WHERE id=?`, t.CreatedAt, sessionID)
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

var sanitizePatterns = []*regexp.Regexp{
	regexp.MustCompile(`(?i)(api[_-]?key\s*[:=]\s*)['"]?[^\s'"]+`),
	regexp.MustCompile(`(?i)(secret\s*[:=]\s*)['"]?[^\s'"]+`),
	regexp.MustCompile(`sk-[A-Za-z0-9-_]{8,}`),
	regexp.MustCompile(`AKIA[0-9A-Z]{16}`),
	regexp.MustCompile(`(?i)bearer\s+[A-Za-z0-9\-._~+/]+=*`),
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
