//! Which files are test code, and the Maven main/test directories the grouped file
//! sidebar merges its headers over.
//!
//! Both are decided from the repo-relative PATH alone — no config, no attributes, no
//! file contents — so they are pure, cheap, and identical for every diff that names
//! the same file. That also bounds them: a test the path does not reveal (a Rust
//! `#[cfg(test)] mod tests` inside a source file) reads as production code.
//!
//! **A Maven/Gradle source set is authoritative where there is one.** Under
//! `src/main/` a directory called `test` is a package of a test-SUPPORT library
//! (`base-test/src/main/java/…/test/`), which ships as production code of its module,
//! so the generic directory rules stand down there. The file-name rules do not: an
//! Angular `foo.spec.ts` is a test wherever the frontend happens to live.

/// Where a path sits in a Maven/Gradle layout: `<module>src/<set>/<rest>`.
#[derive(Debug, PartialEq, Eq)]
struct SourceSet<'a> {
    /// Everything before `src/` — empty or `/`-terminated.
    module: &'a str,
    set: &'a str,
    /// Everything after `src/<set>/`.
    rest: &'a str,
}

/// The directories a JVM source set holds its files under.
const JVM_SOURCE_DIRS: &[&str] = &["java", "kotlin", "groovy", "scala", "resources"];

/// A source set that holds tests. `test` and Maven's `it` by name alone; a Gradle
/// custom set (`testFixtures`, `integrationTest`, `functionalTest`, …) by its name
/// AND a JVM source directory under it (`rest` is what follows `src/<set>/`). The
/// name alone is also an ordinary app folder — `src/testimonials/`, an A/B
/// `src/abTest/` — and a source set overrides every other rule, so a false one would
/// mark a whole tree.
fn is_test_set(set: &str, rest: &str) -> bool {
    if set == "test" || set == "it" {
        return true;
    }
    let named = set
        .strip_prefix("test")
        .is_some_and(|tail| tail.starts_with(|c: char| c.is_ascii_uppercase()))
        || set
            .strip_suffix("Test")
            .is_some_and(|head| !head.is_empty());
    named
        && rest
            .split_once('/')
            .is_some_and(|(lang, _)| JVM_SOURCE_DIRS.contains(&lang))
}

/// The first `src/<set>/` along `path` whose `<set>` is `main` or a test set. Only
/// `/`-terminated segments are looked at, so a file path and its directory give the
/// same answer — `src/main.rs` has no set. The FIRST, because a test resource can
/// itself be a Maven project (`src/test/resources/fixture/src/main/java/…`), and that
/// is still a test resource.
fn source_set(path: &str) -> Option<SourceSet<'_>> {
    let mut pos = 0;
    while let Some(len) = path[pos..].find('/') {
        let next = pos + len + 1;
        if &path[pos..next] == "src/"
            && let Some(set_len) = path[next..].find('/')
        {
            let set = &path[next..next + set_len];
            let rest = &path[next + set_len + 1..];
            if set == "main" || is_test_set(set, rest) {
                return Some(SourceSet {
                    module: &path[..pos],
                    set,
                    rest,
                });
            }
        }
        pos = next;
    }
    None
}

/// A directory that holds tests in the conventions that use one: `test`/`tests`
/// (any case — Swift's `Tests/`), Jest's `__tests__`, Go's `testdata`, and a .NET
/// test project (`Foo.Tests`, `Foo.UnitTests`).
fn is_test_dir(seg: &str) -> bool {
    seg.eq_ignore_ascii_case("test")
        || seg.eq_ignore_ascii_case("tests")
        || seg == "__tests__"
        || seg == "testdata"
        || seg
            .rsplit_once('.')
            .is_some_and(|(_, last)| last == "Test" || last.ends_with("Tests"))
}

/// A file named as a test, in the convention of ITS OWN language — keyed on the
/// extension, because the same spellings mean nothing elsewhere: `api_spec.yaml`,
/// `test_plan.md` and an RPM `gitkay.spec.in` are not tests.
fn is_test_file_name(base: &str) -> bool {
    let Some((stem, ext)) = base.rsplit_once('.') else {
        return false;
    };
    match ext {
        // Jest/Vitest/Jasmine/Angular: `foo.test.ts`, `foo.component.spec.ts`.
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => stem
            .rsplit_once('.')
            .is_some_and(|(_, tag)| tag == "test" || tag == "spec"),
        "go" | "ex" | "exs" => stem.ends_with("_test"),
        "py" => {
            stem.starts_with("test_")
                || stem.ends_with("_test")
                || stem == "tests"
                || stem == "conftest"
        }
        "rb" => stem.ends_with("_spec") || stem.ends_with("_test"),
        "rs" => stem == "tests" || stem.starts_with("test_"),
        // googletest's `foo_test.cc`, Unity's `test_foo.c`.
        "c" | "cc" | "cpp" | "cxx" => stem.ends_with("_test") || stem.starts_with("test_"),
        _ => false,
    }
}

/// Whether the file at repo-relative `path` is test code — see the module docs for
/// the rules and why a source set overrides the directory ones.
pub fn is_test_path(path: &str) -> bool {
    let (dir, base) = crate::split_dir(path);
    if is_test_file_name(base) {
        return true;
    }
    let any_test_dir = |dirs: &str| dirs.split('/').any(is_test_dir);
    // Inside a source set the set decides, and only the module prefix above it is
    // still read for test directories: `tests/fixture/src/main/…` is a fixture.
    source_set(path).map_or_else(
        || any_test_dir(dir),
        |ss| ss.set != "main" || any_test_dir(ss.module),
    )
}

/// A Maven `<module>src/{main,test}/<lang>/<pkg>` directory, split into the parts the
/// grouped sidebar builds its header from (`dir_header` in `main.rs`). `dir` is
/// `/`-terminated (a file's directory); `module` and `pkg` are each empty or
/// `/`-terminated. `None` for anything else.
///
/// Main and test give the SAME parts — which set a directory came from is exactly
/// what the header forgets, so a class and its test land under one. Only `main` and
/// `test`, deliberately: they are the one mirror a build tool guarantees, and
/// `testFixtures` or `integrationTest` still classify as tests under their own header.
#[derive(Debug, PartialEq, Eq)]
pub struct MavenDir<'a> {
    pub module: &'a str,
    pub lang: &'a str,
    pub pkg: &'a str,
}

pub fn maven_dir(dir: &str) -> Option<MavenDir<'_>> {
    let ss = source_set(dir).filter(|ss| ss.set == "main" || ss.set == "test")?;
    let (lang, pkg) = ss.rest.split_once('/')?;
    Some(MavenDir {
        module: ss.module,
        lang,
        pkg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maven_source_sets_decide_under_src() {
        for path in [
            "core/src/test/java/be/acto/MatcherTest.java",
            "core/src/test/java/be/acto/Helper.java", // no Test suffix, still a test
            "core/src/test/resources/fixture.json",
            "src/test/java/Foo.java", // root module
            "core/src/it/java/FooIT.java",
            "core/src/testFixtures/java/Fake.java",
            "core/src/integrationTest/kotlin/Foo.kt",
            "core/src/functionalTest/resources/cv.json",
        ] {
            assert!(is_test_path(path), "{path}");
        }
        for path in [
            "core/src/main/java/be/acto/Matcher.java",
            "core/src/main/resources/logback.xml",
            "src/main/java/Foo.java",
            // A test-support library's package is production code of its module.
            "xam/base-test/src/main/java/com/actonomy/xam/base/test/Base.java",
            // Named like a Gradle test set, but no JVM source directory under it.
            "web/src/testimonials/Card.tsx",
            "web/src/testnet/client.ts",
            "web/src/abTest/Variant.tsx",
        ] {
            assert!(!is_test_path(path), "{path}");
        }
    }

    #[test]
    fn a_test_resource_that_is_itself_a_maven_project_stays_a_test() {
        assert!(is_test_path(
            "plugin/src/test/resources/project/src/main/java/Foo.java"
        ));
        assert!(is_test_path("tests/fixture/src/main/java/Foo.java"));
    }

    #[test]
    fn file_name_rules_apply_everywhere_including_src_main() {
        for path in [
            "web/src/app/foo.component.spec.ts",
            "web/src/app/foo.test.tsx",
            "core/src/main/webapp/app/foo.spec.ts",
            "pkg/match/score_test.go",
            "native/parse_test.cc",
            "lib/foo_spec.rb",
            "app/test_models.py",
            "app/tests.py",
            "conftest.py",
            "src/test_repo.rs",
        ] {
            assert!(is_test_path(path), "{path}");
        }
        for path in [
            "src/main.rs",
            "latest.txt",
            "contest.py",
            "spec.md",
            "src/testcode.rs",
            // The spellings only count in the language whose convention they are.
            "docs/api_spec.yaml",
            "openapi_spec.json",
            "packaging/gitkay.spec.in",
            "docs/test_plan.md",
            "Makefile",
        ] {
            assert!(!is_test_path(path), "{path}");
        }
    }

    #[test]
    fn test_directories_outside_a_source_set() {
        for path in [
            "tests/cli.rs",
            "web/src/components/__tests__/helpers.ts",
            "pkg/parse/testdata/cv.pdf",
            "Tests/AppTests/Foo.swift",
            "Acme.Match.Tests/ScorerFacts.cs",
            "Acme.Match.UnitTests/ScorerFacts.cs",
        ] {
            assert!(is_test_path(path), "{path}");
        }
        for path in [
            "xmp-test-config/app.conf", // a segment CONTAINING test is not one
            "attest/x.rs",
            "Acme.Match/Scorer.cs",
            "spec/openapi.yaml", // `spec/` is too often an API spec to count
        ] {
            assert!(!is_test_path(path), "{path}");
        }
    }

    #[test]
    fn source_set_needs_whole_segments() {
        assert_eq!(source_set("src/main.rs"), None);
        assert_eq!(source_set("mysrc/main/java/X.java"), None);
        assert_eq!(source_set("src/components/X.tsx"), None);
        assert_eq!(
            source_set("a/b/src/main/java/X.java"),
            Some(SourceSet {
                module: "a/b/",
                set: "main",
                rest: "java/X.java"
            })
        );
    }

    #[test]
    fn main_and_test_are_the_same_maven_dir() {
        let main = maven_dir("core/src/main/java/be/acto/match/");
        assert_eq!(main, maven_dir("core/src/test/java/be/acto/match/"));
        assert_eq!(
            main,
            Some(MavenDir {
                module: "core/",
                lang: "java",
                pkg: "be/acto/match/"
            })
        );
        assert_eq!(
            maven_dir("src/main/resources/"),
            Some(MavenDir {
                module: "",
                lang: "resources",
                pkg: ""
            })
        );
    }

    #[test]
    fn only_main_and_test_are_maven_dirs() {
        assert_eq!(maven_dir("core/src/it/java/be/"), None);
        assert_eq!(maven_dir("core/src/testFixtures/java/be/"), None);
        // Directly in the set, with no language directory to name.
        assert_eq!(maven_dir("core/src/main/"), None);
        assert_eq!(maven_dir("docs/"), None);
    }
}
