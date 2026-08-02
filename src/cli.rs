//! Pure parsing of gitkay's command line into a `Scope`, plus the CLI-facing
//! helpers around it (pathspec resolution, the window-title suffix, help/version
//! text). Knows nothing of git or egui: `classify` takes `is_rev`/`is_path`
//! predicates so it's testable without a repo.
//! Grammar: `gitkay [-C <dir>] [--all] [--combined] [--first-parent] [<rev>...] [-- <path>...]`.

/// The resolved command-line scope.
#[derive(Default, Clone)]
pub struct Scope {
    pub all: bool,
    pub revs: Vec<String>,
    pub paths: Vec<String>,
    pub reflog: bool,   // --reflog: show the ref's reflog instead of its history
    pub follow: bool,   // --follow: follow a single path across renames
    pub combined: bool, // --combined: open on the combined range row
    /// `--first-parent`: the mainline only, hiding the commits merged in from
    /// topic branches. Restricts the walk AND truncates each row's drawn parents.
    pub first_parent: bool,
}

/// Flags + raw positional tokens, before rev/path classification.
#[derive(Default)]
pub struct RawArgs {
    pub repo_dir: Option<String>,
    pub all: bool,
    pub reflog: bool,       // --reflog: show the reflog instead of history
    pub follow: bool,       // --follow: follow a single path across renames
    pub combined: bool,     // --combined: open on the combined range row
    pub first_parent: bool, // --first-parent: the mainline only
    pub help: bool,         // -h / --help: print usage and exit
    pub version: bool,      // -V / --version: print version and exit
    pub pre: Vec<String>,   // positional tokens before `--`
    pub post: Vec<String>,  // positional tokens after `--` (always paths)
}

/// The shape of a single `<rev>` token.
#[derive(Debug, PartialEq, Eq)]
pub enum RevTokenKind {
    Single(String),            // main, v1, HEAD~3, @{u}, …
    Exclude(String),           // ^X
    Range(String, String),     // A..B
    Symmetric(String, String), // A...B
}

/// First pass: pull out `-C`, `--all`, and the `--` split. No repo needed.
/// `args` must already exclude argv[0].
pub fn parse_flags(args: impl Iterator<Item = String>) -> Result<RawArgs, String> {
    let mut repo_dir = None;
    let mut all = false;
    let mut reflog = false;
    let mut follow = false;
    let mut combined = false;
    let mut first_parent = false;
    let mut pre = Vec::new();
    let mut post = Vec::new();
    let mut after_dashdash = false;
    let mut iter = args;
    while let Some(arg) = iter.next() {
        if after_dashdash {
            post.push(arg);
            continue;
        }
        if arg == "--" {
            after_dashdash = true;
        } else if arg == "--help" || arg == "-h" {
            // Short-circuit so help wins even alongside other (or invalid) args.
            return Ok(RawArgs {
                help: true,
                ..Default::default()
            });
        } else if arg == "--version" || arg == "-V" {
            return Ok(RawArgs {
                version: true,
                ..Default::default()
            });
        } else if arg == "--all" {
            all = true;
        } else if arg == "--reflog" {
            reflog = true;
        } else if arg == "--follow" {
            follow = true;
        } else if arg == "--combined" {
            combined = true;
        } else if arg == "--first-parent" {
            first_parent = true;
        } else if arg == "-C" {
            repo_dir = Some(iter.next().ok_or("-C requires a directory argument")?);
        } else if let Some(dir) = arg.strip_prefix("-C") {
            repo_dir = Some(dir.to_string());
        } else if arg.starts_with('-') && arg != "-" {
            return Err(format!("unknown flag: {arg}"));
        } else {
            pre.push(arg);
        }
    }
    Ok(RawArgs {
        repo_dir,
        all,
        reflog,
        follow,
        combined,
        first_parent,
        pre,
        post,
        ..Default::default()
    })
}

/// Split positional tokens into revs and paths. Revs come first; the first token
/// that is not a rev switches the rest to paths. A token that is both a rev and an
/// existing path is ambiguous; one that is neither is an error. `post` tokens are
/// paths verbatim.
pub fn classify(
    pre: &[String],
    post: &[String],
    is_rev: impl Fn(&str) -> bool,
    is_path: impl Fn(&str) -> bool,
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut revs = Vec::new();
    let mut paths = Vec::new();
    let mut in_paths = false;
    for tok in pre {
        if in_paths {
            if !is_path(tok) {
                return Err(format!("path does not exist: {tok}"));
            }
            paths.push(tok.clone());
            continue;
        }
        let rev = is_rev(tok);
        let path = is_path(tok);
        if rev && path {
            return Err(format!(
                "ambiguous argument '{tok}': both a revision and a path — use '--' to separate"
            ));
        } else if rev {
            revs.push(tok.clone());
        } else if path {
            in_paths = true;
            paths.push(tok.clone());
        } else {
            return Err(format!(
                "unknown revision or path not in the working tree: {tok}"
            ));
        }
    }
    for tok in post {
        paths.push(tok.clone());
    }
    Ok((revs, paths))
}

/// Validate the flag/positional combination. Pure (no repo, no process exit) so
/// the rules are unit-testable; the caller maps `Err` to a usage message + exit.
///
/// Takes the whole `Scope` rather than a handful of primitives: it needs `all`,
/// `reflog`, `follow`, `combined` AND the rev strings, and four bool parameters would
/// trip `clippy::fn_params_excessive_bools` (a struct may carry them —
/// `struct_excessive_bools` is allowed for exactly this crate's config/scope types).
/// It also removes the chance of being handed a "has a range" answer that disagrees
/// with the revs it was derived from: `combined_range` is consulted here, not passed in.
pub fn validate(scope: &Scope) -> Result<(), String> {
    if scope.follow && scope.reflog {
        return Err("--follow and --reflog cannot be combined".to_string());
    }
    if scope.first_parent && scope.reflog {
        // Reflog mode has its own loader and never builds a revwalk, so the flag
        // would be silently inert. Name it, like `combined_conflict` does.
        return Err("--first-parent and --reflog cannot be combined".to_string());
    }
    if scope.follow && scope.paths.len() != 1 {
        return Err("--follow requires exactly one path".to_string());
    }
    if scope.reflog && (!scope.paths.is_empty() || scope.revs.len() > 1) {
        return Err("--reflog takes at most one ref and no paths".to_string());
    }
    if scope.combined {
        // Name the conflicting flag rather than silently ignoring --combined.
        if let Some(flag) = combined_conflict(scope) {
            return Err(format!("--combined and {flag} cannot be combined"));
        }
        if combined_range(scope).is_none() {
            return Err(
                "--combined requires a single <a>..<b> or <a>...<b> revision range".to_string(),
            );
        }
    }
    Ok(())
}

/// Classify a `<rev>` token's shape (`...` is checked before `..`).
/// Open-ended ranges default the missing endpoint to HEAD, like git
/// (`main..` ⇒ `main..HEAD`, `..main` ⇒ `HEAD..main`). A fully empty `..` is
/// left as-is so it can still classify as the parent-directory path.
pub fn rev_token_kind(tok: &str) -> RevTokenKind {
    let fill_head = |a: &str, b: &str| -> (String, String) {
        if a.is_empty() && b.is_empty() {
            (String::new(), String::new())
        } else {
            let fill = |s: &str| {
                if s.is_empty() {
                    "HEAD".to_string()
                } else {
                    s.to_string()
                }
            };
            (fill(a), fill(b))
        }
    };
    if let Some(rest) = tok.strip_prefix('^') {
        return RevTokenKind::Exclude(rest.to_string());
    }
    if let Some((a, b)) = tok.split_once("...") {
        let (a, b) = fill_head(a, b);
        return RevTokenKind::Symmetric(a, b);
    }
    if let Some((a, b)) = tok.split_once("..") {
        let (a, b) = fill_head(a, b);
        return RevTokenKind::Range(a, b);
    }
    RevTokenKind::Single(tok.to_string())
}

/// The two endpoints of a revision range token, before any repo resolution.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct RangeTokens {
    /// The whole token exactly as typed — what the combined row is labelled with.
    /// Carried here rather than fished back out of `revs` by the caller, so "which
    /// token is the range" is decided once, where it is matched.
    pub token: String,
    pub base: String,
    pub head: String,
    /// `A...B` — the caller resolves `base` through a merge base with `head`.
    pub symmetric: bool,
}

/// The endpoints of a LONE range token, or `None` when the scope is not a single range.
///
/// `Some` requires exactly one rev token that `rev_token_kind` calls `Range` or
/// `Symmetric`, with both endpoints non-empty. That last clause is not redundant:
/// `rev_token_kind("..")` yields `Range("", "")` by design, so a bare `..` can still
/// classify as the parent-directory path. It normally never reaches `revs` for that
/// reason, but this answers for itself rather than depending on `classify` having
/// gotten there first.
pub fn range_tokens(revs: &[String]) -> Option<RangeTokens> {
    let [tok] = revs else { return None };
    let (base, head, symmetric) = match rev_token_kind(tok) {
        RevTokenKind::Range(a, b) => (a, b, false),
        RevTokenKind::Symmetric(a, b) => (a, b, true),
        RevTokenKind::Single(_) | RevTokenKind::Exclude(_) => return None,
    };
    if base.is_empty() || head.is_empty() {
        return None;
    }
    Some(RangeTokens {
        token: tok.clone(),
        base,
        head,
        symmetric,
    })
}

/// Which flag, if any, denies this scope a combined range row — `None` when nothing
/// stands in the way. The single list `combined_range` filters on and `validate` names
/// in its error, so the usage message can never disagree with what actually happens.
///
/// `--all` overrides the walk entirely and the revs are never consulted. `--reflog` is
/// not a range. `--follow` retraces its pathspec per commit (`CommitInfo::follow_path`),
/// so a whole-range diff has no single path to use, and diffing the post-rename name
/// alone would silently drop everything before the rename — the opposite of what
/// `--follow` was asked for.
fn combined_conflict(scope: &Scope) -> Option<&'static str> {
    [
        (scope.all, "--all"),
        (scope.reflog, "--reflog"),
        (scope.follow, "--follow"),
    ]
    .into_iter()
    .find_map(|(hit, flag)| hit.then_some(flag))
}

/// Does this scope get a combined range row, and over which endpoints?
///
/// The single predicate the whole app asks — `validate` for the usage error, and
/// `load_commits` for whether to build the row — so the flag can never be accepted
/// for a scope that then produces no row. Which flags stand in the way is
/// `combined_conflict`'s answer, not a second list.
pub fn combined_range(scope: &Scope) -> Option<RangeTokens> {
    if combined_conflict(scope).is_some() {
        return None;
    }
    range_tokens(&scope.revs)
}

/// Lexically normalize a `/`-separated relative path: drop `.` and empty segments,
/// resolve `..` against a preceding normal segment. Never touches the filesystem, so
/// it works on pathspecs for files that no longer exist.
fn normalize_rel(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(&s) if s != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// Translate a user-supplied path token into a repo-root-relative pathspec. `prefix`
/// is the run directory's location inside the repo (e.g. "src" when started in
/// `<repo>/src`). Relative tokens are joined onto `prefix`; absolute tokens are made
/// relative to `workdir`. A token that resolves to the repo root (e.g. `.` at the
/// top) yields "" — the caller drops those so they impose no restriction.
pub fn token_to_pathspec(token: &str, prefix: &str, workdir: &std::path::Path) -> String {
    let p = std::path::Path::new(token);
    if p.is_absolute() {
        p.strip_prefix(workdir).map_or_else(
            |_| token.to_string(), // outside the repo — will simply match nothing
            |rel| normalize_rel(&rel.to_string_lossy()),
        )
    } else {
        normalize_rel(&format!("{prefix}/{token}"))
    }
}

/// The parenthetical scope shown in the window title, e.g. `--all`, `main`,
/// `a..b -- src`. Empty when the default (current branch, no path filter) is active.
pub fn scope_title_suffix(scope: &Scope) -> String {
    if scope.reflog {
        return scope
            .revs
            .first()
            .map_or_else(|| "reflog".to_string(), |r| format!("reflog {r}"));
    }
    let mut head: Vec<String> = Vec::new();
    if scope.combined {
        head.push("combined".to_string());
    }
    if scope.all {
        head.push("--all".to_string());
    }
    if scope.first_parent {
        head.push("--first-parent".to_string());
    }
    head.extend(scope.revs.iter().cloned());
    let mut s = head.join(" ");
    if !scope.paths.is_empty() {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(if scope.follow { "follow " } else { "-- " });
        s.push_str(&scope.paths.join(" "));
    }
    s
}

pub fn print_help() {
    print!(
        r"gitkay — a git history viewer

USAGE:
    gitkay [-C <dir>] [--all] [--combined] [--first-parent] [<rev>...] [-- <path>...]
    gitkay [-C <dir>] --reflog [<ref>]
    gitkay [-C <dir>] --follow [<rev>...] <path>

OPTIONS:
    -C <dir>        Run as if started in <dir>
    --all           Show all refs (branches, remotes, tags), not just the current branch
    --combined      Open on the combined diff of a single <a>..<b> range
                    (the row is always present for a range; this selects it)
    --first-parent  Follow only the first parent of each merge: show the mainline,
                    hiding the commits merged in from topic branches
    --reflog        Show <ref>'s reflog (default HEAD) instead of its history
    --follow        Follow a single <path> across renames (exactly one path)
    -h, --help      Print this help and exit
    -V, --version   Print version and exit

ARGS:
    <rev>...        Revisions to show: <rev>, <a>..<b>, <a>...<b>, ^<rev>
                    (default: the current branch)
    <path>...       Limit history and diffs to commits touching these paths
                    (relative to the current directory, like git)
"
    );
}

pub fn print_version() {
    println!("gitkay {}", env!("CARGO_PKG_VERSION"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn normalize_rel_resolves_dot_and_dotdot() {
        assert_eq!(normalize_rel("src/./foo"), "src/foo");
        assert_eq!(normalize_rel("src/../foo"), "foo");
        assert_eq!(normalize_rel("a//b"), "a/b");
        assert_eq!(normalize_rel("src/.."), "");
        assert_eq!(normalize_rel("./."), "");
        assert_eq!(normalize_rel("/foo"), "foo"); // leading slash from an empty prefix
    }

    #[test]
    fn token_to_pathspec_anchors_relative_to_prefix() {
        let wd = std::path::Path::new("/repo"); // only consulted for absolute tokens
        // In <repo>/src, `.` is the whole src dir.
        assert_eq!(token_to_pathspec(".", "src", wd), "src");
        assert_eq!(token_to_pathspec("foo.rs", "src", wd), "src/foo.rs");
        assert_eq!(token_to_pathspec("../README", "src", wd), "README");
        // At the repo root `.` is the whole repo → "" (dropped by the caller).
        assert_eq!(token_to_pathspec(".", "", wd), "");
        assert_eq!(token_to_pathspec("a/b", "", wd), "a/b");
        // Absolute token under the worktree → made repo-root-relative.
        assert_eq!(
            token_to_pathspec("/repo/src/foo.rs", "src", wd),
            "src/foo.rs"
        );
    }

    #[test]
    fn scope_title_suffix_formats() {
        let s = |all: bool, revs: &[&str], paths: &[&str]| Scope {
            all,
            revs: revs.iter().map(std::string::ToString::to_string).collect(),
            paths: paths.iter().map(std::string::ToString::to_string).collect(),
            ..Default::default()
        };
        assert_eq!(scope_title_suffix(&s(false, &[], &[])), "");
        assert_eq!(scope_title_suffix(&s(true, &[], &[])), "--all");
        assert_eq!(scope_title_suffix(&s(false, &["main"], &[])), "main");
        assert_eq!(scope_title_suffix(&s(false, &[], &["src"])), "-- src");
        assert_eq!(
            scope_title_suffix(&s(false, &["a..b"], &["src", "x"])),
            "a..b -- src x"
        );
    }

    #[test]
    fn parse_flags_extracts_c_all_and_dashdash() {
        let r = parse_flags(v(&["-C", "/repo", "--all", "main", "--", "a.rs", "b.rs"]).into_iter())
            .unwrap();
        assert_eq!(r.repo_dir.as_deref(), Some("/repo"));
        assert!(r.all);
        assert_eq!(r.pre, v(&["main"]));
        assert_eq!(r.post, v(&["a.rs", "b.rs"]));
    }

    #[test]
    fn parse_flags_c_attached_and_unknown_flag() {
        let r = parse_flags(v(&["-C/repo"]).into_iter()).unwrap();
        assert_eq!(r.repo_dir.as_deref(), Some("/repo"));
        assert!(parse_flags(v(&["--bogus"]).into_iter()).is_err());
        assert!(parse_flags(v(&["-C"]).into_iter()).is_err()); // missing dir
    }

    #[test]
    fn classify_revs_then_paths() {
        let is_rev = |t: &str| t == "main" || t == "dev";
        let is_path = |t: &str| t == "src" || t == "x.rs";
        let (revs, paths) = classify(&v(&["main", "src", "x.rs"]), &[], is_rev, is_path).unwrap();
        assert_eq!(revs, v(&["main"]));
        assert_eq!(paths, v(&["src", "x.rs"]));
    }

    #[test]
    fn classify_ambiguous_and_unknown_errors() {
        let is_rev = |t: &str| t == "main";
        let is_path = |t: &str| t == "main" || t == "x.rs";
        assert!(classify(&v(&["main"]), &[], is_rev, is_path).is_err()); // both → ambiguous
        assert!(classify(&v(&["nope"]), &[], |_| false, |_| false).is_err()); // neither
    }

    #[test]
    fn classify_post_dashdash_are_paths_verbatim() {
        // even a deleted path (is_path=false) is accepted after `--`
        let (revs, paths) = classify(&[], &v(&["gone.rs"]), |_| false, |_| false).unwrap();
        assert!(revs.is_empty());
        assert_eq!(paths, v(&["gone.rs"]));
    }

    #[test]
    fn parse_flags_help_and_version() {
        assert!(parse_flags(v(&["--help"]).into_iter()).unwrap().help);
        assert!(parse_flags(v(&["-h"]).into_iter()).unwrap().help);
        assert!(parse_flags(v(&["--version"]).into_iter()).unwrap().version);
        assert!(parse_flags(v(&["-V"]).into_iter()).unwrap().version);
        // help wins even alongside an arg that would otherwise error
        assert!(
            parse_flags(v(&["--help", "--bogus"]).into_iter())
                .unwrap()
                .help
        );
        // after `--`, `--help` is a path, not the flag
        let r = parse_flags(v(&["--", "--help"]).into_iter()).unwrap();
        assert!(!r.help);
        assert_eq!(r.post, v(&["--help"]));
    }

    #[test]
    fn parse_flags_reflog() {
        let r = parse_flags(v(&["--reflog"]).into_iter()).unwrap();
        assert!(r.reflog);
        assert!(r.pre.is_empty());
        // A ref after --reflog stays a positional, classified as a rev downstream.
        let r = parse_flags(v(&["--reflog", "main"]).into_iter()).unwrap();
        assert!(r.reflog);
        assert_eq!(r.pre, v(&["main"]));
        // Not set without the flag.
        assert!(!parse_flags(v(&["main"]).into_iter()).unwrap().reflog);
    }

    #[test]
    fn parse_flags_follow() {
        let r = parse_flags(v(&["--follow", "src/foo.rs"]).into_iter()).unwrap();
        assert!(r.follow);
        assert_eq!(r.pre, v(&["src/foo.rs"]));
        assert!(!parse_flags(v(&["src/foo.rs"]).into_iter()).unwrap().follow);
    }

    fn range_scope(revs: &[&str]) -> Scope {
        Scope {
            revs: revs.iter().map(std::string::ToString::to_string).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn range_tokens_accepts_a_lone_range() {
        let t = range_tokens(&v(&["a..b"])).unwrap();
        assert_eq!(
            (t.base.as_str(), t.head.as_str(), t.symmetric),
            ("a", "b", false)
        );

        let t = range_tokens(&v(&["a...b"])).unwrap();
        assert_eq!(
            (t.base.as_str(), t.head.as_str(), t.symmetric),
            ("a", "b", true)
        );

        // rev_token_kind fills open-ended forms with HEAD, like git.
        let t = range_tokens(&v(&["main.."])).unwrap();
        assert_eq!((t.base.as_str(), t.head.as_str()), ("main", "HEAD"));

        let t = range_tokens(&v(&["..main"])).unwrap();
        assert_eq!((t.base.as_str(), t.head.as_str()), ("HEAD", "main"));
    }

    #[test]
    fn range_tokens_rejects_everything_that_is_not_a_lone_range() {
        assert!(range_tokens(&[]).is_none());
        assert!(range_tokens(&v(&["main"])).is_none());
        assert!(range_tokens(&v(&["^main"])).is_none());
        assert!(range_tokens(&v(&["a..b", "c..d"])).is_none());
        assert!(range_tokens(&v(&["a..b", "^side"])).is_none());
    }

    /// `rev_token_kind("..")` yields `Range("", "")` on purpose, so a bare `..`
    /// can still classify as the parent directory. `range_tokens` must answer for
    /// itself rather than trusting `classify` to have caught it first.
    #[test]
    fn range_tokens_rejects_a_bare_dotdot() {
        assert!(range_tokens(&v(&[".."])).is_none());
    }

    #[test]
    fn combined_range_is_offered_only_for_a_plain_lone_range() {
        assert!(combined_range(&range_scope(&["a..b"])).is_some());
        assert!(combined_range(&range_scope(&["main"])).is_none());

        let with_all = Scope {
            all: true,
            ..range_scope(&["a..b"])
        };
        assert!(
            combined_range(&with_all).is_none(),
            "--all overrides the walk"
        );

        let with_reflog = Scope {
            reflog: true,
            ..range_scope(&["a..b"])
        };
        assert!(combined_range(&with_reflog).is_none());

        let with_follow = Scope {
            follow: true,
            paths: v(&["f.rs"]),
            ..range_scope(&["a..b"])
        };
        assert!(
            combined_range(&with_follow).is_none(),
            "--follow has no single path for a range"
        );
    }

    #[test]
    fn validate_combined_requires_a_lone_range() {
        let ok = Scope {
            combined: true,
            ..range_scope(&["a..b"])
        };
        assert!(validate(&ok).is_ok());

        let with_paths = Scope {
            paths: v(&["src"]),
            ..ok.clone()
        };
        assert!(
            validate(&with_paths).is_ok(),
            "a pathspec is fine alongside --combined"
        );

        let bare_rev = Scope {
            combined: true,
            ..range_scope(&["main"])
        };
        assert!(validate(&bare_rev).is_err());

        let no_revs = Scope {
            combined: true,
            ..Default::default()
        };
        assert!(validate(&no_revs).is_err());

        let with_all = Scope {
            all: true,
            ..ok.clone()
        };
        assert!(validate(&with_all).is_err());

        let with_reflog = Scope {
            reflog: true,
            revs: v(&["a..b"]),
            combined: true,
            ..Default::default()
        };
        assert!(validate(&with_reflog).is_err());

        let with_follow = Scope {
            follow: true,
            paths: v(&["f.rs"]),
            ..ok
        };
        assert!(validate(&with_follow).is_err());
    }

    #[test]
    fn validate_still_enforces_the_existing_rules() {
        assert!(
            validate(&Scope {
                revs: v(&["main"]),
                paths: v(&["a", "b"]),
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate(&Scope {
                follow: true,
                paths: v(&["a"]),
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate(&Scope {
                follow: true,
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate(&Scope {
                follow: true,
                paths: v(&["a", "b"]),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate(&Scope {
                follow: true,
                reflog: true,
                paths: v(&["a"]),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate(&Scope {
                reflog: true,
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate(&Scope {
                reflog: true,
                revs: v(&["main"]),
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate(&Scope {
                reflog: true,
                revs: v(&["a", "b"]),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate(&Scope {
                reflog: true,
                paths: v(&["a"]),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn parse_flags_combined() {
        let r = parse_flags(v(&["--combined", "a..b"]).into_iter()).unwrap();
        assert!(r.combined);
        assert_eq!(r.pre, v(&["a..b"]));
        assert!(!parse_flags(v(&["a..b"]).into_iter()).unwrap().combined);
    }

    #[test]
    fn scope_title_suffix_marks_combined() {
        let s = Scope {
            combined: true,
            ..range_scope(&["a..b"])
        };
        assert_eq!(scope_title_suffix(&s), "combined a..b");
    }

    #[test]
    fn parse_flags_first_parent() {
        let r = parse_flags(v(&["--first-parent", "main"]).into_iter()).unwrap();
        assert!(r.first_parent);
        assert_eq!(r.pre, v(&["main"]));
        assert!(!parse_flags(v(&["main"]).into_iter()).unwrap().first_parent);
    }

    #[test]
    fn scope_title_suffix_marks_first_parent() {
        let s = Scope {
            all: true,
            first_parent: true,
            revs: v(&["main"]),
            ..Default::default()
        };
        assert_eq!(scope_title_suffix(&s), "--all --first-parent main");
    }

    /// Reflog mode never calls `history_revwalk`, so the flag would be silently
    /// inert. Name it rather than ignore it — the `combined_conflict` precedent.
    #[test]
    fn validate_rejects_first_parent_with_reflog() {
        assert!(
            validate(&Scope {
                first_parent: true,
                reflog: true,
                ..Default::default()
            })
            .is_err()
        );
    }

    /// Everything else composes: the walk simplification is orthogonal to the
    /// range row, the path filter and rename tracing.
    #[test]
    fn validate_accepts_first_parent_with_everything_else() {
        assert!(
            validate(&Scope {
                first_parent: true,
                all: true,
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate(&Scope {
                first_parent: true,
                follow: true,
                paths: v(&["f.rs"]),
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate(&Scope {
                first_parent: true,
                combined: true,
                revs: v(&["a..b"]),
                paths: v(&["src"]),
                ..Default::default()
            })
            .is_ok()
        );
    }

    #[test]
    fn rev_token_kind_shapes() {
        assert_eq!(rev_token_kind("main"), RevTokenKind::Single("main".into()));
        assert_eq!(
            rev_token_kind("^main"),
            RevTokenKind::Exclude("main".into())
        );
        assert_eq!(
            rev_token_kind("a..b"),
            RevTokenKind::Range("a".into(), "b".into())
        );
        assert_eq!(
            rev_token_kind("a...b"),
            RevTokenKind::Symmetric("a".into(), "b".into())
        );
    }

    #[test]
    fn rev_token_kind_open_ranges_default_to_head() {
        // git accepts `main..` / `..main` as `main..HEAD` / `HEAD..main`.
        assert_eq!(
            rev_token_kind("main.."),
            RevTokenKind::Range("main".into(), "HEAD".into())
        );
        assert_eq!(
            rev_token_kind("..main"),
            RevTokenKind::Range("HEAD".into(), "main".into())
        );
        assert_eq!(
            rev_token_kind("main..."),
            RevTokenKind::Symmetric("main".into(), "HEAD".into())
        );
        // A bare `..` stays empty-endpointed so classify() can still treat the
        // token as the parent-directory path.
        assert_eq!(
            rev_token_kind(".."),
            RevTokenKind::Range(String::new(), String::new())
        );
    }
}
