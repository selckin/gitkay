//! The commit graph's lane/pipe layout: `CommitInfo`s in, per-row node columns and
//! line segments out.
//!
//! Pure and egui-free — a row's colour is an INDEX here, resolved to a `Color32` by
//! the renderer's `graph_color`, so nothing in this module depends on the palette.
//! That is what lets its suite run on fake oids (`oid(n)`) with no repository at all.
//!
//! **The subtle part of the app**, resting on one invariant — a commit's first parent
//! always continues straight, so no merge draws a false diagonal — plus the resume
//! contract `append_commits` needs: laying out a tail from the stored
//! `GraphLayoutState` must equal relaying the whole list, which holds only while no
//! previously out-of-scope merge parent appears in that tail (`deferred_parents` is
//! how the caller is told it does not). Change this only with the suite below green;
//! `layout_resume_matches_full_layout` is the one that pins the contract.

use std::collections::HashSet;

use crate::history::CommitInfo;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GraphRow {
    pub node_col: usize,
    pub node_color: usize,
    pub lines: Vec<(usize, usize, usize)>,
    pub num_cols: usize,
}

/// The graph layout's fold state after some prefix of rows, letting a later
/// append lay out only its tail (`layout_graph_rows` resumes from it) instead of
/// relaying the whole list. `Default` is the before-any-rows state.
#[derive(Clone, Default)]
pub struct GraphLayoutState {
    /// Each pipe tracks `(oid, color_index)`. `None` = empty slot.
    pipes: Vec<Option<(git2::Oid, usize)>>,
    next_color: usize,
    /// Second+ merge parents skipped because they were beyond the laid-out
    /// window (no lane to draw the merge diagonal to). If a later extension
    /// loads one of these, the full layout would give its merge row the
    /// diagonal a pure resume can't add retroactively — the resume is unsound
    /// then and the caller must relayout from scratch (see `append_commits`).
    pub deferred_parents: HashSet<git2::Oid>,
}

/// Place `slot` in the first empty pipe (reusing a freed lane) or append a new one,
/// returning its column.
fn alloc_lane(pipes: &mut Vec<Option<(git2::Oid, usize)>>, slot: (git2::Oid, usize)) -> usize {
    if let Some(pos) = pipes.iter().position(std::option::Option::is_none) {
        pipes[pos] = Some(slot);
        pos
    } else {
        pipes.push(Some(slot));
        pipes.len() - 1
    }
}

/// `layout_graph_rows` over the whole list from a fresh state. Test-suite entry
/// point — production goes through `derive_from_commits` (full layout, keeping
/// the resume state) or `append_commits` (tail resume).
#[cfg(test)]
fn layout_graph(commits: &[CommitInfo]) -> Vec<GraphRow> {
    let oid_set: HashSet<git2::Oid> = commits.iter().map(|c| c.oid).collect();
    layout_graph_rows(commits, &oid_set, &mut GraphLayoutState::default())
}

/// Lay out `commits` as the rows following whatever `state` already describes —
/// the whole list when `state` is fresh (`layout_graph`), or an appended tail
/// resuming from the stored end-of-list state. `oid_set` is the in-scope set for
/// THESE commits only: the walk is topological (a parent never precedes a child),
/// so a tail commit's parent can never be in the already-laid-out prefix, and the
/// tail's own oids answer "will this parent get a row?" exactly like the full
/// list's set would.
pub fn layout_graph_rows(
    commits: &[CommitInfo],
    oid_set: &HashSet<git2::Oid>,
    state: &mut GraphLayoutState,
) -> Vec<GraphRow> {
    let GraphLayoutState {
        pipes,
        next_color,
        deferred_parents,
    } = state;
    let mut rows = Vec::new();
    // Reused across commits: this is rebuilt for every row and read back within the
    // same iteration, so one allocation covers the whole layout instead of one per
    // commit. The layout runs over the full loaded list on every install, append and
    // rebuild, so the per-commit malloc/free pair was the dominant allocation here.
    let mut matching_cols: Vec<usize> = Vec::new();

    for commit in commits {
        // Find which column this commit is in. If multiple lanes point
        // to this commit (convergence), pick the first and mark others
        // for merge lines.
        matching_cols.clear();
        matching_cols.extend(
            pipes
                .iter()
                .enumerate()
                .filter(|(_, p)| p.is_some_and(|(oid, _)| oid == commit.oid))
                .map(|(i, _)| i),
        );

        let node_col = if matching_cols.is_empty() {
            // New commit — find an empty slot or append
            let color = *next_color;
            *next_color += 1;
            alloc_lane(pipes, (commit.oid, color))
        } else {
            matching_cols[0]
        };

        // node_col was just assigned a pipe (or matched an existing one), so this
        // is always Some; fall back to colour 0 rather than panic if it ever isn't.
        debug_assert!(
            pipes[node_col].is_some(),
            "node column {node_col} has no pipe"
        );
        let node_color = pipes[node_col].map_or(0, |p| p.1);

        // Extra lanes that also pointed to this commit — they converge here.
        let mut converge_lines: Vec<(usize, usize, usize)> = Vec::new();
        if matching_cols.len() > 1 {
            for &col in &matching_cols[1..] {
                // A matching column holds this commit's pipe, so this is always
                // Some; fall back to the node's colour rather than panic if not.
                debug_assert!(pipes[col].is_some(), "matching column {col} has no pipe");
                let color = pipes[col].map_or(node_color, |p| p.1);
                converge_lines.push((col, node_col, color));
                pipes[col] = None;
            }
        }

        let mut lines: Vec<(usize, usize, usize)> = Vec::new();
        let mut new_lanes: Vec<usize> = Vec::new(); // columns created by this commit

        // Clear the node's slot
        pipes[node_col] = None;

        // First parent takes the node's slot (same column, same color).
        // If the first parent is already tracked in another lane (convergence),
        // still continue in the node's column — the other lane will merge at
        // the parent's own row.
        for (i, parent_oid) in commit.parents.iter().enumerate() {
            if i == 0 {
                // First parent always continues in the node's column (even if the
                // parent is out of scope / not loaded yet, so the graph doesn't show
                // an orphan), and claims the pipe unconditionally: the node's slot was
                // cleared just above and nothing has written it since, so the parent
                // cannot already occupy exactly this column.
                debug_assert!(
                    pipes[node_col].is_none(),
                    "node column {node_col} was not cleared before the first parent"
                );
                pipes[node_col] = Some((*parent_oid, node_color));
                lines.push((node_col, node_col, node_color));
                continue;
            }

            // Second+ parent. Out of scope is decided first: the in-scope arms below
            // are the only readers of the lane scan, so an unloaded parent never pays
            // for it.
            if !oid_set.contains(parent_oid) {
                // Second+ parent out of scope: skip (can't draw a merge to an
                // unloaded row) — but remember it, so a later append that loads
                // this parent knows a pure resume would miss this row's merge
                // diagonal and falls back to a full relayout.
                deferred_parents.insert(*parent_oid);
                continue;
            }

            // Check if parent is already tracked in a different lane
            let existing = pipes
                .iter()
                .position(|p| p.is_some_and(|(oid, _)| oid == *parent_oid));
            if let Some(existing_col) = existing {
                lines.push((node_col, existing_col, node_color));
            } else {
                let color = *next_color;
                *next_color += 1;
                let col = alloc_lane(pipes, (*parent_oid, color));
                lines.push((node_col, col, color));
                new_lanes.push(col);
            }
        }

        // All other active lanes continue straight — but skip:
        // - lanes consumed by convergence (pipe already cleared)
        // - lanes newly created by this commit's merge (nothing above them)
        for (col, pipe) in pipes.iter().enumerate() {
            if col == node_col {
                continue;
            }
            if new_lanes.contains(&col) {
                continue;
            }
            if let Some((_, color)) = pipe {
                lines.push((col, col, *color));
            }
        }

        // Add convergence lines (other lanes that pointed to this commit)
        lines.extend(converge_lines);

        let num_cols = pipes.len();
        rows.push(GraphRow {
            node_col,
            node_color,
            lines,
            num_cols,
        });

        // Trim trailing empty slots
        while pipes.last() == Some(&None) {
            pipes.pop();
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::CommitInfo;
    use crate::tests::{commit, oid};

    /// The incremental append (`append_commits`) is only sound because resuming
    /// `layout_graph_rows` from the prefix's end state reproduces exactly what a
    /// full relayout would produce — unless a previously out-of-scope merge
    /// parent lands in the tail, which `deferred_parents` must flag. Pin both
    /// halves of that contract over every split point of several topologies.
    #[test]
    fn layout_resume_matches_full_layout() {
        let fixtures: &[Vec<CommitInfo>] = &[
            // Linear chain.
            vec![
                commit(5, &[4]),
                commit(4, &[3]),
                commit(3, &[2]),
                commit(2, &[1]),
                commit(1, &[]),
            ],
            // Merge at the top whose second parent sits several rows down: splits
            // before row 3 loads must flag the resume unsound.
            vec![
                commit(6, &[5, 3]),
                commit(5, &[4]),
                commit(4, &[3]),
                commit(3, &[2]),
                commit(2, &[1]),
                commit(1, &[]),
            ],
            // Two branches converging on a shared parent (no merges — every
            // split resumes cleanly).
            vec![
                commit(4, &[2]),
                commit(3, &[2]),
                commit(2, &[1]),
                commit(1, &[]),
            ],
        ];
        let mut saw_unsound = false;
        for commits in fixtures {
            let full = layout_graph(commits);
            for split in 1..commits.len() {
                let (prefix, tail) = commits.split_at(split);
                let prefix_oids: HashSet<git2::Oid> = prefix.iter().map(|c| c.oid).collect();
                let mut state = GraphLayoutState::default();
                let prefix_rows = layout_graph_rows(prefix, &prefix_oids, &mut state);
                // The same check append_commits performs.
                if tail.iter().any(|c| state.deferred_parents.contains(&c.oid)) {
                    saw_unsound = true;
                    continue;
                }
                let tail_oids: HashSet<git2::Oid> = tail.iter().map(|c| c.oid).collect();
                let tail_rows = layout_graph_rows(tail, &tail_oids, &mut state);
                assert_eq!(
                    prefix_rows,
                    full[..split].to_vec(),
                    "sound split {split}: prefix layout must match the full layout's prefix"
                );
                assert_eq!(
                    tail_rows,
                    full[split..].to_vec(),
                    "sound split {split}: resumed tail must match the full layout's tail"
                );
            }
        }
        assert!(
            saw_unsound,
            "the merge fixture must flag at least one split as unsound, or the guard is dead"
        );
    }

    /// Assert that a specific commit's node stays in the same column as
    /// its first parent in the next row (linear continuation).
    fn assert_linear(rows: &[GraphRow], commits: &[CommitInfo], child: u32, parent: u32) {
        let child_idx = commits.iter().position(|c| c.oid == oid(child)).unwrap();
        let parent_idx = commits.iter().position(|c| c.oid == oid(parent)).unwrap();
        let child_col = rows[child_idx].node_col;
        let parent_col = rows[parent_idx].node_col;
        assert_eq!(
            child_col, parent_col,
            "Linear commit {child} (col {child_col}) should be in same column as parent {parent} (col {parent_col})"
        );
    }

    /// Assert a commit is in a specific column.
    fn assert_col(rows: &[GraphRow], commits: &[CommitInfo], id: u32, expected_col: usize) {
        let idx = commits.iter().position(|c| c.oid == oid(id)).unwrap();
        assert_eq!(
            rows[idx].node_col, expected_col,
            "Commit {id} should be in column {expected_col}, got {}",
            rows[idx].node_col
        );
    }

    /// Assert no diagonal lines exist for a commit (all edges are straight).
    fn assert_no_diagonals(rows: &[GraphRow], commits: &[CommitInfo], id: u32) {
        let idx = commits.iter().position(|c| c.oid == oid(id)).unwrap();
        for &(from, to, _) in &rows[idx].lines {
            assert_eq!(
                from, to,
                "Commit {id} has unexpected diagonal: col {from} → col {to}"
            );
        }
    }

    /// Assert that a lane's color is consistent: if a lane continues from
    /// row A to row B in a given column, the color should be the same.
    fn assert_colors_consistent(rows: &[GraphRow]) {
        for i in 1..rows.len() {
            let prev = &rows[i - 1];
            let curr = &rows[i];
            // For each straight-through lane in curr, find the matching
            // lane in prev that targets the same column
            for &(from, to, color) in &curr.lines {
                if from == to {
                    // Find the prev row edge that targets this column
                    for &(pf, pt, pc) in &prev.lines {
                        if pt == from && pf == pt {
                            // Same column straight-through in both rows
                            assert_eq!(
                                pc, color,
                                "Color inconsistency at row {i}: column {from} has color {color} but previous row had {pc}"
                            );
                        }
                    }
                }
            }
        }
    }

    // ── Test cases ──

    #[test]
    fn test_linear_history() {
        // A → B → C → D (simple linear)
        let commits = vec![
            commit(1, &[2]),
            commit(2, &[3]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        assert_col(&rows, &commits, 1, 0);
        assert_linear(&rows, &commits, 1, 2);
        assert_linear(&rows, &commits, 2, 3);
        assert_linear(&rows, &commits, 3, 4);
        assert_no_diagonals(&rows, &commits, 1);
        assert_no_diagonals(&rows, &commits, 2);
        assert_no_diagonals(&rows, &commits, 3);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_simple_branch_and_merge() {
        //   1 (merge: parents 2, 3)
        //  / \
        // 2   3
        //  \ /
        //   4
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        // Commit 1 starts in column 0
        assert_col(&rows, &commits, 1, 0);
        // First parent (2) should stay in column 0
        assert_linear(&rows, &commits, 1, 2);
        // Commit 3 should be in a different column
        assert_ne!(
            rows[2].node_col, rows[1].node_col,
            "Branch commit 3 should be in different column from 2"
        );
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_linear_branch_no_diagonals() {
        // main:   1 → 2 → 5
        // branch: 3 → 4 (branched from 2, not yet merged)
        // Topological order: 1, 3, 2, 4, 5
        // Wait — topological + time order means children before parents.
        // Actually: 3 is newer than 2 but 1 is newest.
        // 1's parent is 2, 3's parent is 2, 2's parent is 5, 4 is...
        // Let me simplify:
        //
        // Commits in order (newest first):
        // 1 (parent: 2)  — latest on main
        // 3 (parent: 4)  — latest on branch
        // 2 (parent: 5)  — main continues
        // 4 (parent: 5)  — branch continues
        // 5 (parent: none) — root
        let commits = vec![
            commit(1, &[2]),
            commit(3, &[4]),
            commit(2, &[5]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        // 1 and 2 should be in the same column (linear on main)
        assert_linear(&rows, &commits, 1, 2);
        // 3 and 4 should be in the same column (linear on branch)
        assert_linear(&rows, &commits, 3, 4);
        // No diagonals for linear commits
        assert_no_diagonals(&rows, &commits, 2);
        assert_no_diagonals(&rows, &commits, 4);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_many_linear_commits_stay_in_column() {
        // 10 linear commits: 1→2→3→...→10
        let commits: Vec<_> = (1..=10)
            .map(|i| {
                if i == 10 {
                    commit(i, &[])
                } else {
                    commit(i, &[i + 1])
                }
            })
            .collect();
        let rows = layout_graph(&commits);

        for i in 0..9 {
            assert_linear(&rows, &commits, i as u32 + 1, i as u32 + 2);
            assert_no_diagonals(&rows, &commits, i as u32 + 1);
        }
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_parallel_branches_stable_columns() {
        // Two parallel branches that don't interact:
        // Branch A: 1→3→5
        // Branch B: 2→4→6
        // Interleaved by time: 1, 2, 3, 4, 5, 6
        let commits = vec![
            commit(1, &[3]),
            commit(2, &[4]),
            commit(3, &[5]),
            commit(4, &[6]),
            commit(5, &[]),
            commit(6, &[]),
        ];
        let rows = layout_graph(&commits);

        // Branch A stays in one column
        assert_linear(&rows, &commits, 1, 3);
        assert_linear(&rows, &commits, 3, 5);
        // Branch B stays in another column
        assert_linear(&rows, &commits, 2, 4);
        assert_linear(&rows, &commits, 4, 6);
        // They should be in different columns
        assert_ne!(rows[0].node_col, rows[1].node_col);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_branch_after_merge_stays_stable() {
        // 1 (merge: 2, 3)
        // 2 (parent: 4)
        // 3 (parent: 4)
        // 4 (parent: 5)
        // 5 (root)
        // Commit 4 will have a convergence diagonal (lane from 3 merges in)
        // but commit 4 itself should be in col 0 (main line)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        assert_linear(&rows, &commits, 4, 5);
        // Commit 4 has a convergence line (branch lane merging in) — that's correct
        let has_convergence = rows[3].lines.iter().any(|&(f, t, _)| f != t);
        assert!(
            has_convergence,
            "Commit 4 should have convergence line from branch"
        );
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_pr_merge_pattern() {
        // Typical GitHub PR merge pattern:
        // 1 = merge commit (parents: 2, 3)
        // 2 = previous main commit (parent: 5)
        // 3 = PR head commit (parent: 4)
        // 4 = PR commit (parent: 5)
        // 5 = older main commit (root)
        //
        // Expected: main line (1→2→5) in col 0, PR branch (3→4) in col 1
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[5]),
            commit(3, &[4]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        // Main line stays in column 0
        assert_col(&rows, &commits, 1, 0);
        assert_linear(&rows, &commits, 1, 2);
        // PR commits should be linear with each other
        assert_linear(&rows, &commits, 3, 4);
        // After merge resolves, commit 5 should be in main column
        assert_linear(&rows, &commits, 2, 5);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_merge_new_lane_no_vertical_but_diagonal() {
        // A merge commit creates a NEW lane for its second parent: the merge row
        // gets the diagonal but NO vertical for that lane — nothing feeds it from
        // above, so a vertical would be a stub hanging in empty space. The
        // renderer draws the incoming line for the next row from the diagonal's
        // endpoint instead.
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[]),
        ];
        let rows = layout_graph(&commits);

        let merge_row = &rows[0];
        let has_diagonal = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == merge_row.node_col && t != f);
        assert!(has_diagonal, "Merge commit should have a diagonal edge");

        let target_col = merge_row
            .lines
            .iter()
            .find(|&&(f, t, _)| f == merge_row.node_col && t != f)
            .unwrap()
            .1;
        let has_vertical = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == target_col && t == target_col);
        assert!(
            !has_vertical,
            "Newly created merge lane (col {target_col}) should not have vertical"
        );
    }

    #[test]
    fn test_merge_into_feature_main_continues() {
        // Main is merged INTO a feature branch. Main's lane is newly
        // created by the merge, so NO vertical in the merge row. But
        // in subsequent rows (before commit 3 appears), main's lane
        // should have verticals.
        //
        // 1 (merge: 2, 3)  — feature merges main in
        // 2 (parent: 4)    — feature branch continues
        // 3 (parent: 5)    — main continues
        // 4 (parent: 6)    — feature
        // 5 (parent: 6)    — main
        // 6 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[5]),
            commit(4, &[6]),
            commit(5, &[6]),
            commit(6, &[]),
        ];
        let rows = layout_graph(&commits);

        let merge_row = &rows[0];
        let main_col = rows[2].node_col; // commit 3's column

        // Merge row has diagonal to main
        let has_diagonal = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == merge_row.node_col && t == main_col);
        assert!(has_diagonal, "Merge should have diagonal to main's column");

        // Merge row should NOT have vertical for new lane
        let has_vertical_at_merge = merge_row
            .lines
            .iter()
            .any(|&(f, t, _)| f == main_col && t == main_col);
        assert!(
            !has_vertical_at_merge,
            "New merge lane should not have vertical in merge row"
        );

        // But row 1 (commit 2) SHOULD have main's vertical continuation
        let row_2 = &rows[1]; // commit 2
        let has_main_vertical = row_2
            .lines
            .iter()
            .any(|&(f, t, _)| f == main_col && t == main_col);
        assert!(
            has_main_vertical,
            "Main lane (col {main_col}) must continue vertically in rows after the merge"
        );

        // Main should be linear: 3 → 5
        assert_linear(&rows, &commits, 3, 5);
        assert_colors_consistent(&rows);
    }

    #[test]
    fn test_convergence_no_vertical_on_consumed_lane() {
        // When two lanes converge at a commit, the consumed lane should
        // NOT have a vertical continuation.
        // 1 (merge: 2, 3)
        // 2 (parent: 4)    — both 2 and 3 point to 4
        // 3 (parent: 4)
        // 4 (parent: 5)
        // 5 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4]),
            commit(3, &[4]),
            commit(4, &[5]),
            commit(5, &[]),
        ];
        let rows = layout_graph(&commits);

        // At commit 4 (row 3): two lanes converge. The consumed lane
        // should not have a vertical continuation.
        let conv_row = &rows[3]; // commit 4
        let convergence_sources: Vec<usize> = conv_row
            .lines
            .iter()
            .filter(|&&(f, t, _)| f != t && t == conv_row.node_col)
            .map(|&(f, _, _)| f)
            .collect();

        for src_col in &convergence_sources {
            let has_vertical = conv_row
                .lines
                .iter()
                .any(|&(f, t, _)| f == *src_col && t == *src_col);
            assert!(
                !has_vertical,
                "Consumed convergence lane (col {src_col}) should not have vertical"
            );
        }
    }

    #[test]
    fn test_parent_not_in_scope_still_has_line() {
        // When a commit's parent is not in the loaded set,
        // the commit should still have a downward continuation
        // line (not appear as an orphan dot).
        // Commit 1's parent (2) is NOT in the list.
        let commits = vec![commit(1, &[2])];
        let rows = layout_graph(&commits);

        // Should have a continuation line downward
        let has_continuation = rows[0]
            .lines
            .iter()
            .any(|&(f, t, _)| f == rows[0].node_col && t == rows[0].node_col);
        assert!(
            has_continuation,
            "Commit with out-of-scope parent should still have a continuation line"
        );
    }

    #[test]
    fn test_sequential_merges() {
        // Multiple PRs merged in sequence:
        // 1 (merge: 2, 3)  — merge PR-A
        // 2 (merge: 4, 5)  — merge PR-B
        // 3 (parent: 4)    — PR-A commit
        // 4 (parent: 6)    — main
        // 5 (parent: 6)    — PR-B commit
        // 6 (root)
        let commits = vec![
            commit(1, &[2, 3]),
            commit(2, &[4, 5]),
            commit(3, &[4]),
            commit(4, &[6]),
            commit(5, &[6]),
            commit(6, &[]),
        ];
        let rows = layout_graph(&commits);

        // Main line: 1→2→4→6 should all be in col 0
        assert_col(&rows, &commits, 1, 0);
        assert_linear(&rows, &commits, 1, 2);
        assert_linear(&rows, &commits, 2, 4);
        assert_linear(&rows, &commits, 4, 6);
        assert_colors_consistent(&rows);
    }
}
