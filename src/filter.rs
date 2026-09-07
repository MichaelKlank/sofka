//! Structured row-filter grammar.
//!
//! Plain text with no structured markers stays what it always was: one fuzzy
//! pattern over "namespace name", falling back to each rendered column cell
//! (so `/10.96` finds a Service by its CLUSTER-IP). Once any structured marker
//! appears, the input is split into terms on whitespace (a `"quoted"` term
//! keeps its spaces) and every term must match (terms are AND-ed; there is no
//! OR/grouping — deliberately small):
//!
//! - `text`                   fuzzy match (namespace + name + any column cell)
//! - `"text"`                 literal match: contiguous, case-insensitive
//! - `/re/`                   regular expression (case-insensitive)
//! - `!text`                  inverse match (`!"text"` and `!/re/` too)
//! - `-l app=api,env=prod`    Kubernetes label selector (sent server-side)
//! - `-f spec.nodeName=n1`    Kubernetes field selector (sent server-side)
//! - `status=CrashLoopBackOff` column equality (case-insensitive)
//! - `cpu>500m` `memory>1Gi` `restarts>=5` `age<2h` typed comparisons
//!
//! Fuzzy is deliberately loose — `khc` finds `kube-httpcache-0` — which in a
//! namespace with hundreds of pods makes a short needle like `auth` match
//! every name with a scattered `a`…`u`…`t`…`h` in it. Quoting the term
//! (`"auth"`) drops the gaps and matches only what a `grep` would; `/re/`
//! covers the rest.
//!
//! Comparison operators: `=` (or `==`), `!=`, `>`, `>=`, `<`, `<=`. The
//! value's type follows the key: `cpu` parses CPU quantities (millicores),
//! `mem`/`memory` memory quantities (bytes), `age` durations (`90s`, `2h`,
//! `1d2h`); any other key compares numerically when the value is a number
//! and as case-insensitive text otherwise. Parsing never fails hard — a
//! broken term is skipped and reported via [`Structured::error`] so the
//! table doesn't blank out mid-keystroke.

/// The parsed form of the filter input.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedFilter {
    /// The whole input is one pattern (no structured markers) — the original
    /// `/text` behavior, kept byte-for-byte compatible. Always
    /// [`Pattern::Fuzzy`]: a `"literal"` or `/re/` is a marker, so it parses
    /// as a one-term [`Structured`] filter instead.
    Fuzzy(Pattern),
    Structured(Structured),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Structured {
    /// Locally-evaluated terms, AND-ed together.
    pub terms: Vec<Term>,
    /// Combined `-l` selectors, ready for the Kubernetes API.
    pub labels: Option<String>,
    /// Combined `-f` selectors, ready for the Kubernetes API.
    pub fields: Option<String>,
    /// First malformed term, for surfacing in the UI.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    /// One text pattern, inverted when the term was written `!pat`.
    Text {
        negate: bool,
        pat: Pattern,
    },
    Cmp(Cmp),
}

/// How a text term matches a row.
#[derive(Clone)]
pub enum Pattern {
    /// Plain text: a fuzzy subsequence match, gaps allowed (`khc` finds
    /// `kube-httpcache-0`).
    Fuzzy(String),
    /// `"text"`: a contiguous case-insensitive substring — what `grep` would
    /// find, for when fuzzy is too loose to name one thing.
    Literal(Literal),
    /// `/re/`: a case-insensitive regular expression.
    Regex(Box<regex::Regex>),
}

impl Pattern {
    /// The text the user typed inside the markers — the fuzzy needle, the
    /// quoted text, or the regex source.
    pub fn text(&self) -> &str {
        match self {
            Pattern::Fuzzy(pat) => pat,
            Pattern::Literal(lit) => lit.text(),
            Pattern::Regex(re) => re.as_str(),
        }
    }
}

/// Compared by source text: two patterns built from the same term are equal,
/// and a compiled automaton has no meaningful equality of its own.
impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Pattern::Fuzzy(a), Pattern::Fuzzy(b)) => a == b,
            (Pattern::Literal(a), Pattern::Literal(b)) => a.text() == b.text(),
            (Pattern::Regex(a), Pattern::Regex(b)) => a.as_str() == b.as_str(),
            _ => false,
        }
    }
}

/// The text, not the automaton behind it — a `{:?}` of a filter should read
/// like the filter.
impl std::fmt::Debug for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Pattern::Fuzzy(pat) => write!(f, "Fuzzy({pat:?})"),
            Pattern::Literal(lit) => write!(f, "Literal({:?})", lit.text()),
            Pattern::Regex(re) => write!(f, "Regex({:?})", re.as_str()),
        }
    }
}

/// A compiled `"text"` term: the text as typed, for highlighting, alongside
/// the case-insensitive substring automaton that tests it. Built once per
/// filter change — a filter pass runs it against every row in the store.
#[derive(Clone)]
pub struct Literal {
    text: String,
    substring: crate::logfilter::Substring,
}

impl Literal {
    /// `text` must not be empty; the parser rejects `""` before this.
    fn new(text: &str) -> Self {
        Literal {
            text: text.to_string(),
            substring: crate::logfilter::Substring::new(text),
        }
    }

    /// The text as typed, without the quotes.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn matches(&self, haystack: &str) -> bool {
        self.substring.matches(haystack)
    }

    /// Char positions of the first occurrence in `haystack`, for highlighting
    /// it in the NAME cell. `None` when the text does not occur there — a row
    /// can match on a column cell instead, and then its name highlights
    /// nothing.
    ///
    /// Folds one character at a time rather than through `str::to_lowercase`:
    /// the mappings that change length (`İ` → `i̇`) are exactly the ones that
    /// would put the reported position on the wrong character. This is a
    /// highlight, not the match decision — [`matches`](Self::matches) already
    /// made that with full folding.
    pub fn match_span(&self, haystack: &str) -> Option<std::ops::Range<usize>> {
        let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
        let hay: Vec<char> = haystack.chars().map(fold).collect();
        let needle: Vec<char> = self.text.chars().map(fold).collect();
        if needle.is_empty() || needle.len() > hay.len() {
            return None;
        }
        (0..=hay.len() - needle.len())
            .find(|&i| hay[i..i + needle.len()] == needle[..])
            .map(|i| i..i + needle.len())
    }
}

/// One `key<op>value` column comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct Cmp {
    /// Lowercased column key (`status`, `cpu`, `restarts`, …).
    pub key: String,
    pub op: Op,
    pub value: CmpValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Op {
    /// Apply the operator to an already-computed `actual.cmp(&wanted)`.
    pub fn eval(self, ord: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            Op::Eq => ord == Equal,
            Op::Ne => ord != Equal,
            Op::Gt => ord == Greater,
            Op::Ge => ord != Less,
            Op::Lt => ord == Less,
            Op::Le => ord != Greater,
        }
    }
}

/// A comparison value, typed at parse time from the key it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub enum CmpValue {
    /// Plain number (`restarts>=5`).
    Num(f64),
    /// CPU quantity in millicores (`cpu>500m`).
    Cpu(i64),
    /// Memory quantity in bytes (`memory>1Gi`).
    Mem(i64),
    /// Duration in seconds (`age<2h`).
    Duration(i64),
    /// Anything else: case-insensitive text comparison. Stored pre-folded to
    /// lowercase — the comparison runs per object per rebuild, so folding the
    /// needle once at parse time keeps it out of that loop.
    Str(String),
}

impl ParsedFilter {
    pub fn uses_metrics(&self) -> bool {
        matches!(self, Self::Structured(s) if s.terms.iter().any(|term| {
            matches!(term, Term::Cmp(Cmp { value: CmpValue::Cpu(_) | CmpValue::Mem(_), .. }))
        }))
    }

    pub fn labels(&self) -> Option<&str> {
        match self {
            ParsedFilter::Fuzzy(_) => None,
            ParsedFilter::Structured(s) => s.labels.as_deref(),
        }
    }

    pub fn fields(&self) -> Option<&str> {
        match self {
            ParsedFilter::Fuzzy(_) => None,
            ParsedFilter::Structured(s) => s.fields.as_deref(),
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            ParsedFilter::Fuzzy(_) => None,
            ParsedFilter::Structured(s) => s.error.as_deref(),
        }
    }

    /// The pattern NAME-cell highlighting should mark: the legacy fuzzy
    /// pattern, or the first positive text term of a structured filter.
    pub fn highlight_pattern(&self) -> Option<&Pattern> {
        match self {
            ParsedFilter::Fuzzy(pat) => (!pat.text().is_empty()).then_some(pat),
            ParsedFilter::Structured(s) => s.terms.iter().find_map(|t| match t {
                Term::Text { negate: false, pat } => Some(pat),
                _ => None,
            }),
        }
    }
}

pub fn parse(input: &str) -> ParsedFilter {
    let trimmed = input.trim();
    if trimmed.is_empty() || !is_structured(trimmed) {
        return ParsedFilter::Fuzzy(Pattern::Fuzzy(trimmed.to_string()));
    }

    let mut terms = Vec::new();
    let mut labels: Vec<&str> = Vec::new();
    let mut fields: Vec<&str> = Vec::new();
    let mut error: Option<String> = None;
    let fail = |slot: &mut Option<String>, msg: String| {
        if slot.is_none() {
            *slot = Some(msg);
        }
    };

    let tokens = tokenize(trimmed);
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        i += 1;
        // `-l <sel>` / `-f <sel>`, or attached (`-lapp=api`).
        if tok == "-l" || tok == "-f" {
            match tokens.get(i) {
                Some(sel) => {
                    if tok == "-l" {
                        &mut labels
                    } else {
                        &mut fields
                    }
                    .push(sel);
                    i += 1;
                }
                None => fail(&mut error, format!("expected selector after {tok}")),
            }
            continue;
        }
        if let Some(sel) = attached_selector(tok, "-l") {
            labels.push(sel);
            continue;
        }
        if let Some(sel) = attached_selector(tok, "-f") {
            fields.push(sel);
            continue;
        }
        if let Some((key, op, value)) = split_cmp(tok) {
            if value.is_empty() {
                fail(&mut error, format!("missing value in '{tok}'"));
                continue;
            }
            match typed_value(key, value) {
                Ok(v) => terms.push(Term::Cmp(Cmp {
                    key: key.to_ascii_lowercase(),
                    op,
                    value: v,
                })),
                Err(e) => fail(&mut error, e),
            }
            continue;
        }
        // Text: `!` inverts, then the term's own markers pick the matcher.
        let (negate, rest) = match tok.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, tok),
        };
        if rest.is_empty() {
            fail(&mut error, "expected text after '!'".into());
            continue;
        }
        match pattern(rest) {
            Ok(pat) => terms.push(Term::Text { negate, pat }),
            Err(e) => fail(&mut error, e),
        }
    }

    ParsedFilter::Structured(Structured {
        terms,
        labels: (!labels.is_empty()).then(|| labels.join(",")),
        fields: (!fields.is_empty()).then(|| fields.join(",")),
        error,
    })
}

/// Whether any token flips the input from a single legacy fuzzy pattern into
/// the structured grammar. Mirrors the markers `parse` acts on.
fn is_structured(input: &str) -> bool {
    tokenize(input).into_iter().any(|tok| {
        let text = tok.strip_prefix('!').unwrap_or(tok);
        tok == "-l"
            || tok == "-f"
            || attached_selector(tok, "-l").is_some()
            || attached_selector(tok, "-f").is_some()
            || tok.starts_with('!')
            || text.starts_with('"')
            || is_regex(text)
            || split_cmp(tok).is_some()
    })
}

/// Split the input into terms on whitespace, keeping a double-quoted run
/// together so `"my pod"` is one term.
///
/// An unterminated quote runs to the end of the input rather than being
/// dropped: the filter is re-parsed on every keystroke, and a term must keep
/// narrowing the table while it is still being typed.
fn tokenize(input: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    let mut quoted = false;
    for (i, c) in input.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c.is_whitespace() && !quoted {
            if let Some(s) = start.take() {
                tokens.push(&input[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        tokens.push(&input[s..]);
    }
    tokens
}

/// Classify one text term: `"quoted"` is a literal, `/re/` a regex, anything
/// else the fuzzy text the filter has always taken.
fn pattern(tok: &str) -> Result<Pattern, String> {
    if let Some(rest) = tok.strip_prefix('"') {
        // The closing quote is optional — the term may still be being typed.
        let text = rest.strip_suffix('"').unwrap_or(rest);
        if text.is_empty() {
            return Err("expected text between the quotes".into());
        }
        return Ok(Pattern::Literal(Literal::new(text)));
    }
    if is_regex(tok) {
        let source = &tok[1..tok.len() - 1];
        if source.is_empty() {
            return Err("expected a pattern between the slashes".into());
        }
        return regex::RegexBuilder::new(source)
            .case_insensitive(true)
            .build()
            .map(|re| Pattern::Regex(Box::new(re)))
            .map_err(|_| format!("bad regex '{source}'"));
    }
    Ok(Pattern::Fuzzy(tok.to_string()))
}

/// A `/re/` term. Both slashes are required, so a lone `/` and text like
/// `/healthz` stay fuzzy — the same rule the log filter uses.
fn is_regex(tok: &str) -> bool {
    tok.len() >= 2 && tok.starts_with('/') && tok.ends_with('/')
}

/// The selector of an attached `-l`/`-f` form (`-lapp=api`). Requires an `=`
/// so ordinary fuzzy text starting with those letters isn't swallowed.
fn attached_selector<'a>(tok: &'a str, flag: &str) -> Option<&'a str> {
    tok.strip_prefix(flag).filter(|rest| rest.contains('='))
}

/// Split `key<op>value` at the operator following a valid key. `None` when
/// the token has no operator or no leading key — i.e. plain fuzzy text.
fn split_cmp(tok: &str) -> Option<(&str, Op, &str)> {
    if !tok
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let key_end = tok.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))?;
    let (key, rest) = tok.split_at(key_end);
    let (op, value) = if let Some(v) = rest.strip_prefix("!=") {
        (Op::Ne, v)
    } else if let Some(v) = rest.strip_prefix(">=") {
        (Op::Ge, v)
    } else if let Some(v) = rest.strip_prefix("<=") {
        (Op::Le, v)
    } else if let Some(v) = rest.strip_prefix("==") {
        (Op::Eq, v)
    } else if let Some(v) = rest.strip_prefix('=') {
        (Op::Eq, v)
    } else if let Some(v) = rest.strip_prefix('>') {
        (Op::Gt, v)
    } else {
        (Op::Lt, rest.strip_prefix('<')?)
    };
    Some((key, op, value))
}

/// Fold a comparison needle once at parse time.
///
/// Whole-string lowercasing is required for non-ASCII text because it applies
/// context-sensitive mappings such as Greek final sigma. ASCII uses its
/// cheaper equivalent; the resulting `String` is retained in `CmpValue`.
fn fold_lower(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.to_lowercase()
    }
}

/// Compare a cell with a needle already returned by [`fold_lower`].
///
/// The common ASCII path performs no per-cell allocation. If either operand is
/// non-ASCII, use whole-string lowercasing to preserve the case-insensitive
/// behavior that structured filters had before the ASCII optimization.
pub fn cmp_folded_lower(cell: &str, want: &str) -> std::cmp::Ordering {
    if cell.is_ascii() && want.is_ascii() {
        cell.bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .cmp(want.bytes())
    } else {
        cell.to_lowercase().as_str().cmp(want)
    }
}

/// Type a comparison value from its key: quantities for `cpu`/`mem`/`memory`,
/// durations for `age`, and number-or-text for everything else.
fn typed_value(key: &str, raw: &str) -> Result<CmpValue, String> {
    match key.to_ascii_lowercase().as_str() {
        "cpu" => parse_cpu(raw)
            .map(CmpValue::Cpu)
            .ok_or_else(|| format!("bad cpu quantity '{raw}'")),
        "mem" | "memory" => parse_mem(raw)
            .map(CmpValue::Mem)
            .ok_or_else(|| format!("bad memory quantity '{raw}'")),
        "age" => parse_duration(raw)
            .map(CmpValue::Duration)
            .ok_or_else(|| format!("bad duration '{raw}'")),
        _ => Ok(raw
            .parse::<f64>()
            .map(CmpValue::Num)
            .unwrap_or_else(|_| CmpValue::Str(fold_lower(raw)))),
    }
}

/// CPU quantity → millicores: `250m` → 250, `1` → 1000, `500000000n` → 500.
/// Unlike [`crate::columns::parse_cpu_milli`] this rejects garbage instead of
/// defaulting to 0, so a typo can be reported.
fn parse_cpu(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, scale) = match s.chars().last()? {
        'n' => (&s[..s.len() - 1], 1.0 / 1_000_000.0),
        'u' => (&s[..s.len() - 1], 1.0 / 1_000.0),
        'm' => (&s[..s.len() - 1], 1.0),
        _ => (s, 1000.0),
    };
    let v: f64 = num.parse().ok()?;
    (v >= 0.0).then(|| (v * scale).round() as i64)
}

/// Memory quantity → bytes: `1Gi`, `512Mi`, `2000000`. Validating twin of
/// [`crate::columns::parse_mem_bytes`].
fn parse_mem(s: &str) -> Option<i64> {
    crate::columns::parse_memory_quantity(s)
}

/// Duration → seconds: `90s`, `2h`, `1d2h`, `1h30m`, bare `300` (seconds).
/// Units: s, m, h, d, w.
fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = s.parse::<i64>() {
        return (v >= 0).then_some(v);
    }
    let mut total = 0i64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return None,
        };
        if num.is_empty() {
            return None;
        }
        total += num.parse::<i64>().ok()? * unit;
        num.clear();
    }
    // Trailing digits without a unit (`2h30`) are malformed.
    num.is_empty().then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn structured(input: &str) -> Structured {
        match parse(input) {
            ParsedFilter::Structured(s) => s,
            other => panic!("expected structured parse for '{input}', got {other:?}"),
        }
    }

    fn fuzzy(pat: &str) -> Term {
        Term::Text {
            negate: false,
            pat: Pattern::Fuzzy(pat.into()),
        }
    }

    fn not_fuzzy(pat: &str) -> Term {
        Term::Text {
            negate: true,
            pat: Pattern::Fuzzy(pat.into()),
        }
    }

    /// The single text pattern of a one-term filter.
    fn only_pattern(input: &str) -> Pattern {
        let s = structured(input);
        assert_eq!(s.terms.len(), 1, "expected one term in '{input}'");
        match s.terms.into_iter().next() {
            Some(Term::Text { negate: false, pat }) => pat,
            other => panic!("expected a positive text term in '{input}', got {other:?}"),
        }
    }

    #[test]
    fn plain_text_stays_one_legacy_fuzzy_pattern() {
        assert_eq!(
            parse(""),
            ParsedFilter::Fuzzy(Pattern::Fuzzy(String::new()))
        );
        assert_eq!(
            parse("api"),
            ParsedFilter::Fuzzy(Pattern::Fuzzy("api".into()))
        );
        // Spaces included: the whole string is the pattern, as before.
        assert_eq!(
            parse("kube system dns"),
            ParsedFilter::Fuzzy(Pattern::Fuzzy("kube system dns".into()))
        );
        // Leading/trailing whitespace is not part of the pattern.
        assert_eq!(
            parse("  api "),
            ParsedFilter::Fuzzy(Pattern::Fuzzy("api".into()))
        );
        // A lone dash or dashed name is still fuzzy text, not a flag.
        assert_eq!(
            parse("-longname"),
            ParsedFilter::Fuzzy(Pattern::Fuzzy("-longname".into()))
        );
    }

    #[test]
    fn inverse_term() {
        let s = structured("!canary");
        assert_eq!(s.terms, vec![not_fuzzy("canary")]);
        assert_eq!(s.error, None);
    }

    /// The fix for noisy fuzzy hits: a quoted term matches a contiguous run,
    /// so it no longer drags in every name with the characters scattered
    /// through it.
    #[test]
    fn quoted_terms_match_contiguously() {
        let Pattern::Literal(lit) = only_pattern("\"auth\"") else {
            panic!("expected a literal term");
        };
        assert!(lit.matches("default auth-api-0"));
        assert!(lit.matches("AUTH-API"), "literals fold case");
        // Fuzzy's subsequence match, which is what the quotes rule out.
        assert!(!lit.matches("api-gateway-runtime-hash"));
    }

    /// A quoted term keeps its spaces: the tokenizer splits terms on
    /// whitespace, but not inside quotes.
    #[test]
    fn quoted_terms_keep_their_spaces() {
        let Pattern::Literal(lit) = only_pattern("\"kube system\"") else {
            panic!("expected a literal term");
        };
        assert_eq!(lit.text(), "kube system");
        // Still one term when other terms surround it.
        let s = structured("\"kube system\" status=Running");
        assert_eq!(s.terms.len(), 2);
    }

    /// The filter is reparsed on every keystroke, so a term that is still
    /// being typed has to keep working before its closing quote arrives.
    #[test]
    fn an_unterminated_quote_still_narrows() {
        let Pattern::Literal(lit) = only_pattern("\"auth") else {
            panic!("expected a literal term");
        };
        assert_eq!(lit.text(), "auth");
    }

    #[test]
    fn regex_terms_compile_case_insensitively() {
        let Pattern::Regex(re) = only_pattern("/auth-\\d+/") else {
            panic!("expected a regex term");
        };
        assert!(re.is_match("auth-12"));
        assert!(re.is_match("AUTH-12"));
        assert!(!re.is_match("auth-api"));
    }

    /// Both slashes are required, so the paths and image tags people filter
    /// on are still plain fuzzy text.
    #[test]
    fn a_single_slash_is_not_a_regex() {
        assert_eq!(
            parse("/healthz"),
            ParsedFilter::Fuzzy(Pattern::Fuzzy("/healthz".into()))
        );
        assert_eq!(parse("/"), ParsedFilter::Fuzzy(Pattern::Fuzzy("/".into())));
        assert_eq!(
            parse("nginx/nginx:1.2"),
            ParsedFilter::Fuzzy(Pattern::Fuzzy("nginx/nginx:1.2".into()))
        );
    }

    #[test]
    fn quoted_and_regex_terms_invert() {
        let s = structured("!\"canary\"");
        assert_eq!(
            s.terms,
            vec![Term::Text {
                negate: true,
                pat: Pattern::Literal(Literal::new("canary")),
            }]
        );
        assert_eq!(s.error, None);

        let s = structured("!/canary|debug/");
        let Term::Text { negate, pat } = &s.terms[0] else {
            panic!("expected a text term");
        };
        assert!(negate);
        assert!(matches!(pat, Pattern::Regex(_)));
    }

    /// Same contract as the rest of the grammar: a term that cannot be built
    /// is skipped and reported, never a blank table.
    #[test]
    fn malformed_quoted_and_regex_terms_report_without_blanking() {
        let s = structured("\"\" api");
        assert_eq!(s.terms, vec![fuzzy("api")]);
        assert!(s.error.as_deref().is_some_and(|e| e.contains("quotes")));

        let s = structured("// api");
        assert_eq!(s.terms, vec![fuzzy("api")]);
        assert!(s.error.as_deref().is_some_and(|e| e.contains("slashes")));

        let s = structured("/[unclosed/ api");
        assert_eq!(s.terms, vec![fuzzy("api")]);
        assert!(s.error.as_deref().is_some_and(|e| e.contains("bad regex")));
    }

    #[test]
    fn tokenizer_splits_on_whitespace_outside_quotes() {
        assert_eq!(
            tokenize("api !canary -l app=api"),
            ["api", "!canary", "-l", "app=api"]
        );
        assert_eq!(
            tokenize("  \"kube system\"  api "),
            ["\"kube system\"", "api"]
        );
        assert_eq!(tokenize("!\"a b\""), ["!\"a b\""]);
        assert_eq!(tokenize("\"unterminated api"), ["\"unterminated api"]);
        assert!(tokenize("   ").is_empty());
    }

    /// Where case folding stops: the substring matcher folds the *needle*
    /// with full Unicode rules and then reads the haystack's raw bytes. So a
    /// needle that folds onto ASCII finds ASCII text, but an ASCII needle
    /// does not find a haystack character that merely folds to it. Folding
    /// every cell of every row to close that gap is the cost the log filter
    /// and the document search — the same matcher — already decided against,
    /// and a Kubernetes name cannot hold such a character anyway.
    #[test]
    fn literals_fold_the_needle_not_the_haystack() {
        // U+212A KELVIN SIGN folds to a plain `k`.
        assert!(Literal::new("\u{212a}ube").matches("kube-httpcache-0"));
        assert!(!Literal::new("kube").matches("\u{212a}ube-httpcache-0"));
    }

    /// Highlight positions are char indices into the name, so a multibyte
    /// name marks the characters the user sees.
    #[test]
    fn literal_match_spans_are_char_indices() {
        let lit = Literal::new("world");
        assert_eq!(lit.match_span("héllo-world"), Some(6..11));
        let chars: Vec<char> = "héllo-world".chars().collect();
        assert_eq!(chars[6], 'w');
        // Case-insensitive, and absent text has no span to highlight.
        assert_eq!(Literal::new("AUTH").match_span("auth-api"), Some(0..4));
        assert_eq!(lit.match_span("héllo"), None);
    }

    #[test]
    fn label_selector_variants() {
        let s = structured("-l app=api,env=prod");
        assert_eq!(s.labels.as_deref(), Some("app=api,env=prod"));
        assert!(s.terms.is_empty());
        assert_eq!(s.error, None);

        // Attached form and repeated flags joining with a comma.
        let s = structured("-lapp=api -l env=prod");
        assert_eq!(s.labels.as_deref(), Some("app=api,env=prod"));

        // Bare-key (existence) selectors work in the spaced form.
        let s = structured("-l app");
        assert_eq!(s.labels.as_deref(), Some("app"));
    }

    #[test]
    fn field_selector() {
        let s = structured("-f spec.nodeName=node-3");
        assert_eq!(s.fields.as_deref(), Some("spec.nodeName=node-3"));
        assert_eq!(s.labels, None);
        assert!(s.terms.is_empty());
    }

    /// `CmpValue::Str` is stored pre-folded: the comparison is
    /// case-insensitive, so the needle is lowercased once here rather than
    /// once per object per rebuild.
    #[test]
    fn status_equality_and_inequality() {
        let s = structured("status=CrashLoopBackOff");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "status".into(),
                op: Op::Eq,
                value: CmpValue::Str("crashloopbackoff".into()),
            })]
        );

        let s = structured("status!=Running");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "status".into(),
                op: Op::Ne,
                value: CmpValue::Str("running".into()),
            })]
        );
    }

    #[test]
    fn typed_quantity_comparisons() {
        let s = structured("cpu>500m");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "cpu".into(),
                op: Op::Gt,
                value: CmpValue::Cpu(500),
            })]
        );

        let s = structured("cpu>=1");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "cpu".into(),
                op: Op::Ge,
                value: CmpValue::Cpu(1000),
            })]
        );

        let s = structured("memory>1Gi");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "memory".into(),
                op: Op::Gt,
                value: CmpValue::Mem(1024 * 1024 * 1024),
            })]
        );

        let s = structured("mem<=512Mi");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "mem".into(),
                op: Op::Le,
                value: CmpValue::Mem(512 * 1024 * 1024),
            })]
        );

        let s = structured("restarts>=5");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "restarts".into(),
                op: Op::Ge,
                value: CmpValue::Num(5.0),
            })]
        );
    }

    #[test]
    fn age_durations() {
        let s = structured("age<2h");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "age".into(),
                op: Op::Lt,
                value: CmpValue::Duration(7_200),
            })]
        );

        let s = structured("age>1d2h");
        assert_eq!(
            s.terms,
            vec![Term::Cmp(Cmp {
                key: "age".into(),
                op: Op::Gt,
                value: CmpValue::Duration(93_600),
            })]
        );
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("2h"), Some(7_200));
        assert_eq!(parse_duration("1h30m"), Some(5_400));
        assert_eq!(parse_duration("1w"), Some(604_800));
        assert_eq!(parse_duration("300"), Some(300));
        assert_eq!(parse_duration("2h30"), None); // trailing digits, no unit
        assert_eq!(parse_duration("xyz"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn quantity_parsing() {
        assert_eq!(parse_cpu("250m"), Some(250));
        assert_eq!(parse_cpu("1"), Some(1_000));
        assert_eq!(parse_cpu("1.5"), Some(1_500));
        assert_eq!(parse_cpu("500000000n"), Some(500));
        assert_eq!(parse_cpu("abc"), None);
        assert_eq!(parse_mem("1Ki"), Some(1_024));
        assert_eq!(parse_mem("512Mi"), Some(512 * 1024 * 1024));
        assert_eq!(parse_mem("2000000"), Some(2_000_000));
        assert_eq!(parse_mem("1Xi"), None);
    }

    #[test]
    fn terms_combine_with_and_semantics() {
        let s = structured("api !canary -l app=api status=Running");
        assert_eq!(s.labels.as_deref(), Some("app=api"));
        assert_eq!(s.error, None);
        assert_eq!(
            s.terms,
            vec![
                fuzzy("api"),
                not_fuzzy("canary"),
                Term::Cmp(Cmp {
                    key: "status".into(),
                    op: Op::Eq,
                    value: CmpValue::Str("running".into()),
                }),
            ]
        );
    }

    #[test]
    fn malformed_terms_report_without_blanking() {
        // Mid-typing states must degrade to "term skipped + error", never a
        // hard failure.
        let s = structured("-l");
        assert_eq!(s.labels, None);
        assert!(s.error.as_deref().is_some_and(|e| e.contains("-l")));

        let s = structured("cpu>");
        assert!(s.terms.is_empty());
        assert!(s.error.as_deref().is_some_and(|e| e.contains("cpu>")));

        let s = structured("cpu>abc");
        assert!(s.terms.is_empty());
        assert!(s.error.as_deref().is_some_and(|e| e.contains("abc")));

        let s = structured("age<soon");
        assert!(s.error.as_deref().is_some_and(|e| e.contains("soon")));

        let s = structured("! api");
        assert_eq!(s.terms, vec![fuzzy("api")]);
        assert!(s.error.is_some());
    }

    #[test]
    fn highlight_pattern_prefers_first_positive_term() {
        let needle = |input: &str| {
            parse(input)
                .highlight_pattern()
                .map(|p| p.text().to_string())
        };
        assert_eq!(needle("khc").as_deref(), Some("khc"));
        assert_eq!(needle(""), None);
        assert_eq!(needle("!x khc status=Running").as_deref(), Some("khc"));
        assert_eq!(needle("-l app=api"), None);
        // A quoted or regex term highlights too — it is a positive text term.
        assert_eq!(needle("\"auth\"").as_deref(), Some("auth"));
        assert_eq!(needle("/auth-\\d/").as_deref(), Some("auth-\\d"));
        // Negated terms are not what the row matched on, so they never mark it.
        assert_eq!(needle("!\"canary\""), None);
    }

    #[test]
    fn unicode_comparisons_fold_mixed_case_in_both_directions() {
        for (cell, raw_needle) in [("ΟΔΟΣ", "οδος"), ("οδος", "ΟΔΟΣ")] {
            let CmpValue::Str(needle) = typed_value("name", raw_needle).expect("typed") else {
                panic!("expected a text comparison");
            };
            assert_eq!(
                cmp_folded_lower(cell, &needle),
                std::cmp::Ordering::Equal,
                "cell={cell:?}, needle={raw_needle:?}"
            );
        }
    }

    #[test]
    fn ascii_comparisons_remain_case_insensitive() {
        let CmpValue::Str(needle) = typed_value("status", "rUnNiNg").expect("typed") else {
            panic!("expected a text comparison");
        };
        assert_eq!(
            cmp_folded_lower("RUNNING", &needle),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn server_side_selectors_only_from_l_and_f() {
        for local in ["api", "status=Running", "!x cpu>1"] {
            let p = parse(local);
            assert_eq!(p.labels(), None, "{local}");
            assert_eq!(p.fields(), None, "{local}");
        }
        assert_eq!(parse("-l app=api").labels(), Some("app=api"));
        assert_eq!(
            parse("-f spec.nodeName=n1").fields(),
            Some("spec.nodeName=n1")
        );
    }
}
