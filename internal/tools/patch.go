package tools

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/8nite/lumina/internal/security"
)

// This file implements the `patch` tool: opencode's `*** Begin Patch ***`
// envelope (Add/Update/Delete File + `@@` context hunks). It is the
// multi-file counterpart to edit/write: one call can create, rewrite, move,
// and delete files atomically per hunk, failing with partial progress like
// opencode's PatchTool.

// patchHunk is one parsed file operation.
type patchHunk struct {
	typ      string // "add" | "update" | "delete"
	path     string
	movePath string
	contents string // add only
	chunks   []patchChunk
}

// patchChunk is one @@ section of an update hunk.
type patchChunk struct {
	oldLines      []string
	newLines      []string
	changeContext string
	endOfFile     bool
}

func parsePatch(patchText string) ([]patchHunk, error) {
	text := stripPatchHeredoc(strings.TrimSpace(patchText))
	lines := strings.Split(text, "\n")
	for i, l := range lines {
		lines[i] = strings.TrimSuffix(l, "\r")
	}
	if len(lines) == 0 || strings.TrimSpace(lines[0]) != "*** Begin Patch" {
		return nil, fmt.Errorf("patch: first line must be '*** Begin Patch'")
	}
	end := -1
	for i := len(lines) - 1; i >= 0; i-- {
		if strings.TrimSpace(lines[i]) == "*** End Patch" {
			end = i
			break
		}
	}
	if end <= 0 {
		return nil, fmt.Errorf("patch: last line must be '*** End Patch'")
	}
	var hunks []patchHunk
	i := 1
	for i < end {
		header := strings.TrimSpace(lines[i])
		switch {
		case strings.HasPrefix(header, "*** Add File: "):
			p := strings.TrimSpace(strings.TrimPrefix(header, "*** Add File: "))
			if p == "" {
				return nil, fmt.Errorf("patch: line %d: Add File needs a path", i+1)
			}
			var content []string
			i++
			for i < end && !isPatchBoundary(strings.TrimSpace(lines[i])) {
				if !strings.HasPrefix(lines[i], "+") {
					return nil, fmt.Errorf("patch: line %d: Add File line for '%s' must start with '+', got '%s'", i+1, p, strings.TrimSpace(lines[i]))
				}
				content = append(content, lines[i][1:])
				i++
			}
			hunks = append(hunks, patchHunk{typ: "add", path: p, contents: strings.Join(content, "\n")})
		case strings.HasPrefix(header, "*** Delete File: "):
			p := strings.TrimSpace(strings.TrimPrefix(header, "*** Delete File: "))
			if p == "" {
				return nil, fmt.Errorf("patch: line %d: Delete File needs a path", i+1)
			}
			i++
			if i < end && !isPatchBoundary(strings.TrimSpace(lines[i])) {
				if strings.HasPrefix(strings.TrimSpace(lines[i]), "*** ") {
					return nil, fmt.Errorf("patch: line %d: '%s' is not a valid hunk header", i+1, strings.TrimSpace(lines[i]))
				}
				return nil, fmt.Errorf("patch: line %d: Delete hunks do not contain body lines (got '%s')", i+1, strings.TrimSpace(lines[i]))
			}
			hunks = append(hunks, patchHunk{typ: "delete", path: p})
		case strings.HasPrefix(header, "*** Update File: "):
			p := strings.TrimSpace(strings.TrimPrefix(header, "*** Update File: "))
			if p == "" {
				return nil, fmt.Errorf("patch: line %d: Update File needs a path", i+1)
			}
			i++
			var move string
			for i < end && strings.TrimSpace(lines[i]) == "*** End of File" {
				i++
			}
			if mv := strings.TrimSpace(lines[i]); mv == "*** Move to:" || strings.HasPrefix(mv, "*** Move to: ") {
				move = strings.TrimSpace(strings.TrimPrefix(mv, "*** Move to:"))
				if move == "" {
					return nil, fmt.Errorf("patch: line %d: Move destination for '%s' must not be empty", i+1, p)
				}
				i++
			}
			chunks, next, err := parsePatchChunks(lines, i, end, p)
			if err != nil {
				return nil, err
			}
			hunks = append(hunks, patchHunk{typ: "update", path: p, movePath: move, chunks: chunks})
			i = next
		default:
			return nil, fmt.Errorf("patch: line %d: '%s' is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'", i+1, header)
		}
	}
	return hunks, nil
}

func isPatchBoundary(line string) bool {
	return line == "*** End Patch" ||
		strings.HasPrefix(line, "*** Add File: ") ||
		strings.HasPrefix(line, "*** Delete File: ") ||
		strings.HasPrefix(line, "*** Update File: ")
}

func parsePatchChunks(lines []string, start, end int, path string) ([]patchChunk, int, error) {
	var chunks []patchChunk
	i := start
	afterEOF := false
	for i < end {
		line := lines[i]
		update := strings.TrimSuffix(line, "\r")
		if afterEOF {
			if update == "" {
				i++
				continue
			}
			if update == "@@" || strings.HasPrefix(update, "@@ ") {
				afterEOF = false
			} else if isPatchBoundary(update) {
				break
			} else {
				return nil, 0, fmt.Errorf("patch: line %d: expected update hunk to start with a @@ context marker, got '%s'", i+1, line)
			}
		}
		if update == "*** End of File" {
			if len(chunks) > 0 {
				last := &chunks[len(chunks)-1]
				if len(last.oldLines) == 0 && len(last.newLines) == 0 {
					return nil, 0, fmt.Errorf("patch: line %d: update hunk does not contain any lines", i+1)
				}
				last.endOfFile = true
				afterEOF = true
			}
			i++
			continue
		}
		if isPatchBoundary(update) {
			break
		}
		if update == "@@" || strings.HasPrefix(update, "@@ ") {
			if len(chunks) > 0 {
				last := &chunks[len(chunks)-1]
				if len(last.oldLines) == 0 && len(last.newLines) == 0 {
					return nil, 0, fmt.Errorf("patch: line %d: '%s' is not valid here; every line should start with ' ' (context), '+' (added), or '-' (removed)", i+1, line)
				}
			}
			var ctx string
			if strings.HasPrefix(update, "@@ ") {
				ctx = strings.TrimPrefix(update, "@@ ")
			}
			chunks = append(chunks, patchChunk{changeContext: ctx})
			i++
			continue
		}
		if len(chunks) == 0 {
			chunks = append(chunks, patchChunk{})
		}
		last := &chunks[len(chunks)-1]
		switch {
		case line == "":
			last.oldLines = append(last.oldLines, "")
			last.newLines = append(last.newLines, "")
			i++
		case strings.HasPrefix(line, " "):
			last.oldLines = append(last.oldLines, line[1:])
			last.newLines = append(last.newLines, line[1:])
			i++
		case strings.HasPrefix(line, "-"):
			last.oldLines = append(last.oldLines, line[1:])
			i++
		case strings.HasPrefix(line, "+"):
			last.newLines = append(last.newLines, line[1:])
			i++
		default:
			populated := len(last.oldLines) > 0 || len(last.newLines) > 0
			if populated {
				return nil, 0, fmt.Errorf("patch: line %d: expected update hunk to start with a @@ context marker, got '%s'", i+1, line)
			}
			return nil, 0, fmt.Errorf("patch: line %d: '%s' is not valid; every line should start with ' ' (context), '+' (added), or '-' (removed)", i+1, line)
		}
	}
	if len(chunks) == 0 {
		return nil, 0, fmt.Errorf("patch: update hunk for '%s' is empty", path)
	}
	last := chunks[len(chunks)-1]
	if len(last.oldLines) == 0 && len(last.newLines) == 0 {
		return nil, 0, fmt.Errorf("patch: update hunk does not contain any lines")
	}
	return chunks, i, nil
}

// stripPatchHeredoc unwraps `cat <<EOF ... EOF` quoting like opencode does.
func stripPatchHeredoc(input string) string {
	if !strings.HasPrefix(input, "cat ") {
		lines := strings.SplitN(input, "\n", 2)
		if len(lines) == 2 && strings.HasPrefix(strings.TrimSpace(lines[0]), "<<") {
			rest := strings.TrimSpace(lines[0])
			marker := strings.Trim(strings.TrimPrefix(rest, "<<"), `"'`)
			bl := strings.Split(lines[1], "\n")
			if len(bl) > 0 && strings.TrimSpace(bl[len(bl)-1]) == marker {
				return strings.Join(bl[:len(bl)-1], "\n")
			}
		}
		return input
	}
	lines := strings.SplitN(input, "\n", 2)
	if len(lines) != 2 {
		return input
	}
	first := strings.TrimSpace(lines[0])
	if !strings.HasPrefix(first, "cat <<") {
		return input
	}
	marker := strings.Trim(strings.TrimSpace(strings.TrimPrefix(first, "cat <<")), `"'`)
	bl := strings.Split(lines[1], "\n")
	if len(bl) > 0 && strings.TrimSpace(bl[len(bl)-1]) == marker {
		return strings.Join(bl[:len(bl)-1], "\n")
	}
	return input
}

// patchCompares are the opencode seek fallbacks: exact, rstrip, trim,
// normalized (typographic quote/dash/space variants).
func patchEqual(a, b string) bool { return a == b }

func patchEqualRstrip(a, b string) bool {
	return strings.TrimRight(a, " \t") == strings.TrimRight(b, " \t")
}

func patchEqualTrim(a, b string) bool { return strings.TrimSpace(a) == strings.TrimSpace(b) }

func patchEqualNormalized(a, b string) bool {
	return normalizePatchLine(strings.TrimSpace(a)) == normalizePatchLine(strings.TrimSpace(b))
}

func normalizePatchLine(s string) string {
	r := strings.NewReplacer(
		"‘", "'", "’", "'", "‚", "'", "‛", "'",
		"“", `"`, "”", `"`, "„", `"`, "‟", `"`,
		"‐", "-", "‑", "-", "‒", "-", "–", "-", "—", "-", "―", "-", "−", "-",
	)
	s = r.Replace(s)
	return strings.Map(func(ch rune) rune {
		switch ch {
		case ' ', '　', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', '​':
			return ' '
		}
		return ch
	}, s)
}

var patchCompares = []func(a, b string) bool{patchEqual, patchEqualRstrip, patchEqualTrim, patchEqualNormalized}

func patchSeek(lines, pattern []string, start int, eof bool) int {
	if len(pattern) == 0 {
		return -1
	}
	if eof {
		off := len(lines) - len(pattern)
		if off < start {
			return -1
		}
		for _, cmp := range patchCompares {
			if patchMatch(lines, pattern, off, cmp) {
				return off
			}
		}
		return -1
	}
	for _, cmp := range patchCompares {
		for off := start; off <= len(lines)-len(pattern); off++ {
			if patchMatch(lines, pattern, off, cmp) {
				return off
			}
		}
	}
	return -1
}

func patchMatch(lines, pattern []string, off int, cmp func(a, b string) bool) bool {
	for j, p := range pattern {
		if !cmp(lines[off+j], p) {
			return false
		}
	}
	return true
}

type patchReplacement struct {
	start  int
	remove int
	insert []string
}

// derivePatch applies update chunks to original text (opencode derive parity).
func derivePatch(filePath, original string, chunks []patchChunk) (string, error) {
	lines := strings.Split(original, "\n")
	if len(lines) > 0 && lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	var reps []patchReplacement
	lineIndex := 0
	for _, c := range chunks {
		if c.changeContext != "" {
			found := patchSeek(lines, []string{c.changeContext}, lineIndex, false)
			if found == -1 {
				return "", fmt.Errorf("patch: failed to find context '%s' in %s", c.changeContext, filePath)
			}
			// The body usually repeats the anchor as its first context
			// line, so resume AT the anchor, not past it.
			lineIndex = found
		}
		if len(c.oldLines) == 0 {
			reps = append(reps, patchReplacement{start: len(lines), remove: 0, insert: c.newLines})
			continue
		}
		oldLines, newLines := c.oldLines, c.newLines
		found := patchSeek(lines, oldLines, lineIndex, c.endOfFile)
		if found == -1 && len(oldLines) > 0 && oldLines[len(oldLines)-1] == "" {
			oldLines = oldLines[:len(oldLines)-1]
			if len(newLines) > 0 && newLines[len(newLines)-1] == "" {
				newLines = newLines[:len(newLines)-1]
			}
			found = patchSeek(lines, oldLines, lineIndex, c.endOfFile)
		}
		if found == -1 {
			if len(c.oldLines) > 0 {
				blank := true
				for _, l := range c.oldLines {
					if l != "" {
						blank = false
					}
				}
				if blank {
					return "", fmt.Errorf("patch: failed to find expected blank lines in %s", filePath)
				}
			}
			return "", fmt.Errorf("patch: failed to find expected lines in %s:\n%s", filePath, strings.Join(c.oldLines, "\n"))
		}
		reps = append(reps, patchReplacement{start: found, remove: len(oldLines), insert: newLines})
		// Advance past the matched region so the next chunk cannot
		// re-match the same lines.
		lineIndex = found + len(oldLines)
		lineIndex = found + len(oldLines)
	}
	sort.Slice(reps, func(i, j int) bool { return reps[i].start < reps[j].start })
	for k := len(reps) - 1; k >= 0; k-- {
		r := reps[k]
		lines = append(lines[:r.start], append(append([]string{}, r.insert...), lines[r.start+r.remove:]...)...)
	}
	if len(lines) > 0 && lines[len(lines)-1] != "" {
		lines = append(lines, "")
	}
	return strings.Join(lines, "\n"), nil
}

// doPatch executes the patch tool: parse, per-file gate, apply in order.
// Files outside the workspace or matching credential shapes are denied;
// plan-mode agents may only touch plan.md / .lumina/plans/*.md.
func (r *Registry) doPatch(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	raw := strArg(args, "patchText", "patch_text", "patch")
	if raw == "" {
		return Result{}, fmt.Errorf("patch: missing patchText (wrap operations in *** Begin Patch *** / *** End Patch ***)")
	}
	hunks, err := parsePatch(raw)
	if err != nil {
		return Result{}, err
	}
	if len(hunks) == 0 {
		return Result{}, fmt.Errorf("patch: no file operations found")
	}
	for _, h := range hunks {
		for _, p := range []string{h.path, h.movePath} {
			if p == "" {
				continue
			}
			resolved := p
			if !filepath.IsAbs(p) {
				resolved = filepath.Join(sc.WorkspaceDir, p)
			}
			if !insideWorkspace(sc.WorkspaceDir, resolved) {
				return Result{}, fmt.Errorf("patch: path escapes workspace: %s", p)
			}
			if security.IsCredentialPath(resolved) {
				return Result{}, fmt.Errorf("patch: credential path denied: %s", p)
			}
			if sc.Agent == security.AgentPlan && !security.IsPlanWritable(sc.WorkspaceDir, resolved) {
				return Result{}, fmt.Errorf("patch: plan-mode agents may only change plan.md or .lumina/plans/*.md (got %s)", p)
			}
		}
	}
	var applied []string
	fail := func(op string, err error) (Result, error) {
		msg := fmt.Sprintf("%s: %v", op, err)
		if len(applied) > 0 {
			msg += ". Completed before failure: " + strings.Join(applied, ", ")
		}
		return Result{}, fmt.Errorf("patch: %s", msg)
	}
	resolve := func(p string) string {
		if filepath.IsAbs(p) {
			return p
		}
		return filepath.Join(sc.WorkspaceDir, p)
	}
	for _, h := range hunks {
		switch h.typ {
		case "add":
			target := resolve(h.path)
			if _, err := os.Stat(target); err == nil {
				return fail("add "+h.path, fmt.Errorf("file already exists (use edit to change it)"))
			}
			if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
				return fail("add "+h.path, err)
			}
			content := h.contents
			if content != "" && !strings.HasSuffix(content, "\n") {
				content += "\n"
			}
			if err := os.WriteFile(target, []byte(content), 0o644); err != nil {
				return fail("add "+h.path, err)
			}
			applied = append(applied, "A "+h.path)
		case "delete":
			target := resolve(h.path)
			if err := os.Remove(target); err != nil {
				return fail("delete "+h.path, err)
			}
			applied = append(applied, "D "+h.path)
		case "update":
			target := resolve(h.path)
			b, err := os.ReadFile(target)
			if err != nil {
				return fail("update "+h.path, err)
			}
			next, err := derivePatch(h.path, string(b), h.chunks)
			if err != nil {
				return fail("update "+h.path, err)
			}
			out := target
			if h.movePath != "" {
				out = resolve(h.movePath)
				if err := os.MkdirAll(filepath.Dir(out), 0o755); err != nil {
					return fail("update "+h.path, err)
				}
			}
			if err := os.WriteFile(out, []byte(next), 0o644); err != nil {
				return fail("update "+h.path, err)
			}
			if h.movePath != "" {
				_ = os.Remove(target)
			}
			applied = append(applied, "M "+h.path)
		}
	}
	return Result{Output: "Success. Updated the following files:\n" + strings.Join(applied, "\n")}, nil
}

// patchFirstPath returns the first file path in a patch (for the permission
// broker target). Falls back to "patch" when unparseable.
func patchFirstPath(patchText string) string {
	for _, line := range strings.Split(patchText, "\n") {
		t := strings.TrimSpace(line)
		for _, prefix := range []string{"*** Add File: ", "*** Update File: ", "*** Delete File: "} {
			if strings.HasPrefix(t, prefix) {
				if p := strings.TrimSpace(strings.TrimPrefix(t, prefix)); p != "" {
					return p
				}
			}
		}
	}
	return "patch"
}
