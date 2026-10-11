//! The text of a built diff: one append-only arena cut into chunks of at most
//! `CHUNK_CAP` bytes, and the 20-byte `Row` that points into it.
//!
//! Invariants (each pinned below):
//! - the tail takes its index when its first byte is written and keeps it until it is
//!   sealed, into that same slot; any other chunk — an oversized row's own, an adopted
//!   part chunk — takes the next free index and leaves the tail open, so a merge
//!   alternating adopted and copied chunks fills one tail rather than sealing a sliver
//!   of one per alternation. A `mark` records the tail's slot, and `rewind` takes back
//!   a tail sealed since out of that slot;
//! - the first chunk grows as a `String` does; a tail at any later index holds the
//!   whole cap whenever it is written to — `write` tops up any capacity under it, a
//!   fresh tail's (0) and a rewound one's (its length) alike;
//! - a row of no text writes nothing and points at nothing;
//! - every row lies inside its chunk on char boundaries (`finish` checks it in debug);
//! - a chunk index past `u16::MAX` fails the build — it never wraps.

use super::LineKind;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};

/// The most text one chunk holds. Bounds the tail's spare capacity before it is sealed
/// and a chunk's free to one `munmap` — 64 MiB is far above glibc's mmap threshold, so
/// a freed diff's pages go back to the OS, which per-row frees did not do.
pub const CHUNK_CAP: usize = 64 << 20;
/// A split-pass part's chunk at least this long is adopted by the merge rather than
/// copied: the large deltas, which are what make a split worth doing, are moved.
pub const ADOPT_MIN: usize = 1 << 20;
/// Chunks a `u16` index can name: 0..=65,535.
const CHUNK_LIMIT: usize = u16::MAX as usize + 1;
const _: () = assert!(CHUNK_CAP <= u32::MAX as usize);
const _: () = assert!(ADOPT_MIN <= CHUNK_CAP);

/// One diff row: where its text is, and what it is. 20 B; a `u32` chunk index or `u64`
/// offsets would make it 24.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub off: u32,
    pub len: u32,
    pub old_lineno: Option<NonZeroU32>,
    pub new_lineno: Option<NonZeroU32>,
    pub chunk: u16,
    pub kind: LineKind,
}
const _: () = assert!(std::mem::size_of::<Row>() == 20);

impl Row {
    const fn blank(kind: LineKind, old: Option<NonZeroU32>, new: Option<NonZeroU32>) -> Self {
        Self {
            off: 0,
            len: 0,
            old_lineno: old,
            new_lineno: new,
            chunk: 0,
            kind,
        }
    }
}

/// The sealed chunks of a finished diff. Immutable once built; shared by `Arc`.
#[derive(Clone, Debug, Default)]
pub struct TextArena {
    chunks: Vec<Box<str>>,
}

impl TextArena {
    /// `row`'s text. A row of no text indexes nothing — a reserved row, or any row of
    /// a diff with no text at all, which has no chunk to index. `off + len` is added in
    /// `usize`: a `u32` add can wrap.
    pub fn text(&self, row: &Row) -> &str {
        if row.len == 0 {
            return "";
        }
        let off = row.off as usize;
        &self.chunks[usize::from(row.chunk)][off..off + row.len as usize]
    }
    /// Exact: a `Box<str>` has no spare capacity.
    pub fn heap_bytes(&self) -> usize {
        self.chunks.iter().map(|c| c.len()).sum()
    }
    #[cfg(test)]
    pub const fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
    fn holds(&self, row: &Row) -> bool {
        if row.len == 0 {
            return true;
        }
        let off = row.off as usize;
        self.chunks
            .get(usize::from(row.chunk))
            .is_some_and(|c| c.get(off..off + row.len as usize).is_some())
    }
    #[cfg(test)]
    pub fn chunk_ptrs(&self) -> Vec<*const u8> {
        self.chunks.iter().map(|c| c.as_ptr()).collect()
    }
    #[cfg(test)]
    pub fn chunk_lens(&self) -> Vec<usize> {
        self.chunks.iter().map(|c| c.len()).collect()
    }
}

/// How the arena cuts its text. The app uses `DEFAULT`; tests shrink every bound so the
/// shapes a 64 MiB cap meets on a huge diff happen within a few rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaLimits {
    pub chunk_cap: usize,
    pub adopt_min: usize,
    /// Longest row kept; longer is cut to a char boundary. `u32::MAX` in the app,
    /// where only a commit-message line could reach it.
    pub max_row: usize,
}
impl ArenaLimits {
    pub const DEFAULT: Self = Self {
        chunk_cap: CHUNK_CAP,
        adopt_min: ADOPT_MIN,
        max_row: u32::MAX as usize,
    };
}

/// More chunks than a `u16` can name. The build that hits it fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaFull;

/// A point an `ArenaWriter` can be rewound to.
#[derive(Clone, Copy, Debug)]
pub struct ArenaMark {
    rows: usize,
    chunks: usize,
    tail_at: Option<u16>,
    tail_len: usize,
}

pub struct ArenaWriter {
    rows: Vec<Row>,
    /// Every chunk that has an index, in index order. The open tail's slot holds an
    /// empty placeholder (no allocation) until the tail is sealed into it.
    chunks: Vec<Box<str>>,
    /// The first chunk (index 0) grows as a `String` does; a tail at any later index is
    /// reserved whole by `write`.
    tail: String,
    /// The tail's index: taken when its first byte is written, `None` exactly while it
    /// is empty.
    tail_at: Option<u16>,
    /// Latched when a chunk index ran out: `finish` then fails.
    full: bool,
    limits: ArenaLimits,
    /// Tails reserved whole by `write`, for the suite to count.
    #[cfg(test)]
    whole_reservations: usize,
}

static TRUNCATED: AtomicBool = AtomicBool::new(false);

impl ArenaWriter {
    pub const fn new(limits: ArenaLimits) -> Self {
        // A row's offset and length are `u32`s: an offset lies inside a chunk of at most
        // `chunk_cap`, and a length is at most `max_row`.
        debug_assert!(
            limits.chunk_cap <= u32::MAX as usize,
            "chunk_cap fits a u32 offset"
        );
        debug_assert!(
            limits.max_row <= u32::MAX as usize,
            "max_row fits a u32 length"
        );
        Self {
            rows: Vec::new(),
            chunks: Vec::new(),
            tail: String::new(),
            tail_at: None,
            full: false,
            limits,
            #[cfg(test)]
            whole_reservations: 0,
        }
    }
    /// For a writer whose sizes are known up front — the store's decoder.
    pub fn with_capacity(limits: ArenaLimits, rows: usize, text: usize) -> Self {
        Self {
            rows: Vec::with_capacity(rows),
            tail: String::with_capacity(text.min(limits.chunk_cap)),
            ..Self::new(limits)
        }
    }
    pub const fn len(&self) -> usize {
        self.rows.len()
    }

    /// Append a row whose text is `parts` concatenated, and return that text as stored.
    pub fn push_parts(
        &mut self,
        kind: LineKind,
        old: Option<NonZeroU32>,
        new: Option<NonZeroU32>,
        parts: &[&str],
    ) -> &str {
        let row = self.write_row(kind, old, new, parts);
        self.rows.push(row);
        self.view(row)
    }

    /// `n` rows of no text, for `set` to fill once their text is known.
    pub fn reserve_blank(&mut self, n: usize, kind: LineKind) {
        self.rows
            .resize(self.rows.len() + n, Row::blank(kind, None, None));
    }

    /// Fill a reserved row: its text goes at the tail and the row is pointed at it. Out
    /// of range is a no-op (`None`). Never on a row older than an outstanding `mark` —
    /// the rewind would truncate the text it now points at; the build fills the stat
    /// block after the patch pass, outside any mark.
    pub fn set(&mut self, i: usize, kind: LineKind, text: &str) -> Option<&str> {
        let old = *self.rows.get(i)?;
        debug_assert_eq!(
            old.len, 0,
            "set fills a reserved blank; anything else leaves dead text in the arena"
        );
        let row = self.write_row(kind, old.old_lineno, old.new_lineno, &[text]);
        self.rows[i] = row;
        Some(self.view(row))
    }

    pub const fn mark(&self) -> ArenaMark {
        ArenaMark {
            rows: self.rows.len(),
            chunks: self.chunks.len(),
            tail_at: self.tail_at,
            tail_len: self.tail.len(),
        }
    }

    /// Drop every row and every byte written since `mark`; every chunk given an index
    /// since then is dropped. The tail open at the mark, if one was, holds the slot the
    /// mark recorded: still open, it is cut back to where it stood; sealed since, it is
    /// taken back out of that slot — the chunks indexed after it before the mark (adopted
    /// or own-chunk, pushed beside the open tail) stay where they are. A mark over an
    /// empty tail revives nothing: no chunk pushed since is ever brought back as the
    /// tail. A tail brought back has a capacity of exactly its length; past the first
    /// chunk the next `write` reserves it whole again, by the same rule as a fresh tail.
    pub fn rewind(&mut self, mark: ArenaMark) {
        self.rows.truncate(mark.rows);
        if let Some(at) = mark.tail_at
            && self.tail_at != mark.tail_at
        {
            self.tail = String::from(std::mem::take(&mut self.chunks[usize::from(at)]));
        }
        self.chunks.truncate(mark.chunks);
        self.tail_at = mark.tail_at;
        self.tail.truncate(mark.tail_len);
    }

    /// Append a finished part (a split-pass worker's rows and chunks). A chunk of at
    /// least `adopt_min` bytes is ADOPTED — pushed as is, no copy, at the next index,
    /// leaving the tail open — while two indices remain (its own and one for a tail
    /// after it); anything smaller, or anything near the limit, is copied into the tail.
    /// Each part row is then rebased through the chunk map in one pass.
    pub fn append(&mut self, rows: Vec<Row>, text: TextArena) {
        let map: Vec<Option<(u16, u32)>> = text.chunks.into_iter().map(|c| self.place(c)).collect();
        self.rows.reserve(rows.len());
        for r in rows {
            let placed = if r.len == 0 {
                None
            } else {
                map[usize::from(r.chunk)]
            };
            self.rows.push(placed.map_or(
                Row {
                    off: 0,
                    len: 0,
                    chunk: 0,
                    ..r
                },
                |(chunk, shift)| Row {
                    chunk,
                    off: r.off + shift,
                    ..r
                },
            ));
        }
    }

    /// The rows and the sealed chunks, everything shrunk to what it holds.
    pub fn finish(mut self) -> Result<(Vec<Row>, TextArena), ArenaFull> {
        if self.full {
            return Err(ArenaFull);
        }
        self.seal();
        self.rows.shrink_to_fit();
        self.chunks.shrink_to_fit();
        let text = TextArena {
            chunks: self.chunks,
        };
        debug_assert!(
            text.chunks.iter().all(|c| !c.is_empty()),
            "no placeholder outlives its tail"
        );
        debug_assert!(
            self.rows.iter().all(|r| text.holds(r)),
            "every row lies inside its chunk, on char boundaries"
        );
        Ok((self.rows, text))
    }

    fn place(&mut self, chunk: Box<str>) -> Option<(u16, u32)> {
        if chunk.is_empty() {
            return Some((0, 0));
        }
        if chunk.len() >= self.limits.adopt_min && self.chunks.len() + 2 < CHUNK_LIMIT {
            return self.push_chunk(chunk);
        }
        self.write(&[&*chunk], chunk.len())
    }

    fn write_row(
        &mut self,
        kind: LineKind,
        old: Option<NonZeroU32>,
        new: Option<NonZeroU32>,
        parts: &[&str],
    ) -> Row {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if len == 0 || self.full {
            return Row::blank(kind, old, new);
        }
        if len > self.limits.max_row {
            return self.write_truncated(kind, old, new, parts);
        }
        match self.write(parts, len) {
            Some((chunk, off)) => Row {
                off,
                len: len as u32,
                old_lineno: old,
                new_lineno: new,
                chunk,
                kind,
            },
            None => Row::blank(kind, old, new),
        }
    }

    #[cold]
    fn write_truncated(
        &mut self,
        kind: LineKind,
        old: Option<NonZeroU32>,
        new: Option<NonZeroU32>,
        parts: &[&str],
    ) -> Row {
        let mut joined = parts.concat();
        let mut at = self.limits.max_row;
        while !joined.is_char_boundary(at) {
            at -= 1;
        }
        joined.truncate(at);
        if !TRUNCATED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "gitkay: a diff row over {} bytes was cut short",
                self.limits.max_row
            );
        }
        self.write_row(kind, old, new, &[joined.as_str()])
    }

    /// Put `len` bytes somewhere; `None` once the chunk indices have run out.
    fn write(&mut self, parts: &[&str], len: usize) -> Option<(u16, u32)> {
        if len > self.limits.chunk_cap {
            // Its own chunk, sized exactly, at the next index; the tail stays open.
            let mut own = String::with_capacity(len);
            for p in parts {
                own.push_str(p);
            }
            return self.push_chunk(own.into_boxed_str());
        }
        if self.tail.len() + len > self.limits.chunk_cap {
            self.seal();
        }
        let at = match self.tail_at {
            Some(at) => at,
            None => self.open_tail()?,
        };
        if at > 0 && self.tail.capacity() < self.limits.chunk_cap {
            // Not the first chunk: hold the whole cap. Untouched pages are not resident,
            // and a tail doubling up from small sizes is realloc'd by memcpy below glibc's
            // mmap threshold — ~1.7 GB of copying over the big diff. Asked by capacity,
            // not by "is it fresh": a tail a rewind brought back from a sealed chunk has
            // a capacity of exactly its length, and must not double from there either.
            self.tail
                .reserve_exact(self.limits.chunk_cap - self.tail.len());
            #[cfg(test)]
            {
                self.whole_reservations += 1;
            }
        }
        let off = self.tail.len();
        for p in parts {
            self.tail.push_str(p);
        }
        Some((at, off as u32))
    }

    /// Give an empty tail the next index, holding its slot with an empty placeholder.
    fn open_tail(&mut self) -> Option<u16> {
        let at = self.next_index()?;
        self.chunks.push(Box::default());
        self.tail_at = Some(at);
        Some(at)
    }

    /// A chunk other than the tail, at the next index.
    fn push_chunk(&mut self, chunk: Box<str>) -> Option<(u16, u32)> {
        let at = self.next_index()?;
        self.chunks.push(chunk);
        Some((at, 0))
    }

    /// The index the next chunk to be pushed — a tail being opened, or any other — takes.
    fn next_index(&mut self) -> Option<u16> {
        let at = u16::try_from(self.chunks.len()).ok();
        self.full |= at.is_none();
        at
    }

    /// Seal an open tail into its slot (`into_boxed_str` gives back its spare capacity —
    /// an `mremap` at this size, not a copy). An empty tail has no slot and stays the
    /// tail.
    fn seal(&mut self) {
        if let Some(at) = self.tail_at.take() {
            self.chunks[usize::from(at)] = std::mem::take(&mut self.tail).into_boxed_str();
        }
    }

    fn view(&self, row: Row) -> &str {
        if row.len == 0 {
            return "";
        }
        let chunk: &str = if self.tail_at == Some(row.chunk) {
            &self.tail
        } else {
            &self.chunks[usize::from(row.chunk)]
        };
        let off = row.off as usize;
        &chunk[off..off + row.len as usize]
    }

    #[cfg(test)]
    const fn tail_capacity(&self) -> usize {
        self.tail.capacity()
    }
    /// The index the tail's text has, or takes when it is opened.
    #[cfg(test)]
    fn tail_index(&self) -> usize {
        self.tail_at.map_or(self.chunks.len(), usize::from)
    }
    #[cfg(test)]
    const fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-BYTE cap: seals, own-chunk rows and rewinds across a seal happen in a few rows.
    const TINY: ArenaLimits = ArenaLimits {
        chunk_cap: 64,
        adopt_min: 16,
        max_row: u32::MAX as usize,
    };

    /// xorshift64: deterministic and dependency-free; it only shuffles operations.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Texts straddling every boundary a 64-byte cap has — empty, short, just under and
    /// over the cap, several caps long — and multibyte throughout, so an offset taken
    /// as a byte count would land inside a code point.
    fn sample(rng: &mut Rng) -> String {
        const ATOMS: [&str; 6] = ["a", "é", "€", "😀", "\t", " xy"];
        let len = match rng.below(6) {
            0 => 0,
            1 => 1 + rng.below(8),
            2 => 20 + rng.below(30),
            3 => 60 + rng.below(8),
            4 => 130 + rng.below(70),
            _ => 300,
        };
        let mut s = String::new();
        while s.len() < len {
            s.push_str(ATOMS[rng.below(ATOMS.len())]);
        }
        s
    }

    /// `s` cut into up to three parts at char boundaries, the shape `push_patch_line`
    /// hands over (marker, body).
    fn parts<'s>(s: &'s str, rng: &mut Rng) -> Vec<&'s str> {
        let cuts: Vec<usize> = s
            .char_indices()
            .map(|(i, _)| i)
            .filter(|_| rng.below(8) == 0)
            .take(2)
            .collect();
        let mut out = Vec::new();
        let mut from = 0;
        for c in cuts {
            out.push(&s[from..c]);
            from = c;
        }
        out.push(&s[from..]);
        out
    }

    /// At least `len` bytes of the sample alphabet, multibyte included.
    fn text_of(len: usize, rng: &mut Rng) -> String {
        const ATOMS: [&str; 5] = ["a", "é", "€", "😀", "\t"];
        let mut s = String::new();
        while s.len() < len {
            s.push_str(ATOMS[rng.below(ATOMS.len())]);
        }
        s
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Want {
        text: String,
        kind: LineKind,
        old: Option<NonZeroU32>,
        new: Option<NonZeroU32>,
    }

    /// Chunk caps small enough that every run seals, spills rows into chunks of their
    /// own and adopts or copies part chunks within a few rows.
    const CAPS: [usize; 4] = [8, 16, 64, 100];
    /// Each side of every cap above (and of the sample lengths), plus never adopting.
    const ADOPT_MINS: [usize; 7] = [1, 8, 16, 40, 64, 65, usize::MAX];

    /// The shapes a run has to have met for the oracle's pass to mean anything.
    #[derive(Default)]
    struct Seen {
        rewound_across_a_seal: bool,
        rewound_onto_an_empty_tail: bool,
        adopted: bool,
        copied: bool,
        rewound_an_append: bool,
        nested: bool,
        alternated: bool,
        adopted_beside_an_open_tail: bool,
        /// A rewind took a tail sealed since the mark back out of its slot — and one
        /// with chunks indexed after it before the mark, which must stay put.
        took_back_a_sealed_tail: bool,
        took_back_a_tail_with_chunks_after_it: bool,
    }

    /// Names the failing seed: the asserts fire deep inside a run, which knows none.
    struct NameOnPanic(u64, ArenaLimits);
    impl Drop for NameOnPanic {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("oracle failed at seed {}, {:?}", self.0, self.1);
            }
        }
    }

    /// A writer driven by random operations beside the `Vec<String>` model it must agree
    /// with: rows, the reserved rows still unfilled, and every outstanding mark with the
    /// model as it stood then.
    struct Run {
        w: ArenaWriter,
        want: Vec<Want>,
        blanks: Vec<usize>,
        marks: Vec<(ArenaMark, Vec<Want>, Vec<usize>, usize)>,
        appends: usize,
    }

    impl Run {
        fn new(limits: ArenaLimits) -> Self {
            Self {
                w: ArenaWriter::new(limits),
                want: Vec::new(),
                blanks: Vec::new(),
                marks: Vec::new(),
                appends: 0,
            }
        }

        /// One random operation. `depth` is how many levels of part may still be built
        /// and appended below this writer.
        fn step(&mut self, rng: &mut Rng, depth: u32, seen: &mut Seen) {
            match rng.below(12) {
                0..=5 => {
                    let s = sample(rng);
                    let kind = [
                        LineKind::Context,
                        LineKind::Add,
                        LineKind::Del,
                        LineKind::Meta,
                    ][rng.below(4)];
                    let old = NonZeroU32::new(rng.below(3) as u32);
                    let new = NonZeroU32::new(rng.below(3) as u32);
                    let got = self
                        .w
                        .push_parts(kind, old, new, &parts(&s, rng))
                        .to_owned();
                    assert_eq!(got, s, "push_parts returns what it stored");
                    let last = self.w.rows.len() - 1;
                    self.check_tail_reserved(last..=last);
                    self.want.push(Want {
                        text: s,
                        kind,
                        old,
                        new,
                    });
                }
                6 => {
                    let n = 1 + rng.below(3);
                    self.blanks.extend(self.want.len()..self.want.len() + n);
                    self.w.reserve_blank(n, LineKind::Stat);
                    self.want.extend((0..n).map(|_| Want {
                        text: String::new(),
                        kind: LineKind::Stat,
                        old: None,
                        new: None,
                    }));
                }
                // Only with no mark outstanding: the build fills the stat block after
                // the patch pass, never inside a converted patch's mark/rewind.
                7 if self.marks.is_empty() && !self.blanks.is_empty() => {
                    let i = self.blanks.swap_remove(rng.below(self.blanks.len()));
                    let s = sample(rng);
                    assert_eq!(self.w.set(i, LineKind::Stat, &s), Some(s.as_str()));
                    self.check_tail_reserved(i..=i);
                    self.want[i].text = s;
                }
                8 => self.marks.push((
                    self.w.mark(),
                    self.want.clone(),
                    self.blanks.clone(),
                    self.appends,
                )),
                9 if !self.marks.is_empty() => {
                    // Rewinding to a mark abandons every mark taken after it.
                    self.marks.truncate(rng.below(self.marks.len()) + 1);
                    let (m, want, blanks, appends) = self.marks.pop().expect("non-empty");
                    let crossed = self.w.chunk_count() > m.chunks;
                    seen.rewound_across_a_seal |= crossed;
                    if let Some(at) = m.tail_at
                        && self.w.tail_at != m.tail_at
                    {
                        seen.took_back_a_sealed_tail = true;
                        seen.took_back_a_tail_with_chunks_after_it |=
                            m.chunks > usize::from(at) + 1;
                    }
                    seen.rewound_onto_an_empty_tail |= crossed && m.tail_len == 0;
                    seen.rewound_an_append |= self.appends > appends;
                    self.w.rewind(m);
                    self.want = want;
                    self.blanks = blanks;
                }
                10 if depth > 0 => self.append_part(rng, depth, seen),
                11 => self.append_alternating(rng, seen),
                _ => {}
            }
        }

        /// A row just written into a tail other than the first chunk finds that tail
        /// reserved whole — whether it was fresh or brought back by a rewind. Asked after
        /// every path that writes into the tail (`push_parts`, `set`, a merge's copies),
        /// over the rows that write touched.
        fn check_tail_reserved(&self, written: impl IntoIterator<Item = usize>) {
            for i in written {
                let r = self.w.rows[i];
                if r.len > 0 && r.chunk > 0 && usize::from(r.chunk) == self.w.tail_index() {
                    assert!(
                        self.w.tail_capacity() >= self.w.limits.chunk_cap,
                        "a tail past the first chunk is reserved whole: {} < {}",
                        self.w.tail_capacity(),
                        self.w.limits.chunk_cap
                    );
                }
            }
        }

        /// Build a part of its own random operations — one level of its own parts
        /// included, when `depth` allows — and append it here, its unfilled reserved rows
        /// becoming this writer's to fill.
        fn append_part(&mut self, rng: &mut Rng, depth: u32, seen: &mut Seen) {
            let mut part = Self::new(ArenaLimits {
                adopt_min: ADOPT_MINS[rng.below(ADOPT_MINS.len())],
                ..self.w.limits
            });
            for _ in 0..rng.below(40) {
                part.step(rng, depth - 1, seen);
            }
            seen.nested |= part.appends > 0;
            self.merge(part, seen);
        }

        /// A run of one-row parts whose texts alternate between at least `adopt_min`
        /// bytes (adopted, where this writer adopts at all) and a few bytes (copied):
        /// the merge shape that used to split the tail at every alternation.
        fn append_alternating(&mut self, rng: &mut Rng, seen: &mut Seen) {
            let large = self.w.limits.adopt_min.min(150);
            let start = rng.below(2);
            let n = 2 + rng.below(6);
            for k in start..start + n {
                let mut part = Self::new(self.w.limits);
                let len = if k % 2 == 0 {
                    large + rng.below(20)
                } else {
                    1 + rng.below(6)
                };
                let s = text_of(len, rng);
                part.w
                    .push_parts(LineKind::Add, None, None, &parts(&s, rng));
                part.want.push(Want {
                    text: s,
                    kind: LineKind::Add,
                    old: None,
                    new: None,
                });
                self.merge(part, seen);
            }
            seen.alternated = true;
        }

        /// Finish `part` and append it here. An append whose copies all fit beside the
        /// text the tail already holds leaves that tail where it is: adopting a chunk
        /// does not seal it.
        fn merge(&mut self, part: Self, seen: &mut Seen) {
            let (rows, text, want, blanks) = part.finish();
            let limits = self.w.limits;
            seen.adopted |= text.chunks.iter().any(|c| c.len() >= limits.adopt_min);
            seen.copied |= text
                .chunks
                .iter()
                .any(|c| !c.is_empty() && c.len() < limits.adopt_min);
            let into_tail: usize = text
                .chunks
                .iter()
                .map(|c| c.len())
                .filter(|&l| l < limits.adopt_min && l <= limits.chunk_cap)
                .sum();
            let open = !self.w.tail.is_empty();
            let stays = open && self.w.tail.len() + into_tail <= limits.chunk_cap;
            let before = self.w.tail_index();
            seen.adopted_beside_an_open_tail |=
                stays && text.chunks.iter().any(|c| c.len() >= limits.adopt_min);
            let base = self.want.len();
            self.w.append(rows, text);
            self.check_tail_reserved(base..self.w.rows.len());
            if stays {
                assert_eq!(
                    self.w.tail_index(),
                    before,
                    "an append whose copies fit keeps the tail it found"
                );
            }
            self.want.extend(want);
            self.blanks.extend(blanks.into_iter().map(|i| base + i));
            self.appends += 1;
        }

        /// Finish the writer and hold it to the model: every row's text, kind and line
        /// numbers, and not one byte more than they hold — a byte left behind by a
        /// rewind, a set or a merge is text no row can reach.
        fn finish(self) -> (Vec<Row>, TextArena, Vec<Want>, Vec<usize>) {
            let (rows, text) = self.w.finish().expect("within the chunk limit");
            let got: Vec<Want> = rows
                .iter()
                .map(|r| Want {
                    text: text.text(r).to_owned(),
                    kind: r.kind,
                    old: r.old_lineno,
                    new: r.new_lineno,
                })
                .collect();
            assert_eq!(got, self.want);
            assert_eq!(
                text.heap_bytes(),
                self.want.iter().map(|w| w.text.len()).sum::<usize>(),
                "the arena holds exactly the rows' text"
            );
            (rows, text, self.want, self.blanks)
        }
    }

    /// THE oracle (§8 risk 1): every write the build performs, at random, against a
    /// `Vec` of `String`s that cannot get an offset wrong. Pushes of every length and
    /// script, reserved blanks filled by `set`, nested marks and rewinds across seals, and
    /// split-pass parts — themselves built this way, one level of their own parts
    /// included — appended anywhere, inside marks that are later rewound too, adopted or
    /// copied under every `adopt_min` either side of the caps.
    #[test]
    fn the_arena_reads_back_what_a_vec_of_strings_holds() {
        let mut seen = Seen::default();
        for seed in 1..=256u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut run = Run::new(ArenaLimits {
                chunk_cap: CAPS[rng.below(CAPS.len())],
                adopt_min: ADOPT_MINS[rng.below(ADOPT_MINS.len())],
                max_row: u32::MAX as usize,
            });
            let _named = NameOnPanic(seed, run.w.limits);
            for _ in 0..300 {
                run.step(&mut rng, 2, &mut seen);
            }
            run.finish();
        }
        assert!(
            seen.rewound_across_a_seal,
            "control: some rewind undid a seal"
        );
        assert!(
            seen.rewound_onto_an_empty_tail,
            "control: some rewind undid a chunk pushed past an empty tail"
        );
        assert!(seen.adopted, "control: some part chunk was adopted");
        assert!(seen.copied, "control: some part chunk was copied");
        assert!(
            seen.rewound_an_append,
            "control: some rewind undid an append"
        );
        assert!(seen.nested, "control: some part appended a part of its own");
        assert!(
            seen.alternated,
            "control: some merge alternated large and small"
        );
        assert!(
            seen.adopted_beside_an_open_tail,
            "control: some chunk was adopted while the tail held text"
        );
        assert!(
            seen.took_back_a_sealed_tail,
            "control: some rewind took a sealed tail back"
        );
        assert!(
            seen.took_back_a_tail_with_chunks_after_it,
            "control: some rewind took back a tail with chunks indexed after it"
        );
    }

    /// A row longer than the cap gets a chunk of its own, sized exactly, at the next
    /// index — and the tail stays open in its own slot: the next short row lands in it.
    #[test]
    fn a_row_longer_than_the_cap_gets_a_chunk_of_its_own() {
        let mut w = ArenaWriter::new(TINY);
        w.push_parts(LineKind::Context, None, None, &["short"]);
        let long = "é".repeat(100);
        w.push_parts(LineKind::Add, None, None, &["+", long.as_str()]);
        w.push_parts(LineKind::Context, None, None, &["after"]);
        let (rows, text) = w.finish().expect("fits");
        assert_eq!(
            text.chunk_count(),
            2,
            "the one tail, and the long row's own"
        );
        assert_eq!((rows[1].chunk, rows[1].off, rows[1].len), (1, 0, 201));
        assert_eq!(text.chunks[1].len(), 201, "sized exactly");
        assert_eq!(
            (rows[2].chunk, rows[2].off),
            (0, 5),
            "the tail kept its slot and its text"
        );
        let plus_long = format!("+{long}");
        assert_eq!(
            [
                text.text(&rows[0]),
                text.text(&rows[1]),
                text.text(&rows[2])
            ],
            ["short", plus_long.as_str(), "after"]
        );
    }

    /// After the first seal a tail is reserved at the full cap at once — a tail doubling
    /// from small sizes reallocs by memcpy — and sealing gives back what it did not use.
    #[test]
    fn after_the_first_seal_a_tail_is_allocated_whole_and_sealed_to_its_length() {
        let mut w = ArenaWriter::new(TINY);
        w.push_parts(LineKind::Context, None, None, &["x".repeat(40).as_str()]);
        assert_eq!(w.tail_index(), 0, "control: still the first chunk");
        w.push_parts(LineKind::Context, None, None, &["y".repeat(40).as_str()]);
        assert_eq!(w.tail_index(), 1, "40 + 40 crossed the 64-byte cap");
        assert!(
            w.tail_capacity() >= TINY.chunk_cap,
            "reserved whole: {}",
            w.tail_capacity()
        );
        let (_, text) = w.finish().expect("fits");
        assert_eq!(
            text.chunks.iter().map(|c| c.len()).collect::<Vec<_>>(),
            [40, 40]
        );
        assert_eq!(text.heap_bytes(), 80, "each chunk sealed to what it holds");
    }

    /// A reserved row writes nothing and reads back empty — also with no chunk at all,
    /// where indexing one would panic.
    #[test]
    fn an_empty_row_reads_back_empty_with_or_without_chunks() {
        let mut w = ArenaWriter::new(TINY);
        w.reserve_blank(2, LineKind::Stat);
        w.push_parts(LineKind::Blank, None, None, &[]);
        let (rows, text) = w.finish().expect("fits");
        assert_eq!(text.chunk_count(), 0);
        assert!(rows.iter().all(|r| text.text(r).is_empty()));
    }

    /// `emit_converted` abandons a half-written converted patch with `rewind`; over the
    /// cap that patch has sealed chunks since the mark.
    #[test]
    fn rewinding_across_a_seal_restores_the_tail_as_it_stood() {
        let mut w = ArenaWriter::new(TINY);
        w.push_parts(LineKind::Context, None, None, &["kept"]);
        let m = w.mark();
        for _ in 0..5 {
            w.push_parts(LineKind::Add, None, None, &["+", "z".repeat(30).as_str()]);
        }
        w.push_parts(LineKind::Add, None, None, &["q".repeat(200).as_str()]);
        assert!(
            w.chunk_count() > m.chunks,
            "control: rows since the mark sealed chunks"
        );
        w.rewind(m);
        assert_eq!(w.chunk_count(), m.chunks);
        w.push_parts(LineKind::Context, None, None, &["after"]);
        let (rows, text) = w.finish().expect("fits");
        assert_eq!(
            rows.iter().map(|r| text.text(r)).collect::<Vec<_>>(),
            ["kept", "after"]
        );
        assert_eq!(
            text.heap_bytes(),
            "keptafter".len(),
            "nothing written since the mark survives"
        );
    }

    /// A rewind across a seal brings the old tail back from a `Box<str>`, whose capacity
    /// is its length. Past the first chunk that tail is held to the same rule as a fresh
    /// one: the next write reserves it whole, rather than letting it grow by doubling.
    #[test]
    fn a_tail_brought_back_by_a_rewind_is_reserved_whole_again() {
        let mut w = ArenaWriter::new(TINY);
        w.push_parts(LineKind::Context, None, None, &["x".repeat(62).as_str()]);
        w.push_parts(LineKind::Context, None, None, &["kept"]);
        assert_eq!(w.tail_index(), 1, "control: past the first chunk");
        let m = w.mark();
        w.push_parts(LineKind::Add, None, None, &["y".repeat(62).as_str()]);
        assert!(w.chunk_count() > m.chunks, "control: the tail was sealed");
        w.rewind(m);
        assert!(
            w.tail_capacity() < TINY.chunk_cap,
            "control: brought back at its length, {}",
            w.tail_capacity()
        );
        w.push_parts(LineKind::Context, None, None, &["after"]);
        assert!(
            w.tail_capacity() >= TINY.chunk_cap,
            "reserved whole: {}",
            w.tail_capacity()
        );
        let (rows, text) = w.finish().expect("fits");
        assert_eq!(
            rows.iter().map(|r| text.text(r).len()).collect::<Vec<_>>(),
            [62, 4, 5]
        );
        assert_eq!(text.heap_bytes(), 71);
    }

    /// A merge whose adopted part chunks alternate with copied ones: every copy lands in
    /// the one tail the merge found, so alternating costs an index per adopted chunk and
    /// nothing more — no tail sealed early, and no whole-cap reservation per alternation.
    #[test]
    fn alternating_adopted_and_copied_parts_share_one_tail() {
        const N: usize = 8;
        let part = |text: &str| {
            let mut p = ArenaWriter::new(TINY);
            p.push_parts(LineKind::Add, None, None, &[text]);
            p.finish().expect("fits")
        };
        let mut w = ArenaWriter::new(TINY);
        w.push_parts(LineKind::Context, None, None, &["x".repeat(62).as_str()]);
        w.push_parts(LineKind::Meta, None, None, &["head"]);
        assert_eq!(w.tail_index(), 1, "control: past the first chunk");
        assert_eq!(w.whole_reservations, 1, "control: the tail reserved whole");
        let large = "L".repeat(TINY.adopt_min + 4);
        let mut want = vec!["x".repeat(62), "head".to_owned()];
        for i in 0..N {
            let (rows, text) = part(&large);
            w.append(rows, text);
            let small = format!("s{i}");
            let (rows, text) = part(&small);
            w.append(rows, text);
            want.extend([large.clone(), small]);
        }
        assert_eq!(
            w.whole_reservations, 1,
            "the copies went into the tail already reserved"
        );
        let (rows, text) = w.finish().expect("fits");
        assert_eq!(
            text.chunk_count(),
            N + 2,
            "the first chunk, the one tail and the {N} adopted chunks"
        );
        assert_eq!(rows.iter().map(|r| text.text(r)).collect::<Vec<_>>(), want);
        let small_chunks: std::collections::HashSet<u16> = rows[2..]
            .iter()
            .filter(|r| (r.len as usize) < TINY.adopt_min)
            .map(|r| r.chunk)
            .collect();
        assert_eq!(small_chunks.len(), 1, "every copy shares one chunk");
    }

    /// The merge's two paths read back identically, and adopting moves chunks rather
    /// than copying them (the adopted allocations are still alive, so pointer equality
    /// cannot be a reused address).
    #[test]
    fn a_part_reads_back_the_same_adopted_or_copied() {
        let marked = "ü".repeat(20);
        let euro = "€".repeat(30);
        let texts = ["+one", "+two two", "", marked.as_str(), euro.as_str()];
        for adopt_min in [1, usize::MAX] {
            let mut part = ArenaWriter::new(TINY);
            for t in texts {
                part.push_parts(LineKind::Add, None, NonZeroU32::new(1), &["+", t]);
            }
            let (prows, ptext) = part.finish().expect("fits");
            let adopted = ptext.chunk_ptrs();
            let mut w = ArenaWriter::new(ArenaLimits { adopt_min, ..TINY });
            w.push_parts(LineKind::Meta, None, None, &["header"]);
            w.append(prows, ptext);
            w.push_parts(LineKind::Context, None, None, &["after"]);
            let (rows, text) = w.finish().expect("fits");
            let mut want = vec!["header".to_owned()];
            want.extend(texts.iter().map(|t| format!("+{t}")));
            want.push("after".to_owned());
            assert_eq!(
                rows.iter().map(|r| text.text(r)).collect::<Vec<_>>(),
                want,
                "adopt_min {adopt_min}"
            );
            if adopt_min == 1 {
                let kept = text
                    .chunk_ptrs()
                    .iter()
                    .filter(|p| adopted.contains(p))
                    .count();
                assert_eq!(kept, adopted.len(), "adopted chunks are moved, not copied");
            }
        }
    }

    /// Past 65,536 chunks the index cannot name the next one: the build FAILS rather
    /// than wrapping onto chunk 0 and reading another file's text.
    #[test]
    fn running_out_of_chunk_indices_fails_rather_than_wraps() {
        let one = ArenaLimits {
            chunk_cap: 1,
            adopt_min: 1,
            max_row: u32::MAX as usize,
        };
        let fill = |n: usize| {
            let mut w = ArenaWriter::new(one);
            for _ in 0..n {
                w.push_parts(LineKind::Context, None, None, &["x"]);
            }
            w
        };
        let (rows, text) = fill(CHUNK_LIMIT)
            .finish()
            .expect("exactly as many chunks as u16 names");
        assert_eq!(text.chunk_count(), CHUNK_LIMIT);
        assert_eq!(
            rows.last().map(|r| (r.chunk, text.text(r))),
            Some((u16::MAX, "x"))
        );
        assert_eq!(fill(CHUNK_LIMIT + 1).finish().map(|_| ()), Err(ArenaFull));
    }

    /// Near the limit the merge stops adopting (an adopted chunk needs its own index and
    /// one for the fresh tail) and copies instead, so it degrades rather than fails.
    #[test]
    fn near_the_chunk_limit_a_part_is_copied_not_adopted() {
        let small = ArenaLimits {
            chunk_cap: 4,
            adopt_min: 1,
            max_row: u32::MAX as usize,
        };
        let mut w = ArenaWriter::new(small);
        for _ in 0..CHUNK_LIMIT - 2 {
            w.push_parts(LineKind::Context, None, None, &["12345"]); // over the cap: its own chunk
        }
        assert_eq!(w.chunk_count(), CHUNK_LIMIT - 2, "control");
        let mut part = ArenaWriter::new(small);
        // Exactly `chunk_cap` bytes, so the copy's tail is sized to its length and the
        // final seal reallocates nothing: no allocation follows the part chunk's free.
        part.push_parts(LineKind::Add, None, None, &["+abc"]);
        let (prows, ptext) = part.finish().expect("fits");
        // Adopting here would ALSO finish with CHUNK_LIMIT - 1 chunks (the adopted chunk
        // takes one index, as the copy's tail does): only the allocation says
        // which path ran. The part's chunk is freed only after the tail it is copied into
        // is allocated, and nothing allocates after that, so a copy cannot land at its
        // address.
        let part_chunks = ptext.chunk_ptrs();
        w.append(prows, ptext);
        let (rows, text) = w
            .finish()
            .expect("copied into the tail: no index needed beyond it");
        assert_eq!(text.chunk_count(), CHUNK_LIMIT - 1);
        assert_eq!(rows.last().map(|r| text.text(r)), Some("+abc"));
        assert!(
            text.chunk_ptrs().iter().all(|p| !part_chunks.contains(p)),
            "copied, not adopted"
        );
    }

    /// Running out of indices latches: a mark taken after it and rewound to cannot
    /// un-fail the build, whose rows from then on were blanked rather than written.
    #[test]
    fn a_rewind_does_not_clear_running_out_of_chunk_indices() {
        let one = ArenaLimits {
            chunk_cap: 1,
            adopt_min: 1,
            max_row: u32::MAX as usize,
        };
        let mut w = ArenaWriter::new(one);
        for _ in 0..=CHUNK_LIMIT {
            w.push_parts(LineKind::Context, None, None, &["x"]);
        }
        assert_eq!(
            w.push_parts(LineKind::Context, None, None, &["y"]),
            "",
            "control: the limit was hit, and rows past it are blanked"
        );
        let m = w.mark();
        w.push_parts(LineKind::Context, None, None, &["z"]);
        w.rewind(m);
        assert_eq!(w.finish().map(|_| ()), Err(ArenaFull));
    }

    /// `finish` hands back rows sized to what they hold: a diff's row array outlives the
    /// build by the whole session it is cached for.
    #[test]
    fn finish_shrinks_the_rows_to_their_length() {
        let mut w = ArenaWriter::new(TINY);
        for _ in 0..5 {
            w.push_parts(LineKind::Context, None, None, &["x"]);
        }
        assert!(w.rows.capacity() > w.rows.len(), "control: spare capacity");
        let (rows, _) = w.finish().expect("fits");
        assert_eq!(rows.capacity(), rows.len());
    }

    /// The FIRST chunk grows as a `String` does — a diff of a few rows must not allocate
    /// a whole `CHUNK_CAP` — and only a later tail is reserved whole.
    #[test]
    fn the_first_chunk_grows_rather_than_being_reserved_whole() {
        let mut w = ArenaWriter::new(ArenaLimits {
            chunk_cap: 1 << 10,
            ..TINY
        });
        w.push_parts(LineKind::Context, None, None, &["x"]);
        assert!(
            w.tail_capacity() < 64,
            "grown, not reserved: {}",
            w.tail_capacity()
        );
    }

    /// A writer sized up front reserves its rows and a first tail of the text it was
    /// promised, never past one chunk's cap.
    #[test]
    fn with_capacity_reserves_the_rows_and_at_most_one_chunk_of_text() {
        let w = ArenaWriter::with_capacity(TINY, 10, 20);
        assert!(w.rows.capacity() >= 10);
        assert!(w.tail_capacity() >= 20);
        let mut w = ArenaWriter::with_capacity(TINY, 0, 1000);
        assert!(
            (TINY.chunk_cap..1000).contains(&w.tail_capacity()),
            "one chunk's cap: {}",
            w.tail_capacity()
        );
        w.push_parts(LineKind::Context, None, None, &["a"]);
        w.push_parts(LineKind::Context, None, None, &["b".repeat(70).as_str()]);
        let (rows, text) = w.finish().expect("fits");
        assert_eq!(
            rows.iter().map(|r| text.text(r).len()).collect::<Vec<_>>(),
            [1, 70]
        );
    }

    /// A rewind to a mark taken over an EMPTY tail drops the chunk pushed at its index —
    /// here a row of its own, far larger than a tail — instead of reviving it as the tail
    /// and carrying its allocation until the next seal.
    #[test]
    fn rewinding_to_an_empty_tail_starts_a_fresh_one() {
        let mut w = ArenaWriter::new(TINY);
        let m = w.mark();
        w.push_parts(LineKind::Add, None, None, &["q".repeat(200).as_str()]);
        assert_eq!(
            w.chunk_count(),
            1,
            "control: the long row took a chunk of its own"
        );
        w.rewind(m);
        assert_eq!(w.chunk_count(), 0);
        assert_eq!(
            w.tail_capacity(),
            0,
            "the long row's allocation went with it"
        );
        w.push_parts(LineKind::Context, None, None, &["after"]);
        let (rows, text) = w.finish().expect("fits");
        assert_eq!(
            rows.iter().map(|r| text.text(r)).collect::<Vec<_>>(),
            ["after"]
        );
        assert_eq!(text.heap_bytes(), "after".len());
    }

    /// A row's offset and length are `u32`s: limits past that are a bug in the caller.
    #[test]
    #[should_panic(expected = "chunk_cap fits")]
    fn a_chunk_cap_past_u32_is_a_bug() {
        let _ = ArenaWriter::new(ArenaLimits {
            chunk_cap: u32::MAX as usize + 1,
            ..TINY
        });
    }

    #[test]
    #[should_panic(expected = "max_row fits")]
    fn a_max_row_past_u32_is_a_bug() {
        let _ = ArenaWriter::new(ArenaLimits {
            max_row: u32::MAX as usize + 1,
            ..TINY
        });
    }

    /// A row over `max_row` is cut at a char boundary at or under it, never mid code point.
    #[test]
    fn a_row_past_the_row_limit_is_cut_on_a_char_boundary() {
        let mut w = ArenaWriter::new(ArenaLimits { max_row: 5, ..TINY });
        assert_eq!(
            w.push_parts(LineKind::Meta, None, None, &["ab", "€€"]),
            "ab€"
        );
        assert_eq!(w.push_parts(LineKind::Meta, None, None, &["abc€"]), "abc");
    }

    /// `set` over a row that already holds text would leave that text dead in the arena.
    #[test]
    #[should_panic(expected = "reserved blank")]
    fn setting_a_row_that_holds_text_is_a_bug() {
        let mut w = ArenaWriter::new(TINY);
        w.push_parts(LineKind::Stat, None, None, &["x"]);
        w.set(0, LineKind::Stat, "y");
    }
}
