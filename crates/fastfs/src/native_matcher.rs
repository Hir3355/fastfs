//! FastFs' line-oriented pattern matcher.
//!
//! This module deliberately owns the small amount of policy that a search
//! command needs on top of a general regular-expression engine: smart case,
//! word-half boundaries, whole-line matching, binary-pattern validation and
//! a literal fast path. It does not depend on a grep implementation.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use memchr::memmem;
use regex_automata::{Input, meta::Regex, util::syntax};
use regex_syntax::{
    ast,
    hir::{
        Class, ClassBytes, ClassBytesRange, ClassUnicode, ClassUnicodeRange, Hir, HirKind, Look,
        Repetition,
    },
};
use std::error::Error;
use std::fmt;
use std::sync::OnceLock;

const MAX_FAST_LITERALS: usize = 128;
const MAX_FAST_LITERAL_BYTES: usize = 8 * 1024;

/// Options which affect the interpretation of one search pattern.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MatcherOptions {
    pub(crate) ignore_case: bool,
    pub(crate) smart_case: bool,
    pub(crate) fixed_strings: bool,
    pub(crate) word_regexp: bool,
    pub(crate) line_regexp: bool,
    /// When false, a pattern which explicitly requires a NUL byte is rejected.
    pub(crate) text: bool,
}

/// Byte offsets for a match in the input passed to [`LineMatcher::find_at`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MatchRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Common matcher surface consumed by the native scanner.
///
/// `find_at` searches a complete file buffer from `at`, while `is_match` is
/// the convenient single-line form. A scanner should honour
/// `binary_detection_enabled` before calling `find_at` on a binary buffer.
pub(crate) trait LineMatcher {
    fn supports_block_search(&self) -> bool;
    fn find_at(&self, haystack: &[u8], at: usize) -> Option<MatchRange>;
    fn is_match(&self, line: &[u8]) -> bool;
}

/// A compiled FastFs matcher. The enum keeps dispatch static in the hot path.
#[derive(Clone)]
pub(crate) enum NativeMatcher {
    Literal(FastLiteralMatcher),
    Regex(RegexMatcher),
}

#[derive(Clone)]
pub(crate) struct FastLiteralMatcher {
    engine: LiteralEngine,
    /// ASCII-only case folding is exact for an ASCII input prefix. A generic
    /// fallback preserves Unicode case-folding semantics after non-ASCII data.
    ascii_case_insensitive: bool,
    word_regexp: bool,
    text: bool,
    fallback: Option<Regex>,
}

#[derive(Clone)]
enum LiteralEngine {
    Single(Box<[u8]>),
    Multiple(AhoCorasick),
}

#[derive(Clone)]
pub(crate) struct RegexMatcher {
    regex: Regex,
    text: bool,
}

/// A pattern build failure suitable for presenting to the command layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MatcherError {
    message: String,
}

impl MatcherError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for MatcherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for MatcherError {}

impl NativeMatcher {
    /// Compile a pattern once for both line and block searching.
    pub(crate) fn build(pattern: &str, options: MatcherOptions) -> Result<Self, MatcherError> {
        if !options.text && pattern_explicitly_mentions_nul(pattern, options.fixed_strings)? {
            return Err(MatcherError::new(
                "NUL バイトを明示的に要求するパターンには --text が必要です",
            ));
        }
        let raw_hir = prepare_hir(pattern, options.fixed_strings, false)?;

        let case_insensitive = options.ignore_case || smart_case_enabled(pattern, options)?;
        let raw_literals = literal_alternatives(&raw_hir);

        // A whole-line constraint needs line anchors in the compiled regex.
        // Keeping it on the regex path also avoids restarting a literal search
        // for every physical line in a block.
        if !options.line_regexp
            && let Some(literals) = raw_literals
            && literals_are_fast(&literals)
        {
            if !case_insensitive {
                return FastLiteralMatcher::new(
                    literals,
                    false,
                    options.word_regexp,
                    options.text,
                    None,
                )
                .map(Self::Literal);
            }
            if literals.iter().all(|literal| literal.is_ascii())
                && (options.fixed_strings || !pattern.contains("(?"))
            {
                let regex = build_regex(&apply_boundaries(
                    prepare_hir(pattern, options.fixed_strings, true)?,
                    options,
                ))?;
                return FastLiteralMatcher::new(
                    literals,
                    true,
                    options.word_regexp,
                    options.text,
                    Some(regex),
                )
                .map(Self::Literal);
            }
        }

        let hir = prepare_hir(pattern, options.fixed_strings, case_insensitive)?;
        let regex = build_regex(&apply_boundaries(hir, options))?;
        Ok(Self::Regex(RegexMatcher {
            regex,
            text: options.text,
        }))
    }

    /// All native matchers preserve anchors against a complete input block.
    pub(crate) fn supports_block_search(&self) -> bool {
        true
    }

    /// Find the first match at or after `at` in a complete search buffer.
    ///
    /// The caller owns binary detection for block searching. This avoids an
    /// extra full-buffer NUL scan on every no-match search.
    pub(crate) fn find_at(&self, haystack: &[u8], at: usize) -> Option<MatchRange> {
        match self {
            Self::Literal(matcher) => matcher.find_at(haystack, at),
            Self::Regex(matcher) => matcher.find_at(haystack, at),
        }
    }

    /// Return whether a newline-free line matches.
    pub(crate) fn is_match(&self, line: &[u8]) -> bool {
        if self.binary_detection_enabled() && memchr::memchr(b'\0', line).is_some() {
            return false;
        }
        self.find_at(line, 0).is_some()
    }

    /// True when the scanner should stop text searching after the first NUL.
    pub(crate) fn binary_detection_enabled(&self) -> bool {
        match self {
            Self::Literal(matcher) => !matcher.text,
            Self::Regex(matcher) => !matcher.text,
        }
    }
}

impl LineMatcher for NativeMatcher {
    fn supports_block_search(&self) -> bool {
        Self::supports_block_search(self)
    }

    fn find_at(&self, haystack: &[u8], at: usize) -> Option<MatchRange> {
        Self::find_at(self, haystack, at)
    }

    fn is_match(&self, line: &[u8]) -> bool {
        Self::is_match(self, line)
    }
}

impl FastLiteralMatcher {
    fn new(
        literals: Vec<Vec<u8>>,
        ascii_case_insensitive: bool,
        word_regexp: bool,
        text: bool,
        fallback: Option<Regex>,
    ) -> Result<Self, MatcherError> {
        let engine = if literals.len() == 1 && !ascii_case_insensitive {
            LiteralEngine::Single(literals.into_iter().next().unwrap().into_boxed_slice())
        } else {
            let mut builder = AhoCorasickBuilder::new();
            builder.ascii_case_insensitive(ascii_case_insensitive);
            builder.match_kind(MatchKind::LeftmostFirst);
            let automaton = builder
                .build(&literals)
                .map_err(|error| MatcherError::new(error.to_string()))?;
            LiteralEngine::Multiple(automaton)
        };
        Ok(Self {
            engine,
            ascii_case_insensitive,
            word_regexp,
            text,
            fallback,
        })
    }

    fn find_at(&self, haystack: &[u8], at: usize) -> Option<MatchRange> {
        if at > haystack.len() {
            return None;
        }
        if !self.ascii_case_insensitive {
            return self.find_fast(haystack, at, haystack.len());
        }

        // Unicode simple case folding includes a small number of non-ASCII
        // equivalents for ASCII letters. Search the leading ASCII prefix
        // directly, then delegate the remaining input to the full regex when
        // needed so that no Unicode match is silently lost.
        let first_non_ascii = haystack[at..]
            .iter()
            .position(|byte| !byte.is_ascii())
            .map_or(haystack.len(), |offset| at + offset);
        if let Some(found) = self.find_fast(haystack, at, first_non_ascii) {
            return Some(found);
        }
        if first_non_ascii == haystack.len() {
            return None;
        }
        self.fallback.as_ref().and_then(|regex| {
            regex
                .find(Input::new(haystack).range(at..haystack.len()))
                .map(|matched| MatchRange {
                    start: matched.start(),
                    end: matched.end(),
                })
        })
    }

    fn find_fast(&self, haystack: &[u8], at: usize, end: usize) -> Option<MatchRange> {
        match &self.engine {
            LiteralEngine::Single(needle) => {
                find_single_literal(haystack, at, end, needle, self.word_regexp)
            }
            LiteralEngine::Multiple(automaton) => {
                find_literal_set(haystack, at, end, automaton, self.word_regexp)
            }
        }
    }
}

impl RegexMatcher {
    fn find_at(&self, haystack: &[u8], at: usize) -> Option<MatchRange> {
        if at > haystack.len() {
            return None;
        }
        self.regex
            .find(Input::new(haystack).range(at..haystack.len()))
            .map(|matched| MatchRange {
                start: matched.start(),
                end: matched.end(),
            })
    }
}

fn find_single_literal(
    haystack: &[u8],
    at: usize,
    end: usize,
    needle: &[u8],
    word_regexp: bool,
) -> Option<MatchRange> {
    let mut cursor = at;
    while cursor < end {
        let relative = memmem::find(&haystack[cursor..end], needle)?;
        let start = cursor + relative;
        let matched_end = start + needle.len();
        let range = MatchRange {
            start,
            end: matched_end,
        };
        if !word_regexp || has_word_half_boundaries(haystack, range) {
            return Some(range);
        }
        cursor = start + 1;
    }
    None
}

fn find_literal_set(
    haystack: &[u8],
    at: usize,
    end: usize,
    automaton: &AhoCorasick,
    word_regexp: bool,
) -> Option<MatchRange> {
    let mut cursor = at;
    while cursor < end {
        let matched = automaton.find(&haystack[cursor..end])?;
        let range = MatchRange {
            start: cursor + matched.start(),
            end: cursor + matched.end(),
        };
        if !word_regexp || has_word_half_boundaries(haystack, range) {
            return Some(range);
        }
        // Restart one byte later rather than after the match. That retains
        // overlapping literal candidates when a word boundary rejects one.
        cursor = range.start + 1;
    }
    None
}

fn has_word_half_boundaries(haystack: &[u8], range: MatchRange) -> bool {
    !is_word_before(haystack, range.start) && !is_word_after(haystack, range.end)
}

fn is_word_before(haystack: &[u8], at: usize) -> bool {
    if at == 0 {
        return false;
    }
    let minimum = at.saturating_sub(4);
    let mut start = at - 1;
    while start > minimum && haystack[start] & 0b1100_0000 == 0b1000_0000 {
        start -= 1;
    }
    is_word_character(&haystack[start..at])
}

fn is_word_after(haystack: &[u8], at: usize) -> bool {
    let Some(&first) = haystack.get(at) else {
        return false;
    };
    let width = utf8_width(first);
    let Some(end) = at.checked_add(width) else {
        return false;
    };
    haystack.get(at..end).is_some_and(is_word_character)
}

fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    }
}

fn is_word_character(bytes: &[u8]) -> bool {
    if let Some(&byte) = bytes.first()
        && byte.is_ascii()
    {
        return byte.is_ascii_alphanumeric() || byte == b'_';
    }
    unicode_word_regex().is_match(bytes)
}

fn unicode_word_regex() -> &'static Regex {
    static WORD: OnceLock<Regex> = OnceLock::new();
    WORD.get_or_init(|| Regex::new(r"\w").expect("the built-in word pattern is valid"))
}

fn parse_pattern(
    pattern: &str,
    fixed_strings: bool,
    case_insensitive: bool,
) -> Result<Hir, MatcherError> {
    if fixed_strings && !case_insensitive {
        return Ok(Hir::literal(pattern.as_bytes()));
    }
    let source = if fixed_strings {
        regex_syntax::escape(pattern)
    } else {
        pattern.to_owned()
    };
    syntax::parse_with(&source, &syntax_config(case_insensitive))
        .map_err(|error| MatcherError::new(error.to_string()))
}

fn prepare_hir(
    pattern: &str,
    fixed_strings: bool,
    case_insensitive: bool,
) -> Result<Hir, MatcherError> {
    strip_line_feed_from_hir(&parse_pattern(pattern, fixed_strings, case_insensitive)?)
}

fn syntax_config(case_insensitive: bool) -> syntax::Config {
    syntax::Config::new()
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .unicode(true)
        .utf8(false)
        .octal(false)
        .dot_matches_new_line(false)
        .line_terminator(b'\n')
}

fn build_regex(hir: &Hir) -> Result<Regex, MatcherError> {
    let mut builder = Regex::builder();
    builder.configure(Regex::config().utf8_empty(false).line_terminator(b'\n'));
    builder
        .build_from_hir(hir)
        .map_err(|error| MatcherError::new(error.to_string()))
}

fn apply_boundaries(hir: Hir, options: MatcherOptions) -> Hir {
    if options.line_regexp {
        return Hir::concat(vec![
            Hir::look(Look::StartLF),
            hir,
            optional_carriage_return(),
            Hir::look(Look::EndLF),
        ]);
    }
    if options.word_regexp {
        return Hir::concat(vec![
            Hir::look(Look::WordStartHalfUnicode),
            hir,
            Hir::look(Look::WordEndHalfUnicode),
        ]);
    }
    hir
}

fn optional_carriage_return() -> Hir {
    Hir::repetition(Repetition {
        min: 0,
        max: Some(1),
        greedy: true,
        sub: Box::new(Hir::literal(*b"\r")),
    })
}

fn smart_case_enabled(pattern: &str, options: MatcherOptions) -> Result<bool, MatcherError> {
    if !options.smart_case || options.ignore_case {
        return Ok(false);
    }
    let mut analysis = LiteralCaseAnalysis::default();
    if options.fixed_strings {
        analyse_literal_text(pattern, &mut analysis);
    } else {
        let mut builder = ast::parse::ParserBuilder::new();
        builder.octal(false);
        let parsed = builder
            .build()
            .parse(pattern)
            .map_err(|error| MatcherError::new(error.to_string()))?;
        analyse_ast_literals(&parsed, &mut analysis);
    }
    Ok(analysis.has_literal && !analysis.has_uppercase)
}

#[derive(Default)]
struct LiteralCaseAnalysis {
    has_literal: bool,
    has_uppercase: bool,
}

fn analyse_literal_text(text: &str, analysis: &mut LiteralCaseAnalysis) {
    if !text.is_empty() {
        analysis.has_literal = true;
        analysis.has_uppercase |= text.chars().any(char::is_uppercase);
    }
}

fn analyse_ast_literals(expression: &ast::Ast, analysis: &mut LiteralCaseAnalysis) {
    match expression {
        ast::Ast::Literal(literal) => analyse_literal_char(literal.c, analysis),
        ast::Ast::ClassBracketed(class) => analyse_ast_class_set(&class.kind, analysis),
        ast::Ast::Repetition(repetition) => analyse_ast_literals(&repetition.ast, analysis),
        ast::Ast::Group(group) => analyse_ast_literals(&group.ast, analysis),
        ast::Ast::Alternation(alternation) => {
            for expression in &alternation.asts {
                analyse_ast_literals(expression, analysis);
            }
        }
        ast::Ast::Concat(concat) => {
            for expression in &concat.asts {
                analyse_ast_literals(expression, analysis);
            }
        }
        ast::Ast::Empty(_)
        | ast::Ast::Flags(_)
        | ast::Ast::Dot(_)
        | ast::Ast::Assertion(_)
        | ast::Ast::ClassUnicode(_)
        | ast::Ast::ClassPerl(_) => {}
    }
}

fn analyse_ast_class_set(set: &ast::ClassSet, analysis: &mut LiteralCaseAnalysis) {
    match set {
        ast::ClassSet::Item(item) => analyse_ast_class_item(item, analysis),
        ast::ClassSet::BinaryOp(operation) => {
            analyse_ast_class_set(&operation.lhs, analysis);
            analyse_ast_class_set(&operation.rhs, analysis);
        }
    }
}

fn analyse_ast_class_item(item: &ast::ClassSetItem, analysis: &mut LiteralCaseAnalysis) {
    match item {
        ast::ClassSetItem::Literal(literal) => analyse_literal_char(literal.c, analysis),
        ast::ClassSetItem::Range(range) => {
            analyse_literal_char(range.start.c, analysis);
            analyse_literal_char(range.end.c, analysis);
        }
        ast::ClassSetItem::Bracketed(class) => analyse_ast_class_set(&class.kind, analysis),
        ast::ClassSetItem::Union(union) => {
            for item in &union.items {
                analyse_ast_class_item(item, analysis);
            }
        }
        ast::ClassSetItem::Empty(_)
        | ast::ClassSetItem::Ascii(_)
        | ast::ClassSetItem::Unicode(_)
        | ast::ClassSetItem::Perl(_) => {}
    }
}

fn analyse_literal_char(character: char, analysis: &mut LiteralCaseAnalysis) {
    analysis.has_literal = true;
    analysis.has_uppercase |= character.is_uppercase();
}

fn literals_are_fast(literals: &[Vec<u8>]) -> bool {
    !literals.is_empty()
        && literals.len() <= MAX_FAST_LITERALS
        && literals
            .iter()
            .all(|literal| !literal.is_empty() && literal.len() <= MAX_FAST_LITERAL_BYTES)
}

fn literal_alternatives(hir: &Hir) -> Option<Vec<Vec<u8>>> {
    match hir.kind() {
        HirKind::Empty => Some(vec![Vec::new()]),
        HirKind::Literal(literal) => Some(vec![literal.0.to_vec()]),
        HirKind::Class(class) => class.literal().map(|literal| vec![literal]),
        HirKind::Capture(capture) => literal_alternatives(&capture.sub),
        HirKind::Alternation(subexpressions) => {
            let mut alternatives = Vec::new();
            for subexpression in subexpressions {
                let branch = literal_alternatives(subexpression)?;
                if alternatives.len().saturating_add(branch.len()) > MAX_FAST_LITERALS {
                    return None;
                }
                alternatives.extend(branch);
            }
            Some(alternatives)
        }
        HirKind::Concat(subexpressions) => {
            let mut alternatives = vec![Vec::new()];
            for subexpression in subexpressions {
                let next = literal_alternatives(subexpression)?;
                let count = alternatives.len().checked_mul(next.len())?;
                if count > MAX_FAST_LITERALS {
                    return None;
                }
                let mut combined = Vec::with_capacity(count);
                for prefix in &alternatives {
                    for suffix in &next {
                        let length = prefix.len().checked_add(suffix.len())?;
                        if length > MAX_FAST_LITERAL_BYTES {
                            return None;
                        }
                        let mut literal = Vec::with_capacity(length);
                        literal.extend_from_slice(prefix);
                        literal.extend_from_slice(suffix);
                        combined.push(literal);
                    }
                }
                alternatives = combined;
            }
            Some(alternatives)
        }
        HirKind::Look(_) | HirKind::Repetition(_) => None,
    }
}

/// Enforce line-oriented matching without discarding useful character-class
/// patterns. Explicit line-feed literals are invalid, while a class such as
/// `[\n ]` is narrowed to the equivalent line-safe class (`[ ]`).
fn strip_line_feed_from_hir(hir: &Hir) -> Result<Hir, MatcherError> {
    match hir.kind() {
        HirKind::Literal(literal) if literal.0.contains(&b'\n') => Err(MatcherError::new(
            "FastFs rg の行検索では改行を明示したパターンは使用できません",
        )),
        HirKind::Literal(_) | HirKind::Empty | HirKind::Look(_) => Ok(hir.clone()),
        HirKind::Class(class) => Ok(Hir::class(strip_line_feed_from_class(class))),
        HirKind::Repetition(repetition) => Ok(Hir::repetition(
            repetition.with(strip_line_feed_from_hir(&repetition.sub)?),
        )),
        HirKind::Capture(capture) => {
            let mut capture = capture.clone();
            capture.sub = Box::new(strip_line_feed_from_hir(&capture.sub)?);
            Ok(Hir::capture(capture))
        }
        HirKind::Concat(subexpressions) => Ok(Hir::concat(
            subexpressions
                .iter()
                .map(strip_line_feed_from_hir)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        HirKind::Alternation(subexpressions) => Ok(Hir::alternation(
            subexpressions
                .iter()
                .map(strip_line_feed_from_hir)
                .collect::<Result<Vec<_>, _>>()?,
        )),
    }
}

fn strip_line_feed_from_class(class: &Class) -> Class {
    match class {
        Class::Unicode(class) => {
            let mut ranges = Vec::with_capacity(class.ranges().len() + 1);
            for range in class.ranges() {
                if range.end() < '\n' || range.start() > '\n' {
                    ranges.push(*range);
                    continue;
                }
                if range.start() < '\n' {
                    ranges.push(ClassUnicodeRange::new(range.start(), '\t'));
                }
                if range.end() > '\n' {
                    ranges.push(ClassUnicodeRange::new('\u{000B}', range.end()));
                }
            }
            Class::Unicode(ClassUnicode::new(ranges))
        }
        Class::Bytes(class) => {
            let mut ranges = Vec::with_capacity(class.ranges().len() + 1);
            for range in class.ranges() {
                if range.end() < b'\n' || range.start() > b'\n' {
                    ranges.push(*range);
                    continue;
                }
                if range.start() < b'\n' {
                    ranges.push(ClassBytesRange::new(range.start(), b'\t'));
                }
                if range.end() > b'\n' {
                    ranges.push(ClassBytesRange::new(b'\x0B', range.end()));
                }
            }
            Class::Bytes(ClassBytes::new(ranges))
        }
    }
}

fn pattern_explicitly_mentions_nul(
    pattern: &str,
    fixed_strings: bool,
) -> Result<bool, MatcherError> {
    if fixed_strings {
        return Ok(pattern.as_bytes().contains(&b'\0'));
    }
    let mut builder = ast::parse::ParserBuilder::new();
    builder.octal(false);
    let parsed = builder
        .build()
        .parse(pattern)
        .map_err(|error| MatcherError::new(error.to_string()))?;
    Ok(ast_mentions_nul(&parsed))
}

/// This checks only concrete NUL syntax supplied by the user. Broad classes
/// such as `.` and negated classes remain valid; binary input is stopped by
/// the scanner before they can consume a NUL byte.
fn ast_mentions_nul(expression: &ast::Ast) -> bool {
    match expression {
        ast::Ast::Literal(literal) => literal.c == '\0',
        ast::Ast::ClassBracketed(class) => !class.negated && class_set_mentions_nul(&class.kind),
        ast::Ast::Repetition(repetition) => ast_mentions_nul(&repetition.ast),
        ast::Ast::Group(group) => ast_mentions_nul(&group.ast),
        ast::Ast::Alternation(alternation) => alternation.asts.iter().any(ast_mentions_nul),
        ast::Ast::Concat(concat) => concat.asts.iter().any(ast_mentions_nul),
        ast::Ast::Empty(_)
        | ast::Ast::Flags(_)
        | ast::Ast::Dot(_)
        | ast::Ast::Assertion(_)
        | ast::Ast::ClassUnicode(_)
        | ast::Ast::ClassPerl(_) => false,
    }
}

fn class_set_mentions_nul(set: &ast::ClassSet) -> bool {
    match set {
        ast::ClassSet::Item(item) => class_item_mentions_nul(item),
        ast::ClassSet::BinaryOp(operation) => {
            class_set_mentions_nul(&operation.lhs) || class_set_mentions_nul(&operation.rhs)
        }
    }
}

fn class_item_mentions_nul(item: &ast::ClassSetItem) -> bool {
    match item {
        ast::ClassSetItem::Literal(literal) => literal.c == '\0',
        ast::ClassSetItem::Range(range) => range.start.c <= '\0' && '\0' <= range.end.c,
        ast::ClassSetItem::Bracketed(class) => {
            !class.negated && class_set_mentions_nul(&class.kind)
        }
        ast::ClassSetItem::Union(union) => union.items.iter().any(class_item_mentions_nul),
        ast::ClassSetItem::Empty(_)
        | ast::ClassSetItem::Ascii(_)
        | ast::ClassSetItem::Unicode(_)
        | ast::ClassSetItem::Perl(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{LineMatcher, MatcherOptions, NativeMatcher, literal_alternatives, parse_pattern};

    fn build(pattern: &str, options: MatcherOptions) -> NativeMatcher {
        NativeMatcher::build(pattern, options).unwrap()
    }

    #[test]
    fn fixed_string_treats_regular_expression_syntax_as_text() {
        let matcher = build(
            "a.b|c",
            MatcherOptions {
                fixed_strings: true,
                ..MatcherOptions::default()
            },
        );
        assert!(matcher.is_match(b"prefix a.b|c suffix"));
        assert!(!matcher.is_match(b"aXb"));
    }

    #[test]
    fn literal_alternation_uses_the_fast_path_and_reports_block_offsets() {
        let matcher = build("alpha|beta|gamma", MatcherOptions::default());
        assert!(matches!(&matcher, &NativeMatcher::Literal(_)));
        assert_eq!(
            matcher.find_at(b"zero\nbeta\ngamma", 0),
            Some(super::MatchRange { start: 5, end: 9 })
        );
        assert_eq!(
            matcher.find_at(b"zero\nbeta\ngamma", 9),
            Some(super::MatchRange { start: 10, end: 15 })
        );
    }

    #[test]
    fn word_literal_alternation_keeps_the_first_valid_leftmost_match() {
        let matcher = build(
            "abc|b",
            MatcherOptions {
                word_regexp: true,
                ..MatcherOptions::default()
            },
        );
        assert!(matcher.is_match(b"abc"));
    }

    #[test]
    fn parenthesized_literal_or_is_recognized_without_regex_execution() {
        let hir = parse_pattern("prefix(?:one|two)", false, false).unwrap();
        assert_eq!(
            literal_alternatives(&hir),
            Some(vec![b"prefixone".to_vec(), b"prefixtwo".to_vec()])
        );
    }

    #[test]
    fn smart_case_only_ignores_case_when_the_literals_have_no_uppercase() {
        let insensitive = build(
            "needle",
            MatcherOptions {
                smart_case: true,
                ..MatcherOptions::default()
            },
        );
        let sensitive = build(
            "Needle",
            MatcherOptions {
                smart_case: true,
                ..MatcherOptions::default()
            },
        );
        assert!(insensitive.is_match(b"NEEDLE"));
        assert!(!sensitive.is_match(b"needle"));

        let lower_range = build(
            "[a-z]",
            MatcherOptions {
                smart_case: true,
                ..MatcherOptions::default()
            },
        );
        let upper_range = build(
            "[A-Z]",
            MatcherOptions {
                smart_case: true,
                ..MatcherOptions::default()
            },
        );
        assert!(lower_range.is_match(b"A"));
        assert!(!upper_range.is_match(b"a"));
    }

    #[test]
    fn byte_mode_regular_expressions_match_invalid_utf8() {
        let literal = build(r"(?-u:\xFF)", MatcherOptions::default());
        let dot = build(r"(?-u:.)", MatcherOptions::default());
        assert!(literal.is_match(&[0xFF]));
        assert!(dot.is_match(&[0xFF]));
    }

    #[test]
    fn ignore_case_keeps_unicode_case_folding_correct() {
        let matcher = build(
            "k",
            MatcherOptions {
                ignore_case: true,
                ..MatcherOptions::default()
            },
        );
        assert!(matcher.is_match("\u{212A}".as_bytes()));
    }

    #[test]
    fn word_regexp_uses_half_boundaries_not_plain_word_boundaries() {
        let matcher = build(
            "-2",
            MatcherOptions {
                word_regexp: true,
                ..MatcherOptions::default()
            },
        );
        assert!(matcher.is_match(b"(-2)"));

        let identifier = build(
            "fastfs",
            MatcherOptions {
                word_regexp: true,
                ..MatcherOptions::default()
            },
        );
        assert!(identifier.is_match(b"fastfs."));
        assert!(!identifier.is_match(b"fastfs_core"));
        assert!(!identifier.is_match("猫fastfs".as_bytes()));
    }

    #[test]
    fn line_regexp_applies_to_each_line_in_a_block() {
        let matcher = build(
            "needle",
            MatcherOptions {
                line_regexp: true,
                word_regexp: true,
                ..MatcherOptions::default()
            },
        );
        assert!(matcher.supports_block_search());
        assert_eq!(
            matcher.find_at(b"needlework\nneedle\nneedle!", 0),
            Some(super::MatchRange { start: 11, end: 17 })
        );
        assert!(matcher.is_match(b"needle"));
        assert!(!matcher.is_match(b"needlework"));
    }

    #[test]
    fn line_anchors_search_the_whole_block_without_crossing_lines() {
        let matcher = build("^needle$", MatcherOptions::default());
        assert!(matcher.supports_block_search());
        assert_eq!(
            matcher.find_at(b"prefix\nneedle\nsuffix", 0),
            Some(super::MatchRange { start: 7, end: 13 })
        );
    }

    #[test]
    fn text_anchors_preserve_file_boundaries_in_block_search() {
        let matcher = build("\\Aneedle", MatcherOptions::default());
        assert!(LineMatcher::supports_block_search(&matcher));
        assert_eq!(matcher.find_at(b"prefix\nneedle", 0), None,);
        assert_eq!(
            matcher.find_at(b"needle\nneedle", 0),
            Some(super::MatchRange { start: 0, end: 6 })
        );
        assert_eq!(matcher.find_at(b"needle\nneedle", 7), None);
    }

    #[test]
    fn explicit_line_feeds_are_rejected_but_classes_keep_other_whitespace() {
        assert!(NativeMatcher::build("needle\\nnext", MatcherOptions::default()).is_err());
        let whitespace = build("[\\n ]", MatcherOptions::default());
        assert!(whitespace.is_match(b" "));
        assert!(!whitespace.is_match(b"\n"));
        let any_space = build("\\s", MatcherOptions::default());
        assert!(any_space.is_match(b"\t"));
        assert!(!any_space.is_match(b"\n"));
    }

    #[test]
    fn explicit_binary_literals_and_ranges_require_text_mode() {
        assert!(NativeMatcher::build("\\x00", MatcherOptions::default()).is_err());
        assert!(NativeMatcher::build("[\\x00-\\x03]", MatcherOptions::default()).is_err());
        assert!(NativeMatcher::build(".", MatcherOptions::default()).is_ok());
        assert!(NativeMatcher::build("[^\\x00]", MatcherOptions::default()).is_ok());
        let matcher = build(
            "\\x00",
            MatcherOptions {
                text: true,
                ..MatcherOptions::default()
            },
        );
        assert_eq!(
            matcher.find_at(b"a\0b", 0),
            Some(super::MatchRange { start: 1, end: 2 })
        );
    }

    #[test]
    fn line_regexp_accepts_crlf_without_normalizing_the_input_buffer() {
        let matcher = build(
            "needle",
            MatcherOptions {
                line_regexp: true,
                ..MatcherOptions::default()
            },
        );
        assert_eq!(
            matcher.find_at(b"needle\r\nother", 0),
            Some(super::MatchRange { start: 0, end: 7 })
        );
    }

    #[test]
    fn invalid_regular_expression_returns_a_build_error() {
        assert!(NativeMatcher::build("(", MatcherOptions::default()).is_err());
    }
}
