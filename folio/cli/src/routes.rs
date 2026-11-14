//! Requests by method and path, as a feature writes them: `POST /api/auth/token`, `GET|PUT
//! /cache/**`, `* /mcp`, for the routes a credential is never sent on (forbid), the only ones it's
//! sent on (allow), and where else a tool presents its stand-in (rewrite).
//!
//! Methods are separated by `|`; `*` is any. In the path, `*` is any part of one segment, `**` any
//! number of whole segments (none included), and `{a,b}` either of its alternatives, which may hold
//! slashes. Letters match whatever their case.

use std::collections::BTreeSet;

use crate::repr;

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// As written, trimmed.
    pub text: String,
    /// Uppercase; empty for any.
    pub methods: BTreeSet<String>,
    /// Its path's, braces expanded, lowercase.
    pub paths: Vec<String>,
}

impl Route {
    pub fn parse(text: &str) -> Result<Route, String> {
        let trimmed = text.trim();
        let (methods, path) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
        let path = path.trim();
        if methods.is_empty() || !path.starts_with('/') || path.chars().any(char::is_whitespace) {
            return Err(format!(
                "{} isn't METHOD[|METHOD] /path, e.g. POST /api/auth/token",
                repr(text)
            ));
        }
        let names: BTreeSet<String> = methods.split('|').map(str::to_uppercase).collect();
        let any = names.len() == 1 && names.contains("*");
        if !any
            && !names
                .iter()
                .all(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_uppercase()))
        {
            return Err(format!(
                "{}: methods are names separated by |, or *",
                repr(text)
            ));
        }
        let paths = braces_expanded(path)?
            .into_iter()
            .map(|p| p.to_lowercase())
            .collect();
        Ok(Route {
            text: trimmed.to_string(),
            methods: if any { BTreeSet::new() } else { names },
            paths,
        })
    }

    /// Whether a request for `path`, already canonical, by `method`, is one of this route's.
    pub fn matches(&self, method: &str, path: &str) -> bool {
        let literal: Vec<Token> = path.to_lowercase().chars().map(Token::Char).collect();
        (self.methods.is_empty() || self.methods.contains(&method.to_uppercase()))
            && self.paths.iter().any(|p| paths_meet(&tokens(p), &literal))
    }

    /// Whether a request could be both one of this route's and one `forbidden` refuses: also with
    /// an extension on the last segment, taken here as anything more after it, which errs wide.
    pub fn overlaps_forbidden(&self, forbidden: &Route) -> bool {
        if !self.methods.is_empty()
            && !forbidden.methods.is_empty()
            && self.methods.is_disjoint(&forbidden.methods)
        {
            return false;
        }
        self.paths.iter().any(|mine| {
            forbidden.paths.iter().any(|theirs| {
                let (mine, theirs) = (tokens(mine), tokens(theirs));
                let mut extended = theirs.clone();
                extended.extend([Token::Char('.'), Token::Star]);
                paths_meet(&mine, &theirs) || paths_meet(&mine, &extended)
            })
        })
    }
}

/// A request's path as a server is likely to route it, lowercase; None if it should go to none:
/// escapes decoded until there are none left (a server, or one in front of it, may decode twice), a
/// backslash a slash, a segment's parameters (`;jsessionid=...`) dropped, and empty segments, `.`
/// and `..` resolved. Erring wide is the point: a route is only ever refused on this, never opened.
pub fn canonical(path: &str) -> Option<String> {
    let mut path = path.to_string();
    let mut settled = false;
    for _ in 0..4 {
        let decoded = percent_decoded(&path);
        if decoded == path {
            settled = true;
            break;
        }
        path = decoded;
    }
    if !settled || path.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
        return None;
    }
    let mut segments: Vec<&str> = Vec::new();
    let replaced = path.replace('\\', "/");
    for segment in replaced.split('/') {
        let segment = segment.split(';').next().unwrap_or("");
        match segment {
            ".." => {
                segments.pop();
            }
            "" | "." => {}
            s => segments.push(s),
        }
    }
    Some(format!("/{}", segments.join("/")).to_lowercase())
}

/// `s` with %XX escapes decoded, as UTF-8 (anything else as U+FFFD), as Python's unquote does.
fn percent_decoded(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a request for `path` (canonical) by `method` is one of `forbid`'s: also with an
/// extension on its last segment, which some servers route as the path without it
/// (`/api/auth/token.json`): a refusal errs wide.
pub fn forbids(forbid: &[Route], method: &str, path: &str) -> bool {
    let unsuffixed = match path.rsplit_once('.') {
        Some((stem, ext)) if !ext.contains('/') && !stem.ends_with('/') => stem,
        _ => path,
    };
    forbid
        .iter()
        .any(|r| r.matches(method, path) || r.matches(method, unsuffixed))
}

/// Whether a credential with routes `allow`, and rewrites on `rewritten`, may go on a request for
/// `path` (canonical) by `method`: anywhere, when it has no allow routes.
pub fn allows(allow: &[Route], rewritten: &[Route], method: &str, path: &str) -> bool {
    allow.is_empty()
        || allow
            .iter()
            .chain(rewritten)
            .any(|r| r.matches(method, path))
}

/// A route's path as the paths its `{a,b}` groups stand for: `/{a,b}/c` is `/a/c` and `/b/c`.
fn braces_expanded(path: &str) -> Result<Vec<String>, String> {
    // The first group with no brace inside it.
    let group = path.char_indices().find_map(|(open, c)| {
        if c != '{' {
            return None;
        }
        let rest = &path[open + 1..];
        let close = rest.find(['{', '}'])?;
        (rest.as_bytes()[close] == b'}').then_some((open, open + 1 + close))
    });
    let Some((open, close)) = group else {
        if path.contains(['{', '}']) {
            return Err(format!("{}: a brace without its pair", repr(path)));
        }
        return Ok(vec![path.to_string()]);
    };
    let mut expanded = Vec::new();
    for alternative in path[open + 1..close].split(',') {
        let joined = format!("{}{alternative}{}", &path[..open], &path[close + 1..]);
        expanded.extend(braces_expanded(&joined)?);
    }
    Ok(expanded)
}

/// A route's path, read for telling whether two meet: each a character, or `*` (any characters but
/// a slash) or `**` (any number of slash-led segments).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Token {
    Char(char),
    Star,
    Segments,
}

fn tokens(path: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    for segment in path.split('/') {
        if segment == "**" {
            tokens.push(Token::Segments);
        } else if !segment.is_empty() {
            tokens.push(Token::Char('/'));
            for (i, part) in segment.split('*').enumerate() {
                if i > 0 {
                    tokens.push(Token::Star);
                }
                tokens.extend(part.chars().map(Token::Char));
            }
        }
    }
    if tokens.is_empty() {
        tokens.push(Token::Char('/'));
    }
    tokens
}

/// What a step reads: a character, any character but a slash, or nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Read {
    Char(char),
    NotSlash,
    Nothing,
}

/// A state of a walk of a path's tokens: where it is, and whether within a `**`, past one of its
/// slashes.
type State = (usize, bool);

fn moves(tokens: &[Token], (i, inside): State) -> Vec<(Read, State)> {
    if inside {
        return vec![(Read::Nothing, (i, false)), (Read::NotSlash, (i, true))];
    }
    match tokens.get(i) {
        None => vec![],
        Some(Token::Star) => vec![
            (Read::Nothing, (i + 1, false)),
            (Read::NotSlash, (i, false)),
        ],
        Some(Token::Segments) => vec![
            (Read::Nothing, (i + 1, false)),
            (Read::Char('/'), (i, true)),
        ],
        Some(Token::Char(c)) => vec![(Read::Char(*c), (i + 1, false))],
    }
}

fn meet(x: Read, y: Read) -> bool {
    match (x, y) {
        (Read::NotSlash, Read::NotSlash) => true,
        (Read::NotSlash, Read::Char(c)) | (Read::Char(c), Read::NotSlash) => c != '/',
        (x, y) => x == y,
    }
}

/// Whether some path matches both token lists: a walk of the two automata in step.
fn paths_meet(a: &[Token], b: &[Token]) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<(State, State)> = vec![((0, false), (0, false))];
    while let Some(state) = stack.pop() {
        if !seen.insert(state) {
            continue;
        }
        let (sa, sb) = state;
        if sa == (a.len(), false) && sb == (b.len(), false) {
            return true;
        }
        for (ca, na) in moves(a, sa) {
            if ca == Read::Nothing {
                stack.push((na, sb));
                continue;
            }
            for (cb, nb) in moves(b, sb) {
                if cb == Read::Nothing {
                    stack.push((sa, nb));
                } else if meet(ca, cb) {
                    stack.push((na, nb));
                }
            }
        }
        for (cb, nb) in moves(b, sb) {
            if cb == Read::Nothing {
                stack.push((sa, nb));
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(text: &str) -> Route {
        Route::parse(text).unwrap()
    }

    #[test]
    fn a_route_is_methods_and_a_path() {
        let r = route(" get|POST /api/{a,b/c}/x ");
        assert_eq!(r.text, "get|POST /api/{a,b/c}/x");
        assert_eq!(r.methods.iter().collect::<Vec<_>>(), ["GET", "POST"]);
        assert_eq!(r.paths, ["/api/a/x", "/api/b/c/x"]);
        assert!(route("* /mcp").methods.is_empty());
        for bad in ["POST", "POST api", "POST /a b", "PO5T /a", "POST /{a"] {
            assert!(Route::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(
            Route::parse("POST /{a").unwrap_err(),
            "'/{a': a brace without its pair"
        );
    }

    #[test]
    fn a_route_matches_requests_by_method_and_path_whatever_their_case() {
        let r = route("GET|PUT /cache/**");
        assert!(r.matches("get", "/cache"));
        assert!(r.matches("PUT", "/CACHE/a/b"));
        assert!(!r.matches("POST", "/cache/a"));
        assert!(!r.matches("GET", "/cached"));
        let star = route("POST /api/*/token");
        assert!(star.matches("POST", "/api/v2/token"));
        assert!(!star.matches("POST", "/api/v2/x/token"));
    }

    #[test]
    fn a_path_as_a_server_routes_it() {
        assert_eq!(
            canonical("/api/auth//%74oken;x").as_deref(),
            Some("/api/auth/token")
        );
        assert_eq!(canonical("/a/./b/../c\\D").as_deref(), Some("/a/c/d"));
        assert_eq!(canonical("/%2574").as_deref(), Some("/t"));
        assert_eq!(canonical("/a%00b"), None);
        assert_eq!(
            canonical("/%252525252574"),
            None,
            "escaped past what's decoded"
        );
    }

    #[test]
    fn an_allowed_route_that_reaches_a_forbidden_one_overlaps_it() {
        let forbidden = route("POST /api/auth/token");
        assert!(route("POST /api/**").overlaps_forbidden(&forbidden));
        assert!(route("* /api/*/token").overlaps_forbidden(&forbidden));
        assert!(!route("GET /api/**").overlaps_forbidden(&forbidden));
        assert!(!route("POST /api/other").overlaps_forbidden(&forbidden));
        // With an extension on its last segment, as a server may route it.
        assert!(route("POST /api/auth/token.json").overlaps_forbidden(&forbidden));
    }
}
