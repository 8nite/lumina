// Package mcp is a minimal local MCP client (stdio + SSE transports) for
// calling tools on user-configured servers from .lumina/mcp.json.
// Wire format is JSON-RPC 2.0 (initialize → tools/list → tools/call).
package mcp

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"sync"
	"time"

	"github.com/8nite/lumina/internal/config"
)

// ServerConf describes one MCP server.
type ServerConf struct {
	Transport string   `json:"transport"` // "stdio" | "sse"
	Command   string   `json:"command,omitempty"`
	Args      []string `json:"args,omitempty"`
	URL       string   `json:"url,omitempty"`
}

// ConfigFile is .lumina/mcp.json: { "servers": { name: conf } }.
type ConfigFile struct {
	Servers map[string]ServerConf `json:"servers"`
}

// LoadConfig reads workspace + global MCP configs (workspace wins).
func LoadConfig(workspaceDir string) ConfigFile {
	merged := ConfigFile{Servers: map[string]ServerConf{}}
	home, _ := os.UserHomeDir()
	for _, p := range []string{
		home + "/.config/lumina/mcp.json",
		workspaceDir + "/.lumina/mcp.json",
	} {
		raw, err := os.ReadFile(p)
		if err != nil {
			continue
		}
		var c ConfigFile
		if err := json.Unmarshal(raw, &c); err == nil {
			for k, v := range c.Servers {
				merged.Servers[k] = v
			}
		}
	}
	return merged
}

type rpcReq struct {
	JSONRPC string      `json:"jsonrpc"`
	ID      int         `json:"id"`
	Method  string      `json:"method"`
	Params  interface{} `json:"params,omitempty"`
}

type rpcResp struct {
	ID     int             `json:"id"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *struct {
		Code    int    `json:"code"`
		Message string `json:"message"`
	} `json:"error,omitempty"`
}

// Client holds one server connection.
type Client struct {
	conf   ServerConf
	mu     sync.Mutex
	nextID int

	stdin  io.WriteCloser
	stdout *bufio.Reader
	cancel context.CancelFunc
}

// Dial starts the server transport.
func Dial(conf ServerConf) (*Client, error) {
	c := &Client{conf: conf}
	if conf.URL != "" {
		if err := config.CheckNoGoogle(conf.URL); err != nil {
			return nil, fmt.Errorf("mcp: %w", err)
		}
	}
	switch conf.Transport {
	case "", "stdio":
		if conf.Command == "" {
			return nil, fmt.Errorf("mcp: stdio server missing command")
		}
		ctx, cancel := context.WithCancel(context.Background())
		c.cancel = cancel
		cmd := exec.CommandContext(ctx, conf.Command, conf.Args...)
		cmd.Env = os.Environ()
		in, err := cmd.StdinPipe()
		if err != nil {
			cancel()
			return nil, err
		}
		out, err := cmd.StdoutPipe()
		if err != nil {
			cancel()
			return nil, err
		}
		if err := cmd.Start(); err != nil {
			cancel()
			return nil, err
		}
		c.stdin = in
		c.stdout = bufio.NewReader(out)
		go func() { _ = cmd.Wait() }()
		if err := c.call(context.Background(), "initialize", map[string]interface{}{
			"protocolVersion": "2024-11-05",
			"clientInfo":      map[string]string{"name": "lumina", "version": "0.1.0"},
		}, nil); err != nil {
			c.Close()
			return nil, err
		}
		return c, nil
	case "sse":
		return c, nil // stateless POST per call
	default:
		return nil, fmt.Errorf("mcp: unknown transport %q", conf.Transport)
	}
}

// Close stops the server process.
func (c *Client) Close() error {
	if c.cancel != nil {
		c.cancel()
	}
	return nil
}

func (c *Client) call(ctx context.Context, method string, params interface{}, out interface{}) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.nextID++
	req := rpcReq{JSONRPC: "2.0", ID: c.nextID, Method: method, Params: params}
	switch c.conf.Transport {
	case "", "stdio":
		raw, _ := json.Marshal(req)
		if _, err := fmt.Fprintf(c.stdin, "%s\n", raw); err != nil {
			return err
		}
		deadline := time.Now().Add(30 * time.Second)
		_ = deadline
		line, err := c.stdout.ReadString('\n')
		if err != nil {
			return err
		}
		var resp rpcResp
		if err := json.Unmarshal([]byte(line), &resp); err != nil {
			return err
		}
		if resp.Error != nil {
			return fmt.Errorf("mcp: %s", resp.Error.Message)
		}
		if out != nil && len(resp.Result) > 0 {
			return json.Unmarshal(resp.Result, out)
		}
		return nil
	case "sse":
		body, _ := json.Marshal(req)
		r, err := http.NewRequestWithContext(ctx, http.MethodPost, c.conf.URL, bytes.NewReader(body))
		if err != nil {
			return err
		}
		r.Header.Set("Content-Type", "application/json")
		resp, err := http.DefaultClient.Do(r)
		if err != nil {
			return err
		}
		defer resp.Body.Close()
		var rr rpcResp
		if err := json.NewDecoder(resp.Body).Decode(&rr); err != nil {
			return err
		}
		if rr.Error != nil {
			return fmt.Errorf("mcp: %s", rr.Error.Message)
		}
		if out != nil && len(rr.Result) > 0 {
			return json.Unmarshal(rr.Result, out)
		}
		return nil
	}
	return fmt.Errorf("mcp: unsupported transport")
}

// Tool describes one remote tool.
type Tool struct {
	Name        string `json:"name"`
	Description string `json:"description"`
}

// ListTools returns the server's tool list.
func (c *Client) ListTools(ctx context.Context) ([]Tool, error) {
	var res struct {
		Tools []Tool `json:"tools"`
	}
	if err := c.call(ctx, "tools/list", map[string]interface{}{}, &res); err != nil {
		return nil, err
	}
	return res.Tools, nil
}

// CallTool invokes a remote tool with JSON args.
func (c *Client) CallTool(ctx context.Context, name string, args map[string]interface{}) (string, error) {
	var res struct {
		Content []struct {
			Type string `json:"type"`
			Text string `json:"text"`
		} `json:"content"`
		IsError bool `json:"isError"`
	}
	err := c.call(ctx, "tools/call", map[string]interface{}{"name": name, "arguments": args}, &res)
	if err != nil {
		return "", err
	}
	var sb bytes.Buffer
	for _, b := range res.Content {
		sb.WriteString(b.Text)
	}
	if res.IsError {
		return sb.String(), fmt.Errorf("mcp tool %q failed: %s", name, sb.String())
	}
	return sb.String(), nil
}
