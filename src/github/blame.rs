use crate::loader::unquote_git_path;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::Command;

pub const UNCOMMITTED_SHA: &str = "0000000000000000000000000000000000000000";
pub const MAX_BLAME_STDOUT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BLAME_LINES: usize = 8_000_000;

/// Characters kept by [`short_sha`].
const SHORT_SHA_CHARS: usize = 7;

pub(crate) fn short_sha(value: &str) -> &str {
    match value.char_indices().nth(SHORT_SHA_CHARS) {
        Some((end, _)) => &value[..end],
        None => value,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span(u32, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlameCommit {
    sha: Span,
    author: Span,
    summary: Span,
    author_time: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlameFile {
    text: String,
    commits: Vec<BlameCommit>,
    lines: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlameRef<'a> {
    pub sha: &'a str,
    pub author: &'a str,
    pub summary: &'a str,
    pub author_time: i64,
}

impl BlameFile {
    pub fn at(&self, line: usize) -> Option<BlameRef<'_>> {
        let commit_index = *self.lines.get(line)? as usize;
        let commit = self.commits.get(commit_index)?;
        Some(BlameRef {
            sha: self.resolve(commit.sha)?,
            author: self.resolve(commit.author)?,
            summary: self.resolve(commit.summary)?,
            author_time: commit.author_time,
        })
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn dump(&self) -> String {
        let lines: Vec<String> = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(line_index, &commit_index)| {
                let blame = self.at(line_index)?;
                Some(format!(
                    "{}: #{} {} — {}",
                    line_index + 1,
                    commit_index,
                    blame.author,
                    blame.summary
                ))
            })
            .collect();
        lines.join("\n")
    }

    fn resolve(&self, span: Span) -> Option<&str> {
        self.text.get(span.0 as usize..span.1 as usize)
    }
}

impl BlameRef<'_> {
    /// True for the all-zero object name `git blame` gives lines that are not
    /// committed yet.
    ///
    /// Compares every byte rather than the length: a SHA-256 repository writes
    /// sixty-four zeros, not forty.
    pub fn is_uncommitted(&self) -> bool {
        !self.sha.is_empty() && self.sha.bytes().all(|byte| byte == b'0')
    }

    /// The abbreviated object name shown in a gutter.
    ///
    /// Truncates on a character boundary. `BlameRef` has public fields, so a
    /// caller can construct one holding a non-hex string; byte slicing would
    /// panic on it.
    pub fn short_sha(&self) -> &str {
        short_sha(self.sha)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlameError {
    #[error("git is not installed or not on PATH")]
    GitUnavailable,
    #[error("Not a git repository — blame is unavailable here")]
    NotARepository,
    #[error("this repository has no commits yet — nothing to blame")]
    NoCommitsYet,
    #[error("{path} is not tracked by git — nothing to blame yet")]
    NotTracked { path: String },
    #[error("{path} is missing from the working tree")]
    Missing { path: String },
    #[error("blame output is {bytes} bytes, over the {limit} byte limit")]
    TooLarge { bytes: usize, limit: usize },
    #[error("git blame failed: {0}")]
    Failed(String),
}

/// Parses `git blame --porcelain` output without performing I/O.
///
/// Malformed records are ignored, so parsing always returns the valid subset of
/// the input.
pub fn parse_porcelain(stdout: &str) -> BlameFile {
    parse_porcelain_with_line_limit(stdout, MAX_BLAME_LINES)
}

fn parse_porcelain_with_line_limit(stdout: &str, line_limit: usize) -> BlameFile {
    let mut result = BlameFile::default();
    let mut commit_by_sha: HashMap<&str, u32> = HashMap::new();
    let mut current_commit = None;

    for line in stdout.lines() {
        if line.starts_with('\t') {
            continue;
        }

        if let Some(sha) = parse_header(line) {
            if result.lines.len() >= line_limit {
                break;
            }

            let commit_index = if let Some(&index) = commit_by_sha.get(sha) {
                index
            } else {
                // Stop rather than skip. Skipping would leave this line out of
                // `lines` while later lines keep being pushed, so every
                // subsequent index would name the wrong file line — silently.
                // Truncating is recoverable; a shifted gutter is not.
                let Ok(index) = u32::try_from(result.commits.len()) else {
                    break;
                };
                let Some(sha_span) = push_text(&mut result.text, sha) else {
                    break;
                };
                let empty = Span(sha_span.1, sha_span.1);
                result.commits.push(BlameCommit {
                    sha: sha_span,
                    author: empty,
                    summary: empty,
                    author_time: 0,
                });
                commit_by_sha.insert(sha, index);
                index
            };

            current_commit = Some(commit_index);
            result.lines.push(commit_index);
            continue;
        }

        let Some(commit_index) = current_commit.map(|index| index as usize) else {
            continue;
        };
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };

        match key {
            "author" => {
                if let Some(span) = push_text(&mut result.text, value) {
                    if let Some(commit) = result.commits.get_mut(commit_index) {
                        commit.author = span;
                    }
                }
            }
            "author-time" => {
                if let Some(commit) = result.commits.get_mut(commit_index) {
                    commit.author_time = value.parse::<i64>().unwrap_or(0);
                }
            }
            "summary" => {
                if let Some(span) = push_text(&mut result.text, value) {
                    if let Some(commit) = result.commits.get_mut(commit_index) {
                        commit.summary = span;
                    }
                }
            }
            _ => {}
        }
    }

    result
}

fn parse_header(line: &str) -> Option<&str> {
    let mut fields = line.split(' ');
    let sha = fields.next()?;
    if !matches!(sha.len(), 40 | 64)
        || !sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }

    let original_line = fields.next()?;
    let final_line = fields.next()?;
    if !is_decimal(original_line) || !is_decimal(final_line) {
        return None;
    }

    if let Some(group_size) = fields.next() {
        if !is_decimal(group_size) {
            return None;
        }
    }
    if fields.next().is_some() {
        return None;
    }

    Some(sha)
}

fn is_decimal(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn push_text(text: &mut String, value: &str) -> Option<Span> {
    let start = u32::try_from(text.len()).ok()?;
    let end = u32::try_from(text.len().checked_add(value.len())?).ok()?;
    text.push_str(value);
    Some(Span(start, end))
}

/// Returns the exact arguments used by [`blame_file`].
pub fn blame_argv(path: &str) -> [&str; 5] {
    ["blame", "--porcelain", "-w", "--", path]
}

/// Runs `git blame` for a repository-relative working-tree path.
///
/// This function is blocking and can be slow. Call it from
/// `tokio::task::spawn_blocking` when used from asynchronous code.
pub fn blame_file(repo_root: &Path, path: &str) -> Result<BlameFile, BlameError> {
    // Checked before spawning so `ErrorKind::NotFound` below can only mean the
    // git executable. `current_dir` on a path that is missing or is not a
    // directory fails with the same ErrorKind, and telling the user to install
    // git would send them after the wrong problem.
    if !repo_root.is_dir() {
        return Err(BlameError::NotARepository);
    }

    let path = unquote_git_path(path);
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(blame_argv(path.as_ref()))
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                BlameError::GitUnavailable
            } else {
                BlameError::Failed(error.to_string())
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(classify_failure(&stderr, path.as_ref()));
    }

    if output.stdout.len() > MAX_BLAME_STDOUT_BYTES {
        return Err(BlameError::TooLarge {
            bytes: output.stdout.len(),
            limit: MAX_BLAME_STDOUT_BYTES,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_porcelain(&stdout))
}

/// Classifies a failed `git blame` by the diagnostic git printed.
///
/// Matches the phrase where git puts it — immediately after `fatal: ` — rather
/// than anywhere in the message. git interpolates the blamed path into its own
/// diagnostics (`fatal: no such path 'X' in HEAD`), so an unanchored
/// `stderr.contains(..)` lets the path choose the variant: a file named
/// `no such ref` that is missing from the worktree would report
/// [`BlameError::NoCommitsYet`].
///
/// The phrases are git's English output. Under an NLS-enabled git with a
/// non-English locale none of them match and everything becomes
/// [`BlameError::Failed`], which surfaces git's own words rather than a wrong
/// classification — the safe direction to fail.
fn classify_failure(stderr: &str, path: &str) -> BlameError {
    let diagnostic = stderr
        .lines()
        .find_map(|line| line.trim().strip_prefix("fatal: "))
        .unwrap_or("");

    if diagnostic.starts_with("not a git repository") {
        BlameError::NotARepository
    } else if diagnostic.starts_with("no such ref") {
        BlameError::NoCommitsYet
    } else if diagnostic.starts_with("no such path") {
        BlameError::NotTracked {
            path: path.to_string(),
        }
    } else if diagnostic.starts_with("Cannot lstat") {
        BlameError::Missing {
            path: path.to_string(),
        }
    } else {
        BlameError::Failed(stderr.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Output};

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_256: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const ZERO_SHA_256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn empty_input_is_an_empty_blame_file() {
        let blame = parse_porcelain("");

        assert_eq!(blame, BlameFile::default());
        assert!(blame.is_empty());
        assert_eq!(blame.line_count(), 0);
        assert_eq!(blame.at(0), None);
    }

    #[test]
    fn parses_a_single_group() {
        let blame = parse_porcelain(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice Example\n\
             author-mail <alice@example.com>\n\
             author-time 1700000000\n\
             author-tz +0900\n\
             summary initial commit\n\
             filename src/lib.rs\n\
             \tpub fn hello() {}\n",
        );

        assert_eq!(blame.line_count(), 1);
        let line = blame.at(0).unwrap();
        assert_eq!(line.sha, SHA_A);
        assert_eq!(line.short_sha(), "aaaaaaa");
        assert_eq!(line.author, "Alice Example");
        assert_eq!(line.summary, "initial commit");
        assert_eq!(line.author_time, 1_700_000_000);
        assert!(!line.is_uncommitted());
    }

    #[test]
    fn repeated_sha_without_metadata_reuses_the_original_commit() {
        let blame = parse_porcelain(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1\n\
             summary initial commit\n\
             \tfirst\n\
             bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 1 2 1\n\
             author Bob\n\
             author-time 2\n\
             summary second commit\n\
             \tsecond\n\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 3 1\n\
             \tthird\n",
        );

        assert_eq!(blame.line_count(), 3);
        assert_eq!(blame.at(0).unwrap(), blame.at(2).unwrap());
        assert_eq!(blame.at(0).unwrap().author, "Alice");
        assert_eq!(blame.at(0).unwrap().summary, "initial commit");
        assert_snapshot!(blame.dump(), @"
        1: #0 Alice — initial commit
        2: #1 Bob — second commit
        3: #0 Alice — initial commit
        ");
    }

    #[test]
    fn ignores_known_and_future_metadata_without_breaking_the_group() {
        let blame = parse_porcelain(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 7 1 1\n\
             boundary\n\
             previous bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb old/name.rs\n\
             filename src/current.rs\n\
             author Alice A.\n\
             author-tz -0700\n\
             committer-time 999\n\
             future-key future value\n\
             author-time 42\n\
             summary kept summary\n\
             \tcontent\n",
        );

        let line = blame.at(0).unwrap();
        assert_eq!(line.author, "Alice A.");
        assert_eq!(line.author_time, 42);
        assert_eq!(line.summary, "kept summary");
    }

    #[test]
    fn detects_uncommitted_sha_for_both_hash_lengths() {
        let input = format!(
            "{UNCOMMITTED_SHA} 1 1 1\n\
             author Not Committed Yet\n\
             summary working tree\n\
             \tfirst\n\
             {ZERO_SHA_256} 2 2 1\n\
             author Not Committed Yet\n\
             summary sha256 working tree\n\
             \tsecond\n"
        );
        let blame = parse_porcelain(&input);

        assert!(blame.at(0).unwrap().is_uncommitted());
        assert!(blame.at(1).unwrap().is_uncommitted());
        assert_eq!(blame.at(1).unwrap().short_sha(), "0000000");
    }

    #[test]
    fn content_that_looks_like_a_header_is_never_parsed_as_one() {
        let blame = parse_porcelain(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             summary fixture\n\
             \tdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef 1 1 1\n",
        );

        assert_eq!(blame.line_count(), 1);
        assert_eq!(blame.at(0).unwrap().sha, SHA_A);
    }

    #[test]
    fn crlf_and_lf_inputs_parse_identically() {
        let lf = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
                  author Alice\n\
                  author-time 7\n\
                  summary same\n\
                  \tcontent\n";
        let crlf = lf.replace('\n', "\r\n");

        assert_eq!(parse_porcelain(lf), parse_porcelain(&crlf));
    }

    #[test]
    fn summary_with_spaces_cjk_and_pr_number_round_trips() {
        let blame = parse_porcelain(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author 山田 太郎\n\
             author-time 8\n\
             summary 日本語を含む fix details (#123)\n\
             \tcontent\n",
        );

        let line = blame.at(0).unwrap();
        assert_eq!(line.author, "山田 太郎");
        assert_eq!(line.summary, "日本語を含む fix details (#123)");
    }

    #[test]
    fn preserves_gits_empty_commit_message_summary() {
        let blame = parse_porcelain(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             summary (aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)\n\
             \tcontent\n",
        );

        assert_eq!(
            blame.at(0).unwrap().summary,
            "(aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)"
        );
    }

    #[test]
    fn metadata_before_a_header_is_ignored() {
        let blame = parse_porcelain(
            "author Wrong\n\
             author-time 999\n\
             summary wrong summary\n\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             summary right summary\n\
             \tcontent\n",
        );

        let line = blame.at(0).unwrap();
        assert_eq!(line.author, "");
        assert_eq!(line.author_time, 0);
        assert_eq!(line.summary, "right summary");
    }

    #[test]
    fn parses_sha256_headers() {
        let input = format!(
            "{SHA_256} 1 1 1\n\
             author Carol\n\
             author-time 9\n\
             summary sha256 repository\n\
             \tcontent\n"
        );
        let blame = parse_porcelain(&input);

        assert_eq!(blame.line_count(), 1);
        assert_eq!(blame.at(0).unwrap().sha, SHA_256);
    }

    #[test]
    fn malformed_headers_are_ignored() {
        let blame = parse_porcelain(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA 1 1 1\n\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa x 1 1\n\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1 extra\n\
             too-short 1 1 1\n",
        );

        assert!(blame.is_empty());
    }

    #[test]
    fn blame_argv_has_the_exact_safe_argument_order() {
        assert_eq!(
            blame_argv("--force"),
            ["blame", "--porcelain", "-w", "--", "--force"]
        );
        assert_eq!(
            blame_argv("main"),
            ["blame", "--porcelain", "-w", "--", "main"]
        );
    }

    #[test]
    fn accessors_do_not_clamp_past_the_end() {
        let blame = parse_porcelain(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1\n\
             author Alice\n\
             summary only line\n\
             \tcontent\n",
        );

        assert!(!blame.is_empty());
        assert_eq!(blame.line_count(), 1);
        assert!(blame.at(0).is_some());
        assert_eq!(blame.at(1), None);
        assert_eq!(blame.at(usize::MAX), None);
    }

    #[test]
    fn parser_stops_at_the_configured_line_cap() {
        let blame = parse_porcelain_with_line_limit(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             summary first\n\
             \tfirst\n\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2 1\n\
             \tsecond\n",
            1,
        );

        assert_eq!(blame.line_count(), 1);
    }

    /// Builds porcelain with `line_count` lines drawn from `commit_count`
    /// distinct commits, round-robin so no two adjacent lines share a commit —
    /// the worst case for a run-length scheme and the honest case for this one.
    fn synthetic(line_count: usize, commit_count: usize) -> String {
        let mut out = String::new();
        let mut seen = vec![false; commit_count];
        for line in 0..line_count {
            let commit = line % commit_count;
            let sha = format!("{:040x}", commit + 1);
            out.push_str(&format!("{sha} {} {} 1\n", line + 1, line + 1));
            if !seen[commit] {
                seen[commit] = true;
                out.push_str(&format!(
                    "author Author {commit}\n\
                     author-mail <a@example.com>\n\
                     author-time 1700000000\n\
                     author-tz +0000\n\
                     summary synthetic commit {commit}\n\
                     filename src/synthetic.rs\n"
                ));
            }
            out.push_str("\tsource line\n");
        }
        out
    }

    /// The layout claim the design is built on: per-line cost is one `u32`, and
    /// the arena is a function of the commit count alone.
    ///
    /// Without this, a future refactor that puts a `String` back into
    /// `BlameCommit` — or that stores metadata per line — passes every other
    /// test and every gate.
    #[test]
    fn per_line_cost_is_four_bytes_and_the_arena_does_not_grow_with_line_count() {
        // `size_of::<u32>()` would be 4 no matter what `lines` held, so assert
        // the element type itself. These two are the whole layout claim: four
        // bytes per line, and a commit record that owns no heap.
        assert_eq!(
            std::mem::size_of_val(&BlameFile::default().lines),
            std::mem::size_of::<Vec<u32>>()
        );
        assert_eq!(
            std::mem::size_of::<BlameCommit>(),
            3 * std::mem::size_of::<Span>() + std::mem::size_of::<i64>(),
            "BlameCommit grew — a String or another field has crept in"
        );

        let few = parse_porcelain(&synthetic(200, 8));
        let many = parse_porcelain(&synthetic(2_000, 8));

        assert_eq!(few.line_count(), 200);
        assert_eq!(many.line_count(), 2_000);
        assert_eq!(few.commits.len(), 8);
        assert_eq!(many.commits.len(), 8);

        // Ten times the lines, same commits: the arena must not move.
        assert_eq!(
            few.text.len(),
            many.text.len(),
            "arena grew with line count — metadata is being stored per line"
        );

        // And it does grow with commits, so the check above is not vacuous.
        let more_commits = parse_porcelain(&synthetic(200, 64));
        assert_eq!(more_commits.commits.len(), 64);
        assert!(more_commits.text.len() > few.text.len());
    }

    #[test]
    fn short_sha_truncates_on_a_character_boundary() {
        let hex = BlameRef {
            sha: SHA_A,
            author: "",
            summary: "",
            author_time: 0,
        };
        assert_eq!(hex.short_sha(), "aaaaaaa");

        // BlameRef has public fields, so a caller can build one holding
        // anything. Byte slicing would panic here.
        let cjk = BlameRef {
            sha: "日本語のテキストです",
            author: "",
            summary: "",
            author_time: 0,
        };
        assert_eq!(cjk.short_sha(), "日本語のテキス");

        let short = BlameRef {
            sha: "abc",
            author: "",
            summary: "",
            author_time: 0,
        };
        assert_eq!(short.short_sha(), "abc");

        let empty = BlameRef {
            sha: "",
            author: "",
            summary: "",
            author_time: 0,
        };
        assert_eq!(empty.short_sha(), "");
        assert!(!empty.is_uncommitted());
    }

    #[test]
    fn every_error_carries_a_distinct_actionable_message() {
        let errors = [
            BlameError::GitUnavailable,
            BlameError::NotARepository,
            BlameError::NoCommitsYet,
            BlameError::NotTracked {
                path: "src/a.rs".to_string(),
            },
            BlameError::Missing {
                path: "src/a.rs".to_string(),
            },
            BlameError::TooLarge {
                bytes: 100,
                limit: 10,
            },
            BlameError::Failed("boom".to_string()),
        ];

        let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
        for message in &messages {
            assert!(!message.is_empty());
            // The footer renders a single row; a newline would corrupt it.
            assert!(!message.contains('\n'), "multi-line message: {message}");
        }

        let mut unique = messages.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            messages.len(),
            "two variants share a message, so the user cannot tell them apart"
        );

        // The two that differ only in what the user should do about them.
        assert!(messages[3].contains("not tracked"));
        assert!(messages[4].contains("missing from the working tree"));
    }

    #[test]
    fn classification_reads_the_diagnostic_not_the_whole_message() {
        // git interpolates the path into its own diagnostics, so a path can
        // contain any of the phrases the classifier looks for.
        assert_eq!(
            classify_failure(
                "fatal: no such path 'src/no such ref.txt' in HEAD\n",
                "src/no such ref.txt"
            ),
            BlameError::NotTracked {
                path: "src/no such ref.txt".to_string()
            },
            "the path picked the variant instead of the diagnostic"
        );
        assert_eq!(
            classify_failure(
                "fatal: Cannot lstat 'not a git repository': No such file or directory\n",
                "not a git repository"
            ),
            BlameError::Missing {
                path: "not a git repository".to_string()
            }
        );

        // The genuine diagnostics still classify.
        assert_eq!(
            classify_failure("fatal: no such ref: HEAD\n", "a.txt"),
            BlameError::NoCommitsYet
        );
        assert_eq!(
            classify_failure(
                "fatal: not a git repository (or any of the parent directories): .git\n",
                "a.txt"
            ),
            BlameError::NotARepository
        );

        // Unrecognised git output surfaces git's own words rather than a guess.
        // This is also what a non-English locale produces.
        assert_eq!(
            classify_failure("fatal: no such ref: HEAD (translated)\n", "a.txt"),
            BlameError::NoCommitsYet
        );
        let localized = classify_failure("致命的: そのようなパスはありません\n", "a.txt");
        assert!(matches!(localized, BlameError::Failed(_)));
    }

    #[test]
    fn a_repo_root_that_is_not_a_directory_is_not_reported_as_a_missing_git() {
        let temp = tempfile::tempdir().unwrap();

        // Both of these make Command::current_dir fail with ErrorKind::NotFound,
        // the same kind a missing `git` executable produces. Telling the user to
        // install git would send them after the wrong problem.
        let missing = temp.path().join("does-not-exist");
        assert_eq!(
            blame_file(&missing, "a.txt"),
            Err(BlameError::NotARepository)
        );

        let regular_file = temp.path().join("a-file");
        fs::write(&regular_file, "not a directory\n").unwrap();
        assert_eq!(
            blame_file(&regular_file, "a.txt"),
            Err(BlameError::NotARepository)
        );
    }

    fn git(repo: &Path, args: &[&str]) -> Option<Output> {
        Command::new("git")
            .current_dir(repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .ok()
    }

    fn init_repo(repo: &Path) -> bool {
        git(repo, &["init"]).is_some_and(|output| output.status.success())
    }

    fn commit_all(repo: &Path, message: &str) -> bool {
        let Some(add) = git(repo, &["add", "."]) else {
            return false;
        };
        if !add.status.success() {
            return false;
        }
        git(repo, &["commit", "-m", message]).is_some_and(|output| output.status.success())
    }

    fn init_with_baseline_commit(repo: &Path) -> bool {
        if !init_repo(repo) {
            return false;
        }
        fs::write(repo.join("baseline.txt"), "baseline\n").unwrap();
        commit_all(repo, "baseline")
    }

    #[test]
    fn scenario_untracked_file_is_not_tracked() {
        let temp = tempfile::tempdir().unwrap();
        if !init_with_baseline_commit(temp.path()) {
            return;
        }
        fs::write(temp.path().join("untracked.txt"), "new\n").unwrap();

        assert_eq!(
            blame_file(temp.path(), "untracked.txt"),
            Err(BlameError::NotTracked {
                path: "untracked.txt".to_string()
            })
        );
    }

    #[test]
    fn scenario_staged_uncommitted_file_is_valid_blame() {
        let temp = tempfile::tempdir().unwrap();
        if !init_with_baseline_commit(temp.path()) {
            return;
        }
        fs::write(temp.path().join("staged.txt"), "new work\n").unwrap();
        let Some(add) = git(temp.path(), &["add", "staged.txt"]) else {
            return;
        };
        if !add.status.success() {
            return;
        }

        let blame = blame_file(temp.path(), "staged.txt").unwrap();
        assert_eq!(blame.line_count(), 1);
        assert!(blame.at(0).unwrap().is_uncommitted());
    }

    #[test]
    fn scenario_path_outside_repository_is_classified() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("plain.txt"), "plain\n").unwrap();

        assert_eq!(
            blame_file(temp.path(), "plain.txt"),
            Err(BlameError::NotARepository)
        );
    }

    #[test]
    fn scenario_deleted_head_file_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        if !init_repo(temp.path()) {
            return;
        }
        fs::write(temp.path().join("gone.txt"), "committed\n").unwrap();
        if !commit_all(temp.path(), "add file") {
            return;
        }
        fs::remove_file(temp.path().join("gone.txt")).unwrap();

        assert_eq!(
            blame_file(temp.path(), "gone.txt"),
            Err(BlameError::Missing {
                path: "gone.txt".to_string()
            })
        );
    }

    #[test]
    fn scenario_empty_tracked_file_returns_empty_blame() {
        let temp = tempfile::tempdir().unwrap();
        if !init_repo(temp.path()) {
            return;
        }
        fs::write(temp.path().join("empty.txt"), "").unwrap();
        if !commit_all(temp.path(), "add empty file") {
            return;
        }

        let blame = blame_file(temp.path(), "empty.txt").unwrap();
        assert!(blame.is_empty());
    }

    #[test]
    fn scenario_binary_file_with_invalid_utf8_is_valid_blame() {
        let temp = tempfile::tempdir().unwrap();
        if !init_repo(temp.path()) {
            return;
        }
        fs::write(temp.path().join("binary.dat"), [0xff, 0xfe, 0x00, b'\n']).unwrap();
        if !commit_all(temp.path(), "add binary file") {
            return;
        }

        assert!(blame_file(temp.path(), "binary.dat").is_ok());
    }

    #[test]
    fn scenario_unborn_head_has_no_commits_yet() {
        let temp = tempfile::tempdir().unwrap();
        if !init_repo(temp.path()) {
            return;
        }
        fs::write(temp.path().join("new.txt"), "new\n").unwrap();

        assert_eq!(
            blame_file(temp.path(), "new.txt"),
            Err(BlameError::NoCommitsYet)
        );
    }

    #[test]
    fn scenario_space_and_cquoted_cjk_paths_blame_successfully() {
        let temp = tempfile::tempdir().unwrap();
        if !init_repo(temp.path()) {
            return;
        }
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/space name.txt"), "space\n").unwrap();
        fs::write(temp.path().join("src/日本語.txt"), "cjk\n").unwrap();
        if !commit_all(temp.path(), "add paths") {
            return;
        }

        let spaced = blame_file(temp.path(), "src/space name.txt").unwrap();
        let cjk = blame_file(
            temp.path(),
            r#""src/\346\227\245\346\234\254\350\252\236.txt""#,
        )
        .unwrap();

        assert_eq!(spaced.line_count(), 1);
        assert_eq!(cjk.line_count(), 1);
        assert_eq!(cjk.at(0).unwrap().author, "t");
    }

    #[test]
    fn scenario_paths_that_look_like_a_flag_or_a_ref_reach_git_as_paths() {
        let temp = tempfile::tempdir().unwrap();
        if !init_repo(temp.path()) {
            return;
        }
        // `main` collides with the branch name, and a leading dash looks like
        // an option. Both are only safe because blame_argv puts the path after
        // the `--` separator.
        fs::write(temp.path().join("main"), "branch-shaped name\n").unwrap();
        fs::write(temp.path().join("-weird.txt"), "flag-shaped name\n").unwrap();
        if !commit_all(temp.path(), "add adversarial names") {
            return;
        }

        let as_ref_name = blame_file(temp.path(), "main").unwrap();
        let as_flag = blame_file(temp.path(), "-weird.txt").unwrap();

        assert_eq!(as_ref_name.line_count(), 1);
        assert_eq!(as_flag.line_count(), 1);
        assert!(!as_ref_name.at(0).unwrap().is_uncommitted());
    }
}
