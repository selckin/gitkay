//! Pure parsing of gitkay's command line into a `Scope`. Knows nothing of git or
//! egui: `classify` takes `is_rev`/`is_path` predicates so it's testable without a
//! repo. Grammar: `gitkay [-C <dir>] [--all] [<rev>...] [-- <path>...]`.

/// The resolved command-line scope.
#[derive(Default)]
pub struct Scope {
    pub all: bool,
    pub revs: Vec<String>,
    pub paths: Vec<String>,
    pub reflog: bool, // --reflog: show the ref's reflog instead of its history
    pub follow: bool, // --follow: follow a single path across renames
}

/// Flags + raw positional tokens, before rev/path classification.
#[derive(Default)]
pub struct RawArgs {
    pub repo_dir: Option<String>,
    pub all: bool,
    pub reflog: bool,      // --reflog: show the reflog instead of history
    pub follow: bool,      // --follow: follow a single path across renames
    pub help: bool,        // -h / --help: print usage and exit
    pub version: bool,     // -V / --version: print version and exit
    pub pre: Vec<String>,  // positional tokens before `--`
    pub post: Vec<String>, // positional tokens after `--` (always paths)
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
            return Ok(RawArgs { help: true, ..Default::default() });
        } else if arg == "--version" || arg == "-V" {
            return Ok(RawArgs { version: true, ..Default::default() });
        } else if arg == "--all" {
            all = true;
        } else if arg == "--reflog" {
            reflog = true;
        } else if arg == "--follow" {
            follow = true;
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
    Ok(RawArgs { repo_dir, all, reflog, follow, pre, post, ..Default::default() })
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
            return Err(format!("unknown revision or path not in the working tree: {tok}"));
        }
    }
    for tok in post {
        paths.push(tok.clone());
    }
    Ok((revs, paths))
}

/// Validate the flag/positional combination. Pure (no repo, no process exit) so
/// the rules are unit-testable; the caller maps `Err` to a usage message + exit.
/// `n_revs`/`n_paths` are the counts after classification.
pub fn validate(reflog: bool, follow: bool, n_revs: usize, n_paths: usize) -> Result<(), String> {
    if follow && reflog {
        return Err("--follow and --reflog cannot be combined".to_string());
    }
    if follow && n_paths != 1 {
        return Err("--follow requires exactly one path".to_string());
    }
    if reflog && (n_paths > 0 || n_revs > 1) {
        return Err("--reflog takes at most one ref and no paths".to_string());
    }
    Ok(())
}

/// Classify a `<rev>` token's shape (`...` is checked before `..`).
pub fn rev_token_kind(tok: &str) -> RevTokenKind {
    if let Some(rest) = tok.strip_prefix('^') {
        RevTokenKind::Exclude(rest.to_string())
    } else if let Some((a, b)) = tok.split_once("...") {
        RevTokenKind::Symmetric(a.to_string(), b.to_string())
    } else if let Some((a, b)) = tok.split_once("..") {
        RevTokenKind::Range(a.to_string(), b.to_string())
    } else {
        RevTokenKind::Single(tok.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
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
        assert!(parse_flags(v(&["--help", "--bogus"]).into_iter()).unwrap().help);
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

    #[test]
    fn validate_flag_combinations() {
        // validate(reflog, follow, n_revs, n_paths)
        assert!(validate(false, false, 1, 2).is_ok()); // ordinary scope
        assert!(validate(false, true, 0, 1).is_ok()); // --follow one path
        assert!(validate(false, true, 0, 0).is_err()); // --follow needs a path
        assert!(validate(false, true, 0, 2).is_err()); // --follow rejects two paths
        assert!(validate(true, true, 0, 1).is_err()); // can't combine
        assert!(validate(true, false, 0, 0).is_ok()); // --reflog HEAD
        assert!(validate(true, false, 1, 0).is_ok()); // --reflog <ref>
        assert!(validate(true, false, 2, 0).is_err()); // --reflog rejects two refs
        assert!(validate(true, false, 1, 1).is_err()); // --reflog rejects a path
    }

    #[test]
    fn rev_token_kind_shapes() {
        assert_eq!(rev_token_kind("main"), RevTokenKind::Single("main".into()));
        assert_eq!(rev_token_kind("^main"), RevTokenKind::Exclude("main".into()));
        assert_eq!(rev_token_kind("a..b"), RevTokenKind::Range("a".into(), "b".into()));
        assert_eq!(
            rev_token_kind("a...b"),
            RevTokenKind::Symmetric("a".into(), "b".into())
        );
    }
}
