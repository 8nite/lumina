package main

import (
	"context"
	"fmt"
	"net/http"
	"time"

	"github.com/8nite/lumina/internal/agent"
	"github.com/8nite/lumina/internal/server"
)

func main() {
	st, err := agent.Open("/tmp/tui-e2e.db")
	if err != nil {
		panic(err)
	}
	srv := server.New(server.Options{})
	srv.Store = st
	srv.Exec = func(ctx context.Context, id string, in agent.TurnInput) (string, error) {
		if _, err := st.AddTurn(id, "", "user", in.Text); err != nil {
			return "", err
		}
		for i := 1; i <= 4; i++ {
			select {
			case <-ctx.Done():
				return "", ctx.Err()
			case <-time.After(5 * time.Second):
			}
			srv.Broker().Publish(server.Event{Type: "turn.progress", SessionID: id, Data: map[string]string{"stage": fmt.Sprintf("step %d/4 working…", i)}})
		}
		if _, err := st.AddTurn(id, "", "assistant", "long task done"); err != nil {
			return "", err
		}
		return "long task done", nil
	}
	fmt.Println("tui-e2e daemon on http://127.0.0.1:8742")
	_ = http.ListenAndServe("127.0.0.1:8742", srv.Handler())
}
