package mcp

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// fakeStdioServer speaks enough JSON-RPC to satisfy Dial/ListTools/CallTool.
const fakeServer = `
let buf = "";
process.stdin.on("data", (c) => {
  buf += c.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    const m = JSON.parse(line);
    const reply = (result) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result }) + "\n");
    if (m.method === "initialize") reply({ capabilities: {} });
    else if (m.method === "tools/list") reply({ tools: [{ name: "echo", description: "echo back" }] });
    else if (m.method === "tools/call") reply({ content: [{ type: "text", text: "echo:" + JSON.stringify(m.params.arguments) }] });
    else process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, error: { code: -32601, message: "nope" } }) + "\n");
  }
});
`

func TestStdioRoundtrip(t *testing.T) {
	dir := t.TempDir()
	srv := filepath.Join(dir, "fake.mjs")
	if err := os.WriteFile(srv, []byte(fakeServer), 0o644); err != nil {
		t.Fatal(err)
	}
	c, err := Dial(ServerConf{Transport: "stdio", Command: "node", Args: []string{srv}})
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer c.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	tools, err := c.ListTools(ctx)
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(tools) != 1 || tools[0].Name != "echo" {
		t.Fatalf("tools=%+v", tools)
	}
	out, err := c.CallTool(ctx, "echo", map[string]interface{}{"a": 1})
	if err != nil {
		t.Fatalf("call: %v", err)
	}
	if out != `echo:{"a":1}` {
		t.Fatalf("out=%q", out)
	}
}

func TestLoadConfigMissing(t *testing.T) {
	cfg := LoadConfig(t.TempDir())
	if len(cfg.Servers) != 0 {
		t.Fatalf("expected empty, got %+v", cfg.Servers)
	}
}

func TestGoogleURLBlocked(t *testing.T) {
	_, err := Dial(ServerConf{Transport: "sse", URL: "https://generativelanguage.googleapis.com/v1"})
	if err == nil {
		t.Fatal("expected google rejection")
	}
}

// mockSSEServer speaks JSON-RPC over plain POST (the SSE transport posts
// each request and reads one JSON-RPC reply).
func TestSSETransport(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var req rpcReq
		body, _ := io.ReadAll(r.Body)
		if err := json.Unmarshal(body, &req); err != nil {
			http.Error(w, "bad rpc", 400)
			return
		}
		var result interface{}
		switch req.Method {
		case "initialize":
			result = map[string]interface{}{"capabilities": map[string]interface{}{}}
		case "tools/list":
			result = map[string]interface{}{"tools": []map[string]string{{"name": "ping", "description": "pong"}}}
		case "tools/call":
			result = map[string]interface{}{"content": []map[string]string{{"type": "text", "text": "pong"}}}
		default:
			w.WriteHeader(404)
			return
		}
		raw, _ := json.Marshal(result)
		resp := rpcResp{ID: req.ID, Result: raw}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(resp)
	}))
	defer srv.Close()
	c, err := Dial(ServerConf{Transport: "sse", URL: srv.URL})
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	tools, err := c.ListTools(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(tools) != 1 || tools[0].Name != "ping" {
		t.Fatalf("tools=%+v", tools)
	}
	out, err := c.CallTool(ctx, "ping", map[string]interface{}{})
	if err != nil {
		t.Fatal(err)
	}
	if out != "pong" {
		t.Fatalf("out=%q", out)
	}
}
