// Package config loads ~/.config/lumina/config.toml with hot-reload,
// env/cmd secret indirection, and a hard no-Google validator.
//
// Secret handling: plaintext `api_key = "..."` is rejected at load time.
// Only `api_key_env` (env var name) or `api_key_cmd` (shell command printing
// the key) are accepted. Values never enter logs or errors.
package config

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/BurntSushi/toml"
)

// GoogleBlocklist matches Google API / AI surfaces that Lumina must never call.
var GoogleBlocklist = []string{
	"googleapis.com",
	"google.com",
	"gstatic.com",
	"fonts.googleapis.com",
	"fonts.gstatic.com",
	"generativelanguage",
	"aiplatform",
	"vertexai",
	"gemini",
	"google-workspace",
	"bard.google",
}

// CredentialEnvBlocklist must never be routed to an LLM provider.
var CredentialEnvBlocklist = []string{
	"GEMINI_API_KEY",
	"GOOGLE_API_KEY",
	"GOOGLE_APPLICATION_CREDENTIALS",
}

type ServerConfig struct {
	Host string `toml:"host"`
	Port int    `toml:"port"`
}

type DefaultsConfig struct {
	Agent          string `toml:"agent"`
	PermissionMode string `toml:"permission_mode"`
}

type CompatConfig struct {
	ImportGlobalRules  bool `toml:"import_global_rules"`
	ImportGlobalSkills bool `toml:"import_global_skills"`
}

type ProviderConfig struct {
	Protocol  string `toml:"protocol"`
	BaseURL   string `toml:"base_url"`
	APIKeyEnv string `toml:"api_key_env"`
	APIKeyCmd string `toml:"api_key_cmd"`
	TokenURL  string `toml:"token_url"` // copilot exchange endpoint override (GHES)
}

type fileConfig struct {
	Server      ServerConfig              `toml:"server"`
	Defaults    DefaultsConfig            `toml:"defaults"`
	Compat      CompatConfig              `toml:"compat"`
	Permissions map[string]toml.Primitive `toml:"permissions"`
	Providers   map[string]ProviderConfig `toml:"providers"`
	Router      RouterConfig              `toml:"router"`
	KB          KBConfig                  `toml:"kb"`
	WebSearch   WebSearchConfig           `toml:"websearch"`
}

type RouterConfig struct {
	Fast     string `toml:"fast"`
	Executor string `toml:"executor"`
	Planner  string `toml:"planner"`
	Improver string `toml:"improver"`
}

type KBConfig struct {
	IdleDebounceMinutes int  `toml:"idle_debounce_minutes"`
	AutoCommitAstHook   bool `toml:"auto_commit_ast_hook"`
	StandingOps         bool `toml:"standing_ops"`
}

// WebSearchConfig mirrors opencode's `websearch` setting: pick one of
// exa | firecrawl | parallel | tavily | tinyfish | random, or disable.
// Empty provider means auto-detect from env keys.
type WebSearchConfig struct {
	Provider string `toml:"provider"`
	Disabled bool   `toml:"disabled"`
}

type Config struct {
	Server      ServerConfig              `toml:"server"`
	Defaults    DefaultsConfig            `toml:"defaults"`
	Compat      CompatConfig              `toml:"compat"`
	Permissions map[string]string         `toml:"permissions"`
	BashRules   map[string]string         `toml:"permissions.bash"`
	Providers   map[string]ProviderConfig `toml:"providers"`
	Router      RouterConfig              `toml:"router"`
	KB          KBConfig                  `toml:"kb"`
	WebSearch   WebSearchConfig           `toml:"websearch"`

	path     string
	mu       sync.RWMutex
	mtime    time.Time
	onChange []func(*Config)
}

// Defaults returns a Config populated with plan.md §8.2 defaults.
func Defaults() *Config {
	return &Config{
		Server:   ServerConfig{Host: "127.0.0.1", Port: 7432},
		Defaults: DefaultsConfig{Agent: "build", PermissionMode: "ask"},
		Compat:   CompatConfig{ImportGlobalRules: true, ImportGlobalSkills: true},
		Permissions: map[string]string{
			"edit":               "allow",
			"external_directory": "ask",
		},
		BashRules: map[string]string{
			"git status*": "allow",
			"git diff*":   "allow",
			"go test*":    "allow",
			"pnpm test*":  "allow",
			"*":           "ask",
		},
		Providers: map[string]ProviderConfig{},
		Router:    RouterConfig{},
		KB:        KBConfig{IdleDebounceMinutes: 15, AutoCommitAstHook: true, StandingOps: true},
	}
}

// DefaultPath is ~/.config/lumina/config.toml.
func DefaultPath() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".config", "lumina", "config.toml")
}

// Load reads and validates a TOML config file. Missing files yield Defaults().
func Load(path string) (*Config, error) {
	c := Defaults()
	c.path = path
	data, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return c, nil
		}
		return nil, err
	}
	// Reject plaintext secrets before decoding into structs: scan raw TOML.
	if err := rejectPlaintextSecrets(string(data)); err != nil {
		return nil, err
	}
	var fc fileConfig
	md, err := toml.Decode(string(data), &fc)
	if err != nil {
		return nil, fmt.Errorf("config decode: %w", err)
	}
	c.Server, c.Defaults, c.Compat = fc.Server, fc.Defaults, fc.Compat
	c.Providers, c.Router, c.KB = fc.Providers, fc.Router, fc.KB
	c.WebSearch = fc.WebSearch
	c.Permissions = map[string]string{}
	c.BashRules = map[string]string{}
	for k, prim := range fc.Permissions {
		if k == "bash" {
			if err := md.PrimitiveDecode(prim, &c.BashRules); err != nil {
				return nil, fmt.Errorf("config [permissions.bash]: %w", err)
			}
			continue
		}
		var v string
		if err := md.PrimitiveDecode(prim, &v); err != nil {
			return nil, fmt.Errorf("config [permissions] %q: %w", k, err)
		}
		c.Permissions[k] = v
	}
	if st, err := os.Stat(path); err == nil {
		c.mtime = st.ModTime()
	}
	if err := c.Validate(); err != nil {
		return nil, err
	}
	return c, nil
}

// rejectPlaintextSecrets fails when any [providers.*] table sets api_key literally.
// api_key_env / api_key_cmd are the only allowed forms.
func rejectPlaintextSecrets(raw string) error {
	for _, line := range strings.Split(raw, "\n") {
		t := strings.TrimSpace(line)
		if strings.HasPrefix(t, "#") {
			continue
		}
		// strip inline comments naively
		if i := strings.Index(t, "#"); i >= 0 {
			t = strings.TrimSpace(t[:i])
		}
		kv := strings.SplitN(t, "=", 2)
		if len(kv) != 2 {
			continue
		}
		key := strings.TrimSpace(kv[0])
		key = strings.Trim(key, `"'`)
		if strings.EqualFold(key, "api_key") {
			return fmt.Errorf("config: plaintext `api_key` is forbidden; use `api_key_env` or `api_key_cmd`")
		}
	}
	return nil
}

// Validate enforces permission-mode vocabulary and the no-Google rule.
func (c *Config) Validate() error {
	switch c.Defaults.PermissionMode {
	case "", "ask", "auto-edit", "auto-approve", "bypass", "default":
	case "auto_edit":
		c.Defaults.PermissionMode = "auto-edit"
	case "auto_approve":
		c.Defaults.PermissionMode = "auto-approve"
	default:
		return fmt.Errorf("config: unknown permission_mode %q", c.Defaults.PermissionMode)
	}
	if c.Defaults.Agent == "" {
		c.Defaults.Agent = "build"
	}
	c.WebSearch.Provider = strings.ToLower(strings.TrimSpace(c.WebSearch.Provider))
	if !validWebSearchProviders[c.WebSearch.Provider] {
		return fmt.Errorf("config: unknown websearch provider %q (want exa|firecrawl|parallel|tavily|tinyfish|random)", c.WebSearch.Provider)
	}
	for name, p := range c.Providers {
		switch p.Protocol {
		case "", "openai-compatible", "anthropic-compatible", "ollama", "openai-responses", "responses", "copilot":
		default:
			return fmt.Errorf("config: provider %q has unknown protocol %q", name, p.Protocol)
		}
		if err := CheckNoGoogle(p.BaseURL); err != nil {
			return fmt.Errorf("config: provider %q: %w", name, err)
		}
		if err := CheckNoGoogle(p.TokenURL); err != nil {
			return fmt.Errorf("config: provider %q token_url: %w", name, err)
		}
		for _, blocked := range CredentialEnvBlocklist {
			if p.APIKeyEnv == blocked {
				return fmt.Errorf("config: provider %q uses blocked Google credential env %q", name, blocked)
			}
		}
		if p.APIKeyCmd != "" {
			lc := strings.ToLower(p.APIKeyCmd)
			if strings.Contains(lc, "gemini") || strings.Contains(lc, "google-workspace") || strings.Contains(lc, "google_workspace") {
				return fmt.Errorf("config: provider %q api_key_cmd touches a Google path", name)
			}
		}
	}
	return nil
}

// Validate the websearch provider name (opencode parity).
var validWebSearchProviders = map[string]bool{
	"": true, "random": true, "auto": true,
	"exa": true, "firecrawl": true, "parallel": true,
	"tavily": true, "tinyfish": true,
}

// CheckNoGoogle fails if url targets a blocked Google surface.
func CheckNoGoogle(url string) error {
	lc := strings.ToLower(url)
	for _, b := range GoogleBlocklist {
		if strings.Contains(lc, strings.ToLower(b)) {
			return fmt.Errorf("google services are forbidden (matched %q)", b)
		}
	}
	return nil
}

// ResolveSecret returns the provider key via env or shell command. Never logs the value.
func ResolveSecret(p ProviderConfig) (string, error) {
	if p.APIKeyEnv != "" {
		return os.Getenv(p.APIKeyEnv), nil
	}
	if p.APIKeyCmd != "" {
		cmd := exec.Command("sh", "-c", p.APIKeyCmd)
		cmd.Env = os.Environ()
		out, err := cmd.Output()
		if err != nil {
			return "", fmt.Errorf("api_key_cmd failed")
		}
		return strings.TrimSpace(string(out)), nil
	}
	return "", nil
}

// Path returns the loaded file path (may be empty for pure defaults).
func (c *Config) Path() string { return c.path }

// ModelForRole returns the configured model for a router tier.
func (c *Config) ModelForRole(role string) string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	switch role {
	case "fast":
		return c.Router.Fast
	case "executor":
		return c.Router.Executor
	case "planner":
		if c.Router.Planner != "" {
			return c.Router.Planner
		}
		return c.Router.Executor
	case "improver":
		return c.Router.Improver
	}
	return ""
}

// PersistModel hot-updates the live router model and writes it back to disk.
func (c *Config) PersistModel(role, model string) error {
	c.mu.Lock()
	switch role {
	case "fast":
		c.Router.Fast = model
	case "executor":
		c.Router.Executor = model
	case "planner":
		c.Router.Planner = model
	case "improver":
		c.Router.Improver = model
	default:
		c.mu.Unlock()
		return fmt.Errorf("unknown router role %q", role)
	}
	c.mu.Unlock()
	if c.path == "" {
		return nil
	}
	return c.Save()
}

// Save writes the config back to its source path (creating dirs as needed).
func (c *Config) Save() error {
	if c.path == "" {
		return fmt.Errorf("config: no path to save to")
	}
	c.mu.RLock()
	defer c.mu.RUnlock()
	if err := os.MkdirAll(filepath.Dir(c.path), 0o755); err != nil {
		return err
	}
	f, err := os.Create(c.path)
	if err != nil {
		return err
	}
	defer f.Close()
	return toml.NewEncoder(f).Encode(c)
}

// OnChange registers a hot-reload listener.
func (c *Config) OnChange(fn func(*Config)) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.onChange = append(c.onChange, fn)
}

// PollReload checks mtime and reloads when changed. Call on a ticker.
func (c *Config) PollReload() error {
	if c.path == "" {
		return nil
	}
	st, err := os.Stat(c.path)
	if err != nil {
		return nil // deleted file: keep live state
	}
	c.mu.RLock()
	unchanged := !st.ModTime().After(c.mtime)
	c.mu.RUnlock()
	if unchanged {
		return nil
	}
	fresh, err := Load(c.path)
	if err != nil {
		return err // keep old live config on bad write
	}
	c.mu.Lock()
	c.Server, c.Defaults, c.Compat, c.Permissions, c.BashRules = fresh.Server, fresh.Defaults, fresh.Compat, fresh.Permissions, fresh.BashRules
	c.Providers, c.Router, c.KB = fresh.Providers, fresh.Router, fresh.KB
	c.WebSearch = fresh.WebSearch
	c.mtime = fresh.mtime
	listeners := append([]func(*Config){}, c.onChange...)
	c.mu.Unlock()
	for _, fn := range listeners {
		fn(c)
	}
	return nil
}
