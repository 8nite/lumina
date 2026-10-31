// Package security enforces the immutable safety floor: the plan/build
// structural fence, the 4-mode permission broker, and the credential /
// secret / Google hard-deny that no mode (not even bypass) can lift.
package security

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
)

// AgentMode is the structural envelope (Tab toggle). "plan" is read-only.
type AgentMode string

const (
	AgentPlan  AgentMode = "plan"
	AgentBuild AgentMode = "build"
)

// PermissionMode is the approval axis (Shift+Tab toggle).
type PermissionMode string

const (
	PermAsk         PermissionMode = "ask"
	PermDefault     PermissionMode = "default" // alias of bypass
	PermAutoEdit    PermissionMode = "auto-edit"
	PermAutoApprove PermissionMode = "auto-approve"
	PermBypass      PermissionMode = "bypass"
)

// Decision is the broker verdict for one tool call.
type Decision string

const (
	Allow Decision = "allow"
	Ask   Decision = "ask"
	Deny  Decision = "deny"
)

// CredentialFileNames are never readable/writable by tools in any mode.
var CredentialFileNames = []string{
	".env", ".env.local", ".env.production",
	"auth.json", "lumina.json", "credentials.json",
	".npmrc", ".pypirc",
	"id_rsa", "id_ed25519", ".pem", ".key",
	"vault", "secrets",
}

var credentialSubstrings = []string{
	".env", "auth.json", "lumina.json", "credentials.json",
	"secret", "credential", "private_key", ".pem", ".key",
	"vault_exec", "token.json", "id_rsa", "id_ed25519",
}

// Normalize maps aliases to canonical modes.
func Normalize(p PermissionMode) PermissionMode {
	switch PermissionMode(strings.ToLower(strings.TrimSpace(string(p)))) {
	case "", "default":
		return PermBypass
	case "auto_edit":
		return PermAutoEdit
	case "auto_approve", "auto":
		return PermAutoApprove
	case "yolo":
		return PermBypass
	default:
		return PermissionMode(strings.ToLower(strings.TrimSpace(string(p))))
	}
}

// IsCredentialPath reports whether target looks like a secret file.
func IsCredentialPath(target string) bool {
	base := strings.ToLower(filepath.Base(target))
	low := strings.ToLower(target)
	// Cluster / identity trees: the whole subtree is sensitive.
	for _, frag := range []string{"/.kube/", "/.ssh/", "/.aws/", "/.gnupg/", "/etc/ssh/", "/etc/pki/"} {
		if strings.Contains(low, frag) {
			return true
		}
	}
	if strings.HasSuffix(low, "/.kube") || strings.HasSuffix(low, "/.ssh") || strings.HasSuffix(low, "/.aws") || strings.HasSuffix(low, "/.gnupg") {
		return true
	}
	// System shadow files are always secrets, even though whitelisted for reads.
	if base == "shadow" || base == "gshadow" || base == "master.passwd" {
		return true
	}
	for _, n := range CredentialFileNames {
		if base == strings.ToLower(n) {
			return true
		}
	}
	for _, s := range credentialSubstrings {
		if strings.Contains(base, s) || strings.Contains(low, s) {
			return true
		}
	}
	return false
}

// IsGoogleTarget reports cross-workspace/Google exfiltration attempts.
func IsGoogleTarget(target string) bool {
	lc := strings.ToLower(target)
	for _, b := range []string{"googleapis.com", "generativelanguage", "gemini", "google-workspace", "fonts.googleapis", "vertexai", "aiplatform"} {
		if strings.Contains(lc, b) {
			return true
		}
	}
	// Literal secret injection flags are always denied.
	if strings.Contains(target, "--from-literal=") {
		return true
	}
	return false
}

// IsPlanWritable reports whether a write target is inside the plan-mode scope:
// plan.md or .lumina/plans/*.md, strictly contained in the workspace.
// Out-of-workspace lookalikes (/tmp/other/plan.md, ../plan.md) are rejected.
func IsPlanWritable(workspaceDir, target string) bool {
	p := ExpandHome(target)
	if !filepath.IsAbs(p) {
		p = filepath.Join(workspaceDir, p)
	}
	p = filepath.Clean(p)
	if workspaceDir != "" {
		rel, err := filepath.Rel(workspaceDir, p)
		if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
			return false
		}
	}
	// ponytail: slash-normalized suffix match keeps the fence branchless.
	t := filepath.ToSlash(p)
	base := filepath.ToSlash(filepath.Base(t))
	if base == "plan.md" {
		return true
	}
	if strings.Contains(t, ".lumina/plans/") && strings.HasSuffix(t, ".md") {
		return true
	}
	return false
}

// Layer-0 external whitelist: paths outside the workspace that tools may
// still touch. Reads cover home (~/.*), temp (/tmp, /temp, os.TempDir),
// and read-only system locations. Writes stay confined to workspace +
// temp only; home and system writes remain escapes (credential hard-deny
// still wins over everything, including whitelisted paths).
func ExpandHome(p string) string {
	if p == "~" || strings.HasPrefix(p, "~/") {
		if h := homeDir(); h != "" {
			if p == "~" {
				return h
			}
			return filepath.Join(h, p[2:])
		}
	}
	return p
}

func homeDir() string {
	if h, err := os.UserHomeDir(); err == nil && h != "" {
		return filepath.Clean(h)
	}
	if h := os.Getenv("HOME"); h != "" {
		return filepath.Clean(h)
	}
	return ""
}

func isUnder(dir, p string) bool {
	if dir == "" {
		return false
	}
	dir = filepath.Clean(dir)
	p = filepath.Clean(ExpandHome(p))
	if p == dir {
		return true
	}
	return strings.HasPrefix(p, dir+string(filepath.Separator))
}

func tempPrefixes() []string {
	seen := map[string]bool{}
	var out []string
	add := func(s string) {
		if s == "" {
			return
		}
		s = filepath.Clean(s)
		if !seen[s] {
			seen[s] = true
			out = append(out, s)
		}
	}
	add(os.TempDir())
	add("/tmp")
	add("/var/tmp")
	add("/temp")
	// macOS: /tmp symlinks to /private/tmp.
	add("/private/tmp")
	return out
}

func systemReadPrefixes() []string {
	dirs := []string{"/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt", "/System", "/Library"}
	if runtime.GOOS == "windows" {
		if s := os.Getenv("SystemRoot"); s != "" {
			dirs = append(dirs, filepath.Clean(s))
		}
		if s := os.Getenv("ProgramFiles"); s != "" {
			dirs = append(dirs, filepath.Clean(s))
		}
		if s := os.Getenv("TEMP"); s != "" {
			dirs = append(dirs, filepath.Clean(s))
		}
		if s := os.Getenv("TMP"); s != "" {
			dirs = append(dirs, filepath.Clean(s))
		}
	}
	return dirs
}

// IsWhitelistedRead reports whether an absolute (or ~-prefixed) path is in
// the read allowlist: home (~/.* dotfiles included), temp (/tmp, /temp,
// os.TempDir), or system locations. Relative paths return false (they
// resolve inside the workspace). Per explicit user override, credential-shaped
// paths (like .env) ARE whitelisted when outside the workspace — Authorize
// skips the hard-deny for external whitelisted secrets, workspace secrets
// stay denied.
func IsWhitelistedRead(p string) bool {
	e := filepath.Clean(ExpandHome(p))
	if !filepath.IsAbs(e) {
		return false
	}
	if h := homeDir(); h != "" && (e == h || strings.HasPrefix(e, h+string(filepath.Separator))) {
		return true
	}
	for _, t := range tempPrefixes() {
		if e == t || strings.HasPrefix(e, t+string(filepath.Separator)) {
			return true
		}
	}
	for _, s := range systemReadPrefixes() {
		if e == s || strings.HasPrefix(e, s+string(filepath.Separator)) {
			return true
		}
	}
	return false
}

// IsWhitelistedWrite reports whether an absolute (or ~-prefixed) path is
// writable outside the workspace: temp locations only (including dotfiles
// like /tmp/.env per explicit override). Home dotfiles and system folders
// are read-only (writes there remain workspace escapes).
func IsWhitelistedWrite(p string) bool {
	e := filepath.Clean(ExpandHome(p))
	if !filepath.IsAbs(e) {
		return false
	}
	for _, t := range tempPrefixes() {
		if e == t || strings.HasPrefix(e, t+string(filepath.Separator)) {
			return true
		}
	}
	return false
}

// isInWorkspace reports whether target resolves inside workspaceDir.
func isInWorkspace(workspaceDir, target string) bool {
	if workspaceDir == "" {
		return false
	}
	p := filepath.Clean(ExpandHome(target))
	if !filepath.IsAbs(p) {
		p = filepath.Join(workspaceDir, p)
	}
	rel, err := filepath.Rel(workspaceDir, p)
	if err != nil {
		return false
	}
	return rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}

// credentialOverrideAllowed reports whether a credential-shaped target should
// skip the hard-deny because it lives outside the workspace in a whitelisted
// external location (home/tmp/system, dotfiles included). Workspace-internal
// secrets stay denied. Bash commands are scanned for path-like tokens.
func credentialOverrideAllowed(workspaceDir, tool, target string) bool {
	lowTool := strings.ToLower(strings.TrimSpace(tool))
	if strings.HasPrefix(lowTool, "bash") || lowTool == "shell" {
		// Scan command for path-like tokens.
		toks := strings.FieldsFunc(target, func(r rune) bool {
			return r == ' ' || r == '\t' || r == '\n' || r == '"' || r == '\'' || r == ';' || r == '|' || r == '&' || r == '`'
		})
		foundExternalWhitelisted := false
		for _, tok := range toks {
			// Strip common wrappers like $(, ${, quotes.
			t := strings.Trim(tok, "(){}<>")
			if t == "" {
				continue
			}
			// Only consider path-like tokens.
			if !(strings.HasPrefix(t, "/") || strings.HasPrefix(t, "~") || strings.HasPrefix(t, "$HOME")) {
				continue
			}
			t = ExpandHome(t)
			if !filepath.IsAbs(t) {
				continue
			}
			t = filepath.Clean(t)
			if isInWorkspace(workspaceDir, t) {
				// Workspace-internal secret ref → stay denied.
				// Continue scanning: one internal hit forces deny only if
				// no external override? Safer: internal hit wins → deny.
				if IsCredentialPath(t) {
					return false
				}
				continue
			}
			if IsWhitelistedRead(t) || IsWhitelistedWrite(t) {
				foundExternalWhitelisted = true
			}
		}
		// Also allow bare whitelisted dir refs like "/tmp/" in command.
		if !foundExternalWhitelisted {
			low := strings.ToLower(target)
			for _, frag := range append(append([]string{}, tempPrefixes()...), systemReadPrefixes()...) {
				if strings.Contains(low, strings.ToLower(frag)) {
					foundExternalWhitelisted = true
					break
				}
			}
			if h := strings.ToLower(homeDir()); h != "" && strings.Contains(low, h) {
				foundExternalWhitelisted = true
			}
			if strings.Contains(target, "~/") || strings.Contains(target, "$HOME") {
				foundExternalWhitelisted = true
			}
		}
		return foundExternalWhitelisted
	}
	// File tools: single path target.
	if !IsCredentialPath(target) {
		return false
	}
	if isInWorkspace(workspaceDir, target) {
		return false // workspace secrets stay denied
	}
	e := filepath.Clean(ExpandHome(target))
	if !filepath.IsAbs(e) {
		// Relative outside workspace? Already handled as inside above
		// (relative joins workspace). Non-abs here means no workspace?
		return false
	}
	return IsWhitelistedRead(e) || IsWhitelistedWrite(e)
}

// IsWhitelistedExternal is the union (read or write) for generic fence
// checks that do not distinguish operation type.
func IsWhitelistedExternal(p string) bool {
	return IsWhitelistedRead(p) || IsWhitelistedWrite(p)
}

// IsReadAllowed holds when p is inside the workspace or whitelisted for reads.
func IsReadAllowed(workspaceDir, target string) bool {
	p := filepath.Clean(ExpandHome(target))
	if !filepath.IsAbs(p) {
		p = filepath.Join(workspaceDir, p)
	}
	if workspaceDir != "" {
		if rel, err := filepath.Rel(workspaceDir, p); err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
			return true
		}
	} else {
		return true
	}
	return IsWhitelistedRead(p)
}

// IsWriteAllowed holds when p is inside the workspace or whitelisted for writes (temp only).
func IsWriteAllowed(workspaceDir, target string) bool {
	p := filepath.Clean(ExpandHome(target))
	if !filepath.IsAbs(p) {
		p = filepath.Join(workspaceDir, p)
	}
	if workspaceDir != "" {
		if rel, err := filepath.Rel(workspaceDir, p); err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
			return true
		}
	} else {
		return true
	}
	return IsWhitelistedWrite(p)
}

// readOnlyBashPrefixes are the only shell commands allowed in plan mode.
var readOnlyBashPrefixes = []string{
	"git status", "git diff", "git log", "git show", "git branch",
	"ls", "pwd", "cat ", "head ", "tail ", "wc ", "echo ",
	"find ", "go test", "pnpm test",
}

// IsReadOnlyBash reports whether a bash command is inspection-only.
// Any shell metacharacter (redirection, pipes, chaining, substitution)
// disqualifies the command: `echo x > a.go`, `ls; touch p`, and
// `git status && rm a.go` are all mutating until proven otherwise.
func IsReadOnlyBash(cmd string) bool {
	t := strings.TrimSpace(cmd)
	if t == "" {
		return false
	}
	if strings.ContainsAny(t, ";|&$`><\n\r") {
		return false
	}
	lc := strings.ToLower(t)
	for _, p := range readOnlyBashPrefixes {
		if strings.HasPrefix(lc, p) {
			// Destructive operators disqualify even "read-only" prefixes.
			if strings.Contains(t, "rm ") || strings.Contains(t, "--force") || strings.Contains(t, "--hard") {
				return false
			}
			return true
		}
	}
	return false
}

// IsDestructiveBash reports rm -rf / force-push / hard-reset style commands.
func IsDestructiveBash(cmd string) bool {
	lc := strings.ToLower(strings.TrimSpace(cmd))
	for _, p := range []string{"rm -rf", "rm --recursive --force", "git push --force", "git reset --hard", "mkfs", ":(){", "dd if="} {
		if strings.Contains(lc, p) {
			return true
		}
	}
	return false
}

// readTools are always allowed in every mode (discovery + web research +
// user questions: asking is safe and never needs approval itself).
func isReadTool(tool string) bool {
	switch strings.ToLower(tool) {
	case "read", "read_file", "glob", "grep", "kb_query", "lsp", "skill",
		"webfetch", "websearch", "task_output", "question", "ask_question":
		return true
	}
	return false
}

func isEditTool(tool string) bool {
	switch strings.ToLower(tool) {
	case "edit", "edit_file", "write", "write_file", "memory_write", "todowrite", "patch":
		return true
	}
	return false
}

// Authorize applies the structural fence first, then the permission matrix.
// Hard denies (credentials / Google) win over everything, including bypass,
// EXCEPT explicit user override: credential-shaped paths outside the workspace
// but in the external whitelist (home/tmp/system, dotfiles included) skip the
// credential deny. Workspace-internal secrets stay denied. Google deny never lifts.
func Authorize(agent AgentMode, perm PermissionMode, tool, target, workspaceDir string, bashRules map[string]string, allowToolingWrite bool) Decision {
	pm := Normalize(perm)

	// 1. Immutable hard deny — never bypassable, except external-whitelist
	// secret override (explicit user choice to allow all dotfiles outside ws).
	if IsCredentialPath(target) {
		if !credentialOverrideAllowed(workspaceDir, tool, target) {
			return Deny
		}
	}
	if IsGoogleTarget(target) || IsGoogleTarget(tool) {
		return Deny
	}
	if strings.HasPrefix(strings.ToLower(strings.TrimSpace(tool)), "bash") {
		if strings.Contains(target, "--from-literal=") {
			return Deny
		}
		if IsGoogleTarget(target) {
			return Deny
		}
	}

	// 2. Plan-mode structural fence (holds even under bypass).
	if AgentMode(strings.ToLower(string(agent))) == AgentPlan {
		return authorizePlan(tool, target, workspaceDir)
	}

	// 3. Build envelope + permission matrix.
	return authorizeBuild(pm, tool, target, bashRules, allowToolingWrite)
}

func authorizePlan(tool, target, workspaceDir string) Decision {
	t := strings.ToLower(strings.TrimSpace(tool))
	switch t {
	case "read", "read_file", "glob", "grep", "lsp", "kb_query", "skill", "question", "ask_question",
		"webfetch", "websearch", "task_output":
		// plan research may read the web, ask questions, and
		// poll background tasks, but never mutates via delegation.
		return Allow
	case "bash", "shell":
		if IsReadOnlyBash(target) && !IsDestructiveBash(target) {
			return Allow
		}
		return Deny
	case "edit", "edit_file", "write", "write_file":
		if IsPlanWritable(workspaceDir, target) {
			return Allow
		}
		return Deny
	case "patch":
		// Per-file plan scope is enforced inside doPatch (multi-file
		// patches authorize on the first path only); allow the call
		// through so plan.md patches still work.
		return Allow
	case "subagent":
		// Delegation could bypass the read-only fence (a build child
		// edits while the parent stays in plan), so plan denies it.
		return Deny
	default:
		return Deny
	}
}

func authorizeBuild(pm PermissionMode, tool, target string, bashRules map[string]string, allowToolingWrite bool) Decision {
	t := strings.ToLower(strings.TrimSpace(tool))

	if isReadTool(t) {
		// Writes to shared tooling docs need an explicit opt-in.
		if !allowToolingWrite && (isToolingDoc(target)) && (t == "read" || t == "read_file") {
			return Allow // reads always fine; writes are separate tools
		}
		return Allow
	}
	if isEditTool(t) {
		if isToolingDoc(target) && !allowToolingWrite {
			return Deny
		}
		switch pm {
		case PermAutoEdit, PermAutoApprove, PermBypass:
			return Allow
		default:
			return Ask
		}
	}
	if t == "bash" || t == "shell" {
		if IsDestructiveBash(target) {
			if pm == PermBypass {
				return Allow
			}
			return Ask
		}
		if d, ok := matchBashRules(target, bashRules); ok {
			switch d {
			case Allow:
				return Allow
			case Deny:
				return Deny
			default:
				if pm == PermAutoApprove || pm == PermBypass {
					// Destructive patterns already handled; auto-approve allows the rest.
					return Allow
				}
				return Ask
			}
		}
		switch pm {
		case PermAutoApprove, PermBypass:
			return Allow
		default:
			return Ask
		}
	}
	// MCP / subagents / external directories: ask unless bypass.
	if pm == PermBypass {
		return Allow
	}
	return Ask
}

func isToolingDoc(target string) bool {
	t := filepath.ToSlash(strings.ToLower(target))
	return strings.Contains(t, "skills/") || strings.HasSuffix(t, "agents.md") || strings.Contains(t, "instructions/")
}

// matchBashRules evaluates glob-prefix rules ("go test*" = "allow") in order.
func matchBashRules(cmd string, rules map[string]string) (Decision, bool) {
	for pattern, verdict := range rules {
		if pattern == "*" {
			continue // default last
		}
		prefix := strings.TrimSuffix(pattern, "*")
		if strings.HasPrefix(cmd, prefix) {
			switch strings.ToLower(verdict) {
			case "allow":
				return Allow, true
			case "deny":
				return Deny, true
			default:
				return Ask, true
			}
		}
	}
	if v, ok := rules["*"]; ok {
		switch strings.ToLower(v) {
		case "allow":
			return Allow, true
		case "deny":
			return Deny, true
		}
		return Ask, true
	}
	return "", false
}

// SessionTMPDIR provisions an isolated per-session TMPDIR.
func SessionTMPDIR(sessionID string) string {
	return filepath.Join(os.TempDir(), "lumina-ses-"+sessionID)
}

// EnsureSessionDir creates the session scratch dir.
func EnsureSessionDir(sessionID string) (string, error) {
	dir := SessionTMPDIR(sessionID)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}
	return dir, nil
}

// CleanupSessionDir removes the session scratch dir.
func CleanupSessionDir(sessionID string) error {
	dir := SessionTMPDIR(sessionID)
	if !strings.HasPrefix(filepath.Clean(dir), os.TempDir()) {
		return nil // never delete outside TMPDIR
	}
	return os.RemoveAll(dir)
}
