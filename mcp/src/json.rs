//! Tiny JSON parser/serializer (no dependencies).

#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(v) => v.iter().find(|(a, _)| a == k).map(|(_, b)| b),
            _ => None,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn f64(&self) -> Option<f64> {
        match self {
            J::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn arr(&self) -> Option<&[J]> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn s(x: &str) -> J {
        J::Str(x.to_string())
    }
    pub fn obj(v: Vec<(&str, J)>) -> J {
        J::Obj(v.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
    pub fn ser(&self) -> String {
        let mut o = String::new();
        self.write(&mut o);
        o
    }
    fn write(&self, o: &mut String) {
        match self {
            J::Null => o.push_str("null"),
            J::Bool(b) => o.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => {
                if !n.is_finite() {
                    o.push_str("null")
                } else if n.fract() == 0.0 && n.abs() < 1e15 {
                    o.push_str(&(*n as i64).to_string())
                } else {
                    o.push_str(&n.to_string())
                }
            }
            J::Str(s) => esc(s, o),
            J::Arr(a) => {
                o.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        o.push(',')
                    }
                    x.write(o);
                }
                o.push(']');
            }
            J::Obj(m) => {
                o.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        o.push(',')
                    }
                    esc(k, o);
                    o.push(':');
                    v.write(o);
                }
                o.push('}');
            }
        }
    }
}

fn esc(s: &str, o: &mut String) {
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{8}' => o.push_str("\\b"),
            '\u{c}' => o.push_str("\\f"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
}

pub fn parse(s: &str) -> Result<J, String> {
    let mut p = P { b: s.as_bytes(), i: 0 };
    p.ws();
    let v = p.val(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("trailing data at {}", p.i));
    }
    Ok(v)
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn lit(&mut self, w: &str, v: J) -> Result<J, String> {
        if self.b[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(v)
        } else {
            Err(format!("bad literal at {}", self.i))
        }
    }
    fn val(&mut self, d: usize) -> Result<J, String> {
        if d > 64 {
            return Err("nesting too deep".into());
        }
        match self.b.get(self.i) {
            None => Err("unexpected end".into()),
            Some(b'n') => self.lit("null", J::Null),
            Some(b't') => self.lit("true", J::Bool(true)),
            Some(b'f') => self.lit("false", J::Bool(false)),
            Some(b'"') => Ok(J::Str(self.string()?)),
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(J::Arr(a));
                }
                loop {
                    self.ws();
                    a.push(self.val(d + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(J::Arr(a));
                        }
                        _ => return Err(format!("expected , or ] at {}", self.i)),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut m = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(J::Obj(m));
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return Err(format!("expected key at {}", self.i));
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return Err(format!("expected : at {}", self.i));
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.val(d + 1)?;
                    m.push((k, v));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(J::Obj(m));
                        }
                        _ => return Err(format!("expected , or }} at {}", self.i)),
                    }
                }
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let st = self.i;
                self.i += 1;
                while self.i < self.b.len()
                    && matches!(self.b[self.i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    self.i += 1;
                }
                std::str::from_utf8(&self.b[st..self.i])
                    .ok()
                    .and_then(|t| t.parse::<f64>().ok())
                    .map(J::Num)
                    .ok_or_else(|| format!("bad number at {st}"))
            }
            Some(_) => Err(format!("unexpected character at {}", self.i)),
        }
    }
    fn hex4(&mut self) -> Result<u32, String> {
        if self.i + 4 > self.b.len() {
            return Err("bad \\u escape".into());
        }
        let t = std::str::from_utf8(&self.b[self.i..self.i + 4]).map_err(|_| "bad \\u escape")?;
        let v = u32::from_str_radix(t, 16).map_err(|_| "bad \\u escape".to_string())?;
        self.i += 4;
        Ok(v)
    }
    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.i).ok_or("unterminated string")?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("bad escape")?;
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let mut u = self.hex4()?;
                            if (0xD800..0xDC00).contains(&u) {
                                if self.b[self.i..].starts_with(b"\\u") {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&lo) {
                                        u = 0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00);
                                    } else {
                                        u = 0xFFFD;
                                    }
                                } else {
                                    u = 0xFFFD;
                                }
                            }
                            char::from_u32(u).unwrap_or('\u{FFFD}')
                        }
                        _ => return Err("bad escape".into()),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| "invalid utf-8".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let v = parse(r#"{"a":[1,2.5,-3e2,true,null],"b":"x\ny\"\\ é 😀","c":{}}"#).unwrap();
        assert_eq!(v.get("b").unwrap().str().unwrap(), "x\ny\"\\ é 😀");
        let s = v.ser();
        assert_eq!(parse(&s).unwrap(), v);
        assert!(s.contains("[1,2.5,-300,true,null]"));
    }
    #[test]
    fn escapes_control() {
        let s = J::s("a\u{1}b\tc\"").ser();
        assert_eq!(s, "\"a\\u0001b\\tc\\\"\"");
    }
    #[test]
    fn errors() {
        assert!(parse("{").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("1 2").is_err());
        assert!(parse("\"abc").is_err());
        assert!(parse(&"[".repeat(100)).is_err());
    }
    #[test]
    fn lone_surrogate() {
        assert_eq!(parse(r#""\ud800x""#).unwrap().str().unwrap(), "\u{FFFD}x");
    }
}
