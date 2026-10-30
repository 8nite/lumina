package tui

import (
	"context"
	"encoding/json"
)

func bg() context.Context { return context.Background() }

func jsonUnmarshal(raw json.RawMessage, v interface{}) error {
	return json.Unmarshal(raw, v)
}
