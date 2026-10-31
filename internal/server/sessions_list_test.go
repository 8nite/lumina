package server

import "testing"

// Memory fallback (no Store) must match the SQLite path (ORDER BY
// updated_at DESC) so TUI /continue picks the latest previous session.
func TestListSessionsMemoryOrdersByUpdatedAt(t *testing.T) {
	s := New(Options{})
	s.sessions["old"] = &Session{ID: "old", WorkspaceDir: "/ws", UpdatedAt: "2026-01-01T00:00:00Z"}
	s.sessions["new"] = &Session{ID: "new", WorkspaceDir: "/ws", UpdatedAt: "2026-06-01T00:00:00Z"}
	s.sessions["other"] = &Session{ID: "other", WorkspaceDir: "/other", UpdatedAt: "2026-07-01T00:00:00Z"}

	got := s.listSessions("/ws")
	if len(got) != 2 || got[0].ID != "new" || got[1].ID != "old" {
		ids := []string{}
		for _, x := range got {
			ids = append(ids, x.ID)
		}
		t.Fatalf("workspace order wrong: %v", ids)
	}
	all := s.listSessions("")
	if len(all) != 3 || all[0].ID != "other" {
		ids := []string{}
		for _, x := range all {
			ids = append(ids, x.ID)
		}
		t.Fatalf("global order wrong: %v", ids)
	}
}
