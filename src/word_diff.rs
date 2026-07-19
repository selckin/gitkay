//! Intra-line ("word") diff: given a `-`/`+` line pair, find the changed token runs on
//! each side so the UI can emphasise just what actually changed. Pure — no egui or git2 —
//! which is why it lives on its own with its own tests. The `DiffLine`-aware driver that
//! walks change blocks and calls `line_emphasis` here is `compute_word_emphasis` in
//! `main.rs`.

use std::ops::Range;

/// Split `s` into word-diff tokens: maximal `[A-Za-z0-9_]` runs are single tokens,
/// every other character is its own token (whitespace and punctuation included).
/// Each token carries its byte range in `s` and its text.
fn word_tokens(s: &str) -> Vec<(Range<usize>, &str)> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if is_word(c) {
            let mut end = start;
            while let Some(&(i, c)) = chars.peek() {
                if is_word(c) {
                    end = i + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            out.push((start..end, &s[start..end]));
        } else {
            let end = start + c.len_utf8();
            chars.next();
            out.push((start..end, &s[start..end]));
        }
    }
    out
}

/// The token positions in `a` and `b` that a longest-common-subsequence alignment
/// (by text) leaves unmatched — the changed tokens on each side. (A token whose
/// text also appears elsewhere can still be marked changed; it's the *position*
/// that's unaligned, not the value.) O(n·m), fine for one short line.
fn changed_tokens(a: &[&str], b: &[&str]) -> (Vec<usize>, Vec<usize>) {
    let (n, m) = (a.len(), b.len());
    // dp[at(i, j)] = LCS length of a[i..] and b[j..]. One flat allocation (row-major,
    // stride m+1) instead of n+1 separate Vecs.
    let stride = m + 1;
    let at = |i: usize, j: usize| i * stride + j;
    let mut dp = vec![0u16; (n + 1) * stride];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if a[i] == b[j] {
                dp[at(i + 1, j + 1)] + 1
            } else {
                dp[at(i + 1, j)].max(dp[at(i, j + 1)])
            };
        }
    }
    let (mut a_ch, mut b_ch) = (Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            a_ch.push(i);
            i += 1;
        } else {
            b_ch.push(j);
            j += 1;
        }
    }
    a_ch.extend(i..n);
    b_ch.extend(j..m);
    (a_ch, b_ch)
}

/// Byte ranges of the given (ascending) changed token indices, merging tokens that
/// are contiguous in the source so a changed run becomes one highlight.
fn merge_token_ranges(tokens: &[(Range<usize>, &str)], changed: &[usize]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for &idx in changed {
        let r = tokens[idx].0.clone();
        match out.last_mut() {
            Some(last) if last.end == r.start => last.end = r.end,
            _ => out.push(r),
        }
    }
    out
}

/// Word-level changed ranges for a `-`/`+` line pair, in each body's coordinates.
pub fn line_emphasis(del: &str, add: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let dt = word_tokens(del);
    let at = word_tokens(add);
    let ds: Vec<&str> = dt.iter().map(|(_, s)| *s).collect();
    let as_: Vec<&str> = at.iter().map(|(_, s)| *s).collect();
    let (d_ch, a_ch) = changed_tokens(&ds, &as_);
    (merge_token_ranges(&dt, &d_ch), merge_token_ranges(&at, &a_ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_emphasis_marks_only_changed_tokens() {
        let pick = |body: &str, ranges: &[Range<usize>]| -> Vec<String> {
            ranges.iter().map(|r| body[r.clone()].to_string()).collect()
        };
        // One token differs; the shared tokens (let, =, foo, (), ;) stay plain.
        let (del, add) = line_emphasis("let x = foo();", "let y = foo();");
        assert_eq!(pick("let x = foo();", &del), vec!["x".to_string()]);
        assert_eq!(pick("let y = foo();", &add), vec!["y".to_string()]);
        // `_` is a word char, so a whole identifier is one token.
        let (del, _) = line_emphasis("a.full_name", "a.display_name");
        assert_eq!(pick("a.full_name", &del), vec!["full_name".to_string()]);
    }

    #[test]
    fn changed_tokens_edge_cases() {
        assert_eq!(changed_tokens(&["a", "b"], &["a", "b"]), (vec![], vec![])); // identical
        assert_eq!(changed_tokens(&["a", "c"], &["a", "b", "c"]), (vec![], vec![1])); // insert
        assert_eq!(changed_tokens(&["a", "b", "c"], &["a", "c"]), (vec![1], vec![])); // delete
        assert_eq!(changed_tokens(&[], &["a"]), (vec![], vec![0])); // empty → all inserted
        assert_eq!(changed_tokens(&["a"], &[]), (vec![0], vec![])); // all deleted
        assert_eq!(changed_tokens(&[], &[]), (vec![], vec![])); // both empty
    }

    #[test]
    fn merge_token_ranges_merges_only_contiguous() {
        let toks: Vec<(Range<usize>, &str)> =
            vec![(0..1, "a"), (1..2, "b"), (2..3, "c"), (3..4, "d")];
        assert_eq!(merge_token_ranges(&toks, &[0, 1]), vec![0..2]); // adjacent → merged
        assert_eq!(merge_token_ranges(&toks, &[0, 2]), vec![0..1, 2..3]); // gap → separate
        assert!(merge_token_ranges(&toks, &[]).is_empty());
    }
}
