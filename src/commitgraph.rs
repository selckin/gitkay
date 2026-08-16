//! Reading git's commit-graph file — specifically the **generation numbers**, which
//! are the one thing in it gitkay cannot get from libgit2.
//!
//! ## Why this file is worth reading at all
//!
//! A sorted libgit2 revwalk parses the whole reachable history before yielding row
//! one: measured at **45s and 1.79GB of peak RSS** on a 1.47M-commit kernel clone,
//! for 200 rows. git had the same problem and solved it with this file — the same
//! query through `git log --topo-order` goes from **29.5s to 0.020s** once it
//! exists, in 34MB.
//!
//! libgit2 cannot be made to use it. `revwalk.c` never mentions the commit-graph,
//! and the measurement agrees: with the file present the same walk took 45.3s
//! against 45.1s without. Neither `git2` nor `libgit2-sys` expose any commit-graph
//! API either, so the format is read here.
//!
//! ## What a generation number buys
//!
//! Generation number v1 is the *topological level*: 1 for a root commit, and
//! `1 + max(parents)` otherwise. So `gen(parent) < gen(child)`, **strictly and
//! always** — which is exactly the invariant a lazy walk cannot otherwise
//! establish without traversing the whole DAG, and the reason
//! `provisional_commits` is an approximation today.
//!
//! ## What is deliberately NOT parsed
//!
//! Only `OIDF` (fanout), `OIDL` (the sorted oid list) and `CDAT` (commit data) are
//! read, because the only question asked of this file is "what is this commit's
//! generation number?" — git2 already supplies parents, trees and timestamps, and
//! reading them twice could only introduce a disagreement. That leaves out `EDGE`
//! (octopus parents), `GDA2` (corrected commit dates), and `BIDX`/`BDAT` (the
//! changed-path Bloom filters, which would separately accelerate a path filter and
//! are a different feature).
//!
//! It also means the **split chain needs no position arithmetic**: a commit lives in
//! exactly one layer, so a lookup asks each layer in turn and the first hit wins. The
//! global-position mapping that a parent-reading implementation would need does not
//! arise.
//!
//! ## Reading strategy
//!
//! `pread` against an open file, never a full read and never an mmap. The kernel's
//! graph is 88MB and a walk asks about a few thousand commits, so slurping it would
//! cost more memory than the answers are worth, and an mmap would cost a dependency
//! (`memmap2`) to save syscalls that do not show up in a profile: the fanout narrows
//! a lookup to the commits sharing a first byte (~1/256 of the file), so a lookup is
//! ~13 probes of 20 bytes plus one 4-byte read.
//!
//! Every read is bounds-checked against the chunk it belongs to and every failure
//! answers `None`. A commit-graph is a *cache* — a corrupt or truncated one must
//! degrade to "no generation numbers available", never to a panic or a wrong answer,
//! because a wrong generation would silently break the graph layout it is meant to
//! guarantee.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// `CGPH`, the commit-graph magic.
const SIGNATURE: &[u8; 4] = b"CGPH";
/// The only file version git has ever written.
const VERSION: u8 = 1;

const CHUNK_OID_FANOUT: &[u8; 4] = b"OIDF";
const CHUNK_OID_LOOKUP: &[u8; 4] = b"OIDL";
const CHUNK_COMMIT_DATA: &[u8; 4] = b"CDAT";

/// Bytes of `CDAT` after the tree oid: two 4-byte parent positions, then the
/// packed generation + commit time.
const CDAT_AFTER_TREE: usize = 16;
/// Where the packed `generation << 2 | time_high` sits inside a `CDAT` record,
/// measured from the end of the tree oid.
const CDAT_GENERATION_AT: usize = 8;

/// One commit-graph file. A repository has either exactly one of these or a chain
/// of them (see `CommitGraph::open`).
struct Layer {
    file: File,
    /// 20 for SHA-1, 32 for SHA-256.
    hash_len: usize,
    /// `fanout[b]` is the number of oids whose first byte is `<= b`, so the
    /// candidates for first byte `b` are `fanout[b-1]..fanout[b]` — which is what
    /// turns a lookup into ~13 probes instead of ~21.
    fanout: [u32; 256],
    commits: u32,
    /// File offsets of the two chunks a lookup reads.
    oid_lookup: u64,
    commit_data: u64,
}

impl Layer {
    /// Parse one file's header and chunk table, or `None` for anything that is not
    /// a commit-graph this code can read: wrong magic, a future version, an unknown
    /// hash, a missing chunk, or a chunk too short for the commit count it claims.
    fn open(path: &Path) -> Option<Self> {
        let file = File::open(path).ok()?;
        let mut header = [0u8; 8];
        file.read_exact_at(&mut header, 0).ok()?;
        if &header[0..4] != SIGNATURE || header[4] != VERSION {
            return None;
        }
        let hash_len = match header[5] {
            1 => 20,
            2 => 32,
            _ => return None,
        };
        let chunks = header[6] as usize;
        // The table of contents is `chunks + 1` entries: each names a chunk and its
        // offset, and the terminating entry gives the end of the last one. That
        // trailing entry is what makes every chunk's LENGTH knowable, which is what
        // the bounds checks below rest on.
        let mut toc = vec![0u8; (chunks + 1).checked_mul(12)?];
        file.read_exact_at(&mut toc, 8).ok()?;
        let entry = |i: usize| -> (&[u8], u64) {
            let at = i * 12;
            (
                &toc[at..at + 4],
                u64::from_be_bytes(toc[at + 4..at + 12].try_into().unwrap_or_default()),
            )
        };
        let find = |id: &[u8; 4]| -> Option<(u64, u64)> {
            (0..chunks).find_map(|i| {
                let (this, off) = entry(i);
                // The next entry's offset is this chunk's end — the terminating
                // entry included, which is the whole reason it exists.
                (this == id).then(|| (off, entry(i + 1).1))
            })
        };
        let (fanout_at, fanout_end) = find(CHUNK_OID_FANOUT)?;
        let (oid_lookup, oid_end) = find(CHUNK_OID_LOOKUP)?;
        let (commit_data, data_end) = find(CHUNK_COMMIT_DATA)?;

        // Every chunk must END inside the file. The table of contents is the file's
        // own claim about itself, so checking a chunk's length against it alone
        // passes a TRUNCATED file — the table still promises the records, they are
        // simply not there. Caught here the whole graph is refused; caught later, by
        // a read failing, the file would look usable and answer `None` for whichever
        // commits fell off the end, which reads as "not in the graph" and is a much
        // worse lie than "no graph".
        let size = file.metadata().ok()?.len();
        if fanout_end > size || oid_end > size || data_end > size {
            return None;
        }

        if fanout_end.checked_sub(fanout_at)? < 256 * 4 {
            return None;
        }
        let mut raw = [0u8; 256 * 4];
        file.read_exact_at(&mut raw, fanout_at).ok()?;
        let mut fanout = [0u32; 256];
        for (slot, chunk) in fanout.iter_mut().zip(raw.chunks_exact(4)) {
            *slot = u32::from_be_bytes(chunk.try_into().unwrap_or_default());
        }
        // Monotonic by construction; a file that is not says nothing trustworthy
        // about where an oid lives, and the binary search below would read outside
        // its own chunk.
        if fanout.windows(2).any(|w| w[0] > w[1]) {
            return None;
        }
        let commits = fanout[255];

        let need = u64::from(commits) * hash_len as u64;
        if oid_end.checked_sub(oid_lookup)? < need {
            return None;
        }
        let need = u64::from(commits) * (hash_len + CDAT_AFTER_TREE) as u64;
        if data_end.checked_sub(commit_data)? < need {
            return None;
        }
        Some(Self {
            file,
            hash_len,
            fanout,
            commits,
            oid_lookup,
            commit_data,
        })
    }

    /// This layer's oid at `pos`, read straight out of `OIDL`.
    fn oid_at(&self, pos: u32, buf: &mut [u8]) -> Option<()> {
        let at = self.oid_lookup + u64::from(pos) * self.hash_len as u64;
        self.file.read_exact_at(buf, at).ok()
    }

    /// Where `oid` sits in this layer, or `None` when it is not in it.
    fn position(&self, oid: &[u8]) -> Option<u32> {
        let first = *oid.first()? as usize;
        let mut lo = if first == 0 {
            0
        } else {
            self.fanout[first - 1]
        };
        let mut hi = self.fanout[first];
        let mut buf = [0u8; 32];
        let buf = &mut buf[..self.hash_len];
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            self.oid_at(mid, buf)?;
            match buf[..].cmp(oid) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The generation number recorded for the commit at `pos`.
    ///
    /// `None` for zero, which is not a real generation — a root commit's is 1 — but
    /// the marker a pre-2.19 file leaves when it was written before generation
    /// numbers existed. Treating it as a number would put every such commit below
    /// every root, which is precisely the parent-above-child inversion this exists
    /// to rule out.
    fn generation(&self, pos: u32) -> Option<u32> {
        let at = self.commit_data
            + u64::from(pos) * (self.hash_len + CDAT_AFTER_TREE) as u64
            + (self.hash_len + CDAT_GENERATION_AT) as u64;
        let mut buf = [0u8; 4];
        self.file.read_exact_at(&mut buf, at).ok()?;
        // The upper 30 bits are the generation; the low 2 are the commit time's
        // high bits, which nothing here reads (git2 supplies times).
        let generation = u32::from_be_bytes(buf) >> 2;
        (generation != 0).then_some(generation)
    }
}

/// A repository's commit-graph: one file, or a chain of them.
pub struct CommitGraph {
    /// Base layer first, as the chain file lists them. A commit is in exactly one.
    layers: Vec<Layer>,
}

impl CommitGraph {
    /// Open whatever commit-graph `info_dir` (a repository's `objects/info`) holds.
    ///
    /// Both on-disk shapes are covered, and both occur in the wild: `git gc` and
    /// `git maintenance` write the single `commit-graph`, while
    /// `fetch.writeCommitGraph=true` writes a split chain under `commit-graphs/`.
    /// `None` when there is none, or when what is there cannot be read — a missing
    /// commit-graph is the ordinary case, not an error, since neither `git clone`
    /// nor `git fetch` writes one by default.
    pub fn open(info_dir: &Path) -> Option<Self> {
        if let Some(single) = Layer::open(&info_dir.join("commit-graph")) {
            return Some(Self {
                layers: vec![single],
            });
        }
        let split = info_dir.join("commit-graphs");
        let chain = std::fs::read_to_string(split.join("commit-graph-chain")).ok()?;
        let mut layers = Vec::new();
        for name in chain.lines().map(str::trim).filter(|l| !l.is_empty()) {
            // Hex only. The chain file names the files to open, so anything else
            // would let a repository choose a path — and a repository is untrusted
            // input, which is the same reason the write layer carries paths as raw
            // bytes rather than trusting what a diff displayed.
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            layers.push(Layer::open(&split.join(format!("graph-{name}.graph")))?);
        }
        (!layers.is_empty()).then_some(Self { layers })
    }

    /// `open` for a repository, looking beside its object database.
    ///
    /// `commondir`, not `path`: a linked worktree has its own gitdir but shares the
    /// object database — and with it the commit-graph — with the main one.
    pub fn for_repo(repo: &git2::Repository) -> Option<Self> {
        Self::open(&repo.commondir().join("objects").join("info"))
    }

    /// How many commits this graph describes, across every layer. What it is for is
    /// saying whether a graph COVERS the repository: a walk can only trust
    /// generation numbers for commits that are in it, and one written before the
    /// last few thousand commits landed is not the same thing as no graph at all.
    pub fn len(&self) -> usize {
        self.layers.iter().map(|l| l.commits as usize).sum()
    }

    /// `oid`'s generation number — its topological level, so strictly greater than
    /// every parent's.
    ///
    /// `None` when the commit is not in the graph (written after it, or reachable
    /// only from a ref the write did not cover) or when the file records no
    /// generation for it. A caller ordering a walk by this must treat `None` as
    /// "cannot order exactly" rather than substituting a value: a commit assumed
    /// newest that is not would be drawn above its own children.
    pub fn generation(&self, oid: git2::Oid) -> Option<u32> {
        let bytes = oid.as_bytes();
        self.layers
            .iter()
            .find_map(|l| l.position(bytes).and_then(|pos| l.generation(pos)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build a commit-graph file from `(oid, generation)` pairs, in git's own
    /// layout: header, chunk table of contents, then OIDF/OIDL/CDAT.
    ///
    /// The fixture is written here rather than by shelling out to `git
    /// commit-graph write` so the suite keeps depending on nothing but the
    /// filesystem — and so a test can build files git would never write (a
    /// zeroed generation, a truncated chunk) which are exactly the ones the
    /// reader has to refuse.
    fn write_graph(path: &Path, entries: &[(git2::Oid, u32)]) {
        let mut sorted = entries.to_vec();
        sorted.sort_by_key(|(oid, _)| *oid);
        let n = sorted.len() as u32;

        let mut fanout = [0u32; 256];
        for (oid, _) in &sorted {
            // Every bucket from this oid's first byte upward counts it.
            for slot in &mut fanout[oid.as_bytes()[0] as usize..] {
                *slot += 1;
            }
        }
        let mut oidf = Vec::new();
        for v in fanout {
            oidf.extend_from_slice(&v.to_be_bytes());
        }
        let mut oidl = Vec::new();
        let mut cdat = Vec::new();
        for (oid, generation) in &sorted {
            oidl.extend_from_slice(oid.as_bytes());
            cdat.extend_from_slice(&[0u8; 20]); // tree oid, unread
            cdat.extend_from_slice(&0x7000_0000u32.to_be_bytes()); // parent 1: none
            cdat.extend_from_slice(&0x7000_0000u32.to_be_bytes()); // parent 2: none
            cdat.extend_from_slice(&(generation << 2).to_be_bytes());
            cdat.extend_from_slice(&0u32.to_be_bytes()); // commit time, unread
        }

        let mut out = vec![b'C', b'G', b'P', b'H', VERSION, 1, 3, 0];
        let toc_len = 4 * 12;
        let mut at = out.len() as u64 + toc_len as u64;
        for (id, chunk) in [
            (CHUNK_OID_FANOUT, &oidf),
            (CHUNK_OID_LOOKUP, &oidl),
            (CHUNK_COMMIT_DATA, &cdat),
        ] {
            out.extend_from_slice(id);
            out.extend_from_slice(&at.to_be_bytes());
            at += chunk.len() as u64;
        }
        out.extend_from_slice(&[0u8; 4]); // terminating entry: id 0, end offset
        out.extend_from_slice(&at.to_be_bytes());
        out.extend_from_slice(&oidf);
        out.extend_from_slice(&oidl);
        out.extend_from_slice(&cdat);
        assert_eq!(n, fanout[255]);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(path)
            .unwrap()
            .write_all(&out)
            .unwrap();
    }

    fn oid(n: u8) -> git2::Oid {
        let mut raw = [0u8; 20];
        // Vary the FIRST byte, so the fanout is actually exercised rather than
        // every oid landing in one bucket.
        raw[0] = n;
        raw[19] = n;
        git2::Oid::from_bytes(&raw).unwrap()
    }

    #[test]
    fn a_single_file_graph_answers_every_generation_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        let entries: Vec<(git2::Oid, u32)> =
            (1u8..=40).map(|n| (oid(n), u32::from(n) * 3)).collect();
        write_graph(&info.join("commit-graph"), &entries);

        let g = CommitGraph::open(&info).expect("a graph this code just wrote");
        assert_eq!(g.len(), 40);
        for (o, want) in &entries {
            assert_eq!(g.generation(*o), Some(*want), "generation of {o}");
        }
        // A commit the graph does not hold is `None`, not a guess.
        assert_eq!(g.generation(oid(200)), None);
    }

    #[test]
    fn a_split_chain_is_read_layer_by_layer() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        let graphs = info.join("commit-graphs");
        // Two layers, disjoint as git writes them: a commit is in exactly one.
        write_graph(
            &graphs.join("graph-aa11.graph"),
            &[(oid(1), 1), (oid(2), 2)],
        );
        write_graph(
            &graphs.join("graph-bb22.graph"),
            &[(oid(3), 3), (oid(4), 4)],
        );
        std::fs::write(graphs.join("commit-graph-chain"), "aa11\nbb22\n").unwrap();

        let g = CommitGraph::open(&info).expect("the chain");
        assert_eq!(g.len(), 4, "both layers counted");
        for n in 1u8..=4 {
            assert_eq!(g.generation(oid(n)), Some(u32::from(n)), "layer lookup {n}");
        }
        assert_eq!(g.generation(oid(9)), None);
    }

    #[test]
    fn the_single_file_wins_over_a_chain_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        write_graph(&info.join("commit-graph"), &[(oid(1), 7)]);
        write_graph(
            &info.join("commit-graphs").join("graph-aa11.graph"),
            &[(oid(1), 99)],
        );
        std::fs::write(
            info.join("commit-graphs").join("commit-graph-chain"),
            "aa11\n",
        )
        .unwrap();
        assert_eq!(
            CommitGraph::open(&info).unwrap().generation(oid(1)),
            Some(7)
        );
    }

    /// A zeroed generation is a pre-2.19 file that predates generation numbers,
    /// not a commit at level zero — the lowest real level is 1, a root's. Reading
    /// it as a number would sort every such commit below every root, which is the
    /// parent-above-child inversion the whole feature exists to prevent.
    #[test]
    fn a_zero_generation_is_refused_rather_than_believed() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        write_graph(&info.join("commit-graph"), &[(oid(1), 0), (oid(2), 5)]);
        let g = CommitGraph::open(&info).unwrap();
        assert_eq!(g.generation(oid(1)), None, "not a generation, a marker");
        assert_eq!(g.generation(oid(2)), Some(5), "its neighbour still answers");
    }

    #[test]
    fn a_file_that_is_not_a_commit_graph_is_declined_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        std::fs::create_dir_all(&info).unwrap();
        // No file at all.
        assert!(CommitGraph::open(&info).is_none());
        // Something else entirely.
        std::fs::write(info.join("commit-graph"), b"not a commit graph").unwrap();
        assert!(CommitGraph::open(&info).is_none());
        // Right magic, a version this code does not know.
        std::fs::write(info.join("commit-graph"), b"CGPH\x02\x01\x03\x00").unwrap();
        assert!(CommitGraph::open(&info).is_none());
        // Right header, truncated before the chunk table.
        std::fs::write(info.join("commit-graph"), b"CGPH\x01\x01\x03\x00").unwrap();
        assert!(CommitGraph::open(&info).is_none());
    }

    /// A truncated CDAT must be caught by the header check, not by reading past
    /// the end of the chunk and returning whatever follows it as a generation.
    #[test]
    fn a_chunk_too_short_for_its_commit_count_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        let path = info.join("commit-graph");
        write_graph(&path, &[(oid(1), 1), (oid(2), 2), (oid(3), 3)]);
        let full = std::fs::read(&path).unwrap();
        // Drop the last record's worth of bytes; the fanout still claims three.
        std::fs::write(&path, &full[..full.len() - 36]).unwrap();
        assert!(CommitGraph::open(&info).is_none());
    }

    /// The chain file names files to open, and a repository is untrusted input.
    #[test]
    fn a_chain_naming_something_other_than_a_hash_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        let graphs = info.join("commit-graphs");
        write_graph(&graphs.join("graph-aa11.graph"), &[(oid(1), 1)]);
        for bad in ["../../../etc/passwd", "aa11/../../x", "aa 11", "aa11$"] {
            std::fs::write(graphs.join("commit-graph-chain"), format!("{bad}\n")).unwrap();
            assert!(
                CommitGraph::open(&info).is_none(),
                "chain naming {bad:?} must be refused"
            );
        }
        // …and the well-formed one still works, so the guard is not just refusing
        // everything.
        std::fs::write(graphs.join("commit-graph-chain"), "aa11\n").unwrap();
        assert!(CommitGraph::open(&info).is_some());
    }

    /// Generation numbers are only useful if the strict-parent-below-child property
    /// they promise actually holds in a file, so pin what the reader guarantees
    /// about the values it hands back: they are read verbatim, in oid order that has
    /// nothing to do with generation order.
    #[test]
    fn generations_are_read_verbatim_regardless_of_oid_order() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        // Deliberately anti-correlated: the lowest oid has the highest generation,
        // so a reader that accidentally returned the position, or read a
        // neighbouring record, would be caught.
        let entries: Vec<(git2::Oid, u32)> =
            (1u8..=20).map(|n| (oid(n), u32::from(21 - n))).collect();
        write_graph(&info.join("commit-graph"), &entries);
        let g = CommitGraph::open(&info).unwrap();
        for (o, want) in &entries {
            assert_eq!(g.generation(*o), Some(*want));
        }
    }
}
