//! A tiny JSON reader, enough for snifrig's own files.
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum J { Null, Bool(bool), Num(f64), Str(String), Arr(Vec<J>), Obj(BTreeMap<String, J>) }

impl J {
    pub fn get(&self, k: &str) -> &J { match self { J::Obj(m) => m.get(k).unwrap_or(&J::Null), _ => &J::Null } }
    pub fn str(&self) -> &str { match self { J::Str(s) => s, _ => "" } }
    pub fn num(&self) -> f64 { match self { J::Num(n) => *n, J::Bool(true) => 1.0, _ => 0.0 } }
    pub fn bool(&self) -> bool { matches!(self, J::Bool(true)) }
    pub fn arr(&self) -> &[J] { match self { J::Arr(v) => v, _ => &[] } }
    pub fn is_null(&self) -> bool { matches!(self, J::Null) }
}

pub fn parse(s: &str) -> Option<J> {
    let s = s.trim_start_matches('\u{feff}');
    let b = s.as_bytes();
    let mut i = 0;
    let v = val(b, &mut i)?;
    Some(v)
}

fn ws(b: &[u8], i: &mut usize) { while *i < b.len() && (b[*i] as char).is_ascii_whitespace() { *i += 1; } }

fn val(b: &[u8], i: &mut usize) -> Option<J> {
    ws(b, i);
    match *b.get(*i)? {
        b'{' => {
            *i += 1;
            let mut m = BTreeMap::new();
            loop {
                ws(b, i);
                if *b.get(*i)? == b'}' { *i += 1; return Some(J::Obj(m)); }
                let k = match val(b, i)? { J::Str(s) => s, _ => return None };
                ws(b, i);
                if *b.get(*i)? != b':' { return None; }
                *i += 1;
                let v = val(b, i)?;
                m.insert(k, v);
                ws(b, i);
                match *b.get(*i)? { b',' => *i += 1, b'}' => { *i += 1; return Some(J::Obj(m)); } _ => return None }
            }
        }
        b'[' => {
            *i += 1;
            let mut v = Vec::new();
            loop {
                ws(b, i);
                if *b.get(*i)? == b']' { *i += 1; return Some(J::Arr(v)); }
                v.push(val(b, i)?);
                ws(b, i);
                match *b.get(*i)? { b',' => *i += 1, b']' => { *i += 1; return Some(J::Arr(v)); } _ => return None }
            }
        }
        b'"' => {
            *i += 1;
            let mut out: Vec<u8> = Vec::new();
            while *i < b.len() {
                let c = b[*i];
                *i += 1;
                match c {
                    b'"' => return Some(J::Str(String::from_utf8_lossy(&out).into_owned())),
                    b'\\' => {
                        let e = *b.get(*i)?;
                        *i += 1;
                        match e {
                            b'n' => out.push(b'\n'), b't' => out.push(b'\t'), b'r' => out.push(b'\r'),
                            b'u' => {
                                let h = std::str::from_utf8(b.get(*i..*i + 4)?).ok()?;
                                *i += 4;
                                let ch = char::from_u32(u32::from_str_radix(h, 16).ok()?).unwrap_or('?');
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                            other => out.push(other),
                        }
                    }
                    _ => out.push(c),
                }
            }
            None
        }
        b't' => { *i += 4; Some(J::Bool(true)) }
        b'f' => { *i += 5; Some(J::Bool(false)) }
        b'n' => { *i += 4; Some(J::Null) }
        _ => {
            let st = *i;
            while *i < b.len() && matches!(b[*i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') { *i += 1; }
            std::str::from_utf8(&b[st..*i]).ok()?.parse().ok().map(J::Num)
        }
    }
}

#[cfg(test)]
mod t {
    use super::*;
    #[test]
    fn parses_snifrig_files() {
        let j = parse(r#"{"unix":17,"game":null,"demoted":[{"pid":4,"name":"a\"b","level":2}],"ok":true,"s":"é"}"#).unwrap();
        assert_eq!(j.get("unix").num(), 17.0);
        assert!(j.get("game").is_null() && j.get("ok").bool());
        assert_eq!(j.get("demoted").arr()[0].get("name").str(), "a\"b");
        assert_eq!(j.get("s").str(), "é");
    }
}
