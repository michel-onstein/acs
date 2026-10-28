//! Remote wildcards (ACSCP_DESIGN §5.4, decision 4): `acscp 'host:logs/*.gz' .`.
//!
//! There is no remote shell, so `acs _cp` expands a source path itself,
//! one component at a time, and the receiver accepts at the top level any
//! name the last component matches — the check modern `scp` makes.
//!
//! A component is a pattern when it holds a `*`, `?` or `[`: `*` matches
//! any run of characters, `?` one, `[abc]`, `[a-z]` and `[!a-z]` (or `^`)
//! one in or out of the set; `\` takes the next character literally, and a
//! `[` with no `]` after it is itself. A name beginning with `.` is matched
//! only by a pattern that does, as a shell's glob does. A component with
//! none of the three is a plain name, compared as it is, backslashes and
//! all.

use std::path::{Component, Path, PathBuf};

/// Whether `s` holds a wildcard, and so is matched rather than compared.
pub fn is_pattern(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Whether the top-level name `name` is one `wanted` asks for: matched if
/// it is a pattern, equal otherwise.
pub fn accepts(wanted: &str, name: &str) -> bool {
    match is_pattern(wanted) {
        true => matches(wanted, name),
        false => wanted == name,
    }
}

/// `name` as the pattern that matches only itself, where it would
/// otherwise be read as one: what an upload's receiver is told to accept.
pub fn literal(name: &str) -> String {
    if !is_pattern(name) {
        return name.into();
    }
    let mut s = String::new();
    for c in name.chars() {
        if "*?[\\".contains(c) {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// Whether the one component `name` matches `pat`.
pub fn matches(pat: &str, name: &str) -> bool {
    if name.starts_with('.') && !pat.starts_with('.') {
        return false;
    }
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    // Where the last `*` was, and how much of the name it has taken.
    let mut star = None;
    while ni < n.len() {
        if p.get(pi) == Some(&'*') {
            pi += 1;
            star = Some((pi, ni));
            continue;
        }
        if pi < p.len() {
            let (hit, len) = one(&p[pi..], n[ni]);
            if hit {
                pi += len;
                ni += 1;
                continue;
            }
        }
        // No match here: the last `*` takes one more character.
        let Some((sp, sn)) = star else { return false };
        star = Some((sp, sn + 1));
        (pi, ni) = (sp, sn + 1);
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Whether the element at the start of `p` matches `c`, and its length.
fn one(p: &[char], c: char) -> (bool, usize) {
    match p {
        ['?', ..] => (true, 1),
        ['\\', e, ..] => (*e == c, 2),
        ['[', ..] => class(p, c).unwrap_or((c == '[', 1)),
        [x, ..] => (*x == c, 1),
        [] => (false, 0),
    }
}

/// The bracket expression at the start of `p`: whether `c` is in it, and
/// its length; `None` when it has no closing `]`.
fn class(p: &[char], c: char) -> Option<(bool, usize)> {
    let mut i = 1;
    let not = matches!(p.get(i), Some('!' | '^'));
    i += not as usize;
    let mut hit = false;
    let mut first = true;
    // One character of the set, `\` taking the next literally.
    let take = |i: &mut usize| -> Option<char> {
        let mut ch = *p.get(*i)?;
        if ch == '\\' {
            *i += 1;
            ch = *p.get(*i)?;
        }
        *i += 1;
        Some(ch)
    };
    loop {
        if p.get(i) == Some(&']') && !first {
            return Some((hit != not, i + 1));
        }
        first = false;
        let lo = take(&mut i)?;
        let mut hi = lo;
        if p.get(i) == Some(&'-') && p.get(i + 1).is_some_and(|&x| x != ']') {
            i += 1;
            hi = take(&mut i)?;
        }
        hit |= lo <= c && c <= hi;
    }
}

/// The paths `path` names on this host, each component that is a pattern
/// matched against its directory's entries, in byte order: none when
/// nothing matches. Names that are not UTF-8 match nothing, since a name
/// travels as a string.
pub fn expand(path: &Path) -> Vec<PathBuf> {
    let mut found = vec![PathBuf::new()];
    for c in path.components() {
        let pat = match c {
            Component::Normal(s) => s.to_str().filter(|s| is_pattern(s)),
            _ => None,
        };
        found = match pat {
            None => found.into_iter().map(|f| f.join(c)).collect(),
            Some(pat) => found
                .iter()
                .flat_map(|dir| {
                    let at = if dir.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        dir
                    };
                    super::walk::sorted_names(at)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|n| n.to_str().is_some_and(|n| matches(pat, n)))
                        .map(move |n| dir.join(n))
                })
                .collect(),
        };
    }
    found.retain(|p| p.symlink_metadata().is_ok());
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_as_a_shells_glob_does() {
        for (pat, name, want) in [
            ("*", "a", true),
            ("*", "", true),
            ("*.gz", "a.gz", true),
            ("*.gz", "a.gz.1", false),
            ("*.gz", ".gz", false),
            ("a*b*c", "aXbYbZc", true),
            ("a*b*c", "aXbYbZ", false),
            ("**x", "abx", true),
            ("?", "é", true),
            ("?", "ab", false),
            ("a?c", "abc", true),
            ("[abc]", "b", true),
            ("[abc]", "d", false),
            ("[a-c]x", "bx", true),
            ("[!a-c]", "b", false),
            ("[^a-c]", "d", true),
            ("[]]", "]", true),
            ("[!]]", "]", false),
            ("[a-]", "-", true),
            ("[\\]]", "]", true),
            ("[", "[", true),
            ("a[b", "a[b", true),
            ("\\*", "*", true),
            ("\\*", "a", false),
            ("\\?x", "?x", true),
            ("log-[0-9][0-9].txt", "log-07.txt", true),
            ("log-[0-9][0-9].txt", "log-7.txt", false),
            // Hidden names only for a pattern that names the dot.
            ("*", ".bashrc", false),
            ("?bashrc", ".bashrc", false),
            ("[.]bashrc", ".bashrc", false),
            (".*", ".bashrc", true),
            (".b*", ".bashrc", true),
        ] {
            assert_eq!(matches(pat, name), want, "{pat:?} {name:?}");
        }
    }

    #[test]
    fn a_plain_name_is_compared_and_a_pattern_matched() {
        assert!(is_pattern("*.gz") && is_pattern("a?") && is_pattern("[x"));
        assert!(!is_pattern("notes.txt") && !is_pattern("a\\b"));
        assert!(accepts("notes.txt", "notes.txt"));
        assert!(!accepts("notes.txt", "other"));
        assert!(accepts("a\\b", "a\\b"));
        assert!(accepts("*.gz", "x.gz"));
        assert!(!accepts("*.gz", ".bashrc"));
    }

    /// An upload's names are sent as patterns that match only themselves,
    /// so a file called `[x]` is not refused for not matching `x`.
    #[test]
    fn a_literal_matches_only_itself() {
        for name in ["plain", "a\\b", "[x]", "*", "a?b", "w\\[x", "é*"] {
            let l = literal(name);
            assert!(accepts(&l, name), "{name:?} as {l:?}");
        }
        assert_eq!(literal("plain"), "plain");
        assert_eq!(literal("a\\b"), "a\\b");
        assert_eq!(literal("[x]"), "\\[x]");
        assert!(!accepts(&literal("[x]"), "x"));
        assert!(!accepts(&literal("*"), "a"));
    }

    #[test]
    fn expansion_walks_each_component() {
        let t = crate::testutil::TempDir::new();
        let d = t.path();
        for p in [
            "logs/a.gz",
            "logs/b.gz",
            "logs/c.txt",
            "logs/.h.gz",
            "old/z.gz",
        ] {
            let p = d.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"").unwrap();
        }
        let rel = |v: Vec<PathBuf>| -> Vec<String> {
            v.iter()
                .map(|p| p.strip_prefix(d).unwrap().display().to_string())
                .collect()
        };
        assert_eq!(
            rel(expand(&d.join("logs/*.gz"))),
            ["logs/a.gz", "logs/b.gz"]
        );
        assert_eq!(
            rel(expand(&d.join("*/*.gz"))),
            ["logs/a.gz", "logs/b.gz", "old/z.gz"]
        );
        assert_eq!(rel(expand(&d.join("*/z.gz"))), ["old/z.gz"]);
        assert_eq!(rel(expand(&d.join("l?gs"))), ["logs"]);
        assert!(expand(&d.join("logs/*.zip")).is_empty());
        assert!(expand(&d.join("none/*")).is_empty());
    }
}
