//! FTS5 query construction and score conversion for `mnene`.
//!
//! This module implements a pure builder from user query text to a safe
//! FTS5 conjunctive prefix `MATCH` expression, and the conversion from an
//! FTS5 `bm25()` value to the reported search score. It has no I/O and no dependency on
//! `rusqlite`.

/// Builds a safe FTS5 `MATCH` expression from raw user query text.
///
/// The input is split into tokens on every character that is not
/// alphanumeric (as classified by `char::is_alphanumeric`, which covers
/// Unicode letters and digits, including accented and non-Latin scripts).
/// That includes Unicode whitespace and every FTS5 metacharacter
/// (`" * ( ) : ^ + - { } ~` among others), so a token never contains
/// anything but letters and digits and a run of non-alphanumeric
/// characters between two words never merges them into one token.
///
/// This matches how FTS5's `unicode61` tokenizer indexes stored text:
/// `unicode61` also splits on punctuation, so `agent-memory` is indexed as
/// the two terms `agent` and `memory`, and a hyphenated tag like
/// `agent-memory` or a contraction like `don't` must become two separate
/// match clauses (`"agent"* AND "memory"*`, `"don"* AND "t"*`) rather than
/// one fused token that the index never contains. Splitting on every
/// non-alphanumeric character, rather than only on whitespace, keeps the
/// tokenization here aligned with the tokenization used at index time.
///
/// Splitting this way is also the simplest rule that stays safe: every
/// surviving token contains only letters and digits, so it can never
/// contain a syntactically meaningful FTS5 character (including a double
/// quote), and future FTS5 syntax additions cannot reopen the hole because
/// nothing but letters and digits ever survives.
///
/// A surviving token is wrapped in double quotes with a trailing `*`, for
/// example `"token"*`, so it is always interpreted as a literal quoted
/// prefix term and never as an operator. This handles the FTS5 keywords
/// `AND`, `OR`, `NOT`, and `NEAR` safely too: quoting makes them literal
/// search terms rather than query operators.
///
/// Tokens that are empty (for example a run of separators at the start or
/// end of the input, or between two separators) are dropped. Surviving
/// tokens are joined with ` AND ` to require every token to match.
///
/// Returns `None` when no token survives, meaning the caller should treat
/// the query as matching nothing (or skip the `MATCH` clause entirely,
/// depending on the caller's semantics).
#[must_use]
pub fn build_match_expression(query: &str) -> Option<String> {
    let clauses: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\"*"))
        .collect();

    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" AND "))
    }
}

/// Converts an FTS5 `bm25()` value into the reported search score.
///
/// `SQLite` FTS5's `bm25()` auxiliary function returns a relevance value
/// that is negative and where a value *closer to negative infinity* is a
/// *better* match (lower is better). That convention is inverted here by
/// negation, so that the value returned from this function is higher for
/// a better match, matching the ordering `mnene` reports to callers.
#[must_use]
pub fn score_from_bm25(bm25: f64) -> f64 {
    -bm25
}

#[cfg(test)]
mod tests {
    use super::{build_match_expression, score_from_bm25};

    #[test]
    fn splits_on_fts5_metacharacters_between_words() {
        let result = build_match_expression("hello*world (foo):bar^baz+qux-quux{a}~b");
        assert_eq!(
            result,
            Some(
                "\"hello\"* AND \"world\"* AND \"foo\"* AND \"bar\"* AND \"baz\"* AND \"qux\"* \
                 AND \"quux\"* AND \"a\"* AND \"b\"*"
                    .to_owned()
            )
        );
    }

    #[test]
    fn splits_hyphenated_tags_into_separate_clauses() {
        let result = build_match_expression("agent-memory");
        assert_eq!(result, Some("\"agent\"* AND \"memory\"*".to_owned()));
    }

    #[test]
    fn splits_on_apostrophes() {
        let result = build_match_expression("don't");
        assert_eq!(result, Some("\"don\"* AND \"t\"*".to_owned()));
    }

    #[test]
    fn splits_on_slashes() {
        let result = build_match_expression("foo.bar/baz");
        assert_eq!(
            result,
            Some("\"foo\"* AND \"bar\"* AND \"baz\"*".to_owned())
        );
    }

    #[test]
    fn quotes_each_surviving_token_with_trailing_star() {
        let result = build_match_expression("alpha beta");
        assert_eq!(result, Some("\"alpha\"* AND \"beta\"*".to_owned()));
    }

    #[test]
    fn returns_none_for_empty_input() {
        assert_eq!(build_match_expression(""), None);
    }

    #[test]
    fn returns_none_for_whitespace_only_input() {
        assert_eq!(build_match_expression("   \t\n  "), None);
    }

    #[test]
    fn returns_none_for_all_metacharacter_input() {
        assert_eq!(build_match_expression("***---:::\"\"\""), None);
    }

    #[test]
    fn drops_empty_tokens_but_keeps_survivors() {
        let result = build_match_expression("*** hello ---");
        assert_eq!(result, Some("\"hello\"*".to_owned()));
    }

    #[test]
    fn preserves_unicode_alphanumeric_tokens() {
        let result = build_match_expression("café naïve");
        assert_eq!(result, Some("\"café\"* AND \"naïve\"*".to_owned()));
    }

    #[test]
    fn treats_multiple_spaces_and_tabs_as_separators() {
        let result = build_match_expression("one\t\tthings   two");
        assert_eq!(
            result,
            Some("\"one\"* AND \"things\"* AND \"two\"*".to_owned())
        );
    }

    #[test]
    fn quotes_bare_fts5_keywords_as_literals() {
        let result = build_match_expression("AND OR NOT NEAR");
        assert_eq!(
            result,
            Some("\"AND\"* AND \"OR\"* AND \"NOT\"* AND \"NEAR\"*".to_owned())
        );
    }

    #[test]
    fn splits_on_embedded_double_quotes() {
        let result = build_match_expression("\"quoted\"token\"");
        assert_eq!(result, Some("\"quoted\"* AND \"token\"*".to_owned()));
    }

    #[test]
    fn score_negates_bm25_so_higher_is_better() {
        assert!((score_from_bm25(-2.5) - 2.5).abs() < f64::EPSILON);
        assert!((score_from_bm25(2.5) - -2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn score_of_zero_is_zero() {
        assert!((score_from_bm25(0.0) - 0.0).abs() < f64::EPSILON);
    }
}
