//! Relative IRI resolution (RFC 3986 §5.2). `s-parser` leaves `BASE`
//! handling to the caller; Turtle data and SPARQL queries both resolve here.

/// Whether `iri` starts with a scheme (`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`).
pub fn has_scheme(iri: &str) -> bool {
    let bytes = iri.as_bytes();
    match bytes.first() {
        Some(b) if b.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for &b in &bytes[1..] {
        match b {
            b':' => return true,
            b if b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.') => {}
            _ => return false,
        }
    }
    false
}

struct Parts<'a> {
    scheme: Option<&'a str>,
    authority: Option<&'a str>,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

fn split(iri: &str) -> Parts<'_> {
    let (rest, fragment) = match iri.find('#') {
        Some(i) => (&iri[..i], Some(&iri[i + 1..])),
        None => (iri, None),
    };
    let (rest, query) = match rest.find('?') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let (scheme, rest) = if has_scheme(rest) {
        let i = rest.find(':').expect("has_scheme found one");
        (Some(&rest[..i]), &rest[i + 1..])
    } else {
        (None, rest)
    };
    let (authority, path) = match rest.strip_prefix("//") {
        Some(after) => {
            let end = after.find('/').unwrap_or(after.len());
            (Some(&after[..end]), &after[end..])
        }
        None => (None, rest),
    };
    Parts { scheme, authority, path, query, fragment }
}

/// Resolves `reference` against `base`, appending the result to `out`.
/// An absolute `reference` is appended with only dot-segment removal.
pub fn resolve_into(base: &str, reference: &str, out: &mut String) {
    let r = split(reference);
    let b = split(base);
    let mut path = String::new();
    let (scheme, authority, query);
    if let Some(s) = r.scheme {
        scheme = Some(s);
        authority = r.authority;
        remove_dot_segments(r.path, &mut path);
        query = r.query;
    } else {
        scheme = b.scheme;
        if r.authority.is_some() {
            authority = r.authority;
            remove_dot_segments(r.path, &mut path);
            query = r.query;
        } else {
            authority = b.authority;
            if r.path.is_empty() {
                path.push_str(b.path);
                query = r.query.or(b.query);
            } else {
                if r.path.starts_with('/') {
                    remove_dot_segments(r.path, &mut path);
                } else {
                    let mut merged = String::with_capacity(b.path.len() + r.path.len() + 1);
                    if b.authority.is_some() && b.path.is_empty() {
                        merged.push('/');
                    } else if let Some(i) = b.path.rfind('/') {
                        merged.push_str(&b.path[..=i]);
                    }
                    merged.push_str(r.path);
                    remove_dot_segments(&merged, &mut path);
                }
                query = r.query;
            }
        }
    }
    if let Some(s) = scheme {
        out.push_str(s);
        out.push(':');
    }
    if let Some(a) = authority {
        out.push_str("//");
        out.push_str(a);
    }
    out.push_str(&path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    if let Some(f) = r.fragment {
        out.push('#');
        out.push_str(f);
    }
}

/// RFC 3986 §5.2.4.
fn remove_dot_segments(mut input: &str, out: &mut String) {
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("../") {
            input = rest;
        } else if let Some(rest) = input.strip_prefix("./") {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") || input == "/.." {
            input = if input == "/.." { "/" } else { &input[3..] };
            let cut = out.rfind('/').unwrap_or(0);
            out.truncate(cut);
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = input[start..].find('/').map_or(input.len(), |i| i + start);
            out.push_str(&input[..end]);
            input = &input[end..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(base: &str, r: &str) -> String {
        let mut out = String::new();
        resolve_into(base, r, &mut out);
        out
    }

    #[test]
    fn detects_schemes() {
        assert!(has_scheme("http://x"));
        assert!(has_scheme("urn:a"));
        assert!(has_scheme("a+b-c.d:x"));
        assert!(!has_scheme("/a:b"));
        assert!(!has_scheme("1a:b"));
        assert!(!has_scheme("abc"));
        assert!(!has_scheme(""));
    }

    /// RFC 3986 §5.4.1 normal examples and §5.4.2 abnormal examples.
    #[test]
    fn rfc3986_examples() {
        let base = "http://a/b/c/d;p?q";
        let cases = [
            ("g:h", "g:h"),
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q#s"),
            ("g#s", "http://a/b/c/g#s"),
            ("g?y#s", "http://a/b/c/g?y#s"),
            (";x", "http://a/b/c/;x"),
            ("g;x", "http://a/b/c/g;x"),
            ("g;x?y#s", "http://a/b/c/g;x?y#s"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("./", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../", "http://a/"),
            ("../../g", "http://a/g"),
            ("../../../g", "http://a/g"),
            ("../../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            (".g", "http://a/b/c/.g"),
            ("g..", "http://a/b/c/g.."),
            ("..g", "http://a/b/c/..g"),
            ("./../g", "http://a/b/g"),
            ("./g/.", "http://a/b/c/g/"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
            ("g;x=1/../y", "http://a/b/c/y"),
            ("g?y/./x", "http://a/b/c/g?y/./x"),
            ("g#s/../x", "http://a/b/c/g#s/../x"),
        ];
        for (r, expected) in cases {
            assert_eq!(resolve(base, r), expected, "resolving {r:?}");
        }
    }

    #[test]
    fn base_without_path() {
        assert_eq!(resolve("http://a", "b"), "http://a/b");
        assert_eq!(resolve("http://a#f", "#g"), "http://a#g");
    }
}
