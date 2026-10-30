// Package rules loads the hierarchical AGENTS.md / instructions / skills /
// agents / commands Markdown extensions with hierarchical discovery.
package rules

import (
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// ImmutableSecurityCore is Layer 0: compiled directives that user Markdown
// can never weaken (enforced by internal/security at call time).
const ImmutableSecurityCore = `# Lumina Immutable Security Core (Layer 0)
- Workspace fence: agents may only touch the session workspace_dir (+ TMPDIR).
- Credential files (.env, auth.json, lumina.json, *secret*, *credential*, *.pem) are HARD DENY in every mode.
- Google services (googleapis.com, generativelanguage, gemini, google-workspace) are HARD DENY.
- plan-mode agents are read-only except plan.md / .lumina/plans/*.md.
`

type SkillMeta struct {
	Name        string
	Description string
	Path        string
}

type AgentDef struct {
	Name        string
	Description string
	Mode        string // "primary" | "subagent"
	Model       string
	Tools       map[string]bool
	Path        string
	Body        string
}

type CommandDef struct {
	Name string
	Path string
	Body string
}

type Bundle struct {
	SystemPrompt string
	Layers       []string // files merged, top-to-bottom
	Skills       []SkillMeta
	Agents       []AgentDef
	Commands     []CommandDef
}

// Options controls discovery roots.
type Options struct {
	WorkspaceDir       string
	ImportGlobalRules  bool
	ImportGlobalSkills bool
}

func homeDir() string {
	h, _ := os.UserHomeDir()
	return h
}

func readIfExists(p string) string {
	b, err := os.ReadFile(p)
	if err != nil {
		return ""
	}
	return string(b)
}

// Load merges Layer 0..3 system instructions and discovers extensions.
func Load(o Options) *Bundle {
	b := &Bundle{}
	var sb strings.Builder
	sb.WriteString(ImmutableSecurityCore)
	sb.WriteString("\n")

	addLayer := func(path, content string) {
		if strings.TrimSpace(content) == "" {
			return
		}
		b.Layers = append(b.Layers, path)
		sb.WriteString("\n--- layer: " + path + " ---\n")
		sb.WriteString(content)
		if !strings.HasSuffix(content, "\n") {
			sb.WriteString("\n")
		}
	}

	// Layer 1 — global rules.
	if o.ImportGlobalRules {
		gp := filepath.Join(homeDir(), ".config", "lumina", "AGENTS.md")
		addLayer(gp, readIfExists(gp))
		for _, f := range globFiles(filepath.Join(homeDir(), ".config", "lumina", "instructions", "*.md")) {
			addLayer(f, readIfExists(f))
		}
		// Read-only compat import (Claude rules only).
		cl := filepath.Join(homeDir(), ".claude", "CLAUDE.md")
		addLayer(cl, readIfExists(cl))
	}

	// Layer 2 — project & directory rules: git root -> workspace dir.
	if o.WorkspaceDir != "" {
		root := gitRoot(o.WorkspaceDir)
		chain := dirChain(root, o.WorkspaceDir)
		for _, d := range chain {
			if p := filepath.Join(d, "AGENTS.md"); fileExists(p) {
				addLayer(p, readIfExists(p))
			} else if d == root {
				if p := filepath.Join(d, "CLAUDE.md"); fileExists(p) {
					addLayer(p, readIfExists(p)) // fallback when AGENTS.md absent
				}
			}
		}
		for _, f := range globFiles(filepath.Join(o.WorkspaceDir, ".lumina", "instructions", "*.md")) {
			addLayer(f, readIfExists(f))
		}
		if o.ImportGlobalSkills {
			b.Skills = append(b.Skills, discoverSkillsFromRoots([]string{filepath.Join(homeDir(), ".config", "lumina", "skills")})...)
		}
		b.Skills = append(b.Skills, discoverSkillsFromRoots([]string{
			filepath.Join(o.WorkspaceDir, ".lumina", "skills"),
			filepath.Join(o.WorkspaceDir, ".lumina", "harness", "skills"),
		})...)
		b.Agents = append(discoverAgents(o.WorkspaceDir), BuiltinAgents()...)
		b.Commands = discoverCommands(o.WorkspaceDir)
	}

	// Layer 3 — mutable Harness² overlay: repo root first (shared), then the
	// workspace directory when it differs (scoped overrides last).
	if o.WorkspaceDir != "" {
		hroot := gitRoot(o.WorkspaceDir)
		addHarness := func(base string) {
			sp := filepath.Join(base, ".lumina", "harness", "system_prompt.md")
			addLayer(sp, readIfExists(sp))
			tg := filepath.Join(base, ".lumina", "harness", "tool_guidance.md")
			addLayer(tg, readIfExists(tg))
		}
		addHarness(hroot)
		if o.WorkspaceDir != hroot {
			addHarness(o.WorkspaceDir)
		}
	}

	b.SystemPrompt = sb.String()
	sort.Slice(b.Skills, func(i, j int) bool { return b.Skills[i].Name < b.Skills[j].Name })
	return b
}

func fileExists(p string) bool {
	st, err := os.Stat(p)
	return err == nil && !st.IsDir()
}

func globFiles(pattern string) []string {
	m, _ := filepath.Glob(pattern)
	sort.Strings(m)
	return m
}

func gitRoot(dir string) string {
	d := dir
	for {
		// Marker precedence: HEAD, .git file (worktree/submodule), or a
		// bare .git directory (fixtures, partially-initialized clones).
		if fileExists(filepath.Join(d, ".git", "HEAD")) ||
			fileExists(filepath.Join(d, ".git")) ||
			dirExists(filepath.Join(d, ".git")) {
			return d
		}
		parent := filepath.Dir(d)
		if parent == d {
			return dir
		}
		d = parent
	}
}

func dirExists(p string) bool {
	st, err := os.Stat(p)
	return err == nil && st.IsDir()
}

func dirChain(root, dir string) []string {
	var out []string
	cur := root
	out = append(out, cur)
	if root == dir {
		return out
	}
	rel, err := filepath.Rel(root, dir)
	if err != nil {
		return []string{dir}
	}
	for _, part := range strings.Split(rel, string(filepath.Separator)) {
		if part == "." || part == "" {
			continue
		}
		cur = filepath.Join(cur, part)
		out = append(out, cur)
	}
	return out
}

// parseFrontmatter extracts simple `key: value` YAML frontmatter.
func parseFrontmatter(content string) (map[string]string, string) {
	meta := map[string]string{}
	if !strings.HasPrefix(content, "---") {
		return meta, content
	}
	end := strings.Index(content[3:], "\n---")
	if end < 0 {
		return meta, content
	}
	fm := content[3 : 3+end]
	rest := content[3+end+4:]
	for _, line := range strings.Split(fm, "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		kv := strings.SplitN(line, ":", 2)
		if len(kv) != 2 {
			continue
		}
		meta[strings.TrimSpace(kv[0])] = strings.Trim(strings.TrimSpace(kv[1]), `"'`)
	}
	return meta, strings.TrimLeft(rest, "\n")
}

// discoverSkillsFromRoots scans <root>/<name>/SKILL.md pairs.
func discoverSkillsFromRoots(roots []string) []SkillMeta {
	var out []SkillMeta
	for _, root := range roots {
		entries, err := os.ReadDir(root)
		if err != nil {
			continue
		}
		for _, e := range entries {
			if !e.IsDir() {
				continue
			}
			p := filepath.Join(root, e.Name(), "SKILL.md")
			content := readIfExists(p)
			if content == "" {
				continue
			}
			meta, _ := parseFrontmatter(content)
			name := meta["name"]
			if name == "" {
				name = e.Name()
			}
			out = append(out, SkillMeta{Name: name, Description: meta["description"], Path: p})
		}
	}
	return out
}

// BuiltinAgents lists the shipped subagents (reimplemented):
// general handles multi-step work with broad tools but cannot delegate;
// explore is read-only research over code and the web.
func BuiltinAgents() []AgentDef {
	return []AgentDef{
		{
			Name:        "general",
			Description: "Handles research and multi-step work with broad tool access, but cannot launch subagents.",
			Mode:        "subagent",
			Body: "You are a general-purpose subagent. Research the workspace, run safe diagnostics, " +
				"and complete multi-step tasks. Prefer kb_query and grep before broad edits. " +
				"Summarize what you did with file references.",
		},
		{
			Name:        "explore",
			Description: "Searches and reads code or web sources without editing files.",
			Mode:        "subagent",
			Body: "You are an explore subagent. Search and read code (read, glob, grep, lsp), " +
				"memory (kb_query, skill), and the web (webfetch, websearch). " +
				"Do not edit files. Report findings with file and line references.",
		},
	}
}

func discoverAgents(workspace string) []AgentDef {
	var out []AgentDef
	roots := []string{filepath.Join(homeDir(), ".config", "lumina", "agents")}
	if workspace != "" {
		roots = append(roots, filepath.Join(workspace, ".lumina", "agents"))
	}
	for _, root := range roots {
		for _, f := range globFiles(filepath.Join(root, "*.md")) {
			content := readIfExists(f)
			meta, body := parseFrontmatter(content)
			name := meta["name"]
			if name == "" {
				name = strings.TrimSuffix(filepath.Base(f), ".md")
			}
			out = append(out, AgentDef{
				Name: name, Description: meta["description"],
				Mode:  orDefault(meta["mode"], "subagent"),
				Model: meta["model"], Path: f, Body: body,
				Tools: map[string]bool{},
			})
		}
	}
	return out
}

func discoverCommands(workspace string) []CommandDef {
	var out []CommandDef
	roots := []string{filepath.Join(homeDir(), ".config", "lumina", "commands")}
	if workspace != "" {
		roots = append(roots, filepath.Join(workspace, ".lumina", "commands"))
	}
	for _, root := range roots {
		for _, f := range globFiles(filepath.Join(root, "*.md")) {
			out = append(out, CommandDef{
				Name: strings.TrimSuffix(filepath.Base(f), ".md"),
				Path: f, Body: readIfExists(f),
			})
		}
	}
	return out
}

func orDefault(v, d string) string {
	if v == "" {
		return d
	}
	return v
}
