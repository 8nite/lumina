package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/8nite/lumina/internal/kb"
	"github.com/8nite/lumina/internal/tools"
)

// kbback.go wires tool backends that need CLI-level resources: kb_query and
// skill both resolve against the live workspace (TP-04.4).

// wireToolBackends attaches kb_query and skill executors to a registry.
func wireToolBackends(reg *tools.Registry) {
	reg.KBQuery = func(ctx context.Context, workspace, query string, budget int) (string, error) {
		return kbQueryText(workspace, query, budget)
	}
	reg.Skill = func(workspace, name string) (string, error) {
		return skillBody(workspace, name)
	}
}

// kbQueryText runs constrained-vocab graph traversal + wikilink follows.
func kbQueryText(workspaceDir, query string, budget int) (string, error) {
	if budget <= 0 {
		budget = 4000
	}
	var g *kb.Graph
	var gpath string
	for _, p := range []string{
		filepath.Join(workspaceDir, "memory", "graphify-out", "graph.json"),
		filepath.Join(workspaceDir, "graphify-out", "graph.json"),
	} {
		if gg, err := kb.LoadGraph(p); err == nil {
			g, gpath = gg, p
			break
		}
	}
	if g == nil {
		return "", fmt.Errorf("kb_query: no graph.json found (run graphify first)")
	}
	toks := g.ExpandVocab(query, 12)
	var sb strings.Builder
	fmt.Fprintf(&sb, "graph: %s\nvocab: %s\n", gpath, strings.Join(toks, ", "))
	used := 0
	for _, t := range toks {
		for _, n := range g.BFS(t, 8) {
			line := "- " + n + "\n"
			if used+len(line) > budget {
				break
			}
			sb.WriteString(line)
			used += len(line)
		}
	}
	notes := kb.ResolveWikilinks(filepath.Join(workspaceDir, "memory"), "MEMORY", 5)
	fmt.Fprintf(&sb, "(resolved %d memory notes)\n", len(notes))
	return sb.String(), nil
}

// skillBody loads a SKILL.md from workspace, harness, or global roots.
func skillBody(workspaceDir, name string) (string, error) {
	home, _ := os.UserHomeDir()
	safe := filepath.Base(name) // no traversal
	for _, p := range []string{
		filepath.Join(workspaceDir, ".lumina", "skills", safe, "SKILL.md"),
		filepath.Join(workspaceDir, ".lumina", "harness", "skills", safe, "SKILL.md"),
		filepath.Join(home, ".config", "lumina", "skills", safe, "SKILL.md"),
	} {
		if b, err := os.ReadFile(p); err == nil {
			return string(b), nil
		}
	}
	return "", fmt.Errorf("skill: %q not found", name)
}
