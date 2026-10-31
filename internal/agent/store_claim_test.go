package agent

import (
	"path/filepath"
	"testing"
	"time"
)

// ClaimTurn is the cross-process single-flight slot: exactly one daemon on
// a shared DB file may hold a session's turn.
func TestClaimTurnSingleFlight(t *testing.T) {
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	sess, err := st.CreateSession("/ws", "build", "ask", "", "")
	if err != nil {
		t.Fatal(err)
	}
	if owner, since := st.TurnOwner(sess.ID); owner != "" || since != 0 {
		t.Fatalf("fresh session must be idle, got %q/%d", owner, since)
	}
	if !st.ClaimTurn(sess.ID, "daemon-a", time.Minute) {
		t.Fatal("first claim must succeed")
	}
	if owner, _ := st.TurnOwner(sess.ID); owner != "daemon-a" {
		t.Fatalf("owner=%q", owner)
	}
	if st.ClaimTurn(sess.ID, "daemon-b", time.Minute) {
		t.Fatal("second live claim must fail")
	}
	// Same owner re-claims (chain-safe: queued chains never re-claim, but
	// a retry must not self-deadlock).
	if !st.ClaimTurn(sess.ID, "daemon-a", time.Minute) {
		t.Fatal("owner re-claim must succeed")
	}
	// Release is owner-guarded: a stranger's release must not free the slot.
	st.ReleaseTurn(sess.ID, "daemon-b")
	if owner, _ := st.TurnOwner(sess.ID); owner != "daemon-a" {
		t.Fatalf("foreign release stole the slot: %q", owner)
	}
	st.ReleaseTurn(sess.ID, "daemon-a")
	if owner, since := st.TurnOwner(sess.ID); owner != "" || since != 0 {
		t.Fatalf("slot must be idle after release, got %q/%d", owner, since)
	}
	if !st.ClaimTurn(sess.ID, "daemon-b", time.Minute) {
		t.Fatal("claim after release must succeed")
	}
}

// A claim older than maxAge is dead (turn ctx caps at 10min): a surviving
// daemon may take the slot over instead of 409ing forever on a crashed
// owner's row.
func TestClaimTurnStaleTakeover(t *testing.T) {
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	sess, err := st.CreateSession("/ws", "build", "ask", "", "")
	if err != nil {
		t.Fatal(err)
	}
	if !st.ClaimTurn(sess.ID, "dead-daemon", time.Nanosecond) {
		t.Fatal("first claim must succeed")
	}
	time.Sleep(5 * time.Millisecond)
	if !st.ClaimTurn(sess.ID, "live-daemon", time.Nanosecond) {
		t.Fatal("stale claim must be take-over-able")
	}
	if owner, _ := st.TurnOwner(sess.ID); owner != "live-daemon" {
		t.Fatalf("owner=%q", owner)
	}
}
