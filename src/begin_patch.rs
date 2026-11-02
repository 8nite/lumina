//! `*** Begin Patch ***` envelope (Add/Update/Delete/Move + `@@`
//! context hunks). The multi-file
//! counterpart to edit/write — one call can create, rewrite, move, and
//! delete files atomically per hunk, failing with partial progress.
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PatchError {
    #[error("patch: {0}")]
    Msg(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

struct Hunk {
    typ: String, // "add" | "update" | "delete"
    path: String,
    move_path: String,
    contents: String, // add only
    chunks: Vec<Chunk>,
}

struct Chunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    change_context: String,
    end_of_file: bool,
}

fn is_patch_begin(line: &str) -> bool {
    line == "*** Begin Patch" || line == "*** Begin Patch ***"
}

fn is_patch_end(line: &str) -> bool {
    line == "*** End Patch" || line == "*** End Patch ***"
}

fn strip_patch_stars(p: &str) -> String {
    let mut s = p.trim().to_string();
    if let Some(stripped) = s.strip_suffix("***") {
        s = stripped.trim().to_string();
    }
    s
}

fn is_patch_boundary(line: &str) -> bool {
    is_patch_end(line)
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

pub fn is_begin_patch(text: &str) -> bool {
    let t = text.trim_start_matches(['\r', '\n', ' ', '\t']);
    // Tolerate heredoc wrappers: peek past a first <<MARKER line.
    let first = t.lines().next().unwrap_or("").trim();
    if is_patch_begin(first) {
        return true;
    }
    if first.starts_with("<<") || first.starts_with("cat ") {
        if let Some(second) = t.lines().nth(1) {
            return is_patch_begin(second.trim());
        }
    }
    false
}

fn parse_chunks(lines: &[String], start: usize, end: usize, path: &str) -> Result<(Vec<Chunk>, usize), PatchError> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut i = start;
    let mut after_eof = false;
    while i < end {
        let line = &lines[i];
        let update = line.strip_suffix('\r').unwrap_or(line);
        if after_eof {
            if update.is_empty() {
                i += 1;
                continue;
            }
            if update == "@@" || update.starts_with("@@ ") {
                after_eof = false;
            } else if is_patch_boundary(update) {
                break;
            } else {
                return Err(PatchError::Msg(format!(
                    "patch: line {}: expected update hunk to start with a @@ context marker, got '{line}'",
                    i + 1
                )));
            }
        }
        if update == "*** End of File" {
            if let Some(last) = chunks.last_mut() {
                if last.old_lines.is_empty() && last.new_lines.is_empty() {
                    return Err(PatchError::Msg(format!(
                        "patch: line {}: update hunk does not contain any lines",
                        i + 1
                    )));
                }
                last.end_of_file = true;
                after_eof = true;
            }
            i += 1;
            continue;
        }
        if is_patch_boundary(update) {
            break;
        }
        if update == "@@" || update.starts_with("@@ ") {
            if let Some(last) = chunks.last() {
                if last.old_lines.is_empty() && last.new_lines.is_empty() {
                    return Err(PatchError::Msg(format!(
                        "patch: line {}: '{line}' is not valid here; every line should start with ' ' (context), '+' (added), or '-' (removed)",
                        i + 1
                    )));
                }
            }
            let ctx = update.strip_prefix("@@ ").unwrap_or("").to_string();
            chunks.push(Chunk { old_lines: vec![], new_lines: vec![], change_context: ctx, end_of_file: false });
            i += 1;
            continue;
        }
        if chunks.is_empty() {
            chunks.push(Chunk { old_lines: vec![], new_lines: vec![], change_context: String::new(), end_of_file: false });
        }
        let last = chunks.last_mut().unwrap();
        if line.is_empty() {
            last.old_lines.push(String::new());
            last.new_lines.push(String::new());
            i += 1;
        } else if let Some(rest) = line.strip_prefix(' ') {
            last.old_lines.push(rest.to_string());
            last.new_lines.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            last.old_lines.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = line.strip_prefix('+') {
            last.new_lines.push(rest.to_string());
            i += 1;
        } else {
            let populated = !last.old_lines.is_empty() || !last.new_lines.is_empty();
            if populated {
                return Err(PatchError::Msg(format!(
                    "patch: line {}: expected update hunk to start with a @@ context marker, got '{line}'",
                    i + 1
                )));
            }
            return Err(PatchError::Msg(format!(
                "patch: line {}: '{line}' is not valid; every line should start with ' ' (context), '+' (added), or '-' (removed)",
                i + 1
            )));
        }
    }
    if chunks.is_empty() {
        return Err(PatchError::Msg(format!("patch: update hunk for '{path}' is empty")));
    }
    let last = &chunks[chunks.len() - 1];
    if last.old_lines.is_empty() && last.new_lines.is_empty() {
        return Err(PatchError::Msg("patch: update hunk does not contain any lines".into()));
    }
    Ok((chunks, i))
}

/// Unwrap `cat <<EOF ... EOF` quoting.
fn strip_patch_heredoc(input: &str) -> String {
    let mut lines = input.splitn(2, '\n');
    let first = lines.next().unwrap_or("");
    let rest = lines.next().unwrap_or("");
    let marker_of = |f: &str| -> Option<String> {
        let f = f.trim();
        let rest = f
            .strip_prefix("cat <<")
            .or_else(|| f.strip_prefix("<<"))?;
        Some(rest.trim().trim_matches(['"', '\'']).to_string())
    };
    if let Some(marker) = marker_of(first) {
        if !marker.is_empty() {
            let mut bl: Vec<&str> = rest.split('\n').collect();
            if bl.last().is_some_and(|l| l.trim() == marker) {
                bl.pop();
                return bl.join("\n");
            }
        }
    }
    input.to_string()
}

fn parse_patch(patch_text: &str) -> Result<Vec<Hunk>, PatchError> {
    let text = strip_patch_heredoc(patch_text.trim());
    let lines: Vec<String> = text.lines().map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect();
    if lines.is_empty() || !is_patch_begin(lines[0].trim()) {
        return Err(PatchError::Msg("patch: first line must be '*** Begin Patch ***'".into()));
    }
    let mut end: Option<usize> = None;
    for (idx, l) in lines.iter().enumerate().rev() {
        if is_patch_end(l.trim()) {
            end = Some(idx);
            break;
        }
    }
    let Some(end) = end else {
        return Err(PatchError::Msg("patch: last line must be '*** End Patch ***'".into()));
    };
    if end == 0 {
        return Err(PatchError::Msg("patch: last line must be '*** End Patch ***'".into()));
    }
    let mut hunks = Vec::new();
    let mut i = 1;
    while i < end {
        let header = lines[i].trim().to_string();
        if let Some(rest) = header.strip_prefix("*** Add File: ") {
            let p = strip_patch_stars(rest);
            if p.is_empty() {
                return Err(PatchError::Msg(format!("patch: line {}: Add File needs a path", i + 1)));
            }
            let mut content = Vec::new();
            i += 1;
            while i < end && !is_patch_boundary(lines[i].trim()) {
                if !lines[i].starts_with('+') {
                    return Err(PatchError::Msg(format!(
                        "patch: line {}: Add File line for '{p}' must start with '+', got '{}'",
                        i + 1,
                        lines[i].trim()
                    )));
                }
                content.push(lines[i][1..].to_string());
                i += 1;
            }
            hunks.push(Hunk { typ: "add".into(), path: p, move_path: String::new(), contents: content.join("\n"), chunks: vec![] });
        } else if let Some(rest) = header.strip_prefix("*** Delete File: ") {
            let p = strip_patch_stars(rest);
            if p.is_empty() {
                return Err(PatchError::Msg(format!("patch: line {}: Delete File needs a path", i + 1)));
            }
            i += 1;
            if i < end && !is_patch_boundary(lines[i].trim()) {
                if lines[i].trim().starts_with("*** ") {
                    return Err(PatchError::Msg(format!(
                        "patch: line {}: '{}' is not a valid hunk header",
                        i + 1,
                        lines[i].trim()
                    )));
                }
                return Err(PatchError::Msg(format!(
                    "patch: line {}: Delete hunks do not contain body lines (got '{}')",
                    i + 1,
                    lines[i].trim()
                )));
            }
            hunks.push(Hunk { typ: "delete".into(), path: p, move_path: String::new(), contents: String::new(), chunks: vec![] });
        } else if let Some(rest) = header.strip_prefix("*** Update File: ") {
            let p = strip_patch_stars(rest);
            if p.is_empty() {
                return Err(PatchError::Msg(format!("patch: line {}: Update File needs a path", i + 1)));
            }
            i += 1;
            while i < end && lines[i].trim() == "*** End of File" {
                i += 1;
            }
            let mut mov = String::new();
            if i < end {
                let mv = lines[i].trim().to_string();
                if mv == "*** Move to:" || mv.starts_with("*** Move to: ") {
                    mov = strip_patch_stars(mv.trim_start_matches("*** Move to:"));
                    if mov.is_empty() {
                        return Err(PatchError::Msg(format!(
                            "patch: line {}: Move destination for '{p}' must not be empty",
                            i + 1
                        )));
                    }
                    i += 1;
                }
            }
            let (chunks, next) = parse_chunks(&lines, i, end, &p)?;
            hunks.push(Hunk { typ: "update".into(), path: p, move_path: mov, contents: String::new(), chunks });
            i = next;
        } else {
            return Err(PatchError::Msg(format!(
                "patch: line {}: '{header}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'",
                i + 1
            )));
        }
    }
    Ok(hunks)
}

fn patch_equal(a: &str, b: &str) -> bool {
    a == b
}

fn patch_equal_rstrip(a: &str, b: &str) -> bool {
    a.trim_end_matches([' ', '\t']) == b.trim_end_matches([' ', '\t'])
}

fn patch_equal_trim(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

fn normalize_patch_line(s: &str) -> String {
    let mut o = s.to_string();
    for (f, t) in [
        ('\u{2018}', '\''), ('\u{2019}', '\''), ('\u{201a}', '\''), ('\u{201b}', '\''),
        ('\u{201c}', '"'), ('\u{201d}', '"'), ('\u{201e}', '"'), ('\u{201f}', '"'),
        ('\u{2010}', '-'), ('\u{2011}', '-'), ('\u{2012}', '-'), ('\u{2013}', '-'),
        ('\u{2014}', '-'), ('\u{2015}', '-'), ('\u{2212}', '-'),
    ] {
        o = o.replace(f, &t.to_string());
    }
    o.chars()
        .map(|ch| match ch {
            '\u{00a0}' | '\u{3000}' | '\u{2007}' | '\u{205f}' | '\u{2005}' | '\u{2004}' | '\u{2002}' | '\u{2003}'
            | '\u{2000}' | '\u{2001}' | '\u{2009}' | '\u{200a}' | '\u{200b}' => ' ',
            _ => ch,
        })
        .collect()
}

fn patch_equal_normalized(a: &str, b: &str) -> bool {
    normalize_patch_line(a.trim()) == normalize_patch_line(b.trim())
}

fn patch_match(lines: &[String], pattern: &[String], off: usize, cmp: fn(&str, &str) -> bool) -> bool {
    pattern.iter().enumerate().all(|(j, p)| lines.get(off + j).is_some_and(|l| cmp(l, p)))
}

/// Fuzzy seek: exact, rstrip, trim, normalized.
fn patch_seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    let cmps: [fn(&str, &str) -> bool; 4] =
        [patch_equal, patch_equal_rstrip, patch_equal_trim, patch_equal_normalized];
    if eof {
        if pattern.len() > lines.len() {
            return None;
        }
        let off = lines.len() - pattern.len();
        if off < start {
            return None;
        }
        for cmp in cmps {
            if patch_match(lines, pattern, off, cmp) {
                return Some(off);
            }
        }
        return None;
    }
    for cmp in cmps {
        let mut off = start;
        while off + pattern.len() <= lines.len() {
            if patch_match(lines, pattern, off, cmp) {
                return Some(off);
            }
            off += 1;
        }
    }
    None
}

struct Replacement {
    start: usize,
    remove: usize,
    insert: Vec<String>,
}

fn derive_patch(file_path: &str, original: &str, chunks: &[Chunk]) -> Result<String, PatchError> {
    let mut lines: Vec<String> = original.split('\n').map(|s| s.to_string()).collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut reps: Vec<Replacement> = Vec::new();
    let mut line_index = 0usize;
    for c in chunks {
        if !c.change_context.is_empty() {
            let Some(found) = patch_seek(&lines, std::slice::from_ref(&c.change_context), line_index, false) else {
                return Err(PatchError::Msg(format!(
                    "patch: failed to find context '{}' in {file_path}",
                    c.change_context
                )));
            };
            // The body usually repeats the anchor as its first context
            // line, so resume AT the anchor, not past it.
            line_index = found;
        }
        if c.old_lines.is_empty() {
            reps.push(Replacement { start: lines.len(), remove: 0, insert: c.new_lines.clone() });
            continue;
        }
        let mut old_lines = c.old_lines.clone();
        let mut new_lines = c.new_lines.clone();
        let mut found = patch_seek(&lines, &old_lines, line_index, c.end_of_file);
        if found.is_none() && old_lines.last().is_some_and(|l| l.is_empty()) {
            old_lines.pop();
            if new_lines.last().is_some_and(|l| l.is_empty()) {
                new_lines.pop();
            }
            found = patch_seek(&lines, &old_lines, line_index, c.end_of_file);
        }
        let Some(at) = found else {
            if !c.old_lines.iter().all(|l| l.is_empty()) {
                return Err(PatchError::Msg(format!(
                    "patch: failed to find expected lines in {file_path}:\n{}",
                    c.old_lines.join("\n")
                )));
            }
            return Err(PatchError::Msg(format!("patch: failed to find expected blank lines in {file_path}")));
        };
        reps.push(Replacement { start: at, remove: old_lines.len(), insert: new_lines });
        // Advance past the matched region so the next chunk cannot
        // re-match the same lines.
        line_index = at + old_lines.len();
    }
    reps.sort_by_key(|r| r.start);
    for r in reps.into_iter().rev() {
        lines.splice(r.start..r.start + r.remove, r.insert);
    }
    if lines.last().is_some_and(|l| !l.is_empty()) {
        lines.push(String::new());
    }
    Ok(lines.join("\n"))
}

fn resolve_target(workspace: &Path, p: &str) -> PathBuf {
    let e = crate::security::expand_home(p);
    if Path::new(&e).is_absolute() {
        PathBuf::from(&e)
    } else {
        workspace.join(&e)
    }
}

fn gate_file(
    workspace: &Path,
    agent_plan: bool,
    p: &str,
) -> Result<PathBuf, PatchError> {
    if p.is_empty() {
        return Err(PatchError::Msg("patch: empty path".into()));
    }
    let resolved = resolve_target(workspace, p);
    if !resolved.starts_with(workspace) && !crate::security::is_whitelisted_write(&resolved.to_string_lossy()) {
        return Err(PatchError::Msg(format!("patch: path escapes workspace: {p}")));
    }
    if crate::security::is_credential_path(&resolved.to_string_lossy()) {
        return Err(PatchError::Msg(format!("patch: credential path denied: {p}")));
    }
    if agent_plan && !crate::security::plan_allows_write(workspace, &resolved.to_string_lossy()) {
        return Err(PatchError::Msg(format!(
            "patch: plan-mode agents may only change plan.md or .lumina/plans/*.md (got {p})"
        )));
    }
    Ok(resolved)
}

/// Execute a `*** Begin Patch ***` envelope. Returns the success summary.
pub fn apply_begin_patch(
    workspace: &Path,
    agent_plan: bool,
    raw: &str,
) -> Result<String, PatchError> {
    let hunks = parse_patch(raw)?;
    if hunks.is_empty() {
        return Err(PatchError::Msg("patch: no file operations found".into()));
    }
    for h in &hunks {
        for p in [&h.path, &h.move_path] {
            if p.is_empty() {
                continue;
            }
            gate_file(workspace, agent_plan, p)?;
        }
    }
    let mut applied: Vec<String> = Vec::new();
    let fail = |op: &str, err: PatchError, applied: &[String]| -> PatchError {
        let mut msg = format!("{op}: {err}");
        if !applied.is_empty() {
            msg.push_str(". Completed before failure: ");
            msg.push_str(&applied.join(", "));
        }
        PatchError::Msg(format!("patch: {msg}"))
    };
    for h in &hunks {
        match h.typ.as_str() {
            "add" => {
                let target = resolve_target(workspace, &h.path);
                if target.exists() {
                    return Err(fail(&format!("add {}", h.path), PatchError::Msg("file already exists (use edit to change it)".into()), &applied));
                }
                if let Some(dir) = target.parent() {
                    if let Err(e) = std::fs::create_dir_all(dir) {
                        return Err(fail(&format!("add {}", h.path), PatchError::Msg(e.to_string()), &applied));
                    }
                }
                let mut content = h.contents.clone();
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                if let Err(e) = std::fs::write(&target, content) {
                    return Err(fail(&format!("add {}", h.path), PatchError::Msg(e.to_string()), &applied));
                }
                applied.push(format!("A {}", h.path));
            }
            "delete" => {
                let target = resolve_target(workspace, &h.path);
                if let Err(e) = std::fs::remove_file(&target) {
                    return Err(fail(&format!("delete {}", h.path), PatchError::Msg(e.to_string()), &applied));
                }
                applied.push(format!("D {}", h.path));
            }
            "update" => {
                let target = resolve_target(workspace, &h.path);
                let b = std::fs::read_to_string(&target)
                    .map_err(|e| fail(&format!("update {}", h.path), PatchError::Msg(e.to_string()), &applied))?;
                let next = derive_patch(&h.path, &b, &h.chunks)
                    .map_err(|e| fail(&format!("update {}", h.path), e, &applied))?;
                let mut out = target.clone();
                if !h.move_path.is_empty() {
                    out = resolve_target(workspace, &h.move_path);
                    if let Some(dir) = out.parent() {
                        if let Err(e) = std::fs::create_dir_all(dir) {
                            return Err(fail(&format!("update {}", h.path), PatchError::Msg(e.to_string()), &applied));
                        }
                    }
                }
                if let Err(e) = std::fs::write(&out, next) {
                    return Err(fail(&format!("update {}", h.path), PatchError::Msg(e.to_string()), &applied));
                }
                if !h.move_path.is_empty() {
                    let _ = std::fs::remove_file(&target);
                }
                applied.push(format!("M {}", h.path));
            }
            _ => {
                return Err(fail(&format!("op {}", h.typ), PatchError::Msg("unknown hunk type".into()), &applied));
            }
        }
    }
    Ok(format!("Success. Updated the following files:\n{}", applied.join("\n")))
}

/// First file path in a patch (permission broker target). Scans Begin-Patch
/// headers first, then falls back to "patch" when unparseable.
pub fn patch_first_path(patch_text: &str) -> String {
    for line in patch_text.lines() {
        let t = line.trim();
        for prefix in ["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
            if let Some(rest) = t.strip_prefix(prefix) {
                let p = strip_patch_stars(rest);
                if !p.is_empty() {
                    return p;
                }
            }
        }
    }
    "patch".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_update_delete_roundtrip() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        std::fs::write(root.join("old.txt"), "keep\nremove me\n").unwrap();
        std::fs::write(root.join("gone.txt"), "x\n").unwrap();
        let patch = "*** Begin Patch ***\n*** Add File: new.txt ***\n+hello\n+world\n*** Update File: old.txt ***\n@@\n keep\n-remove me\n+added\n*** Delete File: gone.txt ***\n*** End Patch ***";
        let out = apply_begin_patch(root, false, patch).unwrap();
        assert!(out.contains("A new.txt") && out.contains("M old.txt") && out.contains("D gone.txt"), "{out}");
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "hello\nworld\n");
        assert!(std::fs::read_to_string(root.join("old.txt")).unwrap().contains("added"));
        assert!(!root.join("gone.txt").exists());
    }

    #[test]
    fn fuzzy_rstrip_match_and_move() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        std::fs::write(root.join("f.txt"), "line one   \nline two\n").unwrap();
        // Trailing spaces in the file; hunk omits them (rstrip fallback).
        let patch = "*** Begin Patch ***\n*** Update File: f.txt ***\n*** Move to: g.txt ***\n@@\n-line one\n+line ONE\n line two\n*** End Patch ***";
        let out = apply_begin_patch(root, false, patch).unwrap();
        assert!(out.contains("M f.txt"), "{out}");
        assert!(root.join("g.txt").exists());
        assert!(!root.join("f.txt").exists());
        assert!(std::fs::read_to_string(root.join("g.txt")).unwrap().starts_with("line ONE"));
    }

    #[test]
    fn rejects_bad_envelope_and_escape() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        assert!(apply_begin_patch(root, false, "hello").is_err());
        let evil = "*** Begin Patch ***\n*** Add File: ../evil.txt ***\n+x\n*** End Patch ***";
        assert!(apply_begin_patch(root, false, evil).is_err());
        let cred = "*** Begin Patch ***\n*** Add File: .env ***\n+K=V\n*** End Patch ***";
        assert!(apply_begin_patch(root, false, cred).is_err());
        // Plan scope: source edits denied, plan.md allowed.
        let src = "*** Begin Patch ***\n*** Add File: main.go ***\n+x\n*** End Patch ***";
        assert!(apply_begin_patch(root, true, src).is_err());
        let plan = "*** Begin Patch ***\n*** Add File: plan.md ***\n+x\n*** End Patch ***";
        assert!(apply_begin_patch(root, true, plan).is_ok());
    }
}
