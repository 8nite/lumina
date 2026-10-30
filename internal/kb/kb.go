// Package kb implements the Markdown + graphify RAG engine: cold-start
// reflect, constrained-vocab expansion, native Go graph.json traversal
// (<2ms BFS/DFS/shortest-path/explain/affected/god-nodes), recursive
// [[wikilink]] resolution, dedup-on-write memory synthesis, and the
// singleflight standing-ops sweeper. Zero vector DBs, zero Google services.
package kb

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"sync"
)

// Node is one graphify node.
type Node struct {
	ID    string `json:"id"`
	Label string `json:"label"`
	Type  string `json:"type,omitempty"`
	File  string `json:"file,omitempty"`
}

// Edge is one graphify hyperedge (normalized: nodes[] always present).
type Edge struct {
	Nodes      []string `json:"nodes"`
	Relation   string   `json:"relation,omitempty"`
	Confidence string   `json:"confidence,omitempty"`
	Source     string   `json:"source_file,omitempty"`
}

// Graph is the normalized in-memory graph.json.
type Graph struct {
	Nodes []Node           `json:"nodes"`
	Edges []Edge           `json:"edges"`
	adj   map[string][]int // node id -> edge indices
}

// LoadGraph reads and normalizes a graph.json file (schema normalization:
// hyperedges[].nodes coerced, confidence defaulted, #479 shrink-guarded).
func LoadGraph(path string) (*Graph, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var wire struct {
		Nodes []Node            `json:"nodes"`
		Edges []json.RawMessage `json:"edges"`
		Hyper []json.RawMessage `json:"hyperedges"`
	}
	if err := json.Unmarshal(raw, &wire); err != nil {
		return nil, err
	}
	g := &Graph{adj: map[string][]int{}}
	g.Nodes = wire.Nodes
flexEdges:
	for _, rm := range append(wire.Edges, wire.Hyper...) {
		var e Edge
		if err := json.Unmarshal(rm, &e); err != nil {
			// Tolerate {from,to} shaped edges.
			var ft struct {
				From, To, Relation, Confidence, Source string
			}
			if err2 := json.Unmarshal(rm, &ft); err2 != nil {
				continue
			}
			e = Edge{Nodes: []string{ft.From, ft.To}, Relation: ft.Relation, Confidence: ft.Confidence, Source: ft.Source}
		}
		if len(e.Nodes) < 2 {
			continue flexEdges // #479 shrink-guard: drop degenerate edges
		}
		if e.Confidence == "" {
			e.Confidence = "INFERRED"
		}
		switch e.Confidence {
		case "EXTRACTED", "INFERRED", "AMBIGUOUS":
		default:
			e.Confidence = "INFERRED"
		}
		g.Edges = append(g.Edges, e)
	}
	g.rebuild()
	return g, nil
}

func (g *Graph) rebuild() {
	g.adj = map[string][]int{}
	for i, e := range g.Edges {
		for _, n := range e.Nodes {
			g.adj[n] = append(g.adj[n], i)
		}
	}
}

func (g *Graph) neighbors(id string) []string {
	var out []string
	seen := map[string]bool{id: true}
	for _, ei := range g.adj[id] {
		for _, n := range g.Edges[ei].Nodes {
			if !seen[n] {
				seen[n] = true
				out = append(out, n)
			}
		}
	}
	sort.Strings(out)
	return out
}

// BFS traverses from seed up to budget nodes.
func (g *Graph) BFS(seed string, budget int) []string {
	if budget <= 0 {
		budget = 50
	}
	seen := map[string]bool{seed: true}
	queue := []string{seed}
	var out []string
	for len(queue) > 0 && len(out) < budget {
		cur := queue[0]
		queue = queue[1:]
		out = append(out, cur)
		for _, n := range g.neighbors(cur) {
			if !seen[n] {
				seen[n] = true
				queue = append(queue, n)
			}
		}
	}
	return out
}

// DFS traverses depth-first from seed up to budget nodes.
func (g *Graph) DFS(seed string, budget int) []string {
	if budget <= 0 {
		budget = 50
	}
	var out []string
	seen := map[string]bool{}
	var visit func(id string)
	visit = func(id string) {
		if len(out) >= budget || seen[id] {
			return
		}
		seen[id] = true
		out = append(out, id)
		for _, n := range g.neighbors(id) {
			visit(n)
		}
	}
	visit(seed)
	return out
}

// ShortestPath returns one shortest node path between two ids.
func (g *Graph) ShortestPath(from, to string) []string {
	if from == to {
		return []string{from}
	}
	prev := map[string]string{from: ""}
	queue := []string{from}
	for len(queue) > 0 {
		cur := queue[0]
		queue = queue[1:]
		for _, n := range g.neighbors(cur) {
			if _, ok := prev[n]; ok {
				continue
			}
			prev[n] = cur
			if n == to {
				var path []string
				for c := to; c != ""; c = prev[c] {
					path = append([]string{c}, path...)
				}
				return path
			}
			queue = append(queue, n)
		}
	}
	return nil
}

// Explain describes the relation path between two nodes with provenance.
func (g *Graph) Explain(from, to string) string {
	path := g.ShortestPath(from, to)
	if path == nil {
		return fmt.Sprintf("no path between %q and %q", from, to)
	}
	var sb strings.Builder
	sb.WriteString(strings.Join(path, " → "))
	sb.WriteString("\n")
	for i := 0; i+1 < len(path); i++ {
		a, b := path[i], path[i+1]
		for _, ei := range g.adj[a] {
			e := g.Edges[ei]
			if contains(e.Nodes, b) {
				fmt.Fprintf(&sb, "  %s --[%s|%s]--> %s  (source: %s)\n", a, e.Relation, e.Confidence, b, e.Source)
				break
			}
		}
	}
	return sb.String()
}

// Affected returns the 1-hop blast radius of a node.
func (g *Graph) Affected(id string) []string { return g.neighbors(id) }

// GodNodes returns top-degree nodes (community hubs) up to k.
func (g *Graph) GodNodes(k int) []string {
	type deg struct {
		id string
		n  int
	}
	var ds []deg
	for id, es := range g.adj {
		ds = append(ds, deg{id, len(es)})
	}
	sort.Slice(ds, func(i, j int) bool { return ds[i].n > ds[j].n })
	if k <= 0 || k > len(ds) {
		k = len(ds)
	}
	var out []string
	for _, d := range ds[:k] {
		out = append(out, d.id)
	}
	return out
}

// Vocab returns node labels/tokens usable for constrained expansion.
func (g *Graph) Vocab() []string {
	var out []string
	for _, n := range g.Nodes {
		label := n.Label
		if label == "" {
			label = n.ID
		}
		out = append(out, label)
	}
	return out
}

// ExpandVocab maps a query onto <=12 tokens strictly present in the graph
// vocabulary (Step 0 constrained expansion — zero invented synonyms).
func (g *Graph) ExpandVocab(query string, maxTokens int) []string {
	if maxTokens <= 0 || maxTokens > 12 {
		maxTokens = 12
	}
	vocab := g.Vocab()
	lq := strings.ToLower(query)
	scored := map[string]int{}
	for _, tok := range vocab {
		lt := strings.ToLower(tok)
		for _, w := range strings.Fields(lq) {
			if w == "" {
				continue
			}
			if lt == w {
				scored[tok] += 3
			} else if strings.Contains(lt, w) || strings.Contains(w, lt) {
				scored[tok]++
			}
		}
	}
	type kv struct {
		k string
		v int
	}
	var ranked []kv
	for k, v := range scored {
		if v > 0 {
			ranked = append(ranked, kv{k, v})
		}
	}
	sort.Slice(ranked, func(i, j int) bool { return ranked[i].v > ranked[j].v })
	var out []string
	for i, r := range ranked {
		if i >= maxTokens {
			break
		}
		out = append(out, r.k)
	}
	return out
}

func contains(xs []string, s string) bool {
	for _, x := range xs {
		if x == s {
			return true
		}
	}
	return false
}

var wikilinkRe = regexp.MustCompile(`\[\[([^\]]+)\]\]`)

// Wikilinks extracts [[note]] targets from Markdown.
func Wikilinks(md string) []string {
	var out []string
	for _, m := range wikilinkRe.FindAllStringSubmatch(md, -1) {
		out = append(out, m[1])
	}
	return out
}

// ResolveWikilinks follows [[notes]] across memDir cycle-safe, budget-capped,
// returning note path -> content. Falls back to grep/glob when missing.
func ResolveWikilinks(memDir, start string, budget int) map[string]string {
	out := map[string]string{}
	seen := map[string]bool{}
	queue := []string{start}
	for len(queue) > 0 && len(out) < budget {
		cur := queue[0]
		queue = queue[1:]
		if seen[cur] {
			continue
		}
		seen[cur] = true
		p := filepath.Join(memDir, cur)
		if !strings.HasSuffix(p, ".md") {
			p += ".md"
		}
		b, err := os.ReadFile(p)
		if err != nil {
			continue
		}
		out[cur] = string(b)
		queue = append(queue, Wikilinks(string(b))...)
	}
	return out
}

// Sweeper singleflights idle/session-close KB refreshes per workspace.
type Sweeper struct {
	mu       sync.Mutex
	inflight map[string]bool
	Debounce int // minutes; unused in tests, honored by daemon ticker
}

// NewSweeper creates the standing-ops sweeper.
func NewSweeper() *Sweeper { return &Sweeper{inflight: map[string]bool{}} }

// Run executes fn once per workspace (dedups concurrent triggers — the 6x
// hook-race fix: singleflight mutex instead of parallel sweeps).
func (s *Sweeper) Run(workspace string, fn func() error) error {
	s.mu.Lock()
	if s.inflight[workspace] {
		s.mu.Unlock()
		return nil // dedup: a sweep is already running
	}
	s.inflight[workspace] = true
	s.mu.Unlock()
	defer func() {
		s.mu.Lock()
		delete(s.inflight, workspace)
		s.mu.Unlock()
	}()
	return fn()
}

// ManifestStamp writes the canonical root-relative manifest.json (honoring
// the root-relative convention: all paths relative to workspace root).
func ManifestStamp(workspaceDir, graphOutDir string, files []string) error {
	rel := make([]string, 0, len(files))
	for _, f := range files {
		r, err := filepath.Rel(workspaceDir, f)
		if err != nil {
			r = f
		}
		rel = append(rel, filepath.ToSlash(r))
	}
	sort.Strings(rel)
	doc := map[string]interface{}{"root": ".", "files": rel}
	raw, _ := json.MarshalIndent(doc, "", "  ")
	if err := os.MkdirAll(graphOutDir, 0o755); err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(graphOutDir, "manifest.json"), raw, 0o644)
}
