package main

import (
	"context"
	"fmt"
	"os"

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
	// Fan SSE events into the program.
	go func() {
		_ = c.Subscribe(context.Background(), func(e sdk.Event) {
			p.Send(tui.EventMsg{Event: e})
		})
	}()
	if _, err := p.Run(); err != nil {
		return err
	}
	fmt.Fprintln(os.Stderr, "session "+sess.ID)
	return nil
}
