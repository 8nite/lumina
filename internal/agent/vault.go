package agent

import (
	"crypto/rand"
	"fmt"
	"regexp"
	"sort"
	"strings"
)

// Secret vault: user-side transparency with opaque LLM-side stand-ins.
//
// Detection reuses the secret shapes from SanitizeExport (store.go), but
// instead of redacting, each distinct value is minted a stable per-session
// reference like [LUMINA-SECRET-A3F9C2]. Outbound text (history, tool
// feedback, steered input, subagent tasks) is substituted before it enters
// an llm.Message, so the provider never sees the value. Inbound text
// (assistant replies, tool args) is resolved locally, so the user, the
// transcript, and tool execution all see the real value.
//
// Mappings persist in the vault table with values sealed by the turnkey
// (crypt.go): the DB stays encrypted at rest AND the provider stays blind.
// Refs are inert by construction — they match no secret pattern (no re-vault
// loop) and no credential path (security hard-deny unaffected).

// vaultRefRe matches minted references. Keep the alphabet clear of every
// secret pattern above: no "=", no "sk-", no bearer shape.
var vaultRefRe = regexp.MustCompile(`\[LUMINA-SECRET-[0-9A-F]{6}\]`)

// vaultPattern pairs a detector with its label/value group layout
// (0 = absent: the whole match is the secret).
type vaultPattern struct {
	re         *regexp.Regexp
	labelGroup int
	valueGroup int
}

var vaultPatterns = []vaultPattern{
	{regexp.MustCompile(`(?i)(api[_-]?key\s*[:=]\s*)['"]?([^\s'"]+)`), 1, 2},
	{regexp.MustCompile(`(?i)(secret\s*[:=]\s*)['"]?([^\s'"]+)`), 1, 2},
	{regexp.MustCompile(`(sk-[A-Za-z0-9\-_]{8,})`), 0, 1},
	{regexp.MustCompile(`(AKIA[0-9A-Z]{16})`), 0, 1},
	{regexp.MustCompile(`(?i)(bearer\s+)([A-Za-z0-9\-._~+/]+-=*)`), 1, 2},
}

// migrateVault creates the ref→sealed-value mapping table.
func (s *Store) migrateVault() error {
	_, err := s.db.Exec(`CREATE TABLE IF NOT EXISTS vault(
 session_id TEXT NOT NULL, ref TEXT NOT NULL, sealed TEXT NOT NULL, created_at TEXT,
 UNIQUE(session_id, ref))`)
	if err != nil {
		return err
	}
	_, err = s.db.Exec(`CREATE INDEX IF NOT EXISTS idx_vault_session ON vault(session_id)`)
	return err
}

// vaultRefFor returns the stable ref for value in this session, minting and
// persisting one on first sight.
func (s *Store) vaultRefFor(sessionID, value string) (string, error) {
	rows, err := s.db.Query(`SELECT ref,sealed FROM vault WHERE session_id=?`, sessionID)
	if err != nil {
		return "", err
	}
	type entry struct{ ref, sealed string }
	var entries []entry
	for rows.Next() {
		var e entry
		if err := rows.Scan(&e.ref, &e.sealed); err != nil {
			rows.Close()
			return "", err
		}
		entries = append(entries, e)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return "", err
	}
	for _, e := range entries {
		plain, err := openGCM(s.key, e.sealed)
		if err != nil {
			return "", fmt.Errorf("vault %s: %w", e.ref, err)
		}
		if plain == value {
			return e.ref, nil
		}
	}
	for attempt := 0; attempt < 5; attempt++ {
		var b [3]byte
		if _, err := rand.Read(b[:]); err != nil {
			return "", err
		}
		ref := fmt.Sprintf("[LUMINA-SECRET-%02X%02X%02X]", b[0], b[1], b[2])
		sealed, err := sealGCM(s.key, []byte(value))
		if err != nil {
			return "", err
		}
		_, err = s.exec(`INSERT INTO vault(session_id,ref,sealed,created_at) VALUES(?,?,?,?)`,
			sessionID, ref, sealed, nowStr())
		if err == nil {
			return ref, nil
		}
		if !isBusy(err) && !strings.Contains(strings.ToLower(err.Error()), "unique") {
			return "", err
		}
		if isBusy(err) {
			return "", err // exec already retried BUSY; don't spin refs
		}
		// UNIQUE collision on ref: mint another.
	}
	return "", fmt.Errorf("vault: ref collision retries exhausted")
}

// vaultValue resolves one ref to its value; unknown refs pass through so a
// foreign or stale token degrades to inert text, never an error.
func (s *Store) vaultValue(sessionID, ref string) (string, bool, error) {
	var sealed string
	err := s.db.QueryRow(`SELECT sealed FROM vault WHERE session_id=? AND ref=?`, sessionID, ref).Scan(&sealed)
	if err != nil {
		return "", false, nil
	}
	plain, err := openGCM(s.key, sealed)
	if err != nil {
		return "", false, err
	}
	return plain, true, nil
}

// substituteSecrets swaps secret-shaped spans for stable refs. Labels
// (api_key=, bearer) are kept: only values are opaque.
func (s *Store) substituteSecrets(sessionID, text string) (string, error) {
	type span struct{ start, end int; label, value string }
	var spans []span
	for _, p := range vaultPatterns {
		for _, m := range p.re.FindAllStringSubmatchIndex(text, -1) {
			if len(m) < 4 {
				continue
			}
			lo, hi := m[0], m[1]
			label := ""
			if p.labelGroup > 0 && m[2*p.labelGroup] >= 0 {
				label = text[m[2*p.labelGroup]:m[2*p.labelGroup+1]]
			}
			vi := 2 * p.valueGroup
			if vi+1 >= len(m) || m[vi] < 0 {
				continue
			}
			spans = append(spans, span{lo, hi, label, text[m[vi]:m[vi+1]]})
		}
	}
	if len(spans) == 0 {
		return text, nil
	}
	sort.Slice(spans, func(i, j int) bool {
		if spans[i].start != spans[j].start {
			return spans[i].start < spans[j].start
		}
		return spans[i].end > spans[j].end // longest wins ties
	})
	// Drop overlaps (e.g. api_key=sk-… matches two patterns).
	kept := spans[:0]
	end := -1
	for _, sp := range spans {
		if sp.start < end {
			continue
		}
		if vaultRefRe.MatchString(sp.value) {
			// Already opaque (e.g. api_key=[LUMINA-SECRET-…] on a later
			// round): re-vaulting the ref would nest forever. Skip.
			end = sp.end
			continue
		}
		kept = append(kept, sp)
		end = sp.end
	}
	var sb strings.Builder
	pos := 0
	for _, sp := range kept {
		ref, err := s.vaultRefFor(sessionID, sp.value)
		if err != nil {
			return "", err // fail closed: never send unvaulted text
		}
		sb.WriteString(text[pos:sp.start])
		sb.WriteString(sp.label)
		sb.WriteString(ref)
		pos = sp.end
	}
	sb.WriteString(text[pos:])
	return sb.String(), nil
}

// resolveRefs swaps refs back to values for user display, storage, and
// tool execution. Unknown refs pass through untouched.
func (s *Store) resolveRefs(sessionID, text string) (string, error) {
	if !strings.Contains(text, "[LUMINA-SECRET-") {
		return text, nil // ponytail: skip the regex on the common path
	}
	var out strings.Builder
	last := 0
	for _, loc := range vaultRefRe.FindAllStringIndex(text, -1) {
		ref := text[loc[0]:loc[1]]
		val, ok, err := s.vaultValue(sessionID, ref)
		if err != nil {
			return "", err
		}
		out.WriteString(text[last:loc[0]])
		if ok {
			out.WriteString(val)
		} else {
			out.WriteString(ref)
		}
		last = loc[1]
	}
	out.WriteString(text[last:])
	return out.String(), nil
}

// resolveArgRefs deep-resolves refs inside tool args (maps, slices, strings).
func (s *Store) resolveArgRefs(sessionID string, v interface{}) (interface{}, error) {
	switch t := v.(type) {
	case string:
		return s.resolveRefs(sessionID, t)
	case map[string]interface{}:
		out := make(map[string]interface{}, len(t))
		for k, e := range t {
			r, err := s.resolveArgRefs(sessionID, e)
			if err != nil {
				return nil, err
			}
			out[k] = r
		}
		return out, nil
	case []interface{}:
		out := make([]interface{}, len(t))
		for i, e := range t {
			r, err := s.resolveArgRefs(sessionID, e)
			if err != nil {
				return nil, err
			}
			out[i] = r
		}
		return out, nil
	default:
		return v, nil
	}
}
