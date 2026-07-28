use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use smallvec::SmallVec;
use tokio_util::sync::CancellationToken;

use crate::github::comment::ReviewComment;
use crate::github::{BlameError, BlameFile, CommitPrLookupError, CommitPrResolution};

use super::CommentThread;

pub(crate) const MAX_DISCUSSION_ANCHOR_GROUPS: usize = 16;
pub(crate) const MAX_DISCUSSION_ANCHOR_LINES: u32 = 20_000;
pub(crate) const MAX_DISCUSSION_COMMIT_LOOKUPS: usize = 256;
pub(crate) const MAX_DISCUSSION_PULL_REQUESTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct LineOrigin {
    pub sha: Arc<str>,
    pub path: Arc<str>,
    pub line: u32,
}

#[derive(Debug)]
pub(crate) struct DiscussionIndex {
    pub comments: Vec<ReviewComment>,
    pub threads: Vec<CommentThread>,
    pub line_threads: Vec<SmallVec<[usize; 1]>>,
    pub file_thread_count: usize,
    pub comment_paths: Vec<String>,
    pub outcome: DiscussionOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscussionOutcome {
    Complete,
    LookupLimited {
        omitted_commits: usize,
        omitted_pull_requests: usize,
    },
    BudgetExhausted {
        max_groups: usize,
        max_lines: u32,
    },
    AnchorWarning {
        message: String,
    },
    UnplacedThreads {
        count: usize,
    },
}

impl DiscussionOutcome {
    pub(crate) fn confidence_note(&self) -> Option<String> {
        match self {
            Self::Complete => None,
            Self::LookupLimited {
                omitted_commits,
                omitted_pull_requests,
            } => Some(format!(
                "lookup stopped at the explicit limit ({omitted_commits} additional commit(s), {omitted_pull_requests} additional pull request(s) not consulted)"
            )),
            Self::BudgetExhausted {
                max_groups,
                max_lines,
            } => Some(format!(
                "the anchoring budget was reached ({max_groups} groups or {max_lines} lines maximum)"
            )),
            Self::AnchorWarning { message } => Some(format!("some anchors failed: {message}")),
            Self::UnplacedThreads { count } => {
                Some(format!("{count} thread(s) could not be placed confidently"))
            }
        }
    }
}

impl DiscussionIndex {
    pub(crate) fn thread_indices_at(&self, line: usize) -> &[usize] {
        self.line_threads.get(line).map_or(&[], SmallVec::as_slice)
    }

    pub(crate) fn confidence_note(&self) -> Option<String> {
        self.outcome.confidence_note()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DiscussionLookupLimit {
    pub omitted_commits: usize,
    pub omitted_pull_requests: usize,
}

impl DiscussionLookupLimit {
    pub(crate) fn outcome(self) -> Option<DiscussionOutcome> {
        if self.omitted_commits == 0 && self.omitted_pull_requests == 0 {
            None
        } else {
            Some(DiscussionOutcome::LookupLimited {
                omitted_commits: self.omitted_commits,
                omitted_pull_requests: self.omitted_pull_requests,
            })
        }
    }
}

#[derive(Debug, Default)]
pub(crate) enum DiscussionView {
    #[default]
    Closed,
    ThreadList {
        line: usize,
        selected: usize,
        scroll: usize,
    },
    Expanded {
        line: usize,
        thread_position: usize,
        selected: usize,
        scroll: usize,
    },
}

#[derive(Debug)]
pub(crate) enum LineDiscussionFailure {
    NoPullRequest,
    Api,
    Anchor,
}

#[derive(Debug, Default)]
pub(crate) enum LineDiscussionState {
    #[default]
    Idle,
    ResolvingPullRequests {
        request_id: u64,
        path: String,
        cancel: CancellationToken,
    },
    LoadingComments {
        request_id: u64,
        path: String,
        pr_numbers: Vec<u32>,
        cancel: CancellationToken,
    },
    Ready {
        path: String,
        pr_numbers: Vec<u32>,
        index: DiscussionIndex,
        view: DiscussionView,
    },
    Failed {
        failure: LineDiscussionFailure,
    },
}

pub(crate) enum LineDiscussionDelivery {
    PullRequests {
        request_id: u64,
        path: String,
        limit: DiscussionLookupLimit,
        result: Result<Vec<(String, CommitPrResolution)>, CommitPrLookupError>,
    },
    Comments {
        request_id: u64,
        path: String,
        pr_numbers: Vec<u32>,
        fetched_comments: Vec<(u32, Vec<ReviewComment>)>,
        result: Result<DiscussionIndex, LineDiscussionLoadError>,
    },
}

pub(crate) enum LineDiscussionLoadError {
    Api(String),
    Anchor(String),
}

pub(crate) fn build_discussion_index<F, C>(
    comments: Vec<ReviewComment>,
    current_path: &str,
    current_origins: &[Option<LineOrigin>],
    mut load_blame: F,
    is_cancelled: C,
) -> Result<DiscussionIndex, String>
where
    F: FnMut(&str, &str, u32, u32) -> Result<BlameFile, BlameError>,
    C: Fn() -> bool,
{
    let mut known_paths = HashSet::new();
    known_paths.insert(current_path);
    for origin in current_origins.iter().flatten() {
        known_paths.insert(origin.path.as_ref());
    }
    let threads = super::comments::build_review_threads_for(&comments)
        .into_iter()
        .filter(|thread| known_paths.contains(comments[thread.root].path.as_str()))
        .collect::<Vec<_>>();
    let mut comment_paths = threads
        .iter()
        .map(|thread| comments[thread.root].path.clone())
        .collect::<Vec<_>>();
    comment_paths.sort();
    comment_paths.dedup();
    let mut line_threads = vec![SmallVec::new(); current_origins.len()];
    let mut current_matches = HashMap::<LineOrigin, Option<usize>>::new();
    for (line_index, origin) in current_origins.iter().enumerate() {
        let Some(origin) = origin else {
            continue;
        };
        match current_matches.entry(origin.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Some(line_index));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }

    let mut same_file = vec![true; threads.len()];
    let mut placed = vec![false; threads.len()];
    let mut groups = BTreeMap::<CoordinateGroup, CoordinateBatch>::new();
    for (thread_index, thread) in threads.iter().enumerate() {
        let root = &comments[thread.root];
        let Some(coordinate) = confident_coordinate(root) else {
            continue;
        };
        let key = CoordinateGroup {
            revision: coordinate.revision,
            path: coordinate.path,
        };
        let batch = groups.entry(key).or_insert_with(|| CoordinateBatch {
            start: coordinate.start,
            end: coordinate.end,
            threads: Vec::new(),
        });
        batch.start = batch.start.min(coordinate.start);
        batch.end = batch.end.max(coordinate.end);
        batch
            .threads
            .push((thread_index, coordinate.start, coordinate.end));
    }

    let mut group_count = 0;
    let mut line_budget = 0_u32;
    let mut budget_exhausted = false;
    let mut anchor_warning = None;
    for (key, batch) in groups {
        if is_cancelled() {
            return Err("Review discussion lookup was cancelled".to_string());
        }
        let span = batch
            .end
            .checked_sub(batch.start)
            .and_then(|value| value.checked_add(1));
        let admitted = span.is_some_and(|span| {
            group_count < MAX_DISCUSSION_ANCHOR_GROUPS
                && line_budget
                    .checked_add(span)
                    .is_some_and(|total| total <= MAX_DISCUSSION_ANCHOR_LINES)
        });
        if !admitted {
            budget_exhausted = true;
            continue;
        }
        let span = span.expect("admitted spans are present");
        group_count += 1;
        line_budget += span;

        let historical = match load_blame(&key.revision, &key.path, batch.start, batch.end) {
            Ok(blame) => blame,
            Err(error) => {
                anchor_warning.get_or_insert_with(|| error.to_string());
                continue;
            }
        };
        if is_cancelled() {
            return Err("Review discussion lookup was cancelled".to_string());
        }

        for (thread_index, start, end) in batch.threads {
            let mut current_lines = Vec::new();
            let mut complete = true;
            for historical_line in start..=end {
                let Some(offset) = historical_line
                    .checked_sub(batch.start)
                    .and_then(|value| usize::try_from(value).ok())
                else {
                    complete = false;
                    break;
                };
                let Some(reference) = historical.at(offset) else {
                    complete = false;
                    break;
                };
                let origin = LineOrigin {
                    sha: Arc::from(reference.sha),
                    path: Arc::from(reference.original_path),
                    line: reference.original_line,
                };
                let Some(current_line) = current_matches.get(&origin) else {
                    complete = false;
                    break;
                };
                same_file[thread_index] = true;
                let Some(current_line) = *current_line else {
                    complete = false;
                    break;
                };
                current_lines.push(current_line);
            }
            let contiguous = current_lines.first().is_some_and(|first| {
                current_lines
                    .iter()
                    .enumerate()
                    .all(|(offset, line)| first.checked_add(offset) == Some(*line))
            });
            if !complete || !contiguous {
                continue;
            }
            for current_line in current_lines {
                if !line_threads[current_line].contains(&thread_index) {
                    line_threads[current_line].push(thread_index);
                }
                placed[thread_index] = true;
            }
        }
    }

    let unplaced_thread_count = placed.iter().filter(|&&value| !value).count();
    let outcome = if budget_exhausted {
        DiscussionOutcome::BudgetExhausted {
            max_groups: MAX_DISCUSSION_ANCHOR_GROUPS,
            max_lines: MAX_DISCUSSION_ANCHOR_LINES,
        }
    } else if let Some(message) = anchor_warning {
        DiscussionOutcome::AnchorWarning { message }
    } else if unplaced_thread_count > 0 {
        DiscussionOutcome::UnplacedThreads {
            count: unplaced_thread_count,
        }
    } else {
        DiscussionOutcome::Complete
    };

    Ok(DiscussionIndex {
        comments,
        threads,
        line_threads,
        file_thread_count: same_file.iter().filter(|&&value| value).count(),
        comment_paths,
        outcome,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CoordinateGroup {
    revision: String,
    path: String,
}

struct CoordinateBatch {
    start: u32,
    end: u32,
    threads: Vec<(usize, u32, u32)>,
}

struct CommentCoordinate {
    revision: String,
    path: String,
    start: u32,
    end: u32,
}

fn confident_coordinate(comment: &ReviewComment) -> Option<CommentCoordinate> {
    if comment.path.is_empty()
        || comment
            .location
            .subject_type
            .as_deref()
            .is_some_and(|subject| subject != "line")
    {
        return None;
    }

    if let (Some(revision), Some(end)) = (
        comment.location.original_commit_id.as_ref(),
        comment.location.original_line,
    ) {
        if let Some((start, end)) = right_side_range(
            comment.location.original_start_line,
            end,
            comment.location.start_side.as_deref(),
            comment.location.side.as_deref(),
        ) {
            return Some(CommentCoordinate {
                revision: revision.clone(),
                path: comment.path.clone(),
                start,
                end,
            });
        }
    }

    let revision = comment.location.commit_id.as_ref()?;
    let end = comment.line?;
    let (start, end) = right_side_range(
        comment.start_line,
        end,
        comment.location.start_side.as_deref(),
        comment.location.side.as_deref(),
    )?;
    Some(CommentCoordinate {
        revision: revision.clone(),
        path: comment.path.clone(),
        start,
        end,
    })
}

fn right_side_range(
    start: Option<u32>,
    end: u32,
    start_side: Option<&str>,
    side: Option<&str>,
) -> Option<(u32, u32)> {
    if end == 0 || side != Some("RIGHT") {
        return None;
    }
    match start {
        Some(start) if start > 0 && start <= end && start_side == Some("RIGHT") => {
            Some((start, end))
        }
        None => Some((end, end)),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Output};

    use crate::github::comment::ReviewCommentLocation;
    use crate::github::{parse_porcelain, User};

    use super::*;

    const ORIGIN_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const COMMENT_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn comment(
        id: u64,
        line: u32,
        start_line: Option<u32>,
        side: &str,
        path: &str,
        parent: Option<u64>,
    ) -> ReviewComment {
        ReviewComment {
            id,
            path: path.to_string(),
            line: None,
            start_line: None,
            body: format!("comment {id}"),
            user: User {
                login: "reviewer".to_string(),
            },
            created_at: format!("2026-07-28T00:{id:02}:00Z"),
            in_reply_to_id: parent,
            location: ReviewCommentLocation {
                original_line: Some(line),
                original_start_line: start_line,
                side: Some(side.to_string()),
                start_side: start_line.map(|_| side.to_string()),
                original_commit_id: Some(COMMENT_SHA.to_string()),
                ..ReviewCommentLocation::default()
            },
        }
    }

    fn current_origins() -> Vec<Option<LineOrigin>> {
        (1..=5)
            .map(|line| {
                Some(LineOrigin {
                    sha: Arc::from(ORIGIN_SHA),
                    path: Arc::from("src/old name.rs"),
                    line,
                })
            })
            .collect()
    }

    fn historical_blame(start: u32, end: u32) -> BlameFile {
        let mut porcelain = String::new();
        for line in start..=end {
            porcelain.push_str(&format!(
                "{ORIGIN_SHA} {line} {line} 1\n\
                 author Alice\n\
                 summary original\n\
                 filename src/old name.rs\n\
                 \tline {line}\n"
            ));
        }
        parse_porcelain(&porcelain)
    }

    fn historical_blame_with_rewritten_middle(start: u32, end: u32) -> BlameFile {
        let mut porcelain = String::new();
        let middle = start + (end - start) / 2;
        for line in start..=end {
            let sha = if line == middle {
                "cccccccccccccccccccccccccccccccccccccccc"
            } else {
                ORIGIN_SHA
            };
            porcelain.push_str(&format!(
                "{sha} {line} {line} 1\n\
                 author Alice\n\
                 summary original\n\
                 filename src/old name.rs\n\
                 \tline {line}\n"
            ));
        }
        parse_porcelain(&porcelain)
    }

    fn placement_snapshot(index: &DiscussionIndex) -> String {
        let placed = index
            .line_threads
            .iter()
            .enumerate()
            .filter_map(|(line, threads)| (!threads.is_empty()).then_some(line + 1))
            .collect::<Vec<_>>();
        let unplaced = match &index.outcome {
            DiscussionOutcome::UnplacedThreads { count } => *count,
            DiscussionOutcome::Complete
            | DiscussionOutcome::LookupLimited { .. }
            | DiscussionOutcome::BudgetExhausted { .. }
            | DiscussionOutcome::AnchorWarning { .. } => 0,
        };
        format!(
            "placed current lines: {placed:?}\nunplaced threads: {}",
            unplaced
        )
    }

    #[test]
    fn discussion_outcome_variants_have_distinct_exhaustive_user_facing_text() {
        let outcomes = [
            DiscussionOutcome::Complete,
            DiscussionOutcome::LookupLimited {
                omitted_commits: 3,
                omitted_pull_requests: 2,
            },
            DiscussionOutcome::BudgetExhausted {
                max_groups: MAX_DISCUSSION_ANCHOR_GROUPS,
                max_lines: MAX_DISCUSSION_ANCHOR_LINES,
            },
            DiscussionOutcome::AnchorWarning {
                message: "git blame failed".to_string(),
            },
            DiscussionOutcome::UnplacedThreads { count: 4 },
        ];
        let rendered = outcomes
            .iter()
            .map(|outcome| {
                let variant = match outcome {
                    DiscussionOutcome::Complete => "complete",
                    DiscussionOutcome::LookupLimited { .. } => "lookup-limited",
                    DiscussionOutcome::BudgetExhausted { .. } => "budget-exhausted",
                    DiscussionOutcome::AnchorWarning { .. } => "anchor-warning",
                    DiscussionOutcome::UnplacedThreads { .. } => "unplaced-threads",
                };
                format!(
                    "{variant}: {}",
                    outcome
                        .confidence_note()
                        .unwrap_or_else(|| "<none>".to_string())
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        insta::assert_snapshot!(rendered, @"
        complete: <none>
        lookup-limited: lookup stopped at the explicit limit (3 additional commit(s), 2 additional pull request(s) not consulted)
        budget-exhausted: the anchoring budget was reached (16 groups or 20000 lines maximum)
        anchor-warning: some anchors failed: git blame failed
        unplaced-threads: 4 thread(s) could not be placed confidently
        ");
        assert_eq!(
            DiscussionOutcome::Complete.confidence_note(),
            None,
            "a clean index cannot simultaneously carry a warning"
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

    fn commit_all(repo: &Path, message: &str) -> bool {
        git(repo, &["add", "."]).is_some_and(|output| output.status.success())
            && git(repo, &["commit", "-m", message]).is_some_and(|output| output.status.success())
    }

    #[test]
    fn integration_rest_json_through_real_rename_history_marks_the_current_line() {
        let temp = tempfile::tempdir().unwrap();
        if !git(temp.path(), &["init"]).is_some_and(|output| output.status.success()) {
            return;
        }
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(
            temp.path().join("src/日 本.rs"),
            "fn first() {}\nfn discussed() {}\n",
        )
        .unwrap();
        if !commit_all(temp.path(), "original") {
            return;
        }
        let original = git(temp.path(), &["rev-parse", "HEAD"]).unwrap();
        let original = String::from_utf8(original.stdout).unwrap();
        let original = original.trim().to_string();
        let rename = git(temp.path(), &["mv", "src/日 本.rs", "src/renamed file.rs"]).unwrap();
        assert!(rename.status.success());
        assert!(commit_all(temp.path(), "rename"));
        fs::write(
            temp.path().join("src/renamed file.rs"),
            "// inserted\nfn first() {}\nfn discussed() {}\n",
        )
        .unwrap();
        assert!(commit_all(temp.path(), "insert"));

        let comment: ReviewComment = serde_json::from_value(serde_json::json!({
            "id": 1,
            "path": "src/日 本.rs",
            "line": null,
            "original_line": 2,
            "side": "RIGHT",
            "original_commit_id": original,
            "body": "Keep this behavior",
            "user": { "login": "reviewer" },
            "created_at": "2026-07-28T00:00:00Z"
        }))
        .unwrap();
        let current = crate::github::blame_file(temp.path(), "src/renamed file.rs").unwrap();
        let origins: Vec<Option<LineOrigin>> = (0..current.line_count())
            .map(|line| {
                current.at(line).map(|reference| LineOrigin {
                    sha: Arc::from(reference.sha),
                    path: Arc::from(reference.original_path),
                    line: reference.original_line,
                })
            })
            .collect();

        let index = build_discussion_index(
            vec![comment],
            "src/renamed file.rs",
            &origins,
            |revision, path, start, end| {
                crate::github::blame_file_at_revision_range(temp.path(), revision, path, start, end)
            },
            || false,
        )
        .unwrap();

        assert!(index.thread_indices_at(0).is_empty());
        assert!(index.thread_indices_at(1).is_empty());
        assert_eq!(index.thread_indices_at(2), &[0]);
    }

    #[test]
    fn anchors_a_renamed_range_and_keeps_its_reply_in_the_same_thread() {
        let comments = vec![
            comment(1, 4, Some(2), "RIGHT", "src/old name.rs", None),
            comment(2, 0, None, "RIGHT", "ignored reply path", Some(1)),
        ];
        let calls = Cell::new(0);
        let index = build_discussion_index(
            comments,
            "src/current.rs",
            &current_origins(),
            |revision, path, start, end| {
                calls.set(calls.get() + 1);
                assert_eq!(revision, COMMENT_SHA);
                assert_eq!(path, "src/old name.rs");
                assert_eq!((start, end), (2, 4));
                Ok(historical_blame(start, end))
            },
            || false,
        )
        .unwrap();

        assert_eq!(calls.get(), 1);
        assert_eq!(index.file_thread_count, 1);
        assert_eq!(index.threads.len(), 1);
        assert_eq!(index.threads[0].replies, vec![1]);
        assert!(index.thread_indices_at(0).is_empty());
        assert_eq!(index.thread_indices_at(1), &[0]);
        assert_eq!(index.thread_indices_at(2), &[0]);
        assert_eq!(index.thread_indices_at(3), &[0]);
        assert!(index.thread_indices_at(4).is_empty());
    }

    #[test]
    fn multiline_comment_requires_the_whole_contiguous_unambiguous_range() {
        let comments = vec![comment(1, 4, Some(2), "RIGHT", "src/current.rs", None)];
        let rewritten = build_discussion_index(
            comments.clone(),
            "src/current.rs",
            &current_origins(),
            |_, _, start, end| Ok(historical_blame_with_rewritten_middle(start, end)),
            || false,
        )
        .unwrap();
        let intact = build_discussion_index(
            comments,
            "src/current.rs",
            &current_origins(),
            |_, _, start, end| Ok(historical_blame(start, end)),
            || false,
        )
        .unwrap();

        insta::assert_snapshot!(
            format!(
                "--- middle rewritten ---\n{}\n--- range intact ---\n{}",
                placement_snapshot(&rewritten),
                placement_snapshot(&intact)
            ),
            @"
        --- middle rewritten ---
        placed current lines: []
        unplaced threads: 1
        --- range intact ---
        placed current lines: [2, 3, 4]
        unplaced threads: 0
        "
        );
    }

    #[test]
    fn left_side_and_ambiguous_origins_are_never_placed() {
        let calls = Cell::new(0);
        let left = build_discussion_index(
            vec![comment(1, 2, None, "LEFT", "src/current.rs", None)],
            "src/current.rs",
            &current_origins(),
            |_, _, _, _| {
                calls.set(calls.get() + 1);
                Ok(historical_blame(2, 2))
            },
            || false,
        )
        .unwrap();
        assert_eq!(calls.get(), 0);
        assert_eq!(left.file_thread_count, 1);
        assert!(left.thread_indices_at(1).is_empty());
        assert_eq!(
            left.outcome,
            DiscussionOutcome::UnplacedThreads { count: 1 }
        );

        let duplicate = LineOrigin {
            sha: Arc::from(ORIGIN_SHA),
            path: Arc::from("src/old name.rs"),
            line: 2,
        };
        let ambiguous = build_discussion_index(
            vec![comment(2, 2, None, "RIGHT", "src/current.rs", None)],
            "src/current.rs",
            &[Some(duplicate.clone()), Some(duplicate)],
            |_, _, start, end| Ok(historical_blame(start, end)),
            || false,
        )
        .unwrap();
        assert!(ambiguous.line_threads.iter().all(SmallVec::is_empty));
        assert_eq!(
            ambiguous.outcome,
            DiscussionOutcome::UnplacedThreads { count: 1 }
        );
    }

    #[test]
    fn a_reviewed_line_deleted_from_the_current_file_stays_unanchored() {
        let index = build_discussion_index(
            vec![comment(1, 9, None, "RIGHT", "src/current.rs", None)],
            "src/current.rs",
            &current_origins(),
            |_, _, start, end| Ok(historical_blame(start, end)),
            || false,
        )
        .unwrap();

        assert_eq!(index.file_thread_count, 1);
        assert_eq!(
            index.outcome,
            DiscussionOutcome::UnplacedThreads { count: 1 }
        );
        assert!(index.line_threads.iter().all(SmallVec::is_empty));
    }

    #[test]
    fn hundreds_of_comments_share_one_blame_and_build_a_per_line_lookup() {
        let comments = (1..=500)
            .map(|id| {
                comment(
                    id,
                    (id % 5 + 1) as u32,
                    None,
                    "RIGHT",
                    "src/current.rs",
                    None,
                )
            })
            .collect();
        let calls = Cell::new(0);
        let index = build_discussion_index(
            comments,
            "src/current.rs",
            &current_origins(),
            |_, _, start, end| {
                calls.set(calls.get() + 1);
                Ok(historical_blame(start, end))
            },
            || false,
        )
        .unwrap();

        assert_eq!(calls.get(), 1);
        assert_eq!(index.file_thread_count, 500);
        assert_eq!(
            index.line_threads.iter().map(SmallVec::len).sum::<usize>(),
            500
        );
    }

    #[test]
    fn other_files_are_filtered_before_anchor_budget_or_blame_work() {
        let mut comments = (1..=18)
            .map(|id| comment(id, 1, None, "RIGHT", &format!("aaa/other-{id:02}.rs"), None))
            .collect::<Vec<_>>();
        comments.push(comment(19, 2, None, "RIGHT", "src/current.rs", None));
        comments.push(comment(20, 4, None, "RIGHT", "src/old name.rs", None));

        let current_calls = Cell::new(0);
        let other_calls = Cell::new(0);
        let index = build_discussion_index(
            comments,
            "src/current.rs",
            &current_origins(),
            |_, path, start, end| {
                if matches!(path, "src/current.rs" | "src/old name.rs") {
                    current_calls.set(current_calls.get() + 1);
                } else {
                    other_calls.set(other_calls.get() + 1);
                }
                Ok(historical_blame(start, end))
            },
            || false,
        )
        .unwrap();

        insta::assert_snapshot!(
            format!(
                "current-file blame calls: {}\nother-file blame calls: {}\nbudget exhausted: {}\nfile threads: {}",
                current_calls.get(),
                other_calls.get(),
                matches!(
                    index.outcome,
                    DiscussionOutcome::BudgetExhausted { .. }
                ),
                index.file_thread_count
            ),
            @"
        current-file blame calls: 2
        other-file blame calls: 0
        budget exhausted: false
        file threads: 2
        "
        );
    }
}
