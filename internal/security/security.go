// Package security enforces the immutable safety floor: the plan/build
// structural fence, the 4-mode permission broker, and the credential /
// secret / Google hard-deny that no mode (not even bypass) can lift.
package security

import (
	"os"
	"path/filepath"
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
	PermDefault     PermissionMode = "default" // alias of ask
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
		return PermAsk
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
	// Kubernetes cluster credentials: the whole .kube tree is sensitive.
	if strings.Contains(low, "/.kube/") || strings.HasSuffix(low, "/.kube") {
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
	p := target
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

// readTools are always allowed in every mode.
func isReadTool(tool string) bool {
	switch strings.ToLower(tool) {
	case "read", "read_file", "glob", "grep", "kb_query", "lsp", "skill":
		return true
	}
	return false
}

func isEditTool(tool string) bool {
	switch strings.ToLower(tool) {
	case "edit", "edit_file", "write", "write_file", "memory_write":
		return true
	}
	return false
}

// Authorize applies the structural fence first, then the permission matrix.
// Hard denies (credentials / Google) win over everything, including bypass.
func Authorize(agent AgentMode, perm PermissionMode, tool, target, workspaceDir string, bashRules map[string]string, allowToolingWrite bool) Decision {
	pm := Normalize(perm)

	// 1. Immutable hard deny — never bypassable.
	if IsCredentialPath(target) || IsGoogleTarget(target) || IsGoogleTarget(tool) {
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
	case "read", "read_file", "glob", "grep", "lsp", "kb_query", "skill", "ask_question":
		return Allow
	case "bash":
		if IsReadOnlyBash(target) && !IsDestructiveBash(target) {
			return Allow
		}
		return Deny
	case "edit", "edit_file", "write", "write_file":
		if IsPlanWritable(workspaceDir, target) {
			return Allow
		}
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
	if t == "bash" {
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
