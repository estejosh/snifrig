//! Minimal JSON helpers for the flat one-line objects snifrig writes. No serde, by design.

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// String value of "key" in a flat object. Handles escaped quotes and backslashes.
pub fn str_field(s: &str, key: &str) -> Option<String> {
    let k = format!("\"{}\":\"", key);
    let start = s.find(&k)? + k.len();
    let mut out = String::new();
    let mut it = s[start..].chars();
    while let Some(c) = it.next() {
        match c {
            '"' => return Some(out),
            '\\' => match it.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let h: String = it.by_ref().take(4).collect();
                    if let Some(ch) = u32::from_str_radix(&h, 16).ok().and_then(char::from_u32) { out.push(ch); }
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// Numeric value of "key" in a flat object.
pub fn num_field(s: &str, key: &str) -> Option<f64> {
    let k = format!("\"{}\":", key);
    let r = &s[s.find(&k)? + k.len()..];
    let r = r.trim_start();
    let end = r.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e' || c == 'E' || c == '+')).unwrap_or(r.len());
    r[..end].parse().ok()
}
