//! A lazy commit walk that reproduces `git log --graph`'s order.
//!
//! ## What order that is, and why it is not the one gitkay shows today
//!
//! `--graph` implies `--topo-order`, which is **not** date order. gitkay asks
//! libgit2 for `TIME | TOPOLOGICAL`, and that is measurably `git rev-list
//! --date-order`: on a kernel clone the first 120 rows matched it exactly and shared
//! only 82 commits with `--topo-order`'s first 120. The visible difference is that
//! date order stacks a maintainer's merges together and pushes what they merged
//! hundreds of rows down, where topological order shows each merge followed by the
//! commits it brought in — which is the shape everyone recognises from `git log
//! --graph`.
//!
//! ## The algorithm
//!
//! Kahn's algorithm over the reachable DAG, with a **LIFO** ready-queue:
//!
//! - `indegree[c]` counts c itself plus each of its children in the walk, so a commit
//!   is ready to emit at exactly 1 — git's own convention, kept because the
//!   off-by-one is easier to check against git than a plain child count.
//! - Emitting a commit decrements each parent's indegree and pushes any parent that
//!   reaches 1 onto a **stack**, parents in order.
//!
//! The stack is what produces the grouping. A merge pushes parent 1 (the previous
//! mainline commit) then parent 2 (the merged branch tip), so parent 2 pops first and
//! the walk descends into the merged branch before continuing along the mainline.
//! Swap the queue for a date-ordered one and you get `--date-order` instead; that one
//! difference is the whole distinction between the two.
//!
//! ## What makes it lazy
//!
//! Kahn's needs `indegree[c]` to be FINAL before emitting c, which naively means
//! discovering every child of c — i.e. traversing the whole history, which is the 45s
//! this exists to avoid.
//!
//! Generation numbers make it bounded instead. Every child of c has
//! `gen(child) > gen(c)` strictly, so once every commit with generation above some
//! floor has been discovered, `indegree` is final for every commit above that floor.
//! The walk therefore alternates: expand the frontier in decreasing generation order
//! down to a floor, then emit everything the floor makes safe, then lower it.
//!
//! That is the same insight `git log --topo-order` uses, and it is why git needs a
//! commit-graph to be fast here: without generation numbers there is no floor, and
//! the only way to finalise an indegree is to walk everything.
//!
//! ## Commits the commit-graph does not know
//!
//! A stale graph is the ORDINARY state, not an edge case: `git fetch` does not update
//! the file, so every commit pulled since it was written is missing from it — the
//! kernel clone this was developed against had a HEAD the graph had never heard of.
//! Declining there would make the whole feature unreachable in practice.
//!
//! Such commits are treated as generation `u32::MAX`, which is what git does, and it
//! is sound for the same reason: git writes commit-graphs **closed under ancestry**,
//! so a commit's absence implies its children are absent too. The missing set is
//! therefore upward-closed, and giving all of it the same maximal generation makes
//! the frontier expand every missing commit before any known one — exactly the order
//! the floor rule needs.
//!
//! That soundness rests entirely on the closure property, so it is CHECKED rather
//! than assumed: a known commit with an unknown parent means the file is not
//! ancestor-closed, and the walk declines instead of emitting a parent above its own
//! children — the one corruption its caller could not detect.
//!
//! ## More than one tip
//!
//! `--all` seeds hundreds of them, and one tip is routinely an ancestor of another —
//! every tag on a commit the current branch descends from. git filters those out
//! before it starts, by computing indegrees down to the lowest tip's generation; that
//! is a whole-history pass on a repository with an old tag, and it is exactly the pass
//! this walk exists to avoid.
//!
//! So a tip goes on the stack unconditionally and is filtered at the moment it is
//! POPPED, by the indegree the floor has just made final: above 1 means some child has
//! not emitted yet, so the tip is not a starting point at all and is dropped — its last
//! child's emission pushes it back at the right time. The emitted sequence is the same
//! either way, because the stack order is not disturbed by dropping an entry that could
//! not have been emitted there.
//!
//! What the tips DO decide is how much gets expanded before row one: the walk only
//! ever looks at the top of the stack, so a tip there that cannot be emitted yet holds
//! everything up until its generation is cleared. The newest tip on top is what keeps
//! that from costing anything — see `history::topo_tips`.

use std::collections::{BinaryHeap, HashMap};

use crate::commitgraph::CommitGraph;

/// A commit's generation paired with its oid, ordered so a `BinaryHeap` pops the
/// highest generation first. The oid breaks ties only to make the order total; which
/// commit wins a tie does not matter, because commits of equal generation are never
/// ancestors of one another.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct ByGeneration(u32, git2::Oid);

/// How many commits one expansion round discovers before the walk checks whether it
/// can emit again.
///
/// Purely a batching constant: the result is identical for any value, only the number
/// of "can I emit yet" checks changes. Sized so an ordinary screenful never needs a
/// second round.
const EXPAND_BATCH: usize = 512;

/// What the walk knows about one discovered commit: its generation as the walk sees it
/// (`u32::MAX` for one the graph does not hold), and its indegree in git's convention
/// (1 = ready). The generation is kept rather than re-read because a lookup is a binary
/// search through a file, and the walk asks repeatedly.
#[derive(Clone, Copy)]
struct Node {
    generation: u32,
    indegree: u32,
}

/// The state of one lazy topological walk.
pub struct TopoWalk<'a> {
    repo: &'a git2::Repository,
    graph: &'a CommitGraph,
    first_parent: bool,
    /// Commits discovered but not yet expanded, highest generation first. Its peak
    /// generation is the floor below which indegrees are not yet final.
    frontier: BinaryHeap<ByGeneration>,
    /// Every commit discovered. One map and not two: the generation and the indegree
    /// are written together in `discover` and have the same key set forever after, so
    /// two maps would be a second copy of every walked oid — tens of MB on the
    /// 1.47M-commit clone this walk exists for — plus a second hash on every lookup,
    /// keeping in step by convention alone.
    nodes: HashMap<git2::Oid, Node>,
    /// Ready to emit, LIFO — see the module docs.
    ready: Vec<git2::Oid>,
    /// Commits already handed out, so a diamond cannot emit one twice.
    emitted: std::collections::HashSet<git2::Oid>,
    /// Whether this walk has GIVEN UP — an unclosed commit-graph, or a commit whose
    /// object the odb would not hand back part-way through. Latched, and what makes
    /// `done` mean "ran out" rather than "stopped": both are a `None` from `next`, and
    /// the two queues do not tell them apart. A refusal can empty them (the last commit
    /// popped off `ready`, the frontier already spent), and `take` reading that as the
    /// end would hand back a topological PREFIX indistinguishable from a complete
    /// answer — which is exactly the shape `take` is all-or-nothing to avoid.
    declined: bool,
}

impl<'a> TopoWalk<'a> {
    /// Start a walk from `tips`.
    ///
    /// Infallible: a tip the commit-graph has never heard of is the ordinary
    /// stale-graph case and is handled, so the only thing that can make this walk
    /// give up is discovered later, while expanding — see `expand`.
    pub fn new(
        repo: &'a git2::Repository,
        graph: &'a CommitGraph,
        tips: &[git2::Oid],
        first_parent: bool,
    ) -> Self {
        let mut walk = Self {
            repo,
            graph,
            first_parent,
            frontier: BinaryHeap::new(),
            nodes: HashMap::new(),
            ready: Vec::new(),
            emitted: std::collections::HashSet::new(),
            declined: false,
        };
        // Seeded in reverse so the FIRST tip ends up on top of the LIFO stack and is
        // emitted first, matching `git log`'s treatment of the order its tips were
        // given in. Deduped forward, so a repository naming one commit twice (a tag on
        // a branch tip, under `--all`) keeps that commit at its FIRST position, which
        // is the one git keeps.
        let mut seen = std::collections::HashSet::new();
        let unique: Vec<git2::Oid> = tips.iter().copied().filter(|o| seen.insert(*o)).collect();
        for &tip in unique.iter().rev() {
            walk.discover(tip);
            walk.ready.push(tip);
        }
        walk
    }

    /// Record a commit as part of the walk, at indegree 1, and queue it for
    /// expansion. Idempotent — a commit reached twice keeps its first indegree.
    ///
    /// A commit the graph has never heard of is taken as `u32::MAX`; see the module
    /// docs for why that is sound and where the assumption behind it is checked.
    fn discover(&mut self, oid: git2::Oid) {
        if self.nodes.contains_key(&oid) {
            return;
        }
        let generation = self.graph.generation(oid).unwrap_or(u32::MAX);
        self.nodes.insert(
            oid,
            Node {
                generation,
                indegree: 1,
            },
        );
        self.frontier.push(ByGeneration(generation, oid));
    }

    /// This commit's generation as the walk sees it — `u32::MAX` for one the graph
    /// does not hold.
    fn generation_of(&self, oid: git2::Oid) -> u32 {
        self.nodes.get(&oid).map_or(u32::MAX, |n| n.generation)
    }

    /// This commit's parents, honouring `--first-parent` exactly as the sorted walk
    /// does.
    fn parents(&self, oid: git2::Oid) -> Option<Vec<git2::Oid>> {
        let commit = self.repo.find_commit(oid).ok()?;
        Some(crate::history::commit_parents(&commit, self.first_parent))
    }

    /// Discover one commit's parents and count this commit as a child of each — the
    /// step that builds the indegrees Kahn's algorithm consumes.
    fn expand(&mut self, oid: git2::Oid) -> Option<()> {
        let known = self.generation_of(oid) != u32::MAX;
        for parent in self.parents(oid)? {
            self.discover(parent);
            // The ancestry-closure check. A commit the graph knows must have parents
            // it knows; if it does not, the file is not closed under ancestry and the
            // `u32::MAX` treatment above stops being sound — a missing ANCESTOR would
            // outrank its own descendants and be drawn above them.
            if known && self.generation_of(parent) == u32::MAX {
                return None;
            }
            self.nodes.get_mut(&parent)?.indegree += 1;
        }
        Some(())
    }

    /// The generation above which every indegree is final: nothing left to expand can
    /// be a child of a commit at or above it. `None` once the frontier is empty, when
    /// every indegree is final.
    fn floor(&self) -> Option<u32> {
        self.frontier.peek().map(|ByGeneration(g, _)| *g)
    }

    /// Whether `oid` can be emitted: ready, and high enough that no unexpanded commit
    /// could still turn out to be a child of it.
    fn safe_to_emit(&self, oid: git2::Oid) -> bool {
        let g = self.generation_of(oid);
        self.floor().is_none_or(|floor| floor < g)
    }

    /// The next commit in `git log --graph` order, or `None` when the walk is done or
    /// the commit-graph cannot answer for something it needs.
    pub fn next(&mut self) -> Option<git2::Oid> {
        loop {
            // Emit as soon as the top of the stack is provably final. Checking only
            // the TOP is enough: the stack is the emission order, so nothing below it
            // can be emitted first anyway.
            if let Some(&top) = self.ready.last()
                && self.safe_to_emit(top)
            {
                self.ready.pop();
                // The floor has just made this indegree final, which is the first
                // moment it can be read. Above 1 means a child of this commit has not
                // emitted, so it is not ready after all — a seeded tip that turned out
                // to be another tip's ancestor. Drop it; the child's emission pushes it
                // back. Every entry pushed by that route arrives at exactly 1 and, being
                // safe to emit, can gain no further child, so this filters tips alone.
                if self.nodes.get(&top).map(|n| n.indegree) != Some(1) {
                    continue;
                }
                if !self.emitted.insert(top) {
                    continue;
                }
                let Some(parents) = self.parents(top) else {
                    return self.decline();
                };
                for parent in parents {
                    let Some(node) = self.nodes.get_mut(&parent) else {
                        return self.decline();
                    };
                    node.indegree -= 1;
                    if node.indegree == 1 {
                        self.ready.push(parent);
                    }
                }
                return Some(top);
            }
            // Not safe yet (or nothing ready): lower the floor.
            if self.frontier.is_empty() {
                return None;
            }
            for _ in 0..EXPAND_BATCH {
                let Some(ByGeneration(_, oid)) = self.frontier.pop() else {
                    break;
                };
                if self.expand(oid).is_none() {
                    return self.decline();
                }
            }
        }
    }

    /// Latch the refusal and answer `None`. Every bail-out goes through here, so
    /// "gave up" cannot be reported as "ran out" by a path that forgot to say so.
    const fn decline(&mut self) -> Option<git2::Oid> {
        self.declined = true;
        None
    }

    /// The next `max` commits, or `None` if the walk had to give up part-way — an
    /// all-or-nothing answer, since a partial one in this order is indistinguishable
    /// from a complete one and would be drawn as though it were.
    pub fn take(&mut self, max: usize) -> Option<Vec<git2::Oid>> {
        let mut out = Vec::with_capacity(max.min(4096));
        while out.len() < max {
            // A finished walk is success; a walk that cannot answer is not, and the
            // two are told apart by whether anything is left to do. Asked AFTER the
            // call, which also covers the already-finished case: `next` over an empty
            // stack and an empty frontier changes nothing, so `done` is still true.
            match self.next() {
                Some(oid) => out.push(oid),
                None if self.done() => break,
                None => return None,
            }
        }
        Some(out)
    }

    /// Whether the walk has genuinely run out of commits, as opposed to having
    /// declined one. What tells a caller driving `next` itself which of the two a
    /// `None` was — `take` asks it on the caller's behalf.
    ///
    /// The empty queues are not enough on their own: a refusal can leave them empty
    /// too (see `declined`), and the two answers are then the same shape.
    pub fn done(&self) -> bool {
        !self.declined && self.ready.is_empty() && self.frontier.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_repo::{
        commit_file, commit_merge, temp_repo, write_commit_graph, write_commit_graph_exact,
    };

    /// Every commit reachable from `tips`, in a topological order produced the slow,
    /// obvious way: full indegrees over the whole DAG, then Kahn's with a LIFO
    /// queue seeded with the tips that are not one another's ancestors. This is
    /// `git log --graph`'s algorithm without the laziness — `init_topo_walk` down to
    /// generation zero — so it is the oracle the lazy walk has to agree with, the
    /// laziness being the only thing under test.
    fn brute_force(repo: &git2::Repository, tips: &[git2::Oid]) -> Vec<git2::Oid> {
        let mut indegree: HashMap<git2::Oid, u32> = HashMap::new();
        let mut stack = Vec::new();
        for &tip in tips {
            if indegree.insert(tip, 1).is_none() {
                stack.push(tip);
            }
        }
        while let Some(oid) = stack.pop() {
            for p in repo.find_commit(oid).unwrap().parent_ids() {
                let fresh = !indegree.contains_key(&p);
                *indegree.entry(p).or_insert(1) += 1;
                if fresh {
                    stack.push(p);
                }
            }
        }
        // Reversed, so the FIRST tip is on top of the stack — git's own
        // `prio_queue_reverse` over the tips it kept.
        let mut ready: Vec<git2::Oid> = tips
            .iter()
            .copied()
            .filter(|t| indegree[t] == 1)
            .rev()
            .collect();
        let mut out = Vec::new();
        let mut done = std::collections::HashSet::new();
        while let Some(oid) = ready.pop() {
            if !done.insert(oid) {
                continue;
            }
            out.push(oid);
            for p in repo.find_commit(oid).unwrap().parent_ids() {
                let d = indegree.get_mut(&p).unwrap();
                *d -= 1;
                if *d == 1 {
                    ready.push(p);
                }
            }
        }
        out
    }

    /// The shared merged-history fixture, extended with a second merge so the walk
    /// has to descend into a merged branch more than once — which is where a LIFO
    /// queue and a date-ordered one visibly disagree.
    fn two_merges(repo: &git2::Repository) -> (git2::Oid, git2::Oid) {
        let (_root, _main_c, _side_c, merge) = crate::tests::merged_history(repo);
        // A second topic branch off the first merge, merged back in turn.
        repo.branch("side2", &repo.find_commit(merge).unwrap(), false)
            .unwrap();
        let mainline = repo.head().unwrap().name().unwrap().to_string();
        repo.set_head("refs/heads/side2").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let topic = commit_file(repo, "h.txt", "topic", "on-topic");
        repo.set_head(&mainline).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let main2 = commit_file(repo, "f.txt", "main2", "on-main-2");
        let tip = commit_merge(repo, main2, topic, "merge topic");
        (tip, topic)
    }

    #[test]
    fn the_lazy_walk_matches_a_brute_force_topological_order() {
        let (_dir, repo) = temp_repo();
        let (tip, _) = two_merges(&repo);
        write_commit_graph(&repo, &[tip]);
        let graph = CommitGraph::for_repo(&repo).expect("the graph just written");
        let mut walk = TopoWalk::new(&repo, &graph, &[tip], false);
        let got = walk.take(1000).expect("a complete walk");
        assert_eq!(got, brute_force(&repo, &[tip]));
        assert!(got.len() >= 6, "the fixture should have real depth");
    }

    /// `--all` seeds every branch and tag, and the walk has to reproduce git's order
    /// over the lot of them — including the two tips that are the same commit, which
    /// git keeps once.
    #[test]
    fn several_tips_are_walked_in_gits_own_order() {
        let (_dir, repo) = temp_repo();
        let (tip, topic) = two_merges(&repo);
        write_commit_graph(&repo, &[tip]);
        let graph = CommitGraph::for_repo(&repo).unwrap();
        let tips = [tip, topic, tip];
        let got = TopoWalk::new(&repo, &graph, &tips, false)
            .take(1000)
            .expect("a complete walk");
        assert_eq!(got, brute_force(&repo, &[tip, topic]));
        assert_eq!(
            got.iter().collect::<std::collections::HashSet<_>>().len(),
            got.len(),
            "a commit named twice is still emitted once"
        );
    }

    /// A tip that is another tip's ancestor — every tag on a commit the checked-out
    /// branch descends from, so the ordinary state of `--all`. It is not a starting
    /// point, and emitting it from the stack position it was seeded at draws it above
    /// its own children.
    ///
    /// The ancestor is seeded FIRST here, which is what makes the test bite: tips
    /// arrive newest-committer-date first, and a tag on an amended or rebased commit
    /// routinely carries a newer date than the branch tip that descends from it. Remove
    /// the indegree filter in `next` and the root is emitted first, ahead of the entire
    /// history that leads down to it.
    #[test]
    fn a_tip_that_is_another_tips_ancestor_waits_for_its_children() {
        let (_dir, repo) = temp_repo();
        let (tip, _) = two_merges(&repo);
        // The oldest commit in the fixture: reachable from every other one, so it can
        // only be emitted last.
        let root = brute_force(&repo, &[tip]).pop().unwrap();
        write_commit_graph(&repo, &[tip]);
        let graph = CommitGraph::for_repo(&repo).unwrap();
        let got = TopoWalk::new(&repo, &graph, &[root, tip], false)
            .take(1000)
            .expect("a complete walk");
        assert_eq!(got, brute_force(&repo, &[root, tip]));
        assert_eq!(
            got.last(),
            Some(&root),
            "the ancestor tip belongs at the bottom: {got:?}"
        );
    }

    /// The property a reader actually sees, and the whole reason for the change: a
    /// merge is followed by what it merged, not by the next merge.
    #[test]
    fn a_merge_is_followed_by_the_commits_it_brought_in() {
        let (_dir, repo) = temp_repo();
        let (tip, topic) = two_merges(&repo);
        write_commit_graph(&repo, &[tip]);
        let graph = CommitGraph::for_repo(&repo).unwrap();
        let mut walk = TopoWalk::new(&repo, &graph, &[tip], false);
        let got = walk.take(1000).unwrap();
        let _ = topic;
        let msg = |oid: git2::Oid| -> String {
            let commit = repo.find_commit(oid).unwrap();
            commit.summary().ok().flatten().unwrap_or("").to_owned()
        };
        let order: Vec<String> = got.iter().map(|&o| msg(o)).collect();
        // The tip merge, then its SECOND parent's line of history, before the walk
        // returns to the mainline it merged into.
        assert_eq!(order[0], "merge topic");
        assert_eq!(
            order[1], "on-topic",
            "the merged branch comes before the mainline: {order:?}"
        );
        assert_eq!(
            order[2], "on-main-2",
            "…and only then the mainline: {order:?}"
        );
    }

    /// A commit-graph that predates the newest commits — what every repository has
    /// moments after a fetch, since `git fetch` does not update the file.
    #[test]
    fn commits_the_graph_has_never_heard_of_are_still_ordered_correctly() {
        let (_dir, repo) = temp_repo();
        let (old_tip, _) = two_merges(&repo);
        // The graph covers only what existed then.
        write_commit_graph(&repo, &[old_tip]);
        let extra = commit_file(&repo, "later.txt", "1", "later 1");
        let tip = commit_file(&repo, "later.txt", "2", "later 2");
        let graph = CommitGraph::for_repo(&repo).unwrap();
        assert_eq!(
            graph.generation(extra),
            None,
            "deliberately not in the graph"
        );

        let mut walk = TopoWalk::new(&repo, &graph, &[tip], false);
        let got = walk.take(1000).expect("a complete walk");
        assert_eq!(got, brute_force(&repo, &[tip]));
        assert_eq!(got[0], tip);
        assert_eq!(got[1], extra);
    }

    /// The soundness of treating an unknown commit as maximal generation rests on
    /// git writing graphs closed under ancestry. A file that is not closed is
    /// refused rather than walked, because the failure it would otherwise produce —
    /// an ancestor drawn above its own descendants — is one the caller cannot see.
    #[test]
    fn a_graph_that_is_not_closed_under_ancestry_is_refused() {
        let (_dir, repo) = temp_repo();
        let root = commit_file(&repo, "a.txt", "1", "root");
        let mid = commit_file(&repo, "a.txt", "2", "mid");
        let tip = commit_file(&repo, "a.txt", "3", "tip");

        // The control: a graph holding an ancestor PREFIX is closed, and walking it
        // is sound — that is the ordinary stale-graph case, not a violation.
        write_commit_graph(&repo, &[root]);
        let graph = CommitGraph::for_repo(&repo).unwrap();
        assert_eq!(graph.generation(mid), None);
        let mut walk = TopoWalk::new(&repo, &graph, &[tip], false);
        assert_eq!(
            walk.take(10),
            Some(vec![tip, mid, root]),
            "a prefix is fine"
        );

        // The violation: a file holding a commit but not its own parent. git never
        // writes one; a corrupt file is what this stands in for. `mid` is missing
        // while `root` beneath it is present, so the `u32::MAX` treatment would put
        // `mid` ABOVE `tip`, which is the inversion the guard exists to catch.
        write_commit_graph_exact(&repo, &[tip, root]);
        let graph = CommitGraph::for_repo(&repo).unwrap();
        assert!(graph.generation(tip).is_some());
        assert_eq!(graph.generation(mid), None, "the hole this test is about");
        let mut walk = TopoWalk::new(&repo, &graph, &[tip], false);
        assert!(walk.take(10).is_none(), "an unclosed graph must be refused");
        // …and it stays refused however empty the queues end up. `done` answering
        // true after a refusal is what would let `take` hand back a topological
        // PREFIX as though it were the whole answer.
        assert!(!walk.done(), "a walk that gave up has not run out");
        assert!(walk.take(10).is_none(), "and it does not change its mind");
    }

    #[test]
    fn first_parent_follows_the_mainline_alone() {
        let (_dir, repo) = temp_repo();
        let (tip, _) = two_merges(&repo);
        write_commit_graph(&repo, &[tip]);
        let graph = CommitGraph::for_repo(&repo).unwrap();
        let mut walk = TopoWalk::new(&repo, &graph, &[tip], true);
        let got = walk.take(1000).unwrap();
        // Every row is the previous row's FIRST parent, and nothing else appears.
        for pair in got.windows(2) {
            let first = repo.find_commit(pair[0]).unwrap().parent_id(0).unwrap();
            assert_eq!(first, pair[1], "--first-parent must walk the mainline");
        }
    }
}
