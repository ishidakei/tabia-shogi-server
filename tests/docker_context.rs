//! The files `Cargo.toml` names, held against the files `docker build` can see.
//!
//! Three lists in this repository name paths, and they have to agree.
//!
//! 1. `Cargo.toml` declares targets explicitly — `[[test]]`, `[[bin]]`,
//!    `[[bench]]`, `[[example]]`, `[lib]`. Each declaration is a path cargo
//!    insists on, and it insists at **manifest parse** time: a declared file
//!    that is absent is not an uncompiled target, it is a manifest that will
//!    not parse.
//! 2. `.dockerignore` is an allow-list — exclude everything, then put back the
//!    few paths the release build needs — so it decides what the build context
//!    holds.
//! 3. The builder stage of `Dockerfile` copies a named handful out of that
//!    context, so it decides what the build can actually open.
//!
//! A declared test the allow-list does not put back makes `docker build` from a
//! clean clone fail at the builder stage with
//!
//! ```text
//! error: Cargo.toml: can't find `panic_containment` test at
//!        `tests/panic_containment.rs` or `tests/panic_containment/main.rs`
//! ```
//!
//! while every cargo gate stays green, because a cargo gate runs against the
//! whole checkout and never sees the narrowed context.
//!
//! This needs no docker daemon and starts none: the question is which paths
//! three text files name. A machine running this test suite need not have a
//! daemon at all, so a check that shelled out to `docker build` would be a check
//! that quietly never ran.
//!
//! Only declared targets are required to be in the context: cargo finds the rest
//! of `tests/` by autodiscovery and is content when an autodiscovered file is
//! absent. That is also why `cargo metadata` is not the source here — it
//! resolves every target's path but does not say which ones the manifest asked
//! for by name.

use std::fs;
use std::path::Path;

use serde::Deserialize;

/// The manifest whose declarations the other two lists have to satisfy.
const MANIFEST: &str = "Cargo.toml";

/// The allow-list that decides what the build context holds.
const IGNORE_FILE: &str = ".dockerignore";

/// The build whose `COPY` set decides what the builder can open.
const DOCKERFILE: &str = "Dockerfile";

/// The stage of [`DOCKERFILE`] that compiles the server.
const BUILDER_STAGE: &str = "builder";

// ---------------------------------------------------------------------------
// The manifest's declared targets
// ---------------------------------------------------------------------------

/// The five target tables of a manifest, and nothing else about it.
///
/// serde ignores unknown keys by default, so dependencies, features and package
/// metadata are read past rather than modelled.
#[derive(Deserialize)]
struct Manifest {
    lib: Option<Target>,
    #[serde(default)]
    bin: Vec<Target>,
    #[serde(default)]
    test: Vec<Target>,
    #[serde(default)]
    bench: Vec<Target>,
    #[serde(default)]
    example: Vec<Target>,
}

/// One declared target: enough of it to work out which file it names.
#[derive(Deserialize)]
struct Target {
    name: Option<String>,
    path: Option<String>,
}

/// The kind of a declared target, which fixes where cargo looks for its file.
#[derive(Clone, Copy)]
enum Kind {
    Lib,
    Bin,
    Test,
    Bench,
    Example,
}

impl Kind {
    /// The table this kind is declared in, for a failure message that points at
    /// the line the reader has to edit.
    fn table(self) -> &'static str {
        match self {
            Kind::Lib => "[lib]",
            Kind::Bin => "[[bin]]",
            Kind::Test => "[[test]]",
            Kind::Bench => "[[bench]]",
            Kind::Example => "[[example]]",
        }
    }

    /// Where cargo looks for a target of this kind declared by name alone, in
    /// the order cargo tries them.
    ///
    /// `package` is the package name, which is the one case a `[[bin]]` maps
    /// onto `src/main.rs` rather than into `src/bin/`.
    fn candidates(self, name: &str, package: &str) -> Vec<String> {
        match self {
            Kind::Lib => vec!["src/lib.rs".to_owned()],
            Kind::Bin if name == package => vec![
                "src/main.rs".to_owned(),
                format!("src/bin/{name}.rs"),
                format!("src/bin/{name}/main.rs"),
            ],
            Kind::Bin => vec![
                format!("src/bin/{name}.rs"),
                format!("src/bin/{name}/main.rs"),
            ],
            Kind::Test => vec![format!("tests/{name}.rs"), format!("tests/{name}/main.rs")],
            Kind::Bench => vec![
                format!("benches/{name}.rs"),
                format!("benches/{name}/main.rs"),
            ],
            Kind::Example => vec![
                format!("examples/{name}.rs"),
                format!("examples/{name}/main.rs"),
            ],
        }
    }
}

/// A declared target resolved to the one path cargo will look for.
struct Declared {
    kind: Kind,
    name: String,
    path: String,
}

/// Every explicitly declared target of `Cargo.toml`, resolved to a path.
///
/// A declaration carrying `path` resolves to it. A declaration carrying only
/// `name` takes the first conventional candidate that exists, and if none does
/// this fails here naming the candidates cargo would name — which is the state
/// this file guards: a manifest that does not parse.
fn declared_targets(manifest_text: &str, package: &str, root: &Path) -> Vec<Declared> {
    let manifest: Manifest = toml::from_str(manifest_text)
        .unwrap_or_else(|error| panic!("{MANIFEST} does not parse: {error}"));

    let mut declared = Vec::new();
    let groups = [
        (Kind::Lib, manifest.lib.into_iter().collect::<Vec<_>>()),
        (Kind::Bin, manifest.bin),
        (Kind::Test, manifest.test),
        (Kind::Bench, manifest.bench),
        (Kind::Example, manifest.example),
    ];

    for (kind, targets) in groups {
        for target in targets {
            let name = target.name.clone().unwrap_or_else(|| package.to_owned());
            let path = match target.path {
                Some(path) => path,
                None => {
                    let candidates = kind.candidates(&name, package);
                    candidates
                        .iter()
                        .find(|candidate| root.join(candidate).exists())
                        .cloned()
                        .unwrap_or_else(|| {
                            panic!(
                                "{MANIFEST} declares {} `{name}`, and none of its files exist: {}. \
                                 This manifest does not parse — declare `path`, or add the file.",
                                kind.table(),
                                candidates.join(", "),
                            )
                        })
                }
            };
            declared.push(Declared { kind, name, path });
        }
    }

    declared
}

/// The `name` of the `[package]` table, which two of the [`Kind`] rules need.
fn package_name(manifest_text: &str) -> String {
    #[derive(Deserialize)]
    struct Named {
        package: Package,
    }
    #[derive(Deserialize)]
    struct Package {
        name: String,
    }

    let named: Named = toml::from_str(manifest_text)
        .unwrap_or_else(|error| panic!("{MANIFEST} has no readable [package] name: {error}"));
    named.package.name
}

// ---------------------------------------------------------------------------
// The allow-list screen
// ---------------------------------------------------------------------------

/// One meaningful line of an ignore file.
struct Rule {
    /// A leading `!`: this line puts back what an earlier line took away.
    negated: bool,
    pattern: String,
}

/// The rules of an ignore file, in the order they are applied.
///
/// Blank lines and `#` comments are dropped, and a leading `/` is stripped:
/// docker reads every pattern as relative to the context root either way.
fn rules(text: &str) -> Vec<Rule> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (negated, rest) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            Rule {
                negated,
                pattern: rest.trim_start_matches('/').to_owned(),
            }
        })
        .collect()
}

/// Whether the build context holds `path`, under docker's own reading of the
/// rules.
///
/// Later rules win, so an allow-list can start with `*` and put entries back
/// afterwards; and a rule is tried against the path and every ancestor of it,
/// which is what makes a bare `*` exclude a file three directories down and a
/// bare `!tests` put one back. Both are docker's `MatchesOrParentMatches`.
fn admits(rules: &[Rule], path: &str) -> bool {
    let mut excluded = false;
    for rule in rules {
        if matches_path_or_ancestor(&rule.pattern, path) {
            excluded = !rule.negated;
        }
    }
    !excluded
}

/// Whether `pattern` matches `path` or any directory above it.
fn matches_path_or_ancestor(pattern: &str, path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    (1..=segments.len()).any(|depth| {
        let pattern_segments: Vec<&str> = pattern.split('/').collect();
        matches_segments(&pattern_segments, &segments[..depth])
    })
}

/// A path matched segment by segment, with `**` spanning any run of segments.
fn matches_segments(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(&"**"), _) => {
            (0..=path.len()).any(|skipped| matches_segments(&pattern[1..], &path[skipped..]))
        }
        (Some(_), None) => false,
        (Some(head), Some(segment)) => {
            matches_segment(head, segment) && matches_segments(&pattern[1..], &path[1..])
        }
    }
}

/// One path segment against one pattern segment: `*` any run of characters,
/// `?` exactly one, and neither crosses a `/` because neither is ever handed
/// one.
fn matches_segment(pattern: &str, segment: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let segment: Vec<char> = segment.chars().collect();

    // `star` remembers the last `*` seen and `resume` the position after the
    // input it had consumed, so a dead end costs one character rather than a
    // whole re-scan.
    let (mut at_pattern, mut at_segment) = (0, 0);
    let (mut star, mut resume) = (None, 0);

    while at_segment < segment.len() {
        if at_pattern < pattern.len()
            && (pattern[at_pattern] == '?' || pattern[at_pattern] == segment[at_segment])
        {
            at_pattern += 1;
            at_segment += 1;
        } else if at_pattern < pattern.len() && pattern[at_pattern] == '*' {
            star = Some(at_pattern);
            at_pattern += 1;
            resume = at_segment;
        } else if let Some(star) = star {
            at_pattern = star + 1;
            resume += 1;
            at_segment = resume;
        } else {
            return false;
        }
    }

    pattern[at_pattern..].iter().all(|part| *part == '*')
}

// ---------------------------------------------------------------------------
// The COPY screen
// ---------------------------------------------------------------------------

/// The source arguments of every `COPY` in the builder stage that reads the
/// build context.
///
/// `COPY --from=` is skipped: it reads an earlier stage, not the context, which
/// is how the runtime stage takes the binary. Continuation lines are joined
/// first, so a `COPY` spread over several lines is one instruction here too.
fn builder_copy_sources(dockerfile: &str) -> Vec<String> {
    let mut instructions: Vec<String> = Vec::new();
    let mut pending = String::new();

    for line in dockerfile.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.strip_suffix('\\') {
            Some(head) => {
                pending.push_str(head.trim_end());
                pending.push(' ');
            }
            None => {
                pending.push_str(line);
                instructions.push(std::mem::take(&mut pending));
            }
        }
    }
    if !pending.is_empty() {
        instructions.push(pending);
    }

    let mut in_builder = false;
    let mut sources = Vec::new();

    for instruction in &instructions {
        let mut words = instruction.split_whitespace();
        let Some(keyword) = words.next() else {
            continue;
        };

        if keyword.eq_ignore_ascii_case("FROM") {
            let rest: Vec<&str> = words.collect();
            let named_stage = rest
                .windows(2)
                .find(|pair| pair[0].eq_ignore_ascii_case("AS"))
                .map(|pair| pair[1]);
            in_builder = named_stage == Some(BUILDER_STAGE);
            continue;
        }

        if !in_builder || !keyword.eq_ignore_ascii_case("COPY") {
            continue;
        }

        let arguments: Vec<&str> = words.collect();
        if arguments.iter().any(|argument| {
            argument.starts_with("--") && argument.to_ascii_lowercase().starts_with("--from=")
        }) {
            continue;
        }

        // Everything but the flags and the trailing destination.
        let mut positional: Vec<&str> = arguments
            .into_iter()
            .filter(|argument| !argument.starts_with("--"))
            .collect();
        positional.pop();
        sources.extend(positional.into_iter().map(|source| {
            source
                .trim_matches('"')
                .trim_start_matches("./")
                .trim_end_matches('/')
                .to_owned()
        }));
    }

    sources
}

/// Whether one of the builder's `COPY` sources brings `path` into the build:
/// either it is the path, or it is a directory above it.
fn copied(sources: &[String], path: &str) -> bool {
    sources
        .iter()
        .any(|source| source == path || path.starts_with(&format!("{source}/")))
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// Every path `Cargo.toml` declares is in the build context and is copied into
/// the build.
///
/// A new `[[bench]]` under a directory the allow-list does not name fails here,
/// in an ordinary `cargo` run, rather than in a `docker build` nobody may have
/// run.
#[test]
#[cfg_attr(miri, ignore)]
fn every_declared_target_is_inside_the_docker_build_context() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let read = |name: &str| {
        fs::read_to_string(root.join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
    };

    let manifest_text = read(MANIFEST);
    let package = package_name(&manifest_text);
    let declared = declared_targets(&manifest_text, &package, root);

    let rules = rules(&read(IGNORE_FILE));
    let sources = builder_copy_sources(&read(DOCKERFILE));

    for target in &declared {
        let Declared { kind, name, path } = target;
        assert!(
            admits(&rules, path),
            "{MANIFEST} declares {} `{name}` at `{path}`, which {IGNORE_FILE} keeps out of the \
             build context. Cargo cannot parse a manifest whose declared target file is missing, \
             so `docker build` fails before it compiles anything. Put the path back in the \
             allow-list.",
            kind.table(),
        );
        assert!(
            copied(&sources, path),
            "{MANIFEST} declares {} `{name}` at `{path}`, and no COPY in the `{BUILDER_STAGE}` \
             stage of {DOCKERFILE} brings it into the build (it copies {}). Cargo cannot parse a \
             manifest whose declared target file is missing, so `docker build` fails before it \
             compiles anything.",
            kind.table(),
            sources.join(", "),
        );
    }
}

/// The allow-list screen says no to a path the allow-list does not put back.
///
/// The test above asserts the tree is right, which proves nothing about whether
/// the screen could ever say otherwise, so the same shape of allow-list is
/// handed a path it admits and one it does not.
#[test]
fn the_allow_list_screen_rejects_a_declared_path_the_context_excludes() {
    let allow_list = "\
# exclude everything, then put back what the build needs
*

!Cargo.toml
!src
!src/**
!tests
!tests/**
";
    let rules = rules(allow_list);

    assert!(admits(&rules, "Cargo.toml"));
    assert!(admits(&rules, "src/lib.rs"));
    assert!(admits(&rules, "tests/panic_containment.rs"));
    // A directory nothing put back, three ways: the file, a nested file, and the
    // directory entry itself.
    assert!(!admits(&rules, "benches/throughput.rs"));
    assert!(!admits(&rules, "benches/parsing/main.rs"));
    assert!(!admits(&rules, "benches"));
    // And an excluded file beside two that are named.
    assert!(!admits(&rules, "Cargo.lock"));
}

/// The `COPY` screen says no to a path that is in the context but that no
/// builder `COPY` takes.
///
/// A path can be in the context and still be missing from `/build`, and that
/// failure reads exactly like the first one.
#[test]
fn the_copy_screen_rejects_a_declared_path_no_builder_copy_takes() {
    let dockerfile = "\
FROM rust:1.98.0-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=builder \\
    /build/target/release/tabia-shogi-server \\
    /usr/local/bin/tabia-shogi-server
";
    let sources = builder_copy_sources(dockerfile);

    assert_eq!(sources, ["Cargo.toml", "Cargo.lock", "src", "tests"]);
    assert!(copied(&sources, "Cargo.toml"));
    assert!(copied(&sources, "src/lib.rs"));
    assert!(copied(&sources, "tests/panic_containment.rs"));
    // Not copied, and not to be confused with the stage-to-stage COPY of the
    // runtime stage, whose `--from=` argument is not a context path at all.
    assert!(!copied(&sources, "benches/throughput.rs"));
    assert!(!copied(&sources, "migrations/0001_initial_schema.sql"));
    // A prefix that is not a path prefix: `srcs` is not inside `src`.
    assert!(!copied(&sources, "srcs/lib.rs"));
}

/// A target declared with `path` is taken at its word, and one declared by name
/// resolves the way cargo resolves it.
///
/// Both branches, because the tree exercises only the second: every declaration
/// in it resolves by name.
#[test]
#[cfg_attr(miri, ignore)]
fn a_declared_target_resolves_to_the_file_cargo_would_look_for() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = "\
[package]
name = \"tabia-shogi-server\"

[[test]]
name = \"panic_containment\"

[[test]]
name = \"anything\"
path = \"tests/panic_containment.rs\"
";
    let declared = declared_targets(manifest, "tabia-shogi-server", root);

    let paths: Vec<&str> = declared.iter().map(|target| target.path.as_str()).collect();
    assert_eq!(
        paths,
        ["tests/panic_containment.rs", "tests/panic_containment.rs"],
    );
}
