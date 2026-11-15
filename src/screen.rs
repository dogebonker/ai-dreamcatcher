//! The secret and hidden-character screens, and redaction of both.

/// What a matched secret is replaced with.
pub const REDACTED: &str = "[redacted]";

// ponytail: fixed marker list plus hand-written shapes; add entropy checks if a secret ever gets through.
const SECRET_MARKERS: [&str; 19] = [
    "-----BEGIN",
    "sk-ant-",
    "sk-proj-",
    "sk_live_",
    "rk_live_",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "xoxa-",
    "xapp-",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "AIza",
    "ya29.",
    "AGE-SECRET-KEY-",
    "Bearer ey",
];

/// The first secret-like pattern in `text`.
pub fn secret(text: &str) -> Option<&'static str> {
    spans(text).first().map(|s| s.2)
}

/// True when `text` holds a zero-width, bidi-control or tag character.
pub fn hidden(text: &str) -> bool {
    text.chars().any(is_hidden)
}

/// `text` with hidden characters removed and every secret replaced by `REDACTED`.
pub fn redact(text: &str) -> String {
    // Hidden characters go first, so one cannot split a secret to slip past the matchers.
    let text: String = text.chars().filter(|c| !is_hidden(*c)).collect();
    let mut found = spans(&text);
    if found.is_empty() {
        return text;
    }
    found.sort_by_key(|s| s.0);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end, _) in found {
        // An overlapping match only widens the placeholder already written.
        if start >= at {
            out.push_str(&text[at..start]);
            out.push_str(REDACTED);
        }
        at = at.max(end);
    }
    out.push_str(&text[at..]);
    out
}

fn is_hidden(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}'
        | '\u{FEFF}'
        | '\u{E0000}'..='\u{E007F}')
}

/// Byte ranges and kinds of every secret in `text`, markers first.
fn spans(text: &str) -> Vec<(usize, usize, &'static str)> {
    let mut out = Vec::new();
    let token_end = |from: usize| {
        text[from..]
            .find(|c: char| c.is_whitespace() || "\"'`<>()[]{},;".contains(c))
            .map_or(text.len(), |n| from + n)
    };
    let run = |from: usize, ok: fn(char) -> bool| {
        text[from..]
            .find(|c: char| !ok(c))
            .map_or(text.len(), |n| from + n)
    };
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';

    for m in SECRET_MARKERS {
        for (i, _) in text.match_indices(m) {
            let end = if m == "-----BEGIN" {
                // The whole PEM block, or the rest of the text when it has no end line.
                text[i..].find("-----END").map_or(text.len(), |e| {
                    let after = i + e + "-----END".len();
                    text[after..]
                        .find("-----")
                        .map_or(text.len(), |d| after + d + 5)
                })
            } else {
                token_end(i + m.len())
            };
            out.push((i, end, m));
        }
    }
    for (i, _) in text.match_indices("npm_") {
        let end = run(i + 4, |c| c.is_ascii_alphanumeric());
        if end - (i + 4) >= 36 {
            out.push((i, end, "npm_"));
        }
    }
    for (i, _) in text.match_indices("eyJ") {
        let first = run(i, word);
        if first - i >= 10 && text[first..].starts_with('.') {
            let second = run(first + 1, word);
            if second - (first + 1) >= 10 {
                let end = if text[second..].starts_with('.') {
                    run(second + 1, word)
                } else {
                    second
                };
                out.push((i, end, "JWT"));
            }
        }
    }
    for (i, _) in text.match_indices("://") {
        let rest = &text[i + 3..];
        let authority = &rest[..rest
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(rest.len())];
        if let Some((userinfo, _)) = authority.rsplit_once('@')
            && let Some((user, pass)) = userinfo.split_once(':')
            && !user.is_empty()
            && !pass.is_empty()
        {
            out.push((i + 3, i + 3 + userinfo.len(), "scheme://user:pass@"));
        }
    }
    for (i, _) in text.match_indices("sk-") {
        let end = run(i + 3, |c| c.is_ascii_alphanumeric());
        if end - (i + 3) >= 20 {
            out.push((i, end, "sk-"));
        }
    }
    // Telegram bot token: `\d+:[A-Za-z0-9_-]{35}`, the digits starting a word, so that
    // `sha256:<hex>` is left to the hex rule and its label.
    for (i, _) in text.match_indices(':') {
        // Digits are one byte each, so the count is also the byte length.
        let digits = i - text[..i]
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .count();
        let starts_word = !text[..digits].ends_with(word);
        let end = run(i + 1, word);
        if digits < i && starts_word && end - (i + 1) >= 35 {
            out.push((digits, end, "telegram"));
        }
    }
    // 64 or more hex characters, unless the digest is labelled `sha256:`, `sha256=` or `sha256-`.
    let mut i = 0;
    while i < text.len() {
        let end = run(i, |c| c.is_ascii_hexdigit());
        if end == i {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let labelled = ["sha256:", "sha256=", "sha256-"]
            .iter()
            .any(|l| text[..i].ends_with(l));
        if end - i >= 64 && !labelled {
            out.push((i, end, "hex64"));
        }
        i = end;
    }
    let mut start = 0;
    for line in text.split('\n') {
        if seed_phrase(line) {
            out.push((start, start + line.len(), "seed phrase"));
        }
        start += line.len() + 1;
    }
    out
}

/// A line that is exactly 12 or 24 lowercase words of 3 to 8 letters.
fn seed_phrase(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    matches!(words.len(), 12 | 24)
        && words
            .iter()
            .all(|w| (3..=8).contains(&w.len()) && w.chars().all(|c| c.is_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_and_extra_shapes() {
        let npm = format!("npm_{}", "a".repeat(36));
        let jwt = "eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.sig";
        for (text, kind) in [
            ("key sk-ant-api03-xyz here", "sk-ant-"),
            ("export GH=ghp_abcdef", "ghp_"),
            (
                "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----",
                "-----BEGIN",
            ),
            ("AKIAIOSFODNN7EXAMPLE", "AKIA"),
            ("Authorization: Bearer eyJxyz", "Bearer ey"),
            (npm.as_str(), "npm_"),
            (jwt, "JWT"),
            ("postgres://admin:hunter2@db:5432/x", "scheme://user:pass@"),
        ] {
            assert_eq!(secret(text), Some(kind), "{text}");
        }
        assert_eq!(secret(&format!("npm_{}", "a".repeat(35))), None);
        assert_eq!(secret("eyJshort.eyJshort"), None);
        assert_eq!(secret("https://user@host/path"), None);
        assert_eq!(secret("https://example.com/a:b@c"), None);
        assert_eq!(secret("plain words only"), None);
    }

    #[test]
    fn sk_dash_needs_twenty_alphanumerics() {
        assert_eq!(secret(&format!("sk-{}", "A1".repeat(10))), Some("sk-"));
        assert_eq!(secret(&format!("sk-{}", "a".repeat(19))), None);
        assert_eq!(secret("disk-usage"), None);
    }

    #[test]
    fn telegram_bot_token() {
        let token = format!("123456789:{}", "A-b_".repeat(9));
        assert_eq!(secret(&token), Some("telegram"));
        assert_eq!(secret(&format!("123:{}", "a".repeat(34))), None);
        assert_eq!(secret(&format!("abc:{}", "a".repeat(40))), None);
        assert_eq!(secret("time 12:30:45"), None);
        assert_eq!(secret(&format!("v2:{}", "a".repeat(40))), None);
        assert_eq!(secret(&format!("\u{1F9F5}:{}", "a".repeat(40))), None);
        assert_eq!(
            secret(&format!("\u{1F9F5}7:{}", "a".repeat(40))),
            Some("telegram")
        );
    }

    #[test]
    fn hex64_unless_sha256_labelled() {
        let hex = "0123456789abcdef".repeat(4);
        assert_eq!(secret(&format!("key {hex}")), Some("hex64"));
        assert_eq!(
            secret(&format!("x{}", "ABCDEF0123456789".repeat(4))),
            Some("hex64")
        );
        for label in ["sha256:", "sha256=", "sha256-"] {
            assert_eq!(secret(&format!("{label}{hex}")), None, "{label}");
        }
        assert_eq!(secret(&hex[..63]), None);
        assert_eq!(
            secret("commit 4b825dc642cb6eb9a060e54bf8d69288fbee4904"),
            None
        );
    }

    #[test]
    fn seed_phrase_lines() {
        let twelve =
            "abandon ability able about above absent absorb abstract absurd abuse access accident";
        assert_eq!(secret(twelve), Some("seed phrase"));
        assert_eq!(
            secret(&format!("intro\n{twelve} {twelve}\nend")),
            Some("seed phrase")
        );
        assert_eq!(secret(&format!("{twelve} extra")), None);
        assert_eq!(secret(&twelve.replace("abandon", "Abandon")), None);
        assert_eq!(secret(&twelve.replace("able", "ab")), None);
        assert_eq!(secret(&twelve.replace("accident", "accidents")), None);
    }

    #[test]
    fn hidden_characters() {
        for c in [
            '\u{200B}',
            '\u{200F}',
            '\u{202A}',
            '\u{202E}',
            '\u{2066}',
            '\u{2069}',
            '\u{FEFF}',
            '\u{E0000}',
            '\u{E007F}',
        ] {
            assert!(hidden(&format!("a{c}b")), "{:X}", c as u32);
        }
        for c in [
            '\u{2010}',
            '\u{2029}',
            '\u{2065}',
            '\u{206A}',
            '\u{E0080}',
            'é',
        ] {
            assert!(!hidden(&format!("a{c}b")), "{:X}", c as u32);
        }
    }

    #[test]
    fn redact_replaces_secrets_and_strips_hidden() {
        assert_eq!(redact("token=\"ghp_abc123\" ok"), "token=\"[redacted]\" ok");
        assert_eq!(
            redact("db postgres://u:p@h/x"),
            "db postgres://[redacted]@h/x"
        );
        assert_eq!(
            redact("a\n-----BEGIN KEY-----\nzzz\n-----END KEY-----\nb"),
            "a\n[redacted]\nb"
        );
        // A zero-width space cannot hide a marker.
        assert_eq!(redact("gh\u{200B}p_secret1 x"), "[redacted] x");
        assert_eq!(redact("caf\u{FEFF}é"), "café");
        // Overlapping matches give one placeholder.
        let jwt = "Bearer eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.sig";
        assert_eq!(redact(jwt), "[redacted]");
        assert_eq!(redact("nothing here"), "nothing here");
        assert_eq!(secret(&redact(&format!("x sk-{} y", "a".repeat(30)))), None);
    }
}
