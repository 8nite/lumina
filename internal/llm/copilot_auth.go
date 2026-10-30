package llm

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// GitHub device flow (web auth) for Copilot: no localhost callback, no
// secrets pasted anywhere. Uses the public VS Code OAuth App identifier that
// every official Copilot client uses.
const (
	copilotOAuthClientID = "Iv1.b507a08c87ecfe98"
	copilotDeviceScope   = "read:user"
)

// DeviceCode is one pending web-auth handshake.
type DeviceCode struct {
	DeviceCode      string `json:"device_code"`
	UserCode        string `json:"user_code"`
	VerificationURI string `json:"verification_uri"`
	ExpiresIn       int    `json:"expires_in"`
	Interval        int    `json:"interval"`
}

// deviceHost derives the github host from a token URL:
// https://api.github.com/... -> https://github.com,
// https://HOST/api/v3/...   -> https://HOST (GHES).
func deviceHost(tokenURL string) string {
	u, err := url.Parse(tokenURL)
	if err != nil || u.Host == "" {
		return "https://github.com"
	}
	scheme := u.Scheme
	if scheme == "" {
		scheme = "https"
	}
	if u.Host == "api.github.com" {
		return scheme + "://github.com"
	}
	if rest, ok := strings.CutSuffix(u.Host, ".api.github.com"); ok {
		return scheme + "://" + rest // data-residency style hosts
	}
	if h, _, ok := strings.Cut(u.Path, "/api/"); ok && h != "" {
		return scheme + "://" + h
	}
	return scheme + "://" + u.Host
}

// StartDeviceFlow begins web auth: user opens VerificationURI and enters
// UserCode; the caller then polls PollDeviceFlow until authorized.
func StartDeviceFlow(ctx context.Context, tokenURL, scope string) (*DeviceCode, error) {
	if scope == "" {
		scope = copilotDeviceScope
	}
	form := url.Values{
		"client_id": {copilotOAuthClientID},
		"scope":     {scope},
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		deviceHost(tokenURL)+"/login/device/code", strings.NewReader(form.Encode()))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("Accept", "application/json")
	resp, err := newClient().Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("copilot device start: status %d", resp.StatusCode)
	}
	var dc DeviceCode
	if err := json.NewDecoder(resp.Body).Decode(&dc); err != nil {
		return nil, err
	}
	if dc.DeviceCode == "" || dc.UserCode == "" {
		return nil, fmt.Errorf("copilot device start: bad response")
	}
	if dc.Interval <= 0 {
		dc.Interval = 5
	}
	return &dc, nil
}

// PollDeviceFlow checks authorization once. Returns ("pending", nil) while
// the user hasn't approved, or the GitHub token when done.
func PollDeviceFlow(ctx context.Context, tokenURL, deviceCode string) (string, error) {
	form := url.Values{
		"client_id":   {copilotOAuthClientID},
		"device_code": {deviceCode},
		"grant_type":  {"urn:ietf:params:oauth:grant-type:device_code"},
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		deviceHost(tokenURL)+"/login/oauth/access_token", strings.NewReader(form.Encode()))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("Accept", "application/json")
	resp, err := newClient().Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	var tr struct {
		AccessToken string `json:"access_token"`
		Error       string `json:"error"`
		ErrorDesc   string `json:"error_description"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&tr); err != nil {
		return "", err
	}
	if tr.AccessToken != "" {
		return tr.AccessToken, nil
	}
	switch tr.Error {
	case "authorization_pending":
		return "", fmt.Errorf("pending")
	case "slow_down":
		return "", fmt.Errorf("pending")
	case "expired_token":
		return "", fmt.Errorf("device code expired; start over")
	case "access_denied":
		return "", fmt.Errorf("authorization denied")
	case "":
		return "", fmt.Errorf("pending")
	default:
		return "", fmt.Errorf("device poll: %s %s", tr.Error, tr.ErrorDesc)
	}
}

// tokenStoreDir is ~/.config/lumina/tokens (daemon-owned, 0700).
func tokenStoreDir() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".config", "lumina", "tokens")
}

// tokenStorePath returns the 0600 file for one provider's device token.
func tokenStorePath(provider string) string {
	safe := strings.Map(func(r rune) rune {
		if (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') || r == '-' || r == '_' {
			return r
		}
		return '_'
	}, strings.ToLower(provider))
	return filepath.Join(tokenStoreDir(), safe+".json")
}

// WriteTokenStore saves a device-flow GitHub token (0600, never logged).
// Called only on explicit user auth completion.
func WriteTokenStore(provider, githubToken string) error {
	if provider == "" || githubToken == "" {
		return fmt.Errorf("token store: empty provider or token")
	}
	if err := os.MkdirAll(tokenStoreDir(), 0o700); err != nil {
		return err
	}
	raw, _ := json.Marshal(map[string]interface{}{
		"provider": provider, "github_token": githubToken,
		"obtained_at": time.Now().UTC().Format(time.RFC3339),
	})
	p := tokenStorePath(provider)
	if err := os.WriteFile(p, append(raw, '\n'), 0o600); err != nil {
		return err
	}
	// Enforce perms even when the file already existed.
	_ = os.Chmod(p, 0o600)
	return nil
}

// ReadTokenStore returns the stored GitHub token, or "" when absent.
// Values never leave this function except to the caller that injects them
// into an Authorization header in-process.
func ReadTokenStore(provider string) (string, error) {
	raw, err := os.ReadFile(tokenStorePath(provider))
	if err != nil {
		return "", err
	}
	var doc struct {
		GitHubToken string `json:"github_token"`
	}
	if err := json.Unmarshal(bytes.TrimSpace(raw), &doc); err != nil {
		return "", err
	}
	return doc.GitHubToken, nil
}

// HasTokenStore reports whether a device token exists (no value read).
func HasTokenStore(provider string) bool {
	_, err := os.Stat(tokenStorePath(provider))
	return err == nil
}

// TokenURLForHost maps a user-entered GitHub URL to the Copilot token
// endpoint: github.com -> api.github.com, GHES host -> HOST/api/v3/... .
func TokenURLForHost(githubURL string) string {
	if githubURL == "" {
		return defaultCopilotTokenURL
	}
	u, err := url.Parse(strings.TrimSpace(githubURL))
	if err != nil || u.Host == "" {
		u, err = url.Parse("https://" + strings.TrimSpace(githubURL))
		if err != nil || u.Host == "" {
			return defaultCopilotTokenURL
		}
	}
	host := strings.ToLower(u.Host)
	if host == "github.com" || host == "www.github.com" {
		return defaultCopilotTokenURL
	}
	return "https://" + u.Host + "/api/v3/copilot_internal/v2/token"
}

// CopilotBaseForHost derives the default chat base from a GitHub host:
// github.com -> "" (provider default), GHES -> copilot-api.HOST.
func CopilotBaseForHost(githubURL string) string {
	if githubURL == "" {
		return ""
	}
	u, err := url.Parse(strings.TrimSpace(githubURL))
	if err != nil || u.Host == "" {
		u, _ = url.Parse("https://" + strings.TrimSpace(githubURL))
	}
	if u == nil || u.Host == "" {
		return ""
	}
	host := strings.ToLower(u.Host)
	if host == "github.com" || host == "www.github.com" {
		return ""
	}
	return "https://copilot-api." + u.Host
}

// ProviderDoc is the daemon-side per-provider auth record (0600).
type ProviderDoc struct {
	GitHubToken string `json:"github_token"`
	BaseURL     string `json:"base_url,omitempty"`
	TokenURL    string `json:"token_url,omitempty"`
	ObtainedAt  string `json:"obtained_at,omitempty"`
}

// WriteProviderAuth stores a device-flow result plus optional endpoint
// overrides (0600, never logged). Empty URLs mean "provider defaults".
func WriteProviderAuth(provider, githubToken, baseURL, tokenURL string) error {
	if provider == "" || githubToken == "" {
		return fmt.Errorf("auth: empty provider or token")
	}
	if err := os.MkdirAll(tokenStoreDir(), 0o700); err != nil {
		return err
	}
	raw, _ := json.Marshal(ProviderDoc{
		GitHubToken: githubToken, BaseURL: baseURL, TokenURL: tokenURL,
		ObtainedAt: time.Now().UTC().Format(time.RFC3339),
	})
	p := tokenStorePath(provider)
	if err := os.WriteFile(p, append(raw, '\n'), 0o600); err != nil {
		return err
	}
	return os.Chmod(p, 0o600)
}

// ReadProviderDoc returns the stored auth record, or ("", false) when absent.
func ReadProviderDoc(provider string) (ProviderDoc, bool) {
	raw, err := os.ReadFile(tokenStorePath(provider))
	if err != nil {
		return ProviderDoc{}, false
	}
	var doc ProviderDoc
	if err := json.Unmarshal(bytes.TrimSpace(raw), &doc); err != nil {
		return ProviderDoc{}, false
	}
	return doc, true
}

// StoredTokenURL returns a stored token-URL override, or "".
func StoredTokenURL(provider string) string {
	if doc, ok := ReadProviderDoc(provider); ok {
		return doc.TokenURL
	}
	return ""
}
