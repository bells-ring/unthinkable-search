//! Query language:
//!
//! * `foo bar`      — AND (whitespace)
//! * `foo|bar`      — OR inside one term (binds tighter than AND)
//! * `!foo`         — NOT
//! * `"a b"`        — literal with spaces
//! * `*.pdf`, `a?c` — wildcards: match the WHOLE name
//! * `win\sys`      — a term with `\` (or `/`) matches the full path
//! * `ext:pdf;docx` — extension list
//! * `file:` / `folder:` — type filters (may prefix a term: `file:foo`)
//!
//! Matching is case- and diacritics-insensitive (see `fold`).

use crate::fold::fold;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GTok {
    Lit(Vec<u8>),
    Star,
    One,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pat {
    Substr(Vec<u8>),
    /// Whole-string wildcard match.
    Glob(Vec<GTok>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Atom {
    Name(Pat),
    Path(Pat),
    /// Folded extensions without the dot.
    Ext(Vec<Vec<u8>>),
    IsFile,
    IsDir,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub alts: Vec<Atom>,
    pub neg: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
}

/// A literal the blob scan can be driven by. `lead` = 1 when the key starts
/// with the NUL separator (prefix / exact match).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverKey {
    pub bytes: Vec<u8>,
    pub lead: usize,
}

/// The term driving the blob scan and its keys (union).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Driver {
    pub term: usize,
    pub keys: Vec<DriverKey>,
    /// A hit on the single key alone proves the term (plain substring) — no re-check.
    pub exact: bool,
}

impl Query {
    pub fn parse(q: &str) -> Query {
        let mut terms = Vec::new();
        for tok in tokenize(q) {
            if let Some(t) = parse_term(tok, &mut terms) {
                terms.push(t);
            }
        }
        Query { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Pick the best term to drive the blob scan. A term qualifies when it is
    /// positive and EVERY alternative yields a literal key (`a|b`, `ext:jpg;png`
    /// → union of several SIMD scans). Score = shortest key of the term
    /// (longer keys are more selective and let memmem skip faster); single-key
    /// terms win ties.
    pub fn driver(&self) -> Option<Driver> {
        let mut best: Option<(usize, Driver)> = None;
        for (ti, t) in self.terms.iter().enumerate() {
            if t.neg {
                continue;
            }
            let mut keys = Vec::new();
            // Exact: a key hit alone proves the atom, so the term needs no re-check.
            let mut exact = true;
            for a in &t.alts {
                exact &= atom_key_is_exact(a);
                match a {
                    Atom::Name(Pat::Substr(s)) if !s.is_empty() => keys.push(DriverKey { bytes: s.clone(), lead: 0 }),
                    Atom::Name(Pat::Glob(g)) => match glob_key(g) {
                        Some(k) => keys.push(k),
                        None => {
                            keys.clear();
                            break;
                        }
                    },
                    Atom::Ext(list) if !list.is_empty() => {
                        for e in list {
                            let mut b = Vec::with_capacity(e.len() + 2);
                            b.push(b'.');
                            b.extend_from_slice(e);
                            b.push(0);
                            keys.push(DriverKey { bytes: b, lead: 0 });
                        }
                    }
                    _ => {
                        keys.clear();
                        break;
                    }
                }
            }
            if keys.is_empty() {
                continue;
            }
            let min = keys.iter().map(|k| k.bytes.len()).min().unwrap();
            let score = min * 2 + usize::from(keys.len() == 1);
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, Driver { term: ti, keys, exact }));
            }
        }
        best.map(|b| b.1)
    }

    pub fn needs_path(&self) -> bool {
        self.terms
            .iter()
            .any(|t| t.alts.iter().any(|a| matches!(a, Atom::Path(_))))
    }
}

/// Conservative check: is every result of `new` guaranteed to be a result of `old`?
/// (Lets the caller search only inside the previous result set.)
pub fn is_refinement(old: &str, new: &str) -> bool {
    if old.trim().is_empty() || !new.starts_with(old) {
        return false;
    }
    let tail = &new[old.len()..];
    if old.contains(['|', '"']) || tail.contains(['|', '"']) {
        return false;
    }
    // The last token of `old` gets extended by `tail` unless `old` ends in whitespace.
    if !old.ends_with(char::is_whitespace) && !tail.starts_with(char::is_whitespace) {
        let last = old.split_whitespace().last().unwrap_or("");
        // Extending a wildcard / modifier / negated token does not narrow it.
        if last.contains(['*', '?', ':', '!']) {
            return false;
        }
    }
    true
}

fn tokenize(q: &str) -> Vec<(bool, Vec<String>)> {
    let mut out = Vec::new();
    let mut alts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut neg = false;
    let mut in_q = false;
    let mut at_start = true;
    let flush_alt = |alts: &mut Vec<String>, cur: &mut String| {
        if !cur.is_empty() {
            alts.push(std::mem::take(cur));
        }
    };
    for ch in q.chars() {
        match ch {
            '"' => {
                in_q = !in_q;
                at_start = false;
            }
            c if c.is_whitespace() && !in_q => {
                flush_alt(&mut alts, &mut cur);
                if !alts.is_empty() {
                    out.push((neg, std::mem::take(&mut alts)));
                }
                neg = false;
                at_start = true;
            }
            '|' if !in_q => {
                flush_alt(&mut alts, &mut cur);
                at_start = false;
            }
            '!' if !in_q && at_start => {
                neg = !neg;
            }
            c => {
                cur.push(c);
                at_start = false;
            }
        }
    }
    flush_alt(&mut alts, &mut cur);
    if !alts.is_empty() {
        out.push((neg, alts));
    }
    out
}

fn parse_term((neg, alts): (bool, Vec<String>), extra: &mut Vec<Term>) -> Option<Term> {
    let mut atoms = Vec::with_capacity(alts.len());
    for a in alts {
        let lower = a.to_ascii_lowercase();
        // file:/folder: as a prefix of a plain term → two ANDed terms.
        for (pfx, atom) in [("file:", Atom::IsFile), ("folder:", Atom::IsDir)] {
            if let Some(rest) = lower.strip_prefix(pfx) {
                if rest.is_empty() {
                    atoms.push(atom.clone());
                } else if !neg {
                    extra.push(Term { alts: vec![atom.clone()], neg: false });
                    atoms.push(parse_pattern(&a[pfx.len()..]));
                } else {
                    atoms.push(parse_pattern(&a[pfx.len()..]));
                }
            }
        }
        if lower.starts_with("file:") || lower.starts_with("folder:") {
            continue;
        }
        if let Some(rest) = lower.strip_prefix("ext:") {
            let list: Vec<Vec<u8>> = a[4..]
                .split(';')
                .filter(|s| !s.is_empty())
                .map(|s| fold(s.trim_start_matches('.')))
                .collect();
            let _ = rest;
            atoms.push(Atom::Ext(list));
            continue;
        }
        atoms.push(parse_pattern(&a));
    }
    if atoms.is_empty() {
        None
    } else {
        Some(Term { alts: atoms, neg })
    }
}

fn parse_pattern(s: &str) -> Atom {
    let is_path = s.contains(['\\', '/']);
    let s = if is_path { s.replace('/', "\\") } else { s.to_string() };
    let f = fold(&s);
    let pat = if f.contains(&b'*') || f.contains(&b'?') {
        let mut toks = Vec::new();
        let mut lit = Vec::new();
        for &c in &f {
            match c {
                b'*' | b'?' => {
                    if !lit.is_empty() {
                        toks.push(GTok::Lit(std::mem::take(&mut lit)));
                    }
                    let t = if c == b'*' { GTok::Star } else { GTok::One };
                    if !(t == GTok::Star && toks.last() == Some(&GTok::Star)) {
                        toks.push(t);
                    }
                }
                _ => lit.push(c),
            }
        }
        if !lit.is_empty() {
            toks.push(GTok::Lit(lit));
        }
        Pat::Glob(toks)
    } else {
        Pat::Substr(f)
    };
    if is_path { Atom::Path(pat) } else { Atom::Name(pat) }
}

/// Does a hit of this atom's driver key prove the atom by itself?
/// `foo`, `*foo`, `foo*`, `*foo*` and `ext:x` (no dot in x) do.
fn atom_key_is_exact(a: &Atom) -> bool {
    match a {
        Atom::Name(Pat::Substr(_)) => true,
        Atom::Name(Pat::Glob(g)) => matches!(
            g.as_slice(),
            [GTok::Star, GTok::Lit(_)] | [GTok::Lit(_), GTok::Star] | [GTok::Star, GTok::Lit(_), GTok::Star]
        ),
        Atom::Ext(list) => list.iter().all(|e| !e.contains(&b'.')),
        _ => false,
    }
}

fn glob_key(g: &[GTok]) -> Option<DriverKey> {
    let mut best: Option<DriverKey> = None;
    let last = g.len().saturating_sub(1);
    for (i, t) in g.iter().enumerate() {
        if let GTok::Lit(l) = t {
            let mut b = Vec::with_capacity(l.len() + 2);
            let lead = if i == 0 {
                b.push(0);
                1
            } else {
                0
            };
            b.extend_from_slice(l);
            if i == last {
                b.push(0);
            }
            if best.as_ref().is_none_or(|x| b.len() > x.bytes.len()) {
                best = Some(DriverKey { bytes: b, lead });
            }
        }
    }
    best
}

/// Whole-string wildcard match over folded bytes. `?` matches one UTF-8 char.
/// Classic greedy-with-backtrack algorithm: O(n·m) worst case, no exponential blowup.
pub fn glob_match(g: &[GTok], s: &[u8]) -> bool {
    let (mut gi, mut si) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if gi < g.len() {
            match &g[gi] {
                GTok::Star => {
                    star = Some((gi, si));
                    gi += 1;
                    continue;
                }
                GTok::One if si < s.len() => {
                    si += 1;
                    while si < s.len() && (s[si] & 0xC0) == 0x80 {
                        si += 1;
                    }
                    gi += 1;
                    continue;
                }
                GTok::Lit(l) if s[si..].starts_with(l) => {
                    si += l.len();
                    gi += 1;
                    continue;
                }
                _ => {}
            }
        } else if si == s.len() {
            return true;
        }
        // Mismatch: backtrack to last star, consuming one more char.
        match star {
            Some((sg, ss)) if ss < s.len() => {
                let mut ns = ss + 1;
                while ns < s.len() && (s[ns] & 0xC0) == 0x80 {
                    ns += 1;
                }
                star = Some((sg, ns));
                gi = sg + 1;
                si = ns;
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let q = Query::parse(r#"foo !bar "a b" x|y ext:pdf;Doc file: win\sys *.txt"#);
        assert_eq!(q.terms.len(), 8);
        assert!(q.terms[1].neg);
        assert_eq!(q.terms[2].alts[0], Atom::Name(Pat::Substr(b"a b".to_vec())));
        assert_eq!(q.terms[3].alts.len(), 2);
        assert_eq!(q.terms[4].alts[0], Atom::Ext(vec![b"pdf".to_vec(), b"doc".to_vec()]));
        assert_eq!(q.terms[5].alts[0], Atom::IsFile);
        assert!(matches!(q.terms[6].alts[0], Atom::Path(Pat::Substr(_))));
        assert!(matches!(q.terms[7].alts[0], Atom::Name(Pat::Glob(_))));
    }

    #[test]
    fn driver_choice() {
        let q = Query::parse("ab *.longext");
        let d = q.driver().unwrap();
        assert_eq!(d.term, 1);
        assert_eq!(d.keys[0].bytes, b".longext\0");
        assert!(d.exact); // "*.longext" is a pure suffix
        assert!(!Query::parse("a*b*c").driver().unwrap().exact);
        assert!(!Query::parse("?.txt").driver().unwrap().exact);
        assert!(!Query::parse("ext:tar.gz").driver().unwrap().exact);
        let q = Query::parse("report*");
        assert_eq!(q.driver().unwrap().keys, vec![DriverKey { bytes: b"\0report".to_vec(), lead: 1 }]);
        let q = Query::parse("!foo a|b");
        let d = q.driver().unwrap();
        assert_eq!(d.keys.len(), 2);
        let q = Query::parse("!foo a|*");
        assert!(q.driver().is_none());
        let q = Query::parse("ext:jpg;png abc");
        assert_eq!(q.driver().unwrap().keys.len(), 2); // ".jpg\0" (5) beats "abc" (3)
        assert!(Query::parse("abc").driver().unwrap().exact);
    }

    #[test]
    fn globs() {
        let g = |p: &str| match parse_pattern(p) {
            Atom::Name(Pat::Glob(g)) => g,
            _ => panic!(),
        };
        assert!(glob_match(&g("*.pdf"), b"a.pdf"));
        assert!(!glob_match(&g("*.pdf"), b"a.pdfx"));
        assert!(glob_match(&g("a*b*c"), b"axxbyyc"));
        assert!(!glob_match(&g("a*b*c"), b"axxbyy"));
        assert!(glob_match(&g("?.txt"), "я.txt".as_bytes()));
        assert!(!glob_match(&g("?.txt"), b"ab.txt"));
        assert!(glob_match(&g("*"), b""));
    }

    #[test]
    fn refinement() {
        assert!(is_refinement("foo", "foob"));
        assert!(is_refinement("foo", "foo bar"));
        assert!(is_refinement("foo ", "foo !bar"));
        assert!(!is_refinement("foo", "foo|bar"));
        assert!(!is_refinement("*.p", "*.pd"));
        assert!(!is_refinement("ext:p", "ext:pd"));
        assert!(!is_refinement("!fo", "!foo"));
        assert!(!is_refinement("foo", "fo"));
        assert!(!is_refinement("", "a"));
    }
}
