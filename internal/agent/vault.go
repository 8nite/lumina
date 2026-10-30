package agent

import (
	"crypto/rand"
	"database/sql"
	"fmt"
	"regexp"
	"sort"
	"strings"
)

// Secret vault: user-side transparency with opaque LLM-side stand-ins.
//
// Detection covers labeled secrets (api_key=, password:, token:, user_id=,
// bearer) plus bare shapes (sk-…, AKIA…, PIN/OTP/ID numbers like 123456,
// segmented codes like 1234-12345), each minted a stable per-session
// reference like {placeholder_k3x}. Outbound text (history, tool feedback,
// steered input, subagent tasks) is substituted before it enters an
// llm.Message, so the provider never sees the value. Inbound text
// (assistant replies, tool args) is resolved locally, so the user, the
// transcript, and tool execution all see the real value.
//
// Mappings persist in the vault table as-is (no encryption: the DB is the
// user's own file and every read path already shows values). Refs are inert
// by construction — they match no secret pattern (no re-vault loop) and no
// credential path (security hard-deny unaffected).

// vaultRefRe matches minted references: {placeholder_ + 3 ASCII chars}.
var vaultRefRe = regexp.MustCompile(`\{placeholder_[a-z0-9]{3}\}`)

// vaultGuidance is the layer-0 note telling the model how placeholders work:
// values arrive as {placeholder_xxx} and the local harness remaps each one
// to its real value before any tool or skill runs.
const vaultGuidance = `# Placeholders
- Sensitive values reach you as {placeholder_ABC} tokens (lowercase letters and digits). Pass them through verbatim in replies and tool arguments: the local harness remaps each token to its real value before any tool or skill runs.
- Never guess, expand, or fabricate the value behind a token.`

// placeholderAlphabet is the JSON/brace/regex-safe ASCII subset refs draw
// from (36^3 ≈ 46k per session, with retry on collision).
const placeholderAlphabet = "abcdefghijklmnopqrstuvwxyz0123456789"

// vaultPattern pairs a detector with its label/value group layout
// (0 = absent: the whole match is the secret).
type vaultPattern struct {
	re         *regexp.Regexp
	labelGroup int
	valueGroup int
	// trailWordCheck drops the match when the value runs into a word byte
	// (digit runs only: "1234x" and "v2_1234beta" stay visible).
	trailWordCheck bool
	// skipDates leaves calendar dates alone ("2026-10-30" stays visible).
	skipDates bool
	// skipDateFrag leaves date fragments alone ("2026" in "2026-10-30").
	skipDateFrag bool
	// minDigits is the minimum digit count in the value (0 = none).
	minDigits int
}

var vaultPatterns = []vaultPattern{
	{re: regexp.MustCompile(`(?i)(api[_-]?key\s*[:=]\s*)['"]?([^\s'"]+)`), labelGroup: 1, valueGroup: 2},
	{re: regexp.MustCompile(`(?i)(secret\s*[:=]\s*)['"]?([^\s'"]+)`), labelGroup: 1, valueGroup: 2},
	{re: regexp.MustCompile(`(?i)\b((?:password|passwd|pwd|pass|passphrase)\s*[:=]\s*)['"]?([^\s'"]+)`), labelGroup: 1, valueGroup: 2},
	{re: regexp.MustCompile(`(?i)\b((?:(?:access|refresh|auth|id|api|session)_token|token|otp|pin|passcode|2fa|verification_code|auth_code)\s*[:=]\s*)['"]?([^\s'"]+)`), labelGroup: 1, valueGroup: 2},
	{re: regexp.MustCompile(`(?i)\b((?:(?:client|user|account|session|request|key|token)_id|id|uuid|guid)\s*[:=]\s*)['"]?([^\s'"]+)`), labelGroup: 1, valueGroup: 2},
	{re: regexp.MustCompile(`(sk-[A-Za-z0-9\-_]{8,})`), valueGroup: 1},
	{re: regexp.MustCompile(`(AKIA[0-9A-Z]{16})`), valueGroup: 1},
	{re: regexp.MustCompile(`(?i)(bearer\s+)([A-Za-z0-9\-._~+/]+-=*)`), labelGroup: 1, valueGroup: 2},
	// Bare PIN/OTP/ID-shaped numbers: 4+ digits not touching a word char,
	// so versions (1.3.10), words with digits (abc1234), and short counts
	// (port 80) stay visible while 123456 does not.
	{re: regexp.MustCompile(`(^|[^0-9A-Za-z_])(\d{4,})`), valueGroup: 2, trailWordCheck: true, skipDateFrag: true},
	// Segmented codes (1234-12345, 1234 5678): one ref per code. Short
	// pairs (12-34, scores) stay visible; calendar dates stay visible.
	{re: regexp.MustCompile(`(^|[^0-9A-Za-z_])(\d{2,}(?:[- ]\d{2,})+)`), valueGroup: 2, trailWordCheck: true, skipDates: true, minDigits: 6},
}

func isWordByte(c byte) bool {
	return c >= '0' && c <= '9' || c >= 'A' && c <= 'Z' || c >= 'a' && c <= 'z' || c == '_'
}

func isDigitByte(c byte) bool { return c >= '0' && c <= '9' }

func countDigits(s string) int {
	n := 0
	for i := 0; i < len(s); i++ {
		if isDigitByte(s[i]) {
			n++
		}
	}
	return n
}

var (
	isoDateLike = regexp.MustCompile(`^\d{4}[- ]\d{1,2}[- ]\d{1,2}$`)
	usDateLike  = regexp.MustCompile(`^\d{1,2}[- ]\d{1,2}[- ]\d{2,4}$`)
)

// isDateLike reports calendar-date shapes (2026-10-30, 10-30-2026).
func isDateLike(s string) bool {
	return isoDateLike.MatchString(s) || usDateLike.MatchString(s)
}

// migrateVault creates the ref→value mapping table. The pre-release layout
// used a `sealed` column; existing dev DBs get a `value` column added and
// keep working (old sealed rows read as unknown refs and pass through;
// new writes fill both columns so the old NOT NULL constraint holds).
func (s *Store) migrateVault() error {
	if _, err := s.db.Exec(`CREATE TABLE IF NOT EXISTS vault(
 session_id TEXT NOT NULL, ref TEXT NOT NULL, value TEXT NOT NULL DEFAULT '', created_at TEXT,
 UNIQUE(session_id, ref))`); err != nil {
		return err
	}
	if _, err := s.db.Exec(`CREATE INDEX IF NOT EXISTS idx_vault_session ON vault(session_id)`); err != nil {
		return err
	}
	rows, err := s.db.Query(`PRAGMA table_info(vault)`)
	if err != nil {
		return err
	}
	defer rows.Close()
	hasValue := false
	hasSealed := false
	for rows.Next() {
		var cid int
		var name, typ string
		var notnull, pk int
		var dflt sql.NullString
		if err := rows.Scan(&cid, &name, &typ, &notnull, &dflt, &pk); err != nil {
			return err
		}
		if name == "value" {
			hasValue = true
		}
		if name == "sealed" {
			hasSealed = true
		}
	}
	if err := rows.Err(); err != nil {
		return err
	}
	if !hasValue {
		_, err = s.db.Exec(`ALTER TABLE vault ADD COLUMN value TEXT NOT NULL DEFAULT ''`)
	}
	s.vaultLegacy = hasSealed
	return err
}

// vaultRefFor returns the stable ref for value in this session, minting and
// persisting one on first sight.
func (s *Store) vaultRefFor(sessionID, value string) (string, error) {
	rows, err := s.db.Query(`SELECT ref,value FROM vault WHERE session_id=?`, sessionID)
	if err != nil {
		return "", err
	}
	type entry struct{ ref, value string }
	var entries []entry
	for rows.Next() {
		var e entry
		var v sql.NullString
		if err := rows.Scan(&e.ref, &v); err != nil {
			rows.Close()
			return "", err
		}
		if v.Valid {
			e.value = v.String
			entries = append(entries, e)
		}
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return "", err
	}
	for _, e := range entries {
		if e.value == value {
			return e.ref, nil
		}
	}
	for attempt := 0; attempt < 5; attempt++ {
		var b [3]byte
		if _, err := rand.Read(b[:]); err != nil {
			return "", err
		}
		ref := fmt.Sprintf("{placeholder_%c%c%c}",
			placeholderAlphabet[int(b[0])%len(placeholderAlphabet)],
			placeholderAlphabet[int(b[1])%len(placeholderAlphabet)],
			placeholderAlphabet[int(b[2])%len(placeholderAlphabet)])
		if s.vaultLegacy {
			_, err = s.exec(`INSERT INTO vault(session_id,ref,sealed,value,created_at) VALUES(?,?,?,?,?)`,
				sessionID, ref, value, value, nowStr())
		} else {
			_, err = s.exec(`INSERT INTO vault(session_id,ref,value,created_at) VALUES(?,?,?,?)`,
				sessionID, ref, value, nowStr())
		}
		if err == nil {
			return ref, nil
		}
		if isBusy(err) {
			return "", err // exec already retried BUSY; don't spin refs
		}
		if !strings.Contains(strings.ToLower(err.Error()), "unique") {
			return "", err
		}
		// UNIQUE collision on ref: mint another.
	}
	return "", fmt.Errorf("vault: ref collision retries exhausted")
}

// vaultValue resolves one ref to its value; unknown refs pass through so a
// foreign or stale token degrades to inert text, never an error.
func (s *Store) vaultValue(sessionID, ref string) (string, bool, error) {
	var v sql.NullString
	err := s.db.QueryRow(`SELECT value FROM vault WHERE session_id=? AND ref=?`, sessionID, ref).Scan(&v)
	if err != nil || !v.Valid || v.String == "" {
		return "", false, nil
	}
	return v.String, true, nil
}

// SubstituteForLLM is the exported debug entry to the vault boundary
// (used by `lumina vault check`): what the model would receive for text.
func (s *Store) SubstituteForLLM(sessionID, text string) (string, error) {
	return s.substituteSecrets(sessionID, text)
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
			vlo, vhi := m[vi], m[vi+1]
			value := text[vlo:vhi]
			if p.trailWordCheck && vhi < len(text) && isWordByte(text[vhi]) {
				continue
			}
			if p.skipDateFrag {
				// Year/month/day fragments (2026 in 2026-10-30).
				if vhi+1 < len(text) && (text[vhi] == '-' || text[vhi] == '/') && isDigitByte(text[vhi+1]) {
					continue
				}
				if vlo >= 2 && (text[vlo-1] == '-' || text[vlo-1] == '/') && isDigitByte(text[vlo-2]) {
					continue
				}
			}
			if p.minDigits > 0 && countDigits(value) < p.minDigits {
				continue
			}
			if p.skipDates && isDateLike(value) {
				continue
			}
			if p.labelGroup == 0 {
				lo, hi = vlo, vhi // keep boundary chars in the text
			}
			spans = append(spans, span{lo, hi, label, value})
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
	// Drop overlaps (e.g. api_key=sk-… matches two patterns). Spans whose
	// value is already a ref are left verbatim: re-vaulting the ref would
	// nest a new ref on every round, forever.
	kept := spans[:0]
	end := -1
	for _, sp := range spans {
		if sp.start < end {
			continue
		}
		end = sp.end
		if vaultRefRe.MatchString(sp.value) {
			continue
		}
		kept = append(kept, sp)
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
	if !strings.Contains(text, "{placeholder_") {
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
