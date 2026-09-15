//! Tokenizer shared by index and search time (the two MUST agree):
//! lowercase, split on any byte outside [a-z0-9_], drop tokens > 64 bytes.
//! Index-time fields are indexed as composite `name::value` terms.

pub const MAX_TOKEN: usize = 64;

pub fn tokenize(raw: &[u8], mut emit: impl FnMut(&[u8])) {
    let mut buf = [0u8; MAX_TOKEN];
    let mut len = 0usize;
    let mut oversize = false;
    for &b in raw {
        let c = match b {
            b'a'..=b'z' | b'0'..=b'9' | b'_' => b,
            b'A'..=b'Z' => b + 32,
            _ => 0,
        };
        if c != 0 {
            if len < MAX_TOKEN {
                buf[len] = c;
                len += 1;
            } else {
                oversize = true;
            }
        } else {
            if len > 0 && !oversize {
                emit(&buf[..len]);
            }
            len = 0;
            oversize = false;
        }
    }
    if len > 0 && !oversize {
        emit(&buf[..len]);
    }
}

/// Composite indexed-field term, e.g. `sourcetype::syslog` (value lowercased).
pub fn field_term(name: &str, value: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(name.len() + 2 + value.len());
    t.extend_from_slice(name.as_bytes());
    t.extend_from_slice(b"::");
    t.extend(value.iter().map(u8::to_ascii_lowercase));
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(raw: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        tokenize(raw, |t| out.push(String::from_utf8(t.to_vec()).unwrap()));
        out
    }

    #[test]
    fn tokenizes_lowercase_on_boundaries() {
        assert_eq!(toks(b"GET /api/users?id=42 HTTP/1.1"), ["get", "api", "users", "id", "42", "http", "1", "1"]);
        assert_eq!(toks(b"needle_3=ok"), ["needle_3", "ok"]);
        assert_eq!(toks(b""), Vec::<String>::new());
        assert_eq!(toks(b"...!!!"), Vec::<String>::new());
    }

    #[test]
    fn drops_oversize_tokens() {
        let long = vec![b'a'; 65];
        let mut input = long.clone();
        input.extend_from_slice(b" ok");
        assert_eq!(toks(&input), ["ok"]);
        assert_eq!(toks(&vec![b'a'; 64]).len(), 1); // exactly 64 is kept
    }

    #[test]
    fn field_terms() {
        assert_eq!(field_term("sourcetype", b"NginX"), b"sourcetype::nginx".to_vec());
    }
}
