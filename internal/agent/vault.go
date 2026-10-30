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
// Detection covers labeled secrets (api_key=, password:, token:, user_id=,
// bearer) plus bare shapes (sk-…, AKIA…, PIN/OTP/ID numbers like 123456,
// segmented codes like 1234-12345), each minted a stable per-session
// reference like {placeholder_abc} — lowercase letters only, so a ref can
// never be mistaken for (part of) a numeric code. Outbound text (history,
// tool feedback, steered input, subagent tasks) is substituted before it
// enters an llm.Message, so the provider never sees the value. Inbound text
// (assistant replies, tool args) is resolved locally, so the user, the
// transcript, and tool execution all see the real value.
//
// The vault lives only in memory, keyed by session: mappings die with the
// process and never touch disk. Transcripts keep real values, so after a
// restart substitution simply mints fresh refs — nothing to migrate, nothing
// to clean up. Refs are inert by construction — they match no secret pattern
// (no re-vault loop) and no credential path (security hard-deny unaffected).

// vaultRefRe matches minted references: {placeholder_ + 3 lowercase letters}.
var vaultRefRe = regexp.MustCompile(`\{placeholder_[a-z]{3}\}`)

// vaultGuidance is the layer-0 note telling the model how placeholders work:
// values arrive as {placeholder_abc} and the local harness remaps each one
// to its real value before any tool or skill runs.
const vaultGuidance = `# Placeholders
- Sensitive values reach you as {placeholder_ABC} tokens (lowercase letters only). Pass them through verbatim in replies and tool arguments: the local harness remaps each token to its real value before any tool or skill runs.
- Never guess, expand, or fabricate the value behind a token.`

// placeholderAlphabet is the ref alphabet: lowercase ASCII only, so refs
// stay visually distinct from the numeric codes they stand in for
// (26^3 ≈ 17k per session, with retry on collision).
const placeholderAlphabet = "abcdefghijklmnopqrstuvwxyz"

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

// vaultRefFor returns the stable ref for value in this session, minting one
// on first sight. Memory-only: mappings die with the process.
func (s *Store) vaultRefFor(sessionID, value string) (string, error) {
	s.vaultMu.Lock()
	defer s.vaultMu.Unlock()
	if s.vault == nil {
		s.vault = map[string]map[string]string{}
		s.vaultByValue = map[string]map[string]string{}
	}
	if s.vault[sessionID] == nil {
		s.vault[sessionID] = map[string]string{}
		s.vaultByValue[sessionID] = map[string]string{}
	}
	if ref, ok := s.vaultByValue[sessionID][value]; ok {
		return ref, nil
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
		if _, taken := s.vault[sessionID][ref]; taken {
			continue // collision: mint another
		}
		s.vault[sessionID][ref] = value
		s.vaultByValue[sessionID][value] = ref
		return ref, nil
	}
	return "", fmt.Errorf("vault: ref collision retries exhausted")
}

// vaultValue resolves one ref to its value; unknown refs pass through so a
// foreign or stale token degrades to inert text, never an error.
func (s *Store) vaultValue(sessionID, ref string) (string, bool, error) {
	s.vaultMu.Lock()
	defer s.vaultMu.Unlock()
	v, ok := s.vault[sessionID][ref]
	return v, ok, nil
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
