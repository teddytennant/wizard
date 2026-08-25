//! Keeping shaped paragraphs across layout passes.
//!
//! Shaping text is the expensive thing cosmic-text does: font fallback, glyph
//! clustering, bidi and line breaking, per paragraph. iced re-runs `view()` and
//! `layout()` on every event batch, so a transcript that reshaped its whole
//! history on each keystroke would spend the entire frame budget re-deriving
//! text that has not changed since the session started.
//!
//! # Why this is keyed by content and not by index
//!
//! The obvious cache is a `Vec` parallel to the blocks: keep `paragraphs[i]` for
//! `blocks[i]` and rebuild when the block changed. It is wrong here, and
//! [`crate::transcript`] says exactly why in its own module header:
//! [`Change::Inserted`](crate::transcript::Change::Inserted) is real. The replay
//! path splices a tool's images in *behind* the row that produced them, so every
//! row below shifts down by one. An index-keyed cache survives that by handing
//! every block below the splice the paragraph belonging to the block above it —
//! which is not a stale render, it is the wrong text, and it would persist until
//! something else happened to invalidate it.
//!
//! So the key is [`Block::fingerprint`](super::block::Block::fingerprint): the
//! content, the size and the width. Under that key an insert costs exactly one
//! new paragraph and the other N keep theirs, wherever they moved to. A pool per
//! key rather than a single entry, because a transcript legitimately holds
//! duplicates — two identical `ls` rows, two blank lines — and they must not
//! fight over one slot and reshape each other every frame.
//!
//! The counters are not decoration. "Did the cache work" is otherwise a question
//! about a frame time, and a frame time is not something a test can assert on.

use std::collections::HashMap;

/// Shaped paragraphs, held between layout passes and reachable by content.
///
/// Generic over the paragraph type so it can be unit-tested without a renderer,
/// a font system or a window. What it does is bookkeeping; what it holds is
/// opaque to it.
#[derive(Debug)]
pub struct ParagraphCache<P> {
    /// Paragraphs available for reuse, by fingerprint. A `Vec` per key because
    /// identical blocks are ordinary.
    pool: HashMap<u64, Vec<P>>,
    built: usize,
    reused: usize,
}

impl<P> Default for ParagraphCache<P> {
    fn default() -> Self {
        Self {
            pool: HashMap::new(),
            built: 0,
            reused: 0,
        }
    }
}

impl<P> ParagraphCache<P> {
    /// Open a layout pass, offering the previous pass's paragraphs for reuse.
    ///
    /// Anything not taken during the pass is dropped when the next one begins,
    /// which is what bounds the pool: a transcript that scrolls through ten
    /// thousand rows holds the paragraphs of one pass, not of all of them.
    pub fn begin(&mut self, previous: impl IntoIterator<Item = (u64, P)>) {
        self.pool.clear();
        for (key, paragraph) in previous {
            self.pool.entry(key).or_default().push(paragraph);
        }
    }

    /// The paragraph for `key`, reused if the previous pass had one and built
    /// by `shape` otherwise.
    pub fn take(&mut self, key: u64, shape: impl FnOnce() -> P) -> P {
        match self.pool.get_mut(&key).and_then(Vec::pop) {
            Some(paragraph) => {
                self.reused += 1;
                paragraph
            }
            None => {
                self.built += 1;
                shape()
            }
        }
    }

    /// How many paragraphs this cache has ever shaped, and how many it has
    /// handed back without shaping. Monotonic across passes, so a test can
    /// measure one pass by differencing.
    pub fn stats(&self) -> (usize, usize) {
        (self.built, self.reused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pass over `blocks`, returning what it produced and what it cost.
    fn pass(cache: &mut ParagraphCache<String>, blocks: &[&str]) -> (Vec<(u64, String)>, usize) {
        let before = cache.stats().0;
        let out: Vec<(u64, String)> = blocks
            .iter()
            .map(|text| {
                let key = key_of(text);
                (key, cache.take(key, || (*text).to_string()))
            })
            .collect();
        (out, cache.stats().0 - before)
    }

    fn key_of(text: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        hasher.finish()
    }

    /// The reason this is not a `Vec`: a mid-vector insert must cost one
    /// paragraph, not N. Break the content key back to an index key and this
    /// asserts 4 instead of 1.
    #[test]
    fn a_mid_vector_insert_shapes_exactly_one_paragraph() {
        let mut cache = ParagraphCache::default();
        let (first, built) = pass(&mut cache, &["a", "b", "c", "d"]);
        assert_eq!(built, 4, "the first pass shapes everything");

        cache.begin(first);
        // The image carrier lands between `b` and `c`, shifting `c` and `d`.
        let (_, built) = pass(&mut cache, &["a", "b", "image", "c", "d"]);
        assert_eq!(built, 1, "only the new row is shaped");
    }

    /// And the reused paragraphs must be the *right* ones. An index-keyed cache
    /// also reports one rebuild here — while handing `c` the paragraph shaped
    /// for the image row.
    #[test]
    fn an_insert_does_not_reattach_paragraphs_to_the_wrong_blocks() {
        let mut cache = ParagraphCache::default();
        let (first, _) = pass(&mut cache, &["a", "b", "c", "d"]);
        cache.begin(first);
        let (second, _) = pass(&mut cache, &["a", "b", "image", "c", "d"]);
        let texts: Vec<&str> = second.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(texts, ["a", "b", "image", "c", "d"]);
    }

    /// Two identical rows are ordinary — `ls` twice, a blank line twice. If one
    /// key held one paragraph they would take turns reshaping each other on
    /// every frame, which is the pathological case a naive map has.
    #[test]
    fn duplicate_blocks_each_keep_a_paragraph() {
        let mut cache = ParagraphCache::default();
        let (first, built) = pass(&mut cache, &["ls", "ls", "ls"]);
        assert_eq!(built, 3);
        cache.begin(first);
        let (_, built) = pass(&mut cache, &["ls", "ls", "ls"]);
        assert_eq!(built, 0, "all three came back from the pool");
    }

    /// Nothing survives that was not offered. A pass that drops half the
    /// transcript must not leave the other half's paragraphs alive forever.
    #[test]
    fn paragraphs_not_carried_forward_are_dropped() {
        let mut cache = ParagraphCache::default();
        let (first, _) = pass(&mut cache, &["a", "b", "c"]);
        // Only `a` is offered back.
        cache.begin(first.into_iter().take(1).collect::<Vec<_>>());
        let (_, built) = pass(&mut cache, &["a", "b", "c"]);
        assert_eq!(built, 2, "b and c had to be reshaped");
    }
}
