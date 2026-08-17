//! Rendering a commit's timestamp, absolutely or as an age.
//!
//! Pure, and here rather than in `diff.rs` (where it sat by accident) because it is
//! the commit LIST's concern: `DateCol` picks a style per `[commit_list] date` and
//! this formats under it. `CommitInfo` deliberately stores the raw
//! `(time, tz_offset_min)` rather than a pre-formatted string, since an age moves.
//!
//! The relative form is a **port of git's own `show_date_relative` (`date.c`)**,
//! deliberately rather than an equivalent-looking ladder: the interesting part is the
//! rounding, and it is not what anyone writes from scratch — each rung rounds into
//! the next unit before testing it (`(diff + 30) / 60`) and every threshold overshoots
//! (90s, 90min, 36h, 14d, 10w). Its test expectations come from real git output, not
//! from reading `date.c`; re-derive them that way.

/// Format a commit timestamp (Unix seconds) in its own recorded UTC offset
/// (`tz_offset_min`) as `YYYY-MM-DD HH:MM`, with seconds when asked — matching what
/// `git log` shows. Returns "" if the timestamp or offset is out of range. (A valid
/// time never formats empty, so callers can treat "" as "no date".)
pub fn format_commit_time(secs: i64, tz_offset_min: i32, with_seconds: bool) -> String {
    let fmt = if with_seconds {
        "%Y-%m-%d %H:%M:%S"
    } else {
        "%Y-%m-%d %H:%M"
    };
    match (
        chrono::DateTime::from_timestamp(secs, 0),
        chrono::FixedOffset::east_opt(tz_offset_min * 60),
    ) {
        (Some(dt), Some(off)) => dt.with_timezone(&off).format(fmt).to_string(),
        _ => String::new(),
    }
}

/// The viewer's current UTC offset in minutes, for the "now"-stamped virtual rows.
pub fn local_tz_offset_min() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

/// Now, in Unix seconds — the reference `format_relative_time` measures against.
pub fn now_unix_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The widest string `format_relative_time` can produce — what the commit list
/// measures its date column from. Lives here, beside the function that has to
/// honour it, so the guarantee and its one consumer cannot drift apart; pinned
/// for **every** `i64` input by `relative_time_never_outgrows_the_column_sample`.
///
/// The two-part years-and-months form is the widest ordinary output. It happens
/// to tie with the widest degenerate one — `i64::MAX` seconds of age is
/// `292471208678 years ago`, also 22 characters — which is luck, not design, and
/// is why the test checks the extremes explicitly.
///
/// The bound is in **characters**, while the column it sizes is measured in
/// points. Those agree exactly in a monospace `[text] commit_meta`, which is the
/// default; under a proportional face they only approximate each other, and the
/// degenerate all-digits output above could exceed a sample of the same length
/// in letters. The commit list clips the group to the row, so the cost there is
/// a clipped character rather than a draw over the neighbouring column.
pub const RELATIVE_DATE_SAMPLE: &str = "4 years, 11 months ago";

/// The `s` that pluralizes `unit` for `n`, so a caller can interpolate the whole
/// phrase in one `format!`. Returning the finished `"3 days"` instead would make
/// every rung allocate twice — once for the phrase, once to embed it in
/// `"… ago"` — for a string built per visible row per frame; same reasoning as
/// `compact_count_into` in `main.rs`.
///
/// `diffstat` has its own copy over `usize`, deliberately: sharing this one means a
/// generic with three bounds to carry `if n == 1`, which is more to read than the
/// three lines it removes.
const fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// How long before `now` the instant `secs` was, both in Unix seconds —
/// **`git log --date=relative`'s algorithm**, ported from git's
/// `show_date_relative` (`date.c`).
///
/// Deliberately a port and not an equivalent-looking ladder of our own, because
/// the interesting part is the rounding, and it is not what you would write from
/// scratch. Each rung converts and *rounds* into the next unit before testing it
/// (`(diff + 30) / 60`), and every threshold overshoots its unit — 90 seconds,
/// 90 minutes, 36 hours, 14 days, 10 weeks — so `90s` reads `2 minutes ago` and
/// `36h` reads `2 days ago`. Between 1 and 5 years git switches to a two-part
/// `4 years, 11 months ago`; past 5 years it goes back to whole years, and, in
/// git's own words, centuries get ignored.
///
/// A timestamp **after** `now` is `in the future`, as git has it. Clock skew
/// between machines makes this ordinary rather than corrupt — a commit records
/// whatever its author's clock said — but reporting it is more use than rounding
/// it away, and it is what a git user already expects to see.
///
/// Arithmetic saturates so a corrupt timestamp near `i64`'s edge cannot overflow
/// a row draw. `i64::MIN` against a positive `now` reaches the last rung with a
/// twelve-digit year count, which [`RELATIVE_DATE_SAMPLE`] still covers.
pub fn format_relative_time(secs: i64, now: i64) -> String {
    if secs > now {
        return "in the future".to_string();
    }
    let diff = now.saturating_sub(secs);
    if diff < 90 {
        return format!("{diff} second{} ago", plural(diff));
    }
    let diff = diff.saturating_add(30) / 60; // minutes
    if diff < 90 {
        return format!("{diff} minute{} ago", plural(diff));
    }
    let diff = diff.saturating_add(30) / 60; // hours
    if diff < 36 {
        return format!("{diff} hour{} ago", plural(diff));
    }
    let diff = diff.saturating_add(12) / 24; // days, from here on
    if diff < 14 {
        return format!("{diff} day{} ago", plural(diff));
    }
    // Weeks for the past 10 weeks or so.
    if diff < 70 {
        let n = (diff + 3) / 7;
        return format!("{n} week{} ago", plural(n));
    }
    // Months for the past 12 months or so.
    if diff < 365 {
        let n = (diff + 15) / 30;
        return format!("{n} month{} ago", plural(n));
    }
    // Years and months for 5 years or so. The bound keeps this multiplication
    // far from overflowing.
    if diff < 1825 {
        let total_months = (diff * 12 * 2 + 365) / (365 * 2);
        let years = total_months / 12;
        let months = total_months % 12;
        if months == 0 {
            return format!("{years} year{} ago", plural(years));
        }
        return format!(
            "{years} year{}, {months} month{} ago",
            plural(years),
            plural(months)
        );
    }
    let n = diff.saturating_add(183) / 365;
    format!("{n} year{} ago", plural(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values `git log --date=relative` prints, at and around each rung.
    ///
    /// The rounding is the whole point of porting git's version rather than
    /// writing an equivalent-looking one, and it is what a from-scratch ladder
    /// gets wrong: every threshold overshoots its unit, so 90 seconds is already
    /// "2 minutes" and 36 hours is already "2 days".
    ///
    /// Every expectation below was taken from **real git output** (2.55.0), not
    /// from reading `date.c`: a scratch repo with one empty commit per age, read
    /// back with `%ar`. Re-derive them that way if this ever needs revisiting —
    /// the ladder is easy to transcribe subtly wrong and still look right.
    #[test]
    fn relative_time_matches_gits_ladder() {
        const MIN: i64 = 60;
        const HOUR: i64 = 60 * MIN;
        const DAY: i64 = 24 * HOUR;
        let now = 1_700_000_000;
        let ago = |age: i64| format_relative_time(now - age, now);

        assert_eq!(ago(0), "0 seconds ago");
        assert_eq!(ago(1), "1 second ago", "singular, not '1 seconds ago'");
        assert_eq!(ago(89), "89 seconds ago", "seconds run to 90, not to 60");
        // 90s rounds INTO minutes rather than reading "1 minute ago".
        assert_eq!(ago(90), "2 minutes ago");
        assert_eq!(ago(89 * MIN), "89 minutes ago", "minutes also run to 90");
        assert_eq!(ago(90 * MIN), "2 hours ago");
        assert_eq!(ago(35 * HOUR), "35 hours ago", "hours run to 36");
        assert_eq!(ago(36 * HOUR), "2 days ago");
        assert_eq!(ago(13 * DAY), "13 days ago", "days run to 14");
        assert_eq!(ago(14 * DAY), "2 weeks ago");
        assert_eq!(ago(69 * DAY), "10 weeks ago", "weeks run to 70 days");
        assert_eq!(ago(70 * DAY), "2 months ago");
        assert_eq!(ago(364 * DAY), "12 months ago", "months run to 365 days");
        // 1..5 years is git's two-part form, and drops the months when they're 0.
        assert_eq!(ago(365 * DAY), "1 year ago");
        assert_eq!(ago(400 * DAY), "1 year, 1 month ago");
        assert_eq!(ago(700 * DAY), "1 year, 11 months ago");
        assert_eq!(ago(730 * DAY), "2 years ago");
        // Past ~5 years git goes back to whole years.
        assert_eq!(ago(1825 * DAY), "5 years ago");
        assert_eq!(ago(3650 * DAY), "10 years ago");
    }

    /// A commit's timestamp is whatever was written into it. A clock ahead of
    /// ours is ordinary (git reports it rather than rounding it away); a garbage
    /// value near `i64`'s edge is not, but it must not panic a row draw either.
    #[test]
    fn relative_time_survives_a_future_or_absurd_timestamp() {
        let now = 1_700_000_000;
        assert_eq!(format_relative_time(now + 1, now), "in the future");
        assert_eq!(format_relative_time(i64::MAX, now), "in the future");
        assert_eq!(format_relative_time(now, now), "0 seconds ago");
        assert_eq!(
            format_relative_time(i64::MIN, now),
            "292471208678 years ago"
        );
    }

    /// The commit list gives this column a fixed width measured from
    /// `RELATIVE_DATE_SAMPLE`, so nothing the formatter can produce may be longer
    /// — for any `i64`, not merely for plausible ages. Two rungs are unbounded in
    /// principle: the two-part years-and-months form (widest ordinary output) and
    /// the final whole-years one (widest degenerate output, at `i64::MIN`).
    ///
    /// In characters, which is exactly the column's width only in a monospace
    /// font — see `RELATIVE_DATE_SAMPLE` for what that leaves open.
    #[test]
    fn relative_time_never_outgrows_the_column_sample() {
        let sample = RELATIVE_DATE_SAMPLE.chars().count();
        let now = 1_700_000_000;
        let check = |secs: i64| {
            let s = format_relative_time(secs, now);
            assert!(
                s.chars().count() <= sample,
                "{secs} → {s:?} ({} chars) is wider than the {sample}-char sample",
                s.chars().count()
            );
        };
        // Every minute of the first two days, then every day out to 300 years —
        // which crosses every rung, including the whole two-part range.
        for m in 0..2 * 24 * 60 {
            check(now - m * 60);
        }
        for d in 0..300 * 366 {
            check(now - d * 86_400);
        }
        // The extremes, where saturation decides the answer.
        check(i64::MIN);
        check(i64::MAX);
        check(0);
    }
}
