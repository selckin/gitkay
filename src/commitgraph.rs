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
//! libgit2 cannot be made to use it for that. Its revwalk does read the file (see the
//! last section), but the file makes no difference to how it orders: with the graph
//! present the same walk took 45.3s against 45.1s without. Neither `git2` nor
//! `libgit2-sys` exposes any commit-graph API either, so the format is read here.
//!
//! ## What a generation number buys
//!
//! Generation number v1 is the *topological level*: 1 for a root commit, and
//! `1 + max(parents)` otherwise. So `gen(parent) < gen(child)`, **strictly and
//! always** — which is exactly the invariant a lazy walk cannot otherwise
//! establish without traversing the whole DAG, and the reason
//! `provisional_commits` is an approximation today.
//!
//! ## The second question: which paths a commit changed
//!
//! `BIDX`/`BDAT` hold the **changed-path Bloom filters**, present only when the graph
//! was written with `--changed-paths` (`git gc` writes neither). They answer "did this
//! commit touch this path?" with "definitely not" or "maybe", off a few bits, where the
//! path filter otherwise compares trees — see `ChangedPaths`, and `history::PathBloom`
//! for what asks. The filter is computed against a commit's FIRST parent, which is
//! exactly the question gitkay's path filter asks.
//!
//! ## The third question: who a commit's parents are
//!
//! `CDAT`'s two parent columns and the `EDGE` chunk behind them, read by `parents_at`.
//! git2 supplies parents too, but it parses the commit object out of the pack to do it,
//! and that was the whole cost of a lazy walk: 24.1s of a 29.0s filtered pass over a
//! 1.465M-commit clone, against 4.5s reading them here. `GDA2` (corrected commit dates)
//! is still not read — git2 supplies times, and reading them twice could only introduce
//! a disagreement.
//!
//! **Parents are POSITIONS, and they are global across a split chain.** That is what
//! makes the position arithmetic necessary: a layer's `OIDL` holds only its own
//! commits, so an upper layer could not otherwise name a parent in the base. Layers are
//! numbered in chain-file order, base first, each starting at the running total below
//! it (`bases`). Verified against a two-layer chain git wrote, where the upper layer's
//! oldest commit names its parent as global 0 — a commit living in the base layer.
//! An oid LOOKUP still asks each layer in turn and takes the first hit; only the
//! parent columns need the mapping.
//!
//! ## Reading strategy
//!
//! Two, because the two questions are asked at completely different rates.
//!
//! A generation lookup is `pread` against an open file, never a full read and never an
//! mmap. The kernel's graph is 88MB and a walk asks about a few thousand commits, so
//! slurping it would cost more memory than the answers are worth, and an mmap would
//! cost a dependency (`memmap2`) to save syscalls that do not show up in a profile: the
//! fanout narrows a lookup to the commits sharing a first byte (~1/256 of the file), so
//! a lookup is ~13 probes of 20 bytes plus one 4-byte read.
//!
//! A path filter asks **once per commit examined**, which on a cold pathspec is every
//! commit in the repository, and there those ~13 probes ARE the cost of the feature —
//! measured at ~13.5µs a commit against the ~10µs tree comparison they were meant to
//! replace, i.e. a loss. So `ChangedPaths` loads the oid list and the filter index into
//! memory for as long as a filtered walk runs; see there for the size.
//!
//! Every read is bounds-checked against the chunk it belongs to and every failure
//! answers `None`. A commit-graph is a *cache* — a corrupt or truncated one must
//! degrade to "no generation numbers available", never to a panic or a wrong answer,
//! because a wrong generation would silently break the graph layout it is meant to
//! guarantee, and a wrong Bloom answer would drop commits from a filtered view.
//!
//! ## libgit2 reads this file too
//!
//! Not for ORDERING — that measurement stands, 45.1s without the file and 45.3s with —
//! but its revwalk does take parents from it, exactly as `parents_at` now does.
//! Measured while building the test fixtures: a hand-written graph whose parent columns
//! said "no parent" truncated `git2`'s own walk to a single commit. So any fixture
//! written for this module must describe its commits truthfully, or the walk a test
//! compares against is the one that is wrong — and a fixture that lies about a
//! POSITION is worse still: `write_commit_graph_with_bad_parent` names one past the end
//! of the chain, and libgit2 does not truncate on that, it hangs. It is usable only
//! where nothing afterwards walks the repository through libgit2.

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
const CHUNK_BLOOM_INDEX: &[u8; 4] = b"BIDX";
const CHUNK_BLOOM_DATA: &[u8; 4] = b"BDAT";
/// `EDGE`: an octopus merge's parents past the first, which do not fit `CDAT`'s two
/// columns. Optional — a repository with no octopus merge has no such chunk.
const CHUNK_EXTRA_EDGES: &[u8; 4] = b"EDGE";

/// A parent column holding no parent: the second column of an ordinary commit, both
/// columns of a root.
pub const GRAPH_PARENT_NONE: u32 = 0x7000_0000;
/// Set on the SECOND parent column when the parents past the first live in `EDGE`,
/// the rest of the word being the index they start at.
const GRAPH_EXTRA_EDGES: u32 = 0x8000_0000;
/// Set on the last `EDGE` entry of one commit's list — the only thing that ends it.
const GRAPH_EDGE_LAST: u32 = 0x8000_0000;
/// The position in an `EDGE` entry, once the terminator bit is taken off.
const GRAPH_EDGE_MASK: u32 = 0x7fff_ffff;

/// How many parents this reader will follow before refusing the record.
///
/// git sets no limit and an octopus merge is small in practice (the largest in the
/// kernel is 66). The cap is here because the list is terminated by a BIT IN THE FILE:
/// a corrupt `EDGE` chunk whose terminator never arrives would otherwise be read to the
/// end of the chunk, and a repository is untrusted input.
const MAX_PARENTS: usize = 256;

/// `BDAT` opens with three 4-byte settings — hash version, hash count, bits per entry
/// — and the filters follow them.
const BDAT_HEADER: u64 = 12;

/// The largest single filter this reader will load. git sizes one at
/// `bits_per_entry` (10) bits per changed path and stops at `max_changed_paths` (512),
/// so a real filter is ≤ 640 bytes; the cap is here because the length comes out of the
/// file's own index, and a corrupt one must not turn into an allocation.
const MAX_BLOOM_BYTES: u64 = 64 * 1024;

/// The two seeds git hashes a path with, and the double-hashing scheme
/// (`hash_i = hash0 + i * hash1`) they feed — `fill_bloom_key` in `bloom.c`.
const BLOOM_SEED_0: u32 = 0x293a_e76f;
const BLOOM_SEED_1: u32 = 0x7e64_6e2c;

/// Bytes of `CDAT` after the tree oid: two 4-byte parent positions, then the
/// packed generation + commit time.
const CDAT_AFTER_TREE: usize = 16;
/// Where the packed `generation << 2 | time_high` sits inside a `CDAT` record,
/// measured from the end of the tree oid.
const CDAT_GENERATION_AT: usize = 8;

/// One layer's changed-path Bloom filters: where they are, and how they were hashed.
///
/// Present only when the graph was written with `--changed-paths`, which is not what
/// `git gc` does — so this is `None` far more often than not, and every caller has to
/// degrade to asking the repository itself.
struct Bloom {
    /// `BIDX`: one 4-byte CUMULATIVE end offset per commit, in the same order `OIDL`
    /// uses. A commit's filter is the bytes between its predecessor's end and its own,
    /// so a zero-length span means "no filter was computed for this commit".
    index: u64,
    /// Where the filters themselves start — past `BDAT`'s settings header.
    data: u64,
    data_end: u64,
    /// 1 hashes each byte as a SIGNED char (git's original, and its known bug: a path
    /// with a byte over 0x7f hashes differently on ARM than on x86), 2 as unsigned.
    hash_version: u32,
    num_hashes: u32,
}

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
    /// `EDGE`, when the file carries one: start and end, so a read past the list can
    /// be refused rather than wander into the next chunk.
    extra_edges: Option<(u64, u64)>,
    bloom: Option<Bloom>,
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
        // The Bloom chunks are optional in every sense: absent unless the graph was
        // written with `--changed-paths`, and refused here rather than trusted if
        // anything about them does not add up. `None` costs a caller only the diff it
        // was going to do anyway, where a wrong answer silently drops commits from a
        // filtered view.
        let bloom = Self::open_bloom(&file, &find, commits, size);
        // Absent unless the repository has an octopus merge, so its absence says
        // nothing is wrong — but a chunk that runs past the file is a file that cannot
        // be trusted about anything, which is the rule the three above follow.
        let extra_edges = match find(CHUNK_EXTRA_EDGES) {
            Some((at, end)) if end > size || end < at => return None,
            found => found,
        };

        Some(Self {
            file,
            hash_len,
            fanout,
            commits,
            oid_lookup,
            commit_data,
            extra_edges,
            bloom,
        })
    }

    /// `BIDX`/`BDAT`, when the file carries them and they describe this layer's commit
    /// count. Every failure is a `None` — the filters are an optimisation, and half of
    /// one is worse than none.
    fn open_bloom(
        file: &File,
        find: &impl Fn(&[u8; 4]) -> Option<(u64, u64)>,
        commits: u32,
        size: u64,
    ) -> Option<Bloom> {
        let (index, index_end) = find(CHUNK_BLOOM_INDEX)?;
        let (data, data_end) = find(CHUNK_BLOOM_DATA)?;
        if index_end > size || data_end > size {
            return None;
        }
        // One 4-byte cumulative offset per commit, and a settings header before the
        // filters.
        if index_end.checked_sub(index)? < u64::from(commits) * 4
            || data_end.checked_sub(data)? < BDAT_HEADER
        {
            return None;
        }
        let mut header = [0u8; BDAT_HEADER as usize];
        file.read_exact_at(&mut header, data).ok()?;
        let word =
            |i: usize| u32::from_be_bytes(header[i * 4..i * 4 + 4].try_into().unwrap_or_default());
        let (hash_version, num_hashes) = (word(0), word(1));
        // Two hash versions exist and they differ in one byte's signedness; a third
        // would hash paths some way this code does not know, and guessing is exactly
        // the wrong-but-plausible answer this whole reader avoids. `num_hashes` is
        // bounded because it is a loop count read out of the file.
        if !(1..=2).contains(&hash_version) || num_hashes == 0 || num_hashes > 64 {
            return None;
        }
        Some(Bloom {
            index,
            data: data + BDAT_HEADER,
            data_end,
            hash_version,
            num_hashes,
        })
    }

    /// This layer's oid at `pos`, read straight out of `OIDL`.
    fn oid_at(&self, pos: u32, buf: &mut [u8]) -> Option<()> {
        let at = self.oid_lookup + u64::from(pos) * self.hash_len as u64;
        self.file.read_exact_at(buf, at).ok()
    }

    /// Where `oid` sits in this layer, or `None` when it is not in it.
    fn position(&self, oid: &[u8]) -> Option<u32> {
        let mut buf = [0u8; 32];
        position_by(&self.fanout, oid, |at| {
            let slot = &mut buf[..self.hash_len];
            self.oid_at(at, slot)?;
            Some((*slot).cmp(oid))
        })
    }

    /// The two parent columns of the commit at `pos`, verbatim — `GRAPH_PARENT_NONE`,
    /// a position, or (in the second) an `EDGE` index with `GRAPH_EXTRA_EDGES` set.
    fn parent_words(&self, pos: u32) -> Option<(u32, u32)> {
        let at = self.commit_data
            + u64::from(pos) * (self.hash_len + CDAT_AFTER_TREE) as u64
            + self.hash_len as u64;
        let mut buf = [0u8; 8];
        self.file.read_exact_at(&mut buf, at).ok()?;
        Some((
            u32::from_be_bytes(buf[0..4].try_into().ok()?),
            u32::from_be_bytes(buf[4..8].try_into().ok()?),
        ))
    }

    /// One `EDGE` entry, bounds-checked against the chunk rather than the file: past
    /// its end is a malformed record, not a parent belonging to the next chunk.
    fn extra_edge(&self, idx: u32) -> Option<u32> {
        let (start, end) = self.extra_edges?;
        let at = start.checked_add(u64::from(idx).checked_mul(4)?)?;
        if at.checked_add(4)? > end {
            return None;
        }
        let mut buf = [0u8; 4];
        self.file.read_exact_at(&mut buf, at).ok()?;
        Some(u32::from_be_bytes(buf))
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

/// Binary-search `OIDL` for `oid`, narrowed to the commits sharing its first byte by
/// `fanout`, with `cmp` supplying the oid at a position.
///
/// One search for both readers, which hold the same sorted list two different ways —
/// `Layer` `pread`s it, `ChangedPaths` keeps it in memory (see the module header for
/// why). The bounds and the fanout indexing are exactly what a second copy could get
/// subtly wrong, and a wrong `position` is a wrong generation or a wrong filter, both
/// of which read as data rather than as an error.
///
/// `None` from `cmp` — an unreadable record — takes the whole lookup with it.
fn position_by(
    fanout: &[u32; 256],
    oid: &[u8],
    mut cmp: impl FnMut(u32) -> Option<std::cmp::Ordering>,
) -> Option<u32> {
    let first = *oid.first()? as usize;
    let mut lo = if first == 0 { 0 } else { fanout[first - 1] };
    let mut hi = fanout[first];
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match cmp(mid)? {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Some(mid),
        }
    }
    None
}

/// The two 32-bit hashes git derives a Bloom key from — `fill_bloom_key`, which then
/// expands them to `num_hashes` bit positions as `hash0 + i * hash1`. Keeping the pair
/// rather than the expansion is what lets one key serve layers written with different
/// `num_hashes`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BloomKey(u32, u32);

/// One pathspec entry's keys: the path itself, then every ancestor directory.
///
/// git stores a changed file's path AND each of its leading directories in the filter,
/// so a directory pathspec answers directly; the ancestors are then queried too, which
/// only lowers the false-positive rate — a filter holding `a/b/c.txt` holds `a/b` and
/// `a` as well, so a miss on any of them means the path itself cannot be there.
pub struct PathKeys {
    keys: Vec<BloomKey>,
}

impl PathKeys {
    /// Keys for `path` under a graph's `hash_version`, or `None` when this path cannot
    /// be hashed the way the file was written.
    ///
    /// The refusal that matters is **version 1 with a byte over 0x7f**: git's original
    /// hash reads each byte as a `char`, whose signedness is the compiler's choice, so
    /// such a filter says different things depending on the machine that wrote it. A
    /// path we cannot hash unambiguously is one we decline to answer for — the diff
    /// below is always there to ask.
    pub fn for_path(path: &str, hash_version: u32) -> Option<Self> {
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            return None;
        }
        let signed = match hash_version {
            1 if path.is_ascii() => true,
            2 => false,
            _ => return None,
        };
        let key = |s: &str| {
            BloomKey(
                murmur3(BLOOM_SEED_0, s.as_bytes(), signed),
                murmur3(BLOOM_SEED_1, s.as_bytes(), signed),
            )
        };
        let mut keys = vec![key(path)];
        keys.extend(
            path.match_indices('/')
                .filter(|(at, _)| *at > 0)
                .map(|(at, _)| key(&path[..at])),
        );
        Some(Self { keys })
    }
}

/// git's murmur3-32 over `data`, seeded — `murmur3_seeded_v{1,2}` in `bloom.c`, whose
/// only difference is `signed`: version 1 sign-extends each byte (on any platform whose
/// `char` is signed, which is where the filters in the wild were written), version 2
/// does not.
fn murmur3(seed: u32, data: &[u8], signed: bool) -> u32 {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    let byte = |b: u8| -> u32 {
        if signed {
            // Sign-extended through i8 and back, which is what `(uint32_t)(char)b` does
            // where `char` is signed.
            b as i8 as i32 as u32
        } else {
            u32::from(b)
        }
    };
    let mut h = seed;
    let mut chunks = data.chunks_exact(4);
    for c in &mut chunks {
        let mut k = byte(c[0]) | (byte(c[1]) << 8) | (byte(c[2]) << 16) | (byte(c[3]) << 24);
        k = k.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h ^= k;
        h = h.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    let tail = chunks.remainder();
    if !tail.is_empty() {
        let mut k = 0u32;
        for (i, &b) in tail.iter().enumerate() {
            k ^= byte(b) << (8 * i);
        }
        k = k.wrapping_mul(C1).rotate_left(15).wrapping_mul(C2);
        h ^= k;
    }
    h ^= data.len() as u32;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// How many bits git gives a filter per path it records (`bits_per_entry`, and its
/// default 10). Written into `BDAT`'s header, but a READER never needs it: a filter's
/// length is what decides the modulus, and the length is in the index. Only the fixture
/// writer below has a use for it.
#[cfg(test)]
const BLOOM_BITS_PER_ENTRY: usize = 10;

/// The filter bytes git would write for a commit that changed `paths` — every path and
/// each of its leading directories, sized as `bloom.c` sizes one.
///
/// The fixture side of `definitely_unchanged`, `#[cfg(test)]` because nothing in gitkay
/// WRITES a commit-graph. It lives here, beside the reader, so both suites describe the
/// layout once: the `commitgraph` tests build files by hand, and `test_repo` builds them
/// from a real repository's diffs.
#[cfg(test)]
pub fn bloom_filter_bytes(paths: &[String], num_hashes: u32, hash_version: u32) -> Vec<u8> {
    let mut recorded: Vec<String> = Vec::new();
    for path in paths {
        let mut at: Option<&str> = Some(path.as_str());
        while let Some(p) = at {
            if !p.is_empty() && !recorded.iter().any(|r| r == p) {
                recorded.push(p.to_string());
            }
            at = p.rsplit_once('/').map(|(head, _)| head);
        }
    }
    let bits = recorded.len() * BLOOM_BITS_PER_ENTRY;
    let len = bits.div_ceil(8).max(1);
    let mut out = vec![0u8; len];
    let modulus = len as u64 * 8;
    for path in &recorded {
        let keys = PathKeys::for_path(path, hash_version).expect("a fixture path");
        // Only the path's OWN key is added: `for_path` also yields the ancestors, and
        // those are recorded above as paths in their own right, exactly as git does.
        let key = keys.keys[0];
        for i in 0..num_hashes {
            let h = key.0.wrapping_add(i.wrapping_mul(key.1));
            let pos = u64::from(h) % modulus;
            out[(pos / 8) as usize] |= 1 << (pos % 8);
        }
    }
    out
}

/// The `OIDF` chunk for `sorted` — the 256 cumulative counts git indexes an oid lookup
/// through, one per leading byte, big-endian.
///
/// Beside the reader for the reason `bloom_filter_bytes` is: both fixture writers need
/// it, and it is the half of the format a wrong value corrupts SILENTLY, since a
/// fanout that disagrees with `OIDL` sends the binary search to the wrong window rather
/// than failing to parse.
#[cfg(test)]
pub fn fanout_bytes(sorted: &[git2::Oid]) -> Vec<u8> {
    let mut fanout = [0u32; 256];
    for oid in sorted {
        // Every bucket from this oid's first byte upward counts it.
        for slot in &mut fanout[oid.as_bytes()[0] as usize..] {
            *slot += 1;
        }
    }
    let mut out = Vec::with_capacity(1024);
    for v in fanout {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
}

/// A whole commit-graph file: the header, the chunk table of contents with each
/// chunk's cumulative start offset, the terminating entry naming the end, then the
/// bodies — followed by `trailer` bytes of zeroes where the caller wants the file to
/// have git's trailing hash slot.
///
/// The offset arithmetic here is what both fixture writers had a copy of, and it is
/// the part with no safe failure: libgit2 reads this file too, so a TOC that is wrong
/// by twelve bytes does not fail a test in `commitgraph` — it quietly truncates
/// `git2`'s own revwalk in whatever suite happens to use the fixture. One copy, next
/// to the reader that has to agree with it.
#[cfg(test)]
pub fn graph_file_bytes(chunks: &[(&[u8; 4], &[u8])], trailer: usize) -> Vec<u8> {
    let mut out = vec![b'C', b'G', b'P', b'H', VERSION, 1, chunks.len() as u8, 0];
    // Each entry is a 4-byte id and an 8-byte offset, and the terminating entry is one
    // more of them — so the bodies start past `chunks.len() + 1` of the pair.
    let mut at = out.len() as u64 + (chunks.len() as u64 + 1) * 12;
    for (id, chunk) in chunks {
        out.extend_from_slice(*id);
        out.extend_from_slice(&at.to_be_bytes());
        at += chunk.len() as u64;
    }
    out.extend_from_slice(&[0u8; 4]); // terminating entry: id 0, end offset
    out.extend_from_slice(&at.to_be_bytes());
    for (_, chunk) in chunks {
        out.extend_from_slice(chunk);
    }
    out.extend(std::iter::repeat_n(0u8, trailer));
    out
}

/// Whether every one of `key`'s bits is set in `filter` — git's `bloom_filter_contains`,
/// with the filter's bytes as its words.
fn bloom_contains(filter: &[u8], key: BloomKey, num_hashes: u32) -> bool {
    let bits = filter.len() as u64 * 8;
    if bits == 0 {
        return false;
    }
    (0..num_hashes).all(|i| {
        let h = key.0.wrapping_add(i.wrapping_mul(key.1));
        let pos = u64::from(h) % bits;
        filter[(pos / 8) as usize] & (1 << (pos % 8)) != 0
    })
}

/// A repository's commit-graph: one file, or a chain of them.
pub struct CommitGraph {
    /// Base layer first, as the chain file lists them. A commit is in exactly one.
    layers: Vec<Layer>,
    /// Each layer's first GLOBAL position — the running total of the commits below it.
    ///
    /// The parent columns name a position across the WHOLE chain, not within a layer,
    /// and they have to: a layer's `OIDL` holds only its own commits, so a commit in an
    /// upper layer could not otherwise name a parent in the base. Verified against a
    /// two-layer chain git wrote, where the upper layer's oldest commit names its
    /// parent as global 0 — a commit that lives in the base layer.
    bases: Vec<u32>,
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
            return Some(Self::of(vec![single]));
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
        (!layers.is_empty()).then(|| Self::of(layers))
    }

    /// Layers in chain order, with the position each one starts at accumulated once.
    fn of(layers: Vec<Layer>) -> Self {
        let mut bases = Vec::with_capacity(layers.len());
        let mut at: u32 = 0;
        for layer in &layers {
            bases.push(at);
            at = at.saturating_add(layer.commits);
        }
        Self { layers, bases }
    }

    /// The layer holding global position `at`, and its position within that layer.
    fn locate(&self, at: u32) -> Option<(&Layer, u32)> {
        // Linear over the layers, which are one in the ordinary case and a handful in
        // a split chain — the search that matters is the one INSIDE a layer.
        self.layers
            .iter()
            .zip(&self.bases)
            .rev()
            .find(|(layer, base)| at >= **base && at - **base < layer.commits)
            .map(|(layer, base)| (layer, at - *base))
    }

    /// Where `oid` sits across the whole chain, or `None` when the graph does not hold
    /// it — which is the ordinary post-fetch state, not an error.
    pub fn position(&self, oid: git2::Oid) -> Option<u32> {
        let bytes = oid.as_bytes();
        self.layers
            .iter()
            .zip(&self.bases)
            .find_map(|(layer, base)| layer.position(bytes).map(|pos| base + pos))
    }

    /// The commit at a global position.
    pub fn oid_at(&self, at: u32) -> Option<git2::Oid> {
        let (layer, pos) = self.locate(at)?;
        let mut buf = [0u8; 32];
        let slot = &mut buf[..layer.hash_len];
        layer.oid_at(pos, slot)?;
        git2::Oid::from_bytes(slot).ok()
    }

    /// The generation recorded at a global position — `None` for the pre-2.19 zero
    /// marker, exactly as `generation`.
    pub fn generation_at(&self, at: u32) -> Option<u32> {
        let (layer, pos) = self.locate(at)?;
        layer.generation(pos)
    }

    /// The parents of the commit at a global position, as global positions.
    ///
    /// **This is what makes a lazy walk cheap**: the alternative is `find_commit`, which
    /// parses the commit object out of the pack, and on a filtered walk over a
    /// 1.465M-commit clone that was 19.2s of a 23.1s answer. Here it is one 8-byte read
    /// of two columns already beside the generation the walk just asked for.
    ///
    /// `None` is a MALFORMED record — a position past the end of the chain, an `EDGE`
    /// list running past its chunk or never terminating — and the caller declines
    /// rather than walking a graph that cannot describe its own shape. An empty `Vec`
    /// is the honest answer for a root commit.
    ///
    /// **The parents are taken on trust, as git and libgit2 take them.** A commit named
    /// here is in the graph by construction, so a file that is not closed under ancestry
    /// can no longer be caught the way it used to be — it writes `GRAPH_PARENT_NONE` for
    /// the parent it is missing, which reads as a root and truncates the walk. git never
    /// writes such a file; see `topo`'s module header for what that changed.
    pub fn parents_at(&self, at: u32) -> Option<Vec<u32>> {
        let (layer, pos) = self.locate(at)?;
        let (first, second) = layer.parent_words(pos)?;
        let mut out = Vec::new();
        if first == GRAPH_PARENT_NONE {
            // A second parent without a first is a record that contradicts itself.
            return (second == GRAPH_PARENT_NONE).then_some(out);
        }
        out.push(first);
        if second == GRAPH_PARENT_NONE {
            return Some(out);
        }
        if second & GRAPH_EXTRA_EDGES == 0 {
            out.push(second);
            return Some(out);
        }
        // Three or more parents: the rest are a run in `EDGE`, ended by a bit on the
        // last entry rather than by a count.
        let mut idx = second & GRAPH_EDGE_MASK;
        loop {
            let entry = layer.extra_edge(idx)?;
            out.push(entry & GRAPH_EDGE_MASK);
            if entry & GRAPH_EDGE_LAST != 0 {
                break;
            }
            if out.len() >= MAX_PARENTS {
                return None;
            }
            idx = idx.checked_add(1)?;
        }
        Some(out)
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

    /// Whether every layer carries changed-path filters — `ChangedPaths::open`'s
    /// question, asked without loading anything, which is what a "would this help?"
    /// check needs.
    pub fn has_changed_paths(&self) -> bool {
        self.layers.iter().all(|l| l.bloom.is_some())
    }

    /// `oid`'s generation number — its topological level, so strictly greater than
    /// every parent's.
    ///
    /// `None` when the commit is not in the graph (written after it, or reachable
    /// only from a ref the write did not cover) or when the file records no
    /// generation for it. A caller ordering a walk by this must treat `None` as
    /// "cannot order exactly" rather than substituting a value: a commit assumed
    /// newest that is not would be drawn above its own children.
    ///
    /// The walk asks `position` and `generation_at` separately, because it keeps the
    /// position to read the commit's parents with; this is the same pair for a caller
    /// that wants only the number. `#[allow(dead_code)]` because that caller is the
    /// suite — the app goes through the walk.
    #[allow(dead_code)]
    pub fn generation(&self, oid: git2::Oid) -> Option<u32> {
        self.generation_at(self.position(oid)?)
    }
}

/// The changed-path filters, loaded for QUERYING — one layer's oid list and filter
/// index each, held in memory.
///
/// A second reading mode, deliberately, and the reason is the access pattern rather
/// than taste. `generation` is asked a few thousand times by a walk, so it pays ~13
/// `pread`s a lookup to keep the file on disk; a path filter asks once per commit
/// EXAMINED, which on a cold pathspec is every commit in the repository, and there the
/// binary search is the cost of the feature — measured at ~13.5µs a commit against the
/// ~10µs tree lookup it was meant to replace, i.e. a loss. In memory the same search is
/// a few hundred nanoseconds.
///
/// The price is `hash_len + 4` bytes a commit while a filtered walk runs: 35MB on a
/// 1.47M-commit kernel clone, against the 1.79GB that walk's sorted alternative peaks
/// at. The filters themselves stay on disk, read one at a time — they are the part
/// nothing looks at twice.
pub struct ChangedPaths<'a> {
    layers: Vec<LoadedLayer<'a>>,
    hash_version: u32,
}

struct LoadedLayer<'a> {
    file: &'a File,
    hash_len: usize,
    fanout: [u32; 256],
    /// `OIDL` verbatim: the sorted oids, `hash_len` bytes each.
    oids: Vec<u8>,
    /// `BIDX`: each commit's cumulative filter end.
    index: Vec<u32>,
    /// Where the filters start, and end.
    data: u64,
    data_end: u64,
    num_hashes: u32,
}

impl<'a> ChangedPaths<'a> {
    /// Load `graph`'s filters, or `None` when it has none — the usual case, since only
    /// `git commit-graph write --changed-paths` writes them — or when its layers
    /// disagree about the hash, which git does not write and a reader must not average
    /// over.
    pub fn open(graph: &'a CommitGraph) -> Option<Self> {
        let mut version: Option<u32> = None;
        let mut layers = Vec::with_capacity(graph.layers.len());
        for layer in &graph.layers {
            let bloom = layer.bloom.as_ref()?;
            if *version.get_or_insert(bloom.hash_version) != bloom.hash_version {
                return None;
            }
            let mut oids = vec![0u8; layer.commits as usize * layer.hash_len];
            layer.file.read_exact_at(&mut oids, layer.oid_lookup).ok()?;
            let mut raw = vec![0u8; layer.commits as usize * 4];
            layer.file.read_exact_at(&mut raw, bloom.index).ok()?;
            layers.push(LoadedLayer {
                file: &layer.file,
                hash_len: layer.hash_len,
                fanout: layer.fanout,
                oids,
                index: raw
                    .chunks_exact(4)
                    .map(|c| u32::from_be_bytes(c.try_into().unwrap_or_default()))
                    .collect(),
                data: bloom.data,
                data_end: bloom.data_end,
                num_hashes: bloom.num_hashes,
            });
        }
        Some(Self {
            layers,
            hash_version: version?,
        })
    }

    /// Which murmur variant these filters were written with — what `PathKeys::for_path`
    /// needs, and the one thing a caller must ask before building a key.
    pub const fn hash_version(&self) -> u32 {
        self.hash_version
    }

    /// Whether this commit's changed-path filter rules the path OUT.
    ///
    /// **One direction only, and that asymmetry is the whole safety of the feature.**
    /// A Bloom filter answers "definitely not" or "maybe", so `true` here is a fact —
    /// the commit did not touch the path — and everything else is `false`, meaning ask
    /// the repository. A missing filter, a filter git marked too large, a commit
    /// written since the graph: all of them are `false`, so the only way to drop a
    /// commit wrongly is for the hash itself to be wrong, which is what this module's
    /// oracle test is for.
    ///
    /// The filter is the commit's diff against its FIRST parent (`bloom.c` diffs
    /// exactly that, and the empty tree for a root), which is the same question
    /// `history::commit_touches_paths` asks.
    pub fn definitely_unchanged(&self, oid: git2::Oid, keys: &PathKeys) -> bool {
        let bytes = oid.as_bytes();
        let Some((layer, pos)) = self
            .layers
            .iter()
            .find_map(|l| l.position(bytes).map(|pos| (l, pos)))
        else {
            return false;
        };
        let Some(filter) = layer.filter(pos) else {
            return false;
        };
        // A miss on the path OR on any of its ancestor directories rules it out; git
        // stores every ancestor of a changed path, so a filter holding the path holds
        // them too.
        keys.keys
            .iter()
            .any(|&key| !bloom_contains(&filter, key, layer.num_hashes))
    }
}

impl LoadedLayer<'_> {
    /// `Layer::position`, over the oid list in memory.
    fn position(&self, oid: &[u8]) -> Option<u32> {
        position_by(&self.fanout, oid, |at| {
            let at = at as usize * self.hash_len;
            Some(self.oids.get(at..at + self.hash_len)?.cmp(oid))
        })
    }

    /// This commit's filter bytes, or `None` for the zero-length span git records for a
    /// commit whose filter was never computed.
    fn filter(&self, pos: u32) -> Option<Vec<u8>> {
        let end = u64::from(*self.index.get(pos as usize)?);
        let start = if pos == 0 {
            0
        } else {
            u64::from(*self.index.get(pos as usize - 1)?)
        };
        let len = end.checked_sub(start)?;
        if len == 0 || len > MAX_BLOOM_BYTES || self.data + end > self.data_end {
            return None;
        }
        let mut out = vec![0u8; len as usize];
        self.file.read_exact_at(&mut out, self.data + start).ok()?;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// The changed-path half of a fixture: the settings `BDAT` states, and one filter
    /// per commit that has one. A commit missing from the map gets a zero-length span,
    /// which is how git records one whose filter was never computed.
    struct BloomFixture {
        hash_version: u32,
        num_hashes: u32,
        filters: std::collections::HashMap<git2::Oid, Vec<u8>>,
    }

    /// Build a commit-graph file from `(oid, generation)` pairs, in git's own
    /// layout: header, chunk table of contents, then OIDF/OIDL/CDAT.
    ///
    /// The fixture is written here rather than by shelling out to `git
    /// commit-graph write` so the suite keeps depending on nothing but the
    /// filesystem — and so a test can build files git would never write (a
    /// zeroed generation, a truncated chunk) which are exactly the ones the
    /// reader has to refuse.
    fn write_graph(path: &Path, entries: &[(git2::Oid, u32)]) {
        write_graph_inner(path, entries, None);
    }

    /// `write_graph`, plus the optional `BIDX`/`BDAT` pair that only
    /// `git commit-graph write --changed-paths` produces.
    fn write_graph_inner(path: &Path, entries: &[(git2::Oid, u32)], bloom: Option<&BloomFixture>) {
        let mut sorted = entries.to_vec();
        sorted.sort_by_key(|(oid, _)| *oid);
        let n = sorted.len() as u32;

        let oids: Vec<git2::Oid> = sorted.iter().map(|(oid, _)| *oid).collect();
        let oidf = fanout_bytes(&oids);
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

        // BIDX holds the CUMULATIVE end offset of each commit's filter, in OIDL order;
        // BDAT the settings header followed by the filters themselves.
        let (mut bidx, mut bdat) = (Vec::new(), Vec::new());
        if let Some(b) = bloom {
            for word in [b.hash_version, b.num_hashes, BLOOM_BITS_PER_ENTRY as u32] {
                bdat.extend_from_slice(&word.to_be_bytes());
            }
            for (oid, _) in &sorted {
                if let Some(filter) = b.filters.get(oid) {
                    bdat.extend_from_slice(filter);
                }
                let end = (bdat.len() - BDAT_HEADER as usize) as u32;
                bidx.extend_from_slice(&end.to_be_bytes());
            }
        }

        let chunks: Vec<(&[u8; 4], &[u8])> = if bloom.is_some() {
            vec![
                (CHUNK_OID_FANOUT, &oidf),
                (CHUNK_OID_LOOKUP, &oidl),
                (CHUNK_COMMIT_DATA, &cdat),
                (CHUNK_BLOOM_INDEX, &bidx),
                (CHUNK_BLOOM_DATA, &bdat),
            ]
        } else {
            vec![
                (CHUNK_OID_FANOUT, &oidf),
                (CHUNK_OID_LOOKUP, &oidl),
                (CHUNK_COMMIT_DATA, &cdat),
            ]
        };
        // No trailer: these fixtures are read by `commitgraph` alone, which never
        // looks at one.
        let out = graph_file_bytes(&chunks, 0);
        assert_eq!(n as usize, oids.len());
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

    /// The hash is the one thing here that cannot be checked against gitkay's own
    /// fixtures — a writer and a reader sharing a wrong hash agree perfectly — so it is
    /// pinned against values computed by a SEPARATE implementation, written from the
    /// published algorithm rather than from this port.
    ///
    /// The first block is murmur3-32's own published vectors at seed 0, which say the
    /// port is murmur3 at all; the second is git's two Bloom seeds over paths, which is
    /// what `fill_bloom_key` feeds. Beyond this the check that matters is a real
    /// repository's own filters, and that one cannot live in a suite that depends on no
    /// `git` binary: it was run by hand over 15,719 commits of a repository whose graph
    /// git wrote with `--changed-paths`, asserting no commit was ruled out for a path it
    /// really changed.
    #[test]
    fn the_path_hash_matches_an_independent_murmur3() {
        for (input, want) in [
            ("", 0x0000_0000u32),
            ("a", 0x3c25_69b2),
            ("abc", 0xb3dd_93fa),
            ("Hello, world!", 0xc036_3e43),
            ("The quick brown fox jumps over the lazy dog", 0x2e4f_f723),
        ] {
            assert_eq!(
                murmur3(0, input.as_bytes(), false),
                want,
                "murmur3 {input:?}"
            );
        }
        for (path, want) in [
            ("a/b.txt", (0x5e51_4f85u32, 0xc3ce_b2c2u32)),
            ("a", (0x8fc5_291a, 0xf720_b3be)),
            ("src", (0xe7ca_a49a, 0x9737_bd9d)),
            ("src/main.rs", (0xc679_f386, 0x492e_5467)),
        ] {
            let keys = PathKeys::for_path(path, 2).expect("an ascii path");
            assert_eq!(
                (keys.keys[0].0, keys.keys[0].1),
                want,
                "bloom key for {path:?}"
            );
        }
    }

    /// A path's keys are the path itself and every ancestor DIRECTORY, which is what
    /// git records for each changed file — so a filter can answer for `dir/sub` as well
    /// as for the file under it. Order matters only in that the path's own key is
    /// first, which is what the fixture writer adds.
    #[test]
    fn a_paths_keys_are_the_path_and_its_ancestors() {
        let keys = PathKeys::for_path("a/b/c.txt", 2).unwrap();
        let want: Vec<BloomKey> = ["a/b/c.txt", "a", "a/b"]
            .iter()
            .map(|p| PathKeys::for_path(p, 2).unwrap().keys[0])
            .collect();
        assert_eq!(keys.keys, want);
        // A trailing slash is not part of the name git hashed.
        assert_eq!(
            PathKeys::for_path("src/", 2).unwrap().keys,
            PathKeys::for_path("src", 2).unwrap().keys
        );
        assert!(PathKeys::for_path("", 2).is_none());
        assert!(PathKeys::for_path("/", 2).is_none());
    }

    /// Version 1 hashes each byte as a `char`, whose signedness is the compiler's
    /// choice — so a filter written for a path with a byte over 0x7f says different
    /// things on x86 and on ARM. Such a path is declined rather than guessed at; an
    /// ASCII one is unambiguous and still answered, and version 2 fixed the bug and
    /// answers for anything.
    #[test]
    fn version_one_declines_a_path_it_cannot_hash_unambiguously() {
        assert!(PathKeys::for_path("src/main.rs", 1).is_some());
        assert!(PathKeys::for_path("src/café.rs", 1).is_none());
        assert!(PathKeys::for_path("src/café.rs", 2).is_some());
        // An unknown version is not a version to guess at.
        assert!(PathKeys::for_path("src/main.rs", 3).is_none());
        // And the two versions really do differ once a byte is over 0x7f, which is what
        // makes the refusal above necessary rather than cautious.
        let high = "é".as_bytes();
        assert_ne!(
            murmur3(BLOOM_SEED_0, high, true),
            murmur3(BLOOM_SEED_0, high, false)
        );
    }

    /// The whole point, over a file built the way git builds one: a commit is ruled out
    /// for a path it did not change, and never for one it did.
    #[test]
    fn a_changed_path_filter_rules_out_the_paths_a_commit_did_not_touch() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        let changed = |paths: &[&str]| {
            bloom_filter_bytes(
                &paths.iter().map(|p| (*p).to_string()).collect::<Vec<_>>(),
                7,
                2,
            )
        };
        let filters = std::collections::HashMap::from([
            (oid(1), changed(&["a/b.txt"])),
            (oid(2), changed(&["c.txt", "d/e/f.txt"])),
        ]);
        write_graph_inner(
            &info.join("commit-graph"),
            &[(oid(1), 1), (oid(2), 2), (oid(3), 3)],
            Some(&BloomFixture {
                hash_version: 2,
                num_hashes: 7,
                filters,
            }),
        );
        let g = CommitGraph::open(&info).unwrap();
        let g = ChangedPaths::open(&g).expect("filters");
        assert_eq!(g.hash_version(), 2);
        let keys = |p: &str| PathKeys::for_path(p, 2).unwrap();

        // What the commit changed is never ruled out — the direction that would drop a
        // commit from a filtered view.
        assert!(!g.definitely_unchanged(oid(1), &keys("a/b.txt")));
        assert!(!g.definitely_unchanged(oid(1), &keys("a")));
        assert!(!g.definitely_unchanged(oid(2), &keys("d/e/f.txt")));
        assert!(!g.definitely_unchanged(oid(2), &keys("d/e")));
        // What it did not is.
        assert!(g.definitely_unchanged(oid(1), &keys("c.txt")));
        assert!(g.definitely_unchanged(oid(2), &keys("a/b.txt")));
        assert!(g.definitely_unchanged(oid(1), &keys("nowhere/at/all.txt")));
        // A commit with no filter, and a commit the graph has never heard of, are both
        // "ask the repository" rather than "unchanged".
        assert!(!g.definitely_unchanged(oid(3), &keys("a/b.txt")));
        assert!(!g.definitely_unchanged(oid(99), &keys("a/b.txt")));
    }

    /// git marks a commit that changed more paths than it will record with a single
    /// 0xFF byte, which answers "maybe" for everything by construction — so the reader
    /// needs no special case, and this is what says so.
    #[test]
    fn a_filter_git_marked_too_large_rules_nothing_out() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        write_graph_inner(
            &info.join("commit-graph"),
            &[(oid(1), 1)],
            Some(&BloomFixture {
                hash_version: 2,
                num_hashes: 7,
                filters: std::collections::HashMap::from([(oid(1), vec![0xFFu8])]),
            }),
        );
        let g = CommitGraph::open(&info).unwrap();
        let g = ChangedPaths::open(&g).expect("filters");
        assert!(!g.definitely_unchanged(oid(1), &PathKeys::for_path("any/path", 2).unwrap()));
    }

    /// A graph with no `BIDX`/`BDAT` — what `git gc` writes, so the usual case — has no
    /// version to hash against, and a settings header naming a hash this code does not
    /// know is refused rather than read as version 1.
    #[test]
    fn a_graph_without_usable_filters_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("objects").join("info");
        write_graph(&info.join("commit-graph"), &[(oid(1), 1)]);
        assert!(ChangedPaths::open(&CommitGraph::open(&info).unwrap()).is_none());

        write_graph_inner(
            &info.join("commit-graph"),
            &[(oid(1), 1)],
            Some(&BloomFixture {
                hash_version: 9,
                num_hashes: 7,
                filters: std::collections::HashMap::new(),
            }),
        );
        assert!(ChangedPaths::open(&CommitGraph::open(&info).unwrap()).is_none());
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
