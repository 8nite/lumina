package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/8nite/lumina/internal/tui"
	"github.com/8nite/lumina/pkg/sdk"
	tea "github.com/charmbracelet/bubbletea"
)

// runTUIClient resolves the session (-c/-s/new), opens the SSE stream, and
// starts the Bubble Tea program against the daemon.
func runTUIClient(c *sdk.Client, f tuiFlags) error {
	ctx := context.Background()
	ws := cwd()
	var sess *sdk.Session
	var err error
	switch {
	case f.sessionID != "":
		slist, err := c.ListSessions(ctx, "")
		if err != nil {
			return err
		}
		for _, s := range slist {
			if s.ID == f.sessionID {
				sess = s
			}
		}
		if sess == nil {
			return fmt.Errorf("session %s not found on daemon", f.sessionID)
		}
	case f.cont:
		sess, err = c.LatestSession(ctx, ws)
		if err != nil {
			sess, err = c.CreateSession(ctx, ws, f.agent, f.perm)
			if err != nil {
				return err
			}
		}
	default:
		sess, err = c.CreateSession(ctx, ws, f.agent, f.perm)
		if err != nil {
			return err
		}
	}
	m := tui.New(c, sess)
	p := tea.NewProgram(m, tea.WithAltScreen())
	// Fan SSE events into the program. Reconnect with backoff so a dropped
	// stream resumes instead of leaving the timeline stuck at working…
	// (a turn still running server-side keeps Working until its real
	// terminal event arrives, so disconnects stay silent by design).
	// After every (re)connect the client requests a resync: transcript +
	// running flag + pending prompts reconcile anything missed while the
	// stream was down (same reload recovery as the web UI).
	done := make(chan struct{})
	go func() {
		delay := time.Second
		for {
			_ = c.Subscribe(context.Background(), func(e sdk.Event) {
				delay = time.Second
				p.Send(tui.EventMsg{Event: e})
			})
			select {
			case <-done:
				return
			default:
			}
			// The stream dropped (or never connected): reconcile anything
			// missed while down. Init() already covers startup.
			p.Send(tui.RequestResyncMsg{})
			time.Sleep(delay)
			if delay < 15*time.Second {
				delay *= 2
			}
		}
	}()
	if _, err := p.Run(); err != nil {
		close(done)
		return err
	}
	close(done)
	fmt.Fprintln(os.Stderr, "session "+sess.ID)
	return nil
}
