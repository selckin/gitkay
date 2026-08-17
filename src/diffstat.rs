//! The `--stat` block: the summary git draws above a patch, formatted from counts the
//! diff build already has.
//!
//! **Why this exists rather than a call to `Diff::stats`.** That call is a COMPLETE
//! second pass over the diff: `git_diff_get_stats` generates every patch again, asks it
//! for its line counts and throws it away. Measured on a 900k-line diff, it cost 960ms
//! beside the 1.0s the patch pass we keep costs — **38% of every diff build**, for a
//! handful of summary rows. And the counts it computes are the ones `push_patch_line`
//! is already accumulating per file, so the only thing that pass ever bought was the
//! FORMATTING. That is this module.
//!
//! It is a port of libgit2's `diff_stats.c` under `GIT_DIFF_STATS_FULL`, quirks
//! included — a file with no deletions still gets a single `-` on its bar, because the
//! scaled bar's two runs are each `max(n, 1)`. Deliberate: the block is meant to look
//! like the one git prints, and "nearly" is worse than either alternative.
//!
//! **libgit2 stays the oracle even though it is no longer the implementation**: the
//! tests assert byte-equality against `Diff::stats().to_buf()` over real repositories,
//! so the port is checked against the thing it replaced rather than against itself.
//!
//! Two divergences are deliberate and cannot be tested that way. A **textconv-driven**
//! file is counted from its CONVERTED patch here, where libgit2 counts the raw blob and
//! prints `Bin` — which makes the block agree with the sidebar beside it, a
//! disagreement the old path documented and accepted. And a **non-UTF-8 path** prints
//! through the same lossy display string the rest of the UI uses.

/// One file's row in the block.
pub struct StatFile<'a> {
    /// The pre-image path, only when it differs from `new_path` (a rename or copy) —
    /// exactly the condition libgit2 checks before printing the `old => new` form.
    pub old_path: Option<&'a str>,
    pub new_path: &'a str,
    pub insertions: usize,
    pub deletions: usize,
    /// `Some((old_size, new_size))` for a binary delta, which prints its byte sizes
    /// where a text file prints its counts and bar.
    pub binary: Option<(u64, u64)>,
}

/// libgit2's `DIFF_RENAME_FILE_SEPARATOR`. ASCII, unlike the sidebar's `⇒`: this block
/// imitates git's output and the sidebar does not.
const RENAME_SEPARATOR: &str = " => ";

/// libgit2's `STATS_FULL_MIN_SCALE` — the narrowest bar it will scale to.
const FULL_MIN_SCALE: usize = 7;

impl StatFile<'_> {
    /// The name column as libgit2 prints it: a rename whose paths share a directory
    /// collapses to `dir/{old => new}`, and one that shares none to `old => new`.
    fn printed_name(&self) -> String {
        let Some(old) = self.old_path else {
            return self.new_path.to_owned();
        };
        let common = crate::diff::common_dir_prefix_len(old, self.new_path);
        if common > 0 {
            format!(
                "{}{{{}{RENAME_SEPARATOR}{}}}",
                &old[..common],
                &old[common..],
                &self.new_path[common..]
            )
        } else {
            format!("{old}{RENAME_SEPARATOR}{}", self.new_path)
        }
    }

    const fn changes(&self) -> usize {
        self.insertions + self.deletions
    }
}

/// Decimal digits in `val` — libgit2's `digits_for_value`, which the count column is
/// padded to. Its `placevalue *= 10` is unbounded in C; here it stops at the last place
/// a `usize` can hold, which no real count reaches.
const fn digits_for_value(val: usize) -> usize {
    let mut count = 1;
    let mut place: usize = 10;
    while val >= place {
        count += 1;
        match place.checked_mul(10) {
            Some(next) => place = next,
            None => break,
        }
    }
    count
}

/// The block: one row per file, then the summary row. Rows carry no trailing newline
/// and keep libgit2's leading space.
///
/// `width` is the terminal width the bars are scaled into (libgit2's `to_buf` argument).
pub fn block(files: &[StatFile<'_>], width: usize) -> Vec<String> {
    use std::fmt::Write as _;

    let names: Vec<String> = files.iter().map(StatFile::printed_name).collect();
    let max_name = names.iter().map(String::len).max().unwrap_or(0);
    let max_filestat = files.iter().map(StatFile::changes).max().unwrap_or(0);
    let max_digits = digits_for_value(max_filestat + 1);

    // libgit2's scale preparation, verbatim, including that the subtraction happens
    // only when the width is wide enough to subtract from — a narrow terminal keeps the
    // caller's number rather than going negative. `bar == 0` then means "no scaling":
    // every bar fits at one character per changed line.
    let mut bar = width;
    if bar > 0 {
        if bar > max_name + max_digits + 5 {
            bar -= max_name + max_digits + 5;
        }
        if bar < FULL_MIN_SCALE {
            bar = FULL_MIN_SCALE;
        }
    }
    if bar > max_filestat {
        bar = 0;
    }

    let mut out: Vec<String> = Vec::with_capacity(files.len() + 1);
    for (f, name) in files.iter().zip(&names) {
        let mut row = String::with_capacity(max_name + max_digits + width + 8);
        row.push(' ');
        row.push_str(name);
        pad(&mut row, ' ', max_name - name.len());
        row.push_str(" | ");
        if let Some((old, new)) = f.binary {
            let _ = write!(row, "Bin {old} -> {new} bytes");
        } else {
            let total = f.changes();
            let _ = write!(row, "{total:>max_digits$}");
            if total > 0 {
                row.push(' ');
                let (plus, minus) = if bar == 0 {
                    (f.insertions, f.deletions)
                } else {
                    // Scaled — and each run is at least one character, which is why a
                    // file with no deletions still shows a `-`. libgit2 does this; a
                    // block that quietly did not would not be the block git prints.
                    let full = (total * bar + max_filestat / 2) / max_filestat;
                    let plus = full * f.insertions / total;
                    (plus.max(1), full.saturating_sub(plus).max(1))
                };
                pad(&mut row, '+', plus);
                pad(&mut row, '-', minus);
            }
        }
        out.push(row);
    }
    out.push(summary(files));
    out
}

fn pad(out: &mut String, c: char, n: usize) {
    out.extend(std::iter::repeat_n(c, n));
}

/// ` N files changed, X insertions(+), Y deletions(-)` — with libgit2's rule for when
/// each clause appears at all: a zero side is printed only when the other side is zero
/// too, so an all-additions commit shows no `deletions(-)` clause.
fn summary(files: &[StatFile<'_>]) -> String {
    use std::fmt::Write as _;
    let n = files.len();
    let insertions: usize = files.iter().map(|f| f.insertions).sum();
    let deletions: usize = files.iter().map(|f| f.deletions).sum();
    let mut row = format!(" {n} file{} changed", plural(n));
    if insertions > 0 || deletions == 0 {
        let _ = write!(row, ", {insertions} insertion{}(+)", plural(insertions));
    }
    if deletions > 0 || insertions == 0 {
        let _ = write!(row, ", {deletions} deletion{}(-)", plural(deletions));
    }
    row
}

const fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(new: &str, insertions: usize, deletions: usize) -> StatFile<'_> {
        StatFile {
            old_path: None,
            new_path: new,
            insertions,
            deletions,
            binary: None,
        }
    }

    #[test]
    fn digits_counts_decimal_places() {
        assert_eq!(digits_for_value(0), 1);
        assert_eq!(digits_for_value(9), 1);
        assert_eq!(digits_for_value(10), 2);
        assert_eq!(digits_for_value(999), 3);
        assert_eq!(digits_for_value(1000), 4);
        // Bounded rather than overflowing, unlike the C it is ported from.
        assert_eq!(digits_for_value(usize::MAX), 20);
    }

    /// With every bar fitting, there is no scaling: one character per changed line.
    #[test]
    fn a_small_block_draws_one_character_per_line() {
        let files = [text("a.txt", 3, 1)];
        assert_eq!(
            block(&files, 80),
            [
                " a.txt | 4 +++-".to_string(),
                " 1 file changed, 3 insertions(+), 1 deletion(-)".to_string(),
            ]
        );
    }

    /// Past the width the bar is scaled — and each run is then at least one character,
    /// so a file with no deletions at all still shows a `-`. That is libgit2's
    /// behaviour and the reason this is a port rather than a fresh implementation.
    #[test]
    fn a_scaled_bar_gives_an_absent_side_one_character() {
        let files = [text("a.txt", 500, 0)];
        let rows = block(&files, 40);
        let bar = rows[0].rsplit_once("| ").unwrap().1;
        let (count, marks) = bar.split_once(' ').unwrap();
        assert_eq!(count, "500");
        assert!(marks.starts_with('+') && marks.ends_with('-'), "{marks:?}");
        assert_eq!(marks.matches('-').count(), 1, "exactly the forced one");
        assert_eq!(rows[1], " 1 file changed, 500 insertions(+)");
    }

    /// The name column is padded to the widest printed name, which for a rename is the
    /// braced form and not either path.
    #[test]
    fn a_rename_prints_its_common_directory_once() {
        let files = [
            StatFile {
                old_path: Some("src/a/old.rs"),
                new_path: "src/a/new.rs",
                insertions: 0,
                deletions: 0,
                binary: None,
            },
            text("z.txt", 1, 0),
        ];
        let rows = block(&files, 80);
        assert_eq!(rows[0], " src/a/{old.rs => new.rs} | 0");
        assert_eq!(rows[1], " z.txt                    | 1 +");
    }

    /// Sharing no directory, the two paths are printed whole.
    #[test]
    fn a_rename_across_directories_prints_both_paths() {
        let files = [StatFile {
            old_path: Some("old/a.rs"),
            new_path: "new/b.rs",
            insertions: 2,
            deletions: 2,
            binary: None,
        }];
        assert_eq!(block(&files, 80)[0], " old/a.rs => new/b.rs | 4 ++--");
    }

    #[test]
    fn a_binary_file_prints_its_sizes_instead_of_a_bar() {
        let files = [StatFile {
            old_path: None,
            new_path: "logo.png",
            insertions: 0,
            deletions: 0,
            binary: Some((4, 300)),
        }];
        assert_eq!(block(&files, 80)[0], " logo.png | Bin 4 -> 300 bytes");
    }

    /// Both clauses are singular, and an empty diff still says what it found.
    #[test]
    fn the_summary_pluralises_and_keeps_a_zero_side_only_when_both_are_zero() {
        assert_eq!(
            block(&[text("a", 1, 1)], 80)[1],
            " 1 file changed, 1 insertion(+), 1 deletion(-)"
        );
        assert_eq!(
            block(&[text("a", 0, 2)], 80)[1],
            " 1 file changed, 2 deletions(-)"
        );
        assert_eq!(
            block(&[text("a", 2, 0)], 80)[1],
            " 1 file changed, 2 insertions(+)"
        );
        // Nothing changed anywhere: both clauses appear, so the row still reads as an
        // answer rather than a truncation.
        assert_eq!(
            block(&[], 80),
            [" 0 files changed, 0 insertions(+), 0 deletions(-)".to_string()]
        );
    }
}
