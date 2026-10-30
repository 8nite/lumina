package sdk

import (
	"context"
	"net/http/httptest"
	"sync"
	"testing"
	"time"

	"github.com/8nite/lumina/internal/server"
)

// TestSubscribeOrderedEvents proves the typed SDK receives daemon broadcasts
// in order (TP-08.4).
func TestSubscribeOrderedEvents(t *testing.T) {
	srv := server.New(server.Options{})
	h := httptest.NewServer(srv.Handler())
	defer h.Close()
	c := New(h.URL, "")

	var mu sync.Mutex
	var got []Event
	done := make(chan struct{})
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go func() {
		_ = c.Subscribe(ctx, func(e Event) {
			mu.Lock()
			got = append(got, e)
			if len(got) == 4 {
				close(done)
			}
			mu.Unlock()
		})
	}()
	time.Sleep(300 * time.Millisecond) // let the subscriber attach
	srv.Broker().Publish(server.Event{Type: "session.created", SessionID: "s1"})
	srv.Broker().Publish(server.Event{Type: "session.updated", SessionID: "s1"})
	srv.Broker().Publish(server.Event{Type: "turn.started", SessionID: "s1"})
	srv.Broker().Publish(server.Event{Type: "turn.completed", SessionID: "s1"})

	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("timed out waiting for events")
	}
	mu.Lock()
	defer mu.Unlock()
	want := []string{"session.created", "session.updated", "turn.started", "turn.completed"}
	for i, w := range want {
		if got[i].Type != w || got[i].SessionID != "s1" {
			t.Fatalf("event %d = %+v, want %s/s1", i, got[i], w)
		}
	}
}
