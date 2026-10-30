// Package server implements the Lumina local daemon HTTP API: OpenAPI 3.1
// REST + SSE event stream + PTY WebSocket on 127.0.0.1:7432 by default.
//
// Sessions persist in SQLite (internal/agent.Store) when attached, so turns
// survive daemon restarts and CLI-created sessions appear in every client.
package server

import (
	"context"
	"crypto/sha1"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/8nite/lumina/internal/agent"
	"github.com/8nite/lumina/internal/kb"
	"github.com/8nite/lumina/internal/llm"
	"github.com/google/uuid"
)

// Session is the API view of a persistent workspace session.
type Session struct {
	ID             string `json:"id"`
	WorkspaceDir   string `json:"workspace_dir"`
	Agent          string `json:"agent"`
	PermissionMode string `json:"permission_mode"`
	Model          string `json:"model,omitempty"`
	Title          string `json:"title,omitempty"`
	CreatedAt      string `json:"created_at"`
	UpdatedAt      string `json:"updated_at"`
}

// Event is one SSE message on /api/v1/events.
type Event struct {
	Type      string      `json:"type"`
	SessionID string      `json:"session_id,omitempty"`
	Data      interface{} `json:"data,omitempty"`
}

// Broker multiplexes SSE events to attached clients.
type Broker struct {
	mu   sync.Mutex
	subs map[chan Event]struct{}
}

// NewBroker creates an event broker.
func NewBroker() *Broker { return &Broker{subs: map[chan Event]struct{}{}} }

// Subscribe returns a channel receiving events plus an unsubscribe func.
func (b *Broker) Subscribe(buf int) (<-chan Event, func()) {
	ch := make(chan Event, buf)
	b.mu.Lock()
	b.subs[ch] = struct{}{}
	b.mu.Unlock()
	return ch, func() {
		b.mu.Lock()
		delete(b.subs, ch)
		close(ch)
		b.mu.Unlock()
	}
}

// Publish broadcasts to all subscribers (non-blocking).
func (b *Broker) Publish(e Event) {
	b.mu.Lock()
	defer b.mu.Unlock()
	for ch := range b.subs {
		select {
		case ch <- e:
		default:
		}
	}
}

// Options configures the daemon.
type Options struct {
	AuthToken string // empty = no auth required (loopback trust)
}

// Server is the Lumina daemon.
type Server struct {
	mux      *http.ServeMux
	broker   *Broker
	opts     Options
	mu       sync.Mutex
	sessions map[string]*Session
	// running tracks sessions with an in-flight turn (steer/queue gate).
	running  map[string]bool
	permReqs map[string]map[string]string
	// Blocking permission broker (opencode-style permission.asked flow,
	// reimplemented): Runner.AskPermission publishes permission.asked and
	// waits here until the TUI answers POST /api/v1/permissions/{reqId}.
	permMu      sync.Mutex
	permPending map[string]chan string
	permMeta    map[string]map[string]string
	// Exec runs one assistant turn (wired by cmd/serve to the agent
	// Runner). Nil keeps the route as accept-and-broadcast (tests/embeds).
	Exec func(ctx context.Context, sessionID string, in agent.TurnInput) (string, error)
	// turnStats stashes finished-turn summaries (NoteTurnEnd) for the
	// in-flight prompt goroutine, which merges them into turn.completed.
	turnStats sync.Map // sessionID → agent.TurnStats
	// Store persists sessions + turns (restart-safe). Nil keeps the
	// in-memory map (unit tests, embeds).
	Store *agent.Store
	// ProviderNames advertises BYO-LLM providers (wired from config).
	ProviderNames []string
	// ProviderProtocols maps provider name -> protocol (wired from config).
	// Used to gate Copilot-only flows (/connect, device auth, /models).
	ProviderProtocols map[string]string
	// RouterState tracks live fast/executor/improver models.
	RouterState map[string]string
	// OnRouterUpdate persists a tier model swap (model-persist).
	OnRouterUpdate func(role, model string)
	// OnAuthRefresh installs a freshly device-authed key into the live
	// router so /model populates without a daemon restart.
	OnAuthRefresh func(provider, githubToken string)
	// Redo checkpoints per session for /unrevert.
	Redo map[string]string
	// Inbox hooks (wired to the agent Runner in cmd/serve): steer/queue
	// while a turn is running. Nil preserves the legacy behavior of
	// running every prompt immediately (tests/embeds).
	SteerPrompt func(sessionID, text string)
	QueuePrompt func(sessionID string, in agent.TurnInput) int
	TakeQueued  func(sessionID string) (agent.TurnInput, bool)
	// ListProviderModels lists live models for any provider implementing
	// llm.ModelLister (wired by cmd/serve from config + router).
	ListProviderModels func(ctx context.Context, provider string) ([]llm.ModelMeta, error)
	// CopilotTokenURL resolves the exchange endpoint for device auth.
	CopilotTokenURL func(provider string) string
}

// New creates a daemon with all /api/v1 routes registered.
func New(o Options) *Server {
	s := &Server{
		mux:         http.NewServeMux(),
		broker:      NewBroker(),
		opts:        o,
		sessions:    map[string]*Session{},
		running:     map[string]bool{},
		permReqs:    map[string]map[string]string{},
		permPending: map[string]chan string{},
		permMeta:    map[string]map[string]string{},
		RouterState: map[string]string{},
		Redo:        map[string]string{},
	}
	s.routes()
	return s
}

// MountSPA serves the embedded web workspace at /. API routes are more
// specific and keep precedence under Go 1.22 ServeMux longest-match rules.
func (s *Server) MountSPA(h http.Handler) {
	s.mux.Handle("/", h)
}

// Broker exposes the event broker for in-process subscribers.
func (s *Server) Broker() *Broker { return s.broker }

// NoteTurnEnd stashes one finished turn's summary (opencode v2 turn
// summary: model, duration, tokens) for the in-flight prompt goroutine,
// which merges it into the turn.completed event.
func (s *Server) NoteTurnEnd(sessionID string, st agent.TurnStats) {
	s.turnStats.Store(sessionID, st)
}

// turnEndStats takes and clears the stashed summary for a session.
func (s *Server) turnEndStats(sessionID string) (agent.TurnStats, bool) {
	v, ok := s.turnStats.LoadAndDelete(sessionID)
	if !ok {
		return agent.TurnStats{}, false
	}
	st, ok := v.(agent.TurnStats)
	return st, ok
}

// GetSession returns one session by id (memory first, store fallback).
func (s *Server) GetSession(id string) (*Session, bool) {
	s.mu.Lock()
	sess, ok := s.sessions[id]
	s.mu.Unlock()
	if ok {
		return sess, true
	}
	if s.Store != nil {
		if st, err := s.Store.GetSession(id); err == nil {
			api := toAPI(st)
			s.mu.Lock()
			s.sessions[id] = api
			s.mu.Unlock()
			return api, true
		}
	}
	return nil, false
}

func toAPI(st *agent.Session) *Session {
	return &Session{
		ID: st.ID, WorkspaceDir: st.WorkspaceDir, Agent: st.Agent,
		PermissionMode: st.PermissionMode, Model: st.Model, Title: st.Title,
		CreatedAt: st.CreatedAt, UpdatedAt: st.UpdatedAt,
	}
}

func toStore(s *Session, parentID string) *agent.Session {
	return &agent.Session{
		ID: s.ID, ParentID: parentID, WorkspaceDir: s.WorkspaceDir,
		Agent: s.Agent, PermissionMode: s.PermissionMode, Model: s.Model,
		Title: s.Title, CreatedAt: s.CreatedAt, UpdatedAt: s.UpdatedAt,
	}
}

// Handler returns the HTTP handler with CSP + CORS + auth applied.
func (s *Server) Handler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Headers", "Authorization, Content-Type")
		w.Header().Set("Access-Control-Allow-Methods", "GET, POST, PATCH, PUT, DELETE, OPTIONS")
		w.Header().Set("Content-Security-Policy",
			"default-src 'self'; connect-src 'self' ws: wss:; style-src 'self' 'unsafe-inline'; font-src 'self'; img-src 'self' data:")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		if s.opts.AuthToken != "" && !strings.HasPrefix(r.URL.Path, "/openapi.json") {
			if r.Header.Get("Authorization") != "Bearer "+s.opts.AuthToken {
				writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "unauthorized"})
				return
			}
		}
		s.mux.ServeHTTP(w, r)
	})
}

func writeJSON(w http.ResponseWriter, code int, v interface{}) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(v)
}

func nowStr() string { return time.Now().UTC().Format(time.RFC3339) }

func (s *Server) routes() {
	m := s.mux
	m.HandleFunc("/openapi.json", s.handleOpenAPI)
	m.HandleFunc("/api/v1/events", s.handleEvents)
	m.HandleFunc("/api/v1/sessions", s.handleSessions)
	m.HandleFunc("/api/v1/sessions/", s.handleSessionSub)
	m.HandleFunc("/api/v1/permissions/", s.handlePermissions)
	m.HandleFunc("/api/v1/files", s.handleFiles)
	m.HandleFunc("/api/v1/git/diff", s.handleGitDiff)
	m.HandleFunc("/api/v1/git/branch", s.handleGitBranch)
	m.HandleFunc("/api/v1/pty", s.handlePTY)
	m.HandleFunc("/api/v1/providers", s.handleProviders)
	m.HandleFunc("/api/v1/router", s.handleRouter)
	m.HandleFunc("/api/v1/models", s.handleModels)
	m.HandleFunc("/api/v1/auth/device/start", s.handleDeviceStart)
	m.HandleFunc("/api/v1/auth/device/poll", s.handleDevicePoll)
	m.HandleFunc("/api/v1/kb/", s.handleKB)
	m.HandleFunc("/api/v1/kb/query", s.handleKBQuery)
	m.HandleFunc("/api/v1/harness", s.handleHarness)
	m.HandleFunc("/api/v1/harness/evolve", s.handleEvolve)
}

// OpenAPI 3.1 document — single source of truth for the web TS SDK.
func (s *Server) handleOpenAPI(w http.ResponseWriter, r *http.Request) {
	doc := map[string]interface{}{
		"openapi": "3.1.0",
		"info":    map[string]string{"title": "Lumina Daemon API", "version": "0.1.0"},
		"servers": []map[string]string{{"url": "http://127.0.0.1:7432"}},
		"paths": map[string]interface{}{
			"/api/v1/events":                 map[string]string{"get": "SSE: turn.started/progress/delta/tool/completed/failed, permission.asked/resolved, session.*, config.updated"},
			"/api/v1/sessions":               map[string]string{"get": "list (?directory=, ?latest=1)", "post": "create"},
			"/api/v1/sessions/{id}":          map[string]string{"get": "fetch", "patch": "update agent/permission/model/title"},
			"/api/v1/sessions/{id}/fork":     map[string]string{"post": "fork at turn"},
			"/api/v1/sessions/{id}/export":   map[string]string{"get": "transcript export (?sanitize=1)"},
			"/api/v1/sessions/{id}/prompt":   map[string]string{"post": "submit turn"},
			"/api/v1/sessions/{id}/revert":   map[string]string{"post": "shadow-git undo"},
			"/api/v1/sessions/{id}/unrevert": map[string]string{"post": "shadow-git redo"},
			"/api/v1/permissions/{reqId}":    map[string]string{"post": "allow_once/session/always or deny"},
			"/api/v1/files":                  map[string]string{"get": "file tree (?directory=)"},
			"/api/v1/git/diff":               map[string]string{"get": "working-tree diff (?directory=)"},
			"/api/v1/git/branch":             map[string]string{"get": "branch + last commit (?directory=)"},
			"/api/v1/pty":                    map[string]string{"get": "interactive shell websocket"},
			"/api/v1/providers":              map[string]string{"get": "list providers"},
			"/api/v1/router":                 map[string]string{"get": "router state", "put": "switch fast/executor/improver"},
			"/api/v1/models":                 map[string]string{"get": "live provider models (?provider=)"},
			"/api/v1/auth/device/start":      map[string]string{"post": "begin device web-auth"},
			"/api/v1/auth/device/poll":       map[string]string{"post": "complete device web-auth"},
			"/api/v1/kb/query":               map[string]string{"post": "graphify query"},
			"/api/v1/harness":                map[string]string{"get": "inspect 8 components (?directory=)"},
			"/api/v1/harness/evolve":         map[string]string{"post": "propose-probe-compose run"},
		},
	}
	writeJSON(w, 200, doc)
}

func (s *Server) handleEvents(w http.ResponseWriter, r *http.Request) {
	fl, ok := w.(http.Flusher)
	if !ok {
		writeJSON(w, 500, map[string]string{"error": "sse unsupported"})
		return
	}
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	ch, unsub := s.broker.Subscribe(64)
	defer unsub()
	fmt.Fprintf(w, ": connected\n\n")
	fl.Flush()
	keep := time.NewTicker(20 * time.Second)
	defer keep.Stop()
	for {
		select {
		case <-r.Context().Done():
			return
		case e := <-ch:
			b, _ := json.Marshal(e)
			fmt.Fprintf(w, "event: %s\ndata: %s\n\n", e.Type, b)
			fl.Flush()
		case <-keep.C:
			fmt.Fprintf(w, ": keepalive\n\n")
			fl.Flush()
		}
	}
}

// listSessions reads the store when attached (restart-safe), else memory.
func (s *Server) listSessions(directory string) []*Session {
	if s.Store != nil {
		list, err := s.Store.ListSessions(directory)
		if err != nil {
			return []*Session{}
		}
		out := make([]*Session, 0, len(list))
		for _, st := range list {
			out = append(out, toAPI(st))
		}
		return out
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []*Session
	for _, sess := range s.sessions {
		if directory != "" && sess.WorkspaceDir != directory {
			continue
		}
		out = append(out, sess)
	}
	return out
}

func (s *Server) handleSessions(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet:
		dir := r.URL.Query().Get("directory")
		latest := r.URL.Query().Get("latest") == "1"
		out := s.listSessions(dir)
		if latest {
			if len(out) == 0 {
				writeJSON(w, 404, map[string]string{"error": "no sessions"})
				return
			}
			top := out[0]
			for _, x := range out[1:] {
				if x.UpdatedAt > top.UpdatedAt {
					top = x
				}
			}
			writeJSON(w, 200, top)
			return
		}
		if out == nil {
			out = []*Session{}
		}
		writeJSON(w, 200, out)
	case http.MethodPost:
		var req struct {
			WorkspaceDir   string `json:"workspace_dir"`
			Agent          string `json:"agent"`
			PermissionMode string `json:"permission_mode"`
			Model          string `json:"model"`
			Title          string `json:"title"`
		}
		_ = json.NewDecoder(r.Body).Decode(&req)
		agentMode := req.Agent
		if agentMode == "" {
			agentMode = "build"
		}
		pm := req.PermissionMode
		if pm == "" {
			pm = "ask"
		}
		sess := &Session{
			ID: uuid.NewString()[:8], WorkspaceDir: req.WorkspaceDir,
			Agent: agentMode, PermissionMode: pm, Model: req.Model,
			Title: req.Title, CreatedAt: nowStr(), UpdatedAt: nowStr(),
		}
		if s.Store != nil {
			if err := s.Store.InsertSession(toStore(sess, "")); err != nil {
				writeJSON(w, 500, map[string]string{"error": err.Error()})
				return
			}
		}
		s.mu.Lock()
		s.sessions[sess.ID] = sess
		s.mu.Unlock()
		s.broker.Publish(Event{Type: "session.created", SessionID: sess.ID, Data: sess})
		writeJSON(w, 201, sess)
	default:
		writeJSON(w, 405, map[string]string{"error": "method not allowed"})
	}
}

// handleSessionSub routes /api/v1/sessions/{id}[/{action}].
func (s *Server) handleSessionSub(w http.ResponseWriter, r *http.Request) {
	rest := strings.TrimPrefix(r.URL.Path, "/api/v1/sessions/")
	parts := strings.SplitN(rest, "/", 3)
	id, action := parts[0], ""
	if len(parts) > 1 {
		action = parts[1]
	}
	sess, ok := s.GetSession(id)
	if !ok {
		writeJSON(w, 404, map[string]string{"error": "session not found"})
		return
	}
	switch action {
	case "":
		switch r.Method {
		case http.MethodPatch:
			var req struct {
				Agent          *string `json:"agent"`
				PermissionMode *string `json:"permission_mode"`
				Model          *string `json:"model"`
				Title          *string `json:"title"`
			}
			_ = json.NewDecoder(r.Body).Decode(&req)
			if req.Agent != nil {
				sess.Agent = *req.Agent
			}
			if req.PermissionMode != nil {
				sess.PermissionMode = *req.PermissionMode
			}
			if req.Model != nil {
				sess.Model = *req.Model
			}
			if req.Title != nil {
				sess.Title = *req.Title
			}
			sess.UpdatedAt = nowStr()
			if s.Store != nil {
				_, _ = s.Store.UpdateSession(id, &sess.Agent, &sess.PermissionMode, &sess.Model, &sess.Title)
			}
			s.broker.Publish(Event{Type: "session.updated", SessionID: id, Data: sess})
			writeJSON(w, 200, sess)
		case http.MethodGet:
			writeJSON(w, 200, sess)
		default:
			writeJSON(w, 405, map[string]string{"error": "method not allowed"})
		}
	case "fork":
		if r.Method != http.MethodPost {
			writeJSON(w, 405, map[string]string{"error": "method not allowed"})
			return
		}
		fork := &Session{
			ID: uuid.NewString()[:8], WorkspaceDir: sess.WorkspaceDir,
			Agent: sess.Agent, PermissionMode: sess.PermissionMode,
			Model: sess.Model, Title: sess.Title + " (fork)",
			CreatedAt: nowStr(), UpdatedAt: nowStr(),
		}
		if s.Store != nil {
			f, err := s.Store.ForkSession(id, "")
			if err != nil {
				writeJSON(w, 500, map[string]string{"error": err.Error()})
				return
			}
			fork = toAPI(f)
		}
		s.mu.Lock()
		s.sessions[fork.ID] = fork
		s.mu.Unlock()
		s.broker.Publish(Event{Type: "session.forked", SessionID: fork.ID})
		writeJSON(w, 201, fork)
	case "export":
		sanitize := r.URL.Query().Get("sanitize") == "1"
		turns := []map[string]string{}
		if s.Store != nil {
			if list, err := s.Store.ListTurns(id); err == nil {
				for _, t := range list {
					text := t.Text
					if sanitize {
						text = agent.SanitizeExport(text)
					}
					turns = append(turns, map[string]string{
						"id": t.ID, "role": t.Role, "text": text, "created_at": t.CreatedAt,
					})
				}
			}
		}
		writeJSON(w, 200, map[string]interface{}{"id": id, "sanitized": sanitize, "transcript": turns})
	case "prompt":
		if r.Method != http.MethodPost {
			writeJSON(w, 405, map[string]string{"error": "method not allowed"})
			return
		}
		var req struct {
			Text   string `json:"text"`
			Effort string `json:"effort"`
			// Delivery selects inbox behavior while a turn is running
			// (opencode steer/queue parity): steer (default) joins the
			// active turn at the next round boundary; queue chains after
			// it completes. Ignored when the session is idle.
			Delivery string `json:"delivery"`
			Images   []struct {
				Name      string `json:"name"`
				MediaType string `json:"media_type"`
				Data      string `json:"data"`
			} `json:"images"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			writeJSON(w, 400, map[string]string{"error": "bad body"})
			return
		}
		const maxImageBytes = 8 << 20
		var images []llm.ImageAttachment
		for _, im := range req.Images {
			if len(im.Data) > maxImageBytes {
				writeJSON(w, 413, map[string]string{"error": "image too large (8MB cap): " + im.Name})
				return
			}
			images = append(images, llm.ImageAttachment{Name: im.Name, MediaType: im.MediaType, Data: im.Data})
		}
		in := agent.TurnInput{Text: req.Text, Images: images, Effort: req.Effort}
		delivery := req.Delivery
		if delivery == "" {
			delivery = "steer"
		}
		// While a turn is in flight, steer or queue instead of starting
		// a second concurrent loop on the same session. The gate only
		// applies when a runner is wired (Exec != nil); embeds without
		// one keep the legacy run-immediately behavior.
		s.mu.Lock()
		busy := s.Exec != nil && s.running[id]
		if s.Exec != nil && !busy {
			s.running[id] = true
		}
		s.mu.Unlock()
		if busy {
			switch {
			case delivery == "queue" && s.QueuePrompt != nil:
				pos := s.QueuePrompt(id, in)
				s.broker.Publish(Event{Type: "turn.progress", SessionID: id, Data: map[string]string{"stage": "queued #" + fmt.Sprintf("%d", pos)}})
				writeJSON(w, 202, map[string]interface{}{"status": "queued", "session_id": id, "position": pos})
			case s.SteerPrompt != nil:
				s.SteerPrompt(id, req.Text)
				preview := req.Text
				if len([]rune(preview)) > 80 {
					preview = string([]rune(preview)[:80]) + "…"
				}
				s.broker.Publish(Event{Type: "turn.progress", SessionID: id, Data: map[string]string{"stage": "steered: " + preview}})
				writeJSON(w, 202, map[string]string{"status": "steered", "session_id": id})
			default:
				// Legacy path (no runner wired): run immediately, as before.
				go s.runTurn(id, in)
				writeJSON(w, 202, map[string]string{"status": "accepted", "session_id": id})
			}
			return
		}
		// Idle path: the flag was already set above; runTurns clears it.
		sess.UpdatedAt = nowStr()
		s.broker.Publish(Event{Type: "turn.started", SessionID: id, Data: map[string]string{"text": req.Text}})
		if s.Exec != nil {
			go s.runTurns(id, in)
		} else {
			s.mu.Lock()
			delete(s.running, id)
			s.mu.Unlock()
		}
		writeJSON(w, 202, map[string]string{"status": "accepted", "session_id": id})
	case "revert", "unrevert":
		if r.Method != http.MethodPost {
			writeJSON(w, 405, map[string]string{"error": "method not allowed"})
			return
		}
		if err := s.shadowRevert(sess, action == "unrevert"); err != nil {
			writeJSON(w, 409, map[string]string{"error": err.Error()})
			return
		}
		typ := "session.reverted"
		if action == "unrevert" {
			typ = "session.unreverted"
		}
		s.broker.Publish(Event{Type: typ, SessionID: id})
		writeJSON(w, 200, map[string]string{"status": "ok"})
	default:
		writeJSON(w, 404, map[string]string{"error": "unknown action"})
	}
}

// runTurn executes one turn and publishes its terminal event.
func (s *Server) runTurn(id string, in agent.TurnInput) {
	if s.Exec == nil {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	out, err := s.Exec(ctx, id, in)
	if err != nil {
		s.broker.Publish(Event{Type: "turn.failed", SessionID: id, Data: map[string]string{"error": err.Error()}})
		return
	}
	data := map[string]interface{}{"text": out}
	if st, ok := s.turnEndStats(id); ok {
		data["model"] = st.Model
		data["duration_ms"] = st.DurationMs
		data["input_tokens"] = st.InputTokens
		data["output_tokens"] = st.OutputTokens
	}
	s.broker.Publish(Event{Type: "turn.completed", SessionID: id, Data: data})
}

// runTurns executes a turn, then chains queued turns until the inbox
// drains, holding the session's running flag throughout so prompts that
// arrive mid-chain keep steering or queueing instead of racing the loop.
func (s *Server) runTurns(id string, in agent.TurnInput) {
	defer func() {
		s.mu.Lock()
		delete(s.running, id)
		s.mu.Unlock()
	}()
	for {
		s.runTurn(id, in)
		if s.TakeQueued == nil {
			return
		}
		next, ok := s.TakeQueued(id)
		if !ok {
			return
		}
		s.broker.Publish(Event{Type: "turn.started", SessionID: id, Data: map[string]string{"text": next.Text}})
		in = next
	}
}

// shadowRevert implements /undo (/redo via the saved redo checkpoint).
func (s *Server) shadowRevert(sess *Session, redo bool) error {
	if sess.WorkspaceDir == "" {
		return fmt.Errorf("session has no workspace")
	}
	if redo {
		s.mu.Lock()
		cp := s.Redo[sess.ID]
		s.mu.Unlock()
		if cp == "" {
			return fmt.Errorf("nothing to redo")
		}
		return agent.Revert(sess.WorkspaceDir, sess.ID, cp)
	}
	turnID := ""
	if s.Store != nil {
		if turns, err := s.Store.ListTurns(sess.ID); err == nil {
			for i := len(turns) - 1; i >= 0; i-- {
				if turns[i].Role == "assistant" {
					turnID = turns[i].ID
					break
				}
			}
		}
	}
	if turnID == "" {
		return fmt.Errorf("no checkpoint for session")
	}
	// Save redo point before reverting.
	if cp, err := agent.Snapshot(sess.WorkspaceDir, sess.ID, "redo"); err == nil {
		s.mu.Lock()
		s.Redo[sess.ID] = cp.TurnID
		s.mu.Unlock()
	}
	return agent.Revert(sess.WorkspaceDir, sess.ID, turnID)
}

func (s *Server) handlePermissions(w http.ResponseWriter, r *http.Request) {
	reqID := strings.TrimPrefix(r.URL.Path, "/api/v1/permissions/")
	var req struct {
		Decision string `json:"decision"`
	}
	_ = json.NewDecoder(r.Body).Decode(&req)
	s.mu.Lock()
	s.permReqs[reqID] = map[string]string{"decision": req.Decision}
	s.mu.Unlock()
	// Unblock a waiting turn (opencode-style allow_once/session/deny).
	s.permMu.Lock()
	ch := s.permPending[reqID]
	delete(s.permPending, reqID)
	delete(s.permMeta, reqID)
	s.permMu.Unlock()
	if ch != nil {
		select {
		case ch <- req.Decision:
		default:
		}
	}
	s.broker.Publish(Event{Type: "permission.resolved", Data: map[string]string{"request_id": reqID, "decision": req.Decision}})
	writeJSON(w, 200, map[string]string{"status": "ok"})
}

// RequestPermission publishes permission.asked and blocks until the UI
// answers or ctx ends. Lifecycle mirrors sst/opencode's permission flow
// (MIT): ask → allow_once / allow_session / deny. Reimplemented here.
func (s *Server) RequestPermission(ctx context.Context, sessionID, tool, target string) (string, error) {
	reqID := uuid.NewString()[:8]
	ch := make(chan string, 1)
	s.permMu.Lock()
	s.permPending[reqID] = ch
	s.permMeta[reqID] = map[string]string{"session_id": sessionID, "tool": tool, "target": target}
	s.permMu.Unlock()
	s.broker.Publish(Event{Type: "permission.asked", SessionID: sessionID, Data: map[string]string{
		"request_id": reqID, "tool": tool, "target": target,
	}})
	select {
	case d := <-ch:
		if d == "" {
			return "deny", nil
		}
		return d, nil
	case <-ctx.Done():
		s.permMu.Lock()
		delete(s.permPending, reqID)
		delete(s.permMeta, reqID)
		s.permMu.Unlock()
		return "", ctx.Err()
	}
}

// handleFiles returns a bounded workspace file tree (?directory=).
func (s *Server) handleFiles(w http.ResponseWriter, r *http.Request) {
	dir := r.URL.Query().Get("directory")
	if dir == "" {
		dir = "."
	}
	var files []string
	truncated := false
	_ = filepath.Walk(dir, func(p string, info os.FileInfo, err error) error {
		if err != nil || len(files) >= 500 {
			truncated = truncated || len(files) >= 500
			return nil
		}
		if info.IsDir() {
			if p != dir && (strings.Contains(p, "/.git") || strings.Contains(p, "/node_modules")) {
				return filepath.SkipDir
			}
			return nil
		}
		if strings.Contains(p, "/.git/") || strings.Contains(p, "/node_modules/") {
			return nil
		}
		rel, err := filepath.Rel(dir, p)
		if err != nil {
			return nil
		}
		files = append(files, rel)
		return nil
	})
	sort.Strings(files)
	writeJSON(w, 200, map[string]interface{}{"files": files, "truncated": truncated})
}

// handleGitDiff returns the working-tree diff (?directory=, truncated).
func (s *Server) handleGitDiff(w http.ResponseWriter, r *http.Request) {
	dir := r.URL.Query().Get("directory")
	if dir == "" {
		dir = "."
	}
	cmd := exec.Command("git", "-C", dir, "diff", "--stat")
	cmd.Env = os.Environ()
	stat, _ := cmd.CombinedOutput()
	cmd = exec.Command("git", "-C", dir, "diff", "--name-only")
	cmd.Env = os.Environ()
	names, _ := cmd.CombinedOutput()
	var files []string
	for _, l := range strings.Split(strings.TrimSpace(string(names)), "\n") {
		if l = strings.TrimSpace(l); l != "" {
			files = append(files, l)
		}
	}
	if files == nil {
		files = []string{}
	}
	cmd = exec.Command("git", "-C", dir, "diff")
	cmd.Env = os.Environ()
	diff, err := cmd.CombinedOutput()
	if err != nil && len(diff) == 0 {
		writeJSON(w, 200, map[string]string{"diff": "", "note": "not a git repo or git missing"})
		return
	}
	const capBytes = 200 << 10
	out := string(diff)
	truncated := false
	if len(out) > capBytes {
		out, truncated = out[:capBytes], true
	}
	writeJSON(w, 200, map[string]interface{}{"diff": out, "stat": string(stat), "files": files, "truncated": truncated})
}

// handleGitBranch returns branch + last-commit info (?directory=).
func (s *Server) handleGitBranch(w http.ResponseWriter, r *http.Request) {
	dir := r.URL.Query().Get("directory")
	if dir == "" {
		dir = "."
	}
	out := func(args ...string) string {
		cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
		cmd.Env = os.Environ()
		b, err := cmd.Output()
		if err != nil {
			return ""
		}
		return strings.TrimSpace(string(b))
	}
	branch := out("rev-parse", "--abbrev-ref", "HEAD")
	if branch == "" {
		writeJSON(w, 200, map[string]interface{}{"branch": "", "note": "not a git repo"})
		return
	}
	writeJSON(w, 200, map[string]interface{}{
		"branch": branch, "commit": out("rev-parse", "--short", "HEAD"),
		"modified": out("log", "-1", "--format=%cr"),
	})
}

// handlePTY performs a minimal RFC6455 handshake, then runs a text-frame
// command loop backed by the session shell.
func (s *Server) handlePTY(w http.ResponseWriter, r *http.Request) {
	key := r.Header.Get("Sec-WebSocket-Key")
	if key == "" {
		writeJSON(w, 426, map[string]string{"error": "websocket upgrade required"})
		return
	}
	hij, ok := w.(http.Hijacker)
	if !ok {
		writeJSON(w, 500, map[string]string{"error": "hijack unsupported"})
		return
	}
	conn, rw, err := hij.Hijack()
	if err != nil {
		return
	}
	defer conn.Close()
	accept := wsAccept(key)
	resp := "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + accept + "\r\n\r\n"
	if _, err := rw.WriteString(resp); err != nil {
		return
	}
	_ = rw.Flush()
	wsServeLoop(conn)
}

func wsAccept(key string) string {
	h := sha1.New()
	h.Write([]byte(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))
	return base64.StdEncoding.EncodeToString(h.Sum(nil))
}

func (s *Server) handleProviders(w http.ResponseWriter, r *http.Request) {
	names := s.ProviderNames
	if names == nil {
		names = []string{}
	}
	type info struct {
		Name     string `json:"name"`
		Protocol string `json:"protocol"`
	}
	var out []info
	for _, n := range names {
		out = append(out, info{Name: n, Protocol: s.ProviderProtocols[n]})
	}
	if out == nil {
		out = []info{}
	}
	writeJSON(w, 200, map[string]interface{}{"providers": out})
}

func (s *Server) handleRouter(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodGet {
		s.mu.Lock()
		state := map[string]string{}
		for k, v := range s.RouterState {
			state[k] = v
		}
		s.mu.Unlock()
		writeJSON(w, 200, map[string]interface{}{"router": state})
		return
	}
	if r.Method != http.MethodPut {
		writeJSON(w, 405, map[string]string{"error": "method not allowed"})
		return
	}
	var req map[string]string
	_ = json.NewDecoder(r.Body).Decode(&req)
	s.mu.Lock()
	for k, v := range req {
		s.RouterState[k] = v
	}
	onUpdate := s.OnRouterUpdate
	s.mu.Unlock()
	if onUpdate != nil {
		for k, v := range req {
			onUpdate(k, v)
		}
	}
	s.broker.Publish(Event{Type: "config.updated", Data: req})
	writeJSON(w, 200, map[string]string{"status": "ok"})
}

func (s *Server) handleKB(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, 200, map[string]interface{}{"status": "ok"})
}

// handleKBQuery runs constrained-vocab traversal over workspace graphs.
func (s *Server) handleKBQuery(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Query     string `json:"query"`
		Directory string `json:"directory"`
		Budget    int    `json:"budget"`
	}
	_ = json.NewDecoder(r.Body).Decode(&req)
	if req.Budget <= 0 {
		req.Budget = 4000
	}
	ws := req.Directory
	if ws == "" {
		ws = "."
	}
	var g *kb.Graph
	var gpath string
	for _, p := range []string{
		filepath.Join(ws, "memory", "graphify-out", "graph.json"),
		filepath.Join(ws, "graphify-out", "graph.json"),
	} {
		if gg, err := kb.LoadGraph(p); err == nil {
			g, gpath = gg, p
			break
		}
	}
	if g == nil {
		writeJSON(w, 200, map[string]interface{}{"results": []interface{}{}, "note": "no graph.json (run graphify first)"})
		return
	}
	toks := g.ExpandVocab(req.Query, 12)
	type hit struct {
		Token     string   `json:"token"`
		Neighbors []string `json:"neighbors"`
	}
	var results []hit
	for _, t := range toks {
		results = append(results, hit{Token: t, Neighbors: g.BFS(t, 8)})
	}
	if results == nil {
		results = []hit{}
	}
	writeJSON(w, 200, map[string]interface{}{"graph": gpath, "vocab": toks, "results": results})
}

func (s *Server) handleHarness(w http.ResponseWriter, r *http.Request) {
	ws := r.URL.Query().Get("directory")
	if ws == "" {
		ws = "."
	}
	type comp struct {
		Name   string `json:"name"`
		Exists bool   `json:"exists"`
	}
	names := []string{"system_prompt.md", "guardrails.yaml", "memory", "tool_guidance.md", "subagents", "skills", "scripts", "plugins"}
	var out []comp
	for _, n := range names {
		_, err := os.Stat(filepath.Join(ws, ".lumina", "harness", n))
		out = append(out, comp{Name: strings.TrimSuffix(n, ".md"), Exists: err == nil})
	}
	writeJSON(w, 200, map[string]interface{}{"components": out})
}

// handleModels lists live models for a provider (copilot gateway /models).
func (s *Server) handleModels(w http.ResponseWriter, r *http.Request) {
	provider := r.URL.Query().Get("provider")
	if provider == "" {
		writeJSON(w, 400, map[string]string{"error": "missing ?provider="})
		return
	}
	if s.ListProviderModels == nil {
		writeJSON(w, 501, map[string]string{"error": "model listing not wired for " + provider})
		return
	}
	models, err := s.ListProviderModels(r.Context(), provider)
	if err != nil {
		writeJSON(w, 502, map[string]string{"error": err.Error()})
		return
	}
	writeJSON(w, 200, map[string]interface{}{"provider": provider, "models": models})
}

// copilotOnly rejects device auth for providers that are known to use a
// different protocol. Unknown providers (no wiring) are allowed through so
// tests and custom setups keep working.
func (s *Server) copilotOnly(provider string) bool {
	if len(s.ProviderProtocols) == 0 {
		return true
	}
	p, ok := s.ProviderProtocols[provider]
	return !ok || p == "" || p == "copilot"
}

// handleDeviceStart begins GitHub device web-auth for a provider.
func (s *Server) handleDeviceStart(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeJSON(w, 405, map[string]string{"error": "method not allowed"})
		return
	}
	var req struct {
		Provider  string `json:"provider"`
		GitHubURL string `json:"github_url"`
	}
	_ = json.NewDecoder(r.Body).Decode(&req)
	if req.Provider == "" {
		writeJSON(w, 400, map[string]string{"error": "missing provider"})
		return
	}
	if !s.copilotOnly(req.Provider) {
		writeJSON(w, 400, map[string]string{"error": "device auth is only for copilot providers"})
		return
	}
	tokenURL := llm.TokenURLForHost(req.GitHubURL)
	if req.GitHubURL == "" && s.CopilotTokenURL != nil {
		if t := s.CopilotTokenURL(req.Provider); t != "" {
			tokenURL = t
		}
	}
	dc, err := llm.StartDeviceFlow(r.Context(), tokenURL, "")
	if err != nil {
		writeJSON(w, 502, map[string]string{"error": err.Error()})
		return
	}
	writeJSON(w, 200, map[string]interface{}{
		"provider": req.Provider, "device_code": dc.DeviceCode,
		"user_code": dc.UserCode, "verification_uri": dc.VerificationURI,
		"expires_in": dc.ExpiresIn, "interval": dc.Interval,
	})
}

// handleDevicePoll completes web-auth: on approval the daemon stores the
// GitHub token (0600) and never returns it.
func (s *Server) handleDevicePoll(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeJSON(w, 405, map[string]string{"error": "method not allowed"})
		return
	}
	var req struct {
		Provider   string `json:"provider"`
		DeviceCode string `json:"device_code"`
		GitHubURL  string `json:"github_url"`
	}
	_ = json.NewDecoder(r.Body).Decode(&req)
	if req.Provider == "" || req.DeviceCode == "" {
		writeJSON(w, 400, map[string]string{"error": "missing provider/device_code"})
		return
	}
	if !s.copilotOnly(req.Provider) {
		writeJSON(w, 400, map[string]string{"error": "device auth is only for copilot providers"})
		return
	}
	tokenURL := llm.TokenURLForHost(req.GitHubURL)
	if req.GitHubURL == "" {
		if stored := llm.StoredTokenURL(req.Provider); stored != "" {
			tokenURL = stored
		} else if s.CopilotTokenURL != nil {
			if t := s.CopilotTokenURL(req.Provider); t != "" {
				tokenURL = t
			}
		}
	}
	tok, err := llm.PollDeviceFlow(r.Context(), tokenURL, req.DeviceCode)
	if err != nil {
		if err.Error() == "pending" {
			writeJSON(w, 200, map[string]string{"status": "pending"})
			return
		}
		writeJSON(w, 200, map[string]string{"status": "error", "error": err.Error()})
		return
	}
	base := llm.CopilotBaseForHost(req.GitHubURL)
	turl := ""
	if req.GitHubURL != "" {
		turl = tokenURL
	}
	if err := llm.WriteProviderAuth(req.Provider, tok, base, turl); err != nil {
		writeJSON(w, 500, map[string]string{"error": "token store failed"})
		return
	}
	// Install the fresh token into the live router (in-process only, never
	// logged) so /model and chat use it immediately, even when the daemon
	// started with no usable key (e.g. `gh` logged out).
	if s.OnAuthRefresh != nil {
		s.OnAuthRefresh(req.Provider, tok)
	}
	s.broker.Publish(Event{Type: "auth.connected", SessionID: "", Data: map[string]string{"provider": req.Provider}})
	writeJSON(w, 200, map[string]string{"status": "authorized"})
}

func (s *Server) handleEvolve(w http.ResponseWriter, r *http.Request) {
	s.broker.Publish(Event{Type: "harness.evolve.started"})
	writeJSON(w, 202, map[string]string{"status": "accepted"})
}
