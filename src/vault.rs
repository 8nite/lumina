//! Session secret vault: user-side transparency, LLM-side opacity.
//! Mirrors `internal/agent/vault.go`: secret-shaped spans in outbound text
//! become stable per-session `{placeholder_abc}` refs before the provider
//! sees them; inbound refs rehydrate to values on the user-facing path.
//! Memory-only: mappings die with the process, transcripts keep values.
use regex::Regex;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Layer-0 note teaching the model the placeholder protocol (byte-identical
/// to Go `vaultGuidance`).
pub const VAULT_GUIDANCE: &str = "# Placeholders\n\
- Sensitive values reach you as {placeholder_ABC} tokens (lowercase letters only). Pass them through verbatim in replies and tool arguments: the local harness remaps each token to its real value before any tool or skill runs.\n\
- Never guess, expand, or fabricate the value behind a token.";

pub const PLACEHOLDER_ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz";

fn vault_ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\{placeholder_[a-z]{3}\}").unwrap())
}

struct Pattern {
    re: Regex,
    label_group: usize,
    value_group: usize,
    trail_word_check: bool,
    skip_dates: bool,
    skip_date_frag: bool,
    min_digits: usize,
}

fn patterns() -> &'static [Pattern] {
    static PATS: OnceLock<Vec<Pattern>> = OnceLock::new();
    PATS.get_or_init(|| {
        let p = |re: &str, label_group: usize, value_group: usize| Pattern {
            re: Regex::new(re).unwrap(),
            label_group,
            value_group,
            trail_word_check: false,
            skip_dates: false,
            skip_date_frag: false,
            min_digits: 0,
        };
        let mut v = vec![
            p(r#"(?i)(api[_-]?key\s*[:=]\s*)['"]?([^\s'"]+)"#, 1, 2),
            p(r#"(?i)(secret\s*[:=]\s*)['"]?([^\s'"]+)"#, 1, 2),
            p(r#"(?i)\b((?:private[_-]?key)\s*[:=]\s*)['"]?([^\s'"]+)"#, 1, 2),
            p(r#"(?i)\b((?:password|passwd|pwd|pass|passphrase)\s*[:=]\s*)['"]?([^\s'"]+)"#, 1, 2),
            p(r#"(?i)\b((?:(?:access|refresh|auth|id|api|session)_token|token|otp|pin|passcode|2fa|verification_code|auth_code)\s*[:=]\s*)['"]?([^\s'"]+)"#, 1, 2),
            p(r#"(?i)\b((?:(?:client|user|account|session|request|key|token)_id|id|uuid|guid)\s*[:=]\s*)['"]?([^\s'"]+)"#, 1, 2),
            p(r#"(?i)\b((?:sin|ssn|social[_ \-]?security(?:[_ \-]?number)?|social[_ \-]?insurance(?:[_ \-]?number)?)\s*[:=]?\s*)(\d{3}[- ]?\d{3}[- ]?\d{3}|\d{3}[- ]\d{2}[- ]\d{4})"#, 1, 2),
            p(r#"(sk-[A-Za-z0-9\-_]{8,})"#, 0, 1),
            p(r#"(AKIA[0-9A-Z]{16})"#, 0, 1),
            p(r#"(?i)(bearer\s+)([A-Za-z0-9\-._~+/]+-=*)"#, 1, 2),
            p(r#"(?s)(-----BEGIN [^-]*PRIVATE KEY[^-]*-----.*?-----END [^-]*PRIVATE KEY[^-]*-----)"#, 0, 1),
            p(r#"(^|[^0-9A-Za-z_])(\d{4,})"#, 0, 2),
            p(r#"(^|[^0-9A-Za-z_])(\d{2,}(?:[- ]\d{2,})+)"#, 0, 2),
        ];
        // SIN/SSN: keep label, skip dates.
        v[6].trail_word_check = true;
        v[6].skip_dates = true;
        // Bare digit runs: skip date fragments (2026 in 2026-10-30).
        v[11].trail_word_check = true;
        v[11].skip_date_frag = true;
        // Segmented codes: calendar dates stay visible, 6+ digits.
        v[12].trail_word_check = true;
        v[12].skip_dates = true;
        v[12].min_digits = 6;
        v
    })
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn is_digit_byte(c: u8) -> bool {
    c.is_ascii_digit()
}

fn count_digits(s: &str) -> usize {
    s.bytes().filter(|b| is_digit_byte(*b)).count()
}

fn iso_date_like() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{4}[- ]\d{1,2}[- ]\d{1,2}$").unwrap())
}

fn us_date_like() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{1,2}[- ]\d{1,2}[- ]\d{2,4}$").unwrap())
}

fn is_date_like(s: &str) -> bool {
    iso_date_like().is_match(s) || us_date_like().is_match(s)
}

#[derive(Default)]
struct SessionVault {
    by_ref: HashMap<String, String>,
    by_value: HashMap<String, String>,
}

#[derive(Default)]
pub struct Vault {
    sessions: Mutex<HashMap<String, SessionVault>>,
}

fn mint_ref() -> String {
    // ponytail: 3 bytes of OS entropy, 26^3 space, retry-on-collision upstream.
    let mut buf = [0u8; 3];
    let mut ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut buf).map(|_| ())
        })
        .is_ok();
    if !ok {
        // Fallback: nanos + pid + counter hashed into 3 letters.
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            .wrapping_add(std::process::id() as u64)
            .wrapping_add(CTR.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E3779B97F4A7C15));
        buf = [(n & 0xff) as u8, ((n >> 8) & 0xff) as u8, ((n >> 16) & 0xff) as u8];
        ok = true;
    }
    let _ = ok;
    let alpha = PLACEHOLDER_ALPHABET.as_bytes();
    format!(
        "{{placeholder_{}{}{}}}",
        alpha[(buf[0] % 26) as usize] as char,
        alpha[(buf[1] % 26) as usize] as char,
        alpha[(buf[2] % 26) as usize] as char
    )
}

impl Vault {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stable ref for value in this session, minting on first sight.
    pub fn ref_for(&self, session: &str, value: &str) -> Result<String, String> {
        let mut all = self.sessions.lock().map_err(|e| format!("vault: {e}"))?;
        let sv = all.entry(session.to_string()).or_default();
        if let Some(r) = sv.by_value.get(value) {
            return Ok(r.clone());
        }
        for _ in 0..5 {
            let r = mint_ref();
            if sv.by_ref.contains_key(&r) {
                continue; // collision: mint another
            }
            sv.by_ref.insert(r.clone(), value.to_string());
            sv.by_value.insert(value.to_string(), r.clone());
            return Ok(r);
        }
        Err("vault: ref collision retries exhausted".into())
    }

    fn value(&self, session: &str, r: &str) -> Option<String> {
        self.sessions.lock().ok()?.get(session)?.by_ref.get(r).cloned()
    }

    /// Exported debug entry to the vault boundary (used by `vault check`):
    /// what the model would receive for text.
    pub fn substitute_for_llm(&self, session: &str, text: &str) -> Result<String, String> {
        self.substitute(session, text)
    }

    /// Swap secret-shaped spans for stable refs. Labels kept, values opaque.
    /// Fail-closed: ref-mint failure aborts (never send unvaulted text).
    pub fn substitute(&self, session: &str, text: &str) -> Result<String, String> {
        struct Span {
            start: usize,
            end: usize,
            label: String,
            value: String,
        }
        let b = text.as_bytes();
        let mut spans: Vec<Span> = Vec::new();
        for p in patterns() {
            for m in p.re.captures_iter(text) {
                let whole = m.get(0).unwrap();
                let (mut lo, mut hi) = (whole.start(), whole.end());
                let label = if p.label_group > 0 {
                    m.get(p.label_group).map(|g| g.as_str().to_string()).unwrap_or_default()
                } else {
                    String::new()
                };
                let vg = match m.get(p.value_group) {
                    Some(g) => g,
                    None => continue,
                };
                let (vlo, vhi) = (vg.start(), vg.end());
                let value = vg.as_str().to_string();
                if p.trail_word_check && vhi < b.len() && is_word_byte(b[vhi]) {
                    continue;
                }
                if p.skip_date_frag {
                    // Year/month/day fragments (2026 in 2026-10-30).
                    if vhi + 1 < b.len() && (b[vhi] == b'-' || b[vhi] == b'/') && is_digit_byte(b[vhi + 1]) {
                        continue;
                    }
                    if vlo >= 2 && (b[vlo - 1] == b'-' || b[vlo - 1] == b'/') && is_digit_byte(b[vlo - 2]) {
                        continue;
                    }
                }
                if p.min_digits > 0 && count_digits(&value) < p.min_digits {
                    continue;
                }
                if p.skip_dates && is_date_like(&value) {
                    continue;
                }
                if p.label_group == 0 {
                    lo = vlo;
                    hi = vhi; // keep boundary chars in the text
                }
                spans.push(Span { start: lo, end: hi, label, value });
            }
        }
        if spans.is_empty() {
            return Ok(text.to_string());
        }
        // Longest wins ties; drop overlaps. Spans whose value is already a
        // ref pass through: re-vaulting would nest a new ref every round.
        spans.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        let mut kept: Vec<Span> = Vec::new();
        let mut end = 0usize;
        for sp in spans {
            if sp.start < end {
                continue;
            }
            end = sp.end;
            if vault_ref_re().is_match(&sp.value) {
                continue;
            }
            kept.push(sp);
        }
        let mut out = String::with_capacity(text.len());
        let mut pos = 0usize;
        for sp in kept {
            let r = self.ref_for(session, &sp.value)?;
            out.push_str(&text[pos..sp.start]);
            out.push_str(&sp.label);
            out.push_str(&r);
            pos = sp.end;
        }
        out.push_str(&text[pos..]);
        Ok(out)
    }

    /// Swap refs back to values. Unknown refs pass through untouched.
    pub fn resolve(&self, session: &str, text: &str) -> Result<String, String> {
        if !text.contains("{placeholder_") {
            return Ok(text.to_string()); // ponytail: skip the regex on the common path
        }
        let re = vault_ref_re();
        let mut out = String::with_capacity(text.len());
        let mut last = 0usize;
        for loc in re.find_iter(text) {
            let r = &text[loc.start()..loc.end()];
            out.push_str(&text[last..loc.start()]);
            match self.value(session, r) {
                Some(v) => out.push_str(&v),
                None => out.push_str(r),
            }
            last = loc.end();
        }
        out.push_str(&text[last..]);
        Ok(out)
    }

    /// Deep-resolve refs inside tool args (maps, arrays, strings).
    pub fn resolve_value(&self, session: &str, v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::String(s) => serde_json::Value::String(
                self.resolve(session, s).unwrap_or_else(|_| s.clone()),
            ),
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(|e| self.resolve_value(session, e)).collect())
            }
            serde_json::Value::Object(m) => serde_json::Value::Object(
                m.iter().map(|(k, e)| (k.clone(), self.resolve_value(session, e))).collect(),
            ),
            _ => v.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> Vault {
        Vault::new()
    }

    #[test]
    fn labeled_secret_roundtrip() {
        let v = vault();
        let out = v.substitute_for_llm("s1", "api_key=sk-live-abc123 secret").unwrap();
        assert!(!out.contains("sk-live-abc123"), "value leaked: {out}");
        assert!(out.contains("{placeholder_"), "no ref minted: {out}");
        assert!(out.starts_with("api_key="), "label kept: {out}");
        let back = v.resolve("s1", &out).unwrap();
        assert!(back.contains("sk-live-abc123"), "resolve lost value: {back}");
    }

    #[test]
    fn bare_shapes_vaulted() {
        let v = vault();
        for text in ["sk-abcdefgh1234", "AKIAIOSFODNN7EXAMPLE", "Bearer abc.def_ghi-123"] {
            let out = v.substitute_for_llm("s1", text).unwrap();
            assert!(out.contains("{placeholder_"), "{text} not vaulted: {out}");
        }
    }

    #[test]
    fn dates_and_versions_stay_visible() {
        let v = vault();
        for text in ["2026-10-30", "released 1.3.10 today", "port 80 open"] {
            let out = v.substitute_for_llm("s1", text).unwrap();
            assert_eq!(out, text, "{text} wrongly vaulted: {out}");
        }
    }

    #[test]
    fn pin_vaulted_word_digits_kept() {
        let v = vault();
        let out = v.substitute_for_llm("s1", "code 123456 done").unwrap();
        assert!(out.contains("{placeholder_"), "pin not vaulted: {out}");
        let same = v.substitute_for_llm("s1", "abc1234 stays").unwrap();
        assert_eq!(same, "abc1234 stays", "word digits wrongly vaulted: {same}");
    }

    #[test]
    fn token_id_password_segments() {
        let v = vault();
        for (text, label) in [
            ("login password=123456", "password="),
            ("passwd: hunter2", "passwd:"),
            ("token: abcdef", "token:"),
            ("access_token=xyz789", "access_token="),
            ("user_id=42", "user_id="),
            ("call 123456 now", "call "),
            ("code 1234-12345 ok", "code "),
            ("card 1234 5678 9012", "card "),
        ] {
            let out = v.substitute_for_llm("s1", text).unwrap();
            assert!(out.contains("{placeholder_"), "{text} not vaulted: {out}");
            assert!(out.contains(label), "{text} lost label: {out}");
            let back = v.resolve("s1", &out).unwrap();
            assert!(back.contains(text.split_whitespace().last().unwrap()), "{text} not restored: {back}");
        }
    }

    #[test]
    fn session_scoped_and_stable() {
        let v = vault();
        let a = v.substitute_for_llm("s1", "token=abc123").unwrap();
        let a2 = v.substitute_for_llm("s1", "token=abc123").unwrap();
        assert_eq!(a, a2, "same session must mint stable refs");
        let b = v.substitute_for_llm("s2", "token=abc123").unwrap();
        assert_ne!(a, b, "refs must differ across sessions");
        // Cross-session resolve degrades to inert text, never the value.
        assert!(!v.resolve("s2", &a).unwrap().contains("abc123"));
    }

    #[test]
    fn guidance_is_inert() {
        // The layer-0 note itself must pass through byte-identical: no
        // detector may fire on it (refs are inert by construction).
        let v = vault();
        let out = v.substitute_for_llm("s1", VAULT_GUIDANCE).unwrap();
        assert_eq!(out, VAULT_GUIDANCE, "guidance self-vaulted: {out}");
    }

    #[test]
    fn password_and_id_labels() {
        let v = vault();
        let out = v.substitute_for_llm("s1", "password: hunter2 user_id=42").unwrap();
        assert!(!out.contains("hunter2"), "password leaked: {out}");
        // Short values without shape stay visible; labels kept.
        assert!(out.contains("password:"), "label dropped: {out}");
    }
}
