//! Minimal `java.util.BitSet` work-alike used by the usage maps.

#[derive(Clone, Debug, Default)]
pub struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    pub fn new() -> BitSet {
        BitSet { words: Vec::new() }
    }

    fn ensure(&mut self, bit: usize) {
        let n = bit / 64 + 1;
        if self.words.len() < n {
            self.words.resize(n, 0);
        }
    }

    pub fn get(&self, bit: usize) -> bool {
        let w = bit / 64;
        (w < self.words.len()) && ((self.words[w] >> (bit % 64)) & 1) != 0
    }

    pub fn set(&mut self, bit: usize) {
        self.ensure(bit);
        self.words[bit / 64] |= 1u64 << (bit % 64);
    }

    pub fn clear_bit(&mut self, bit: usize) {
        let w = bit / 64;
        if w < self.words.len() {
            self.words[w] &= !(1u64 << (bit % 64));
        }
    }

    pub fn clear(&mut self) {
        self.words.clear();
    }

    /// `BitSet.set(from, to)`
    pub fn set_range(&mut self, from: usize, to: usize) {
        for b in from..to {
            self.set(b);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    pub fn cardinality(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// `BitSet.length()` - highest set bit + 1.
    pub fn length(&self) -> usize {
        for i in (0..self.words.len()).rev() {
            if self.words[i] != 0 {
                return i * 64 + (64 - self.words[i].leading_zeros() as usize);
            }
        }
        0
    }

    /// `BitSet.nextSetBit(from)`; -1 when none.
    pub fn next_set_bit(&self, from: usize) -> i64 {
        let mut i = from;
        while i < self.words.len() * 64 {
            let w = i / 64;
            let word = self.words[w] & (!0u64 << (i % 64));
            if word != 0 {
                let bit = word.trailing_zeros() as usize;
                return (w * 64 + bit) as i64;
            }
            i = (w + 1) * 64;
        }
        -1
    }

    /// Previous set bit strictly before `from`; -1 when none.
    pub fn prev_set_bit(&self, from: usize) -> i64 {
        if from == 0 {
            return -1;
        }
        let mut i = from - 1;
        loop {
            let w = i / 64;
            if w < self.words.len() {
                let word = self.words[w] & (!0u64 >> (63 - (i % 64)));
                if word != 0 {
                    let bit = 63 - word.leading_zeros() as usize;
                    return (w * 64 + bit) as i64;
                }
            }
            if w == 0 {
                return -1;
            }
            i = w * 64 - 1;
        }
    }

    pub fn iter(&self) -> BitSetIter<'_> {
        BitSetIter { set: self, next: 0 }
    }
}

pub struct BitSetIter<'a> {
    set: &'a BitSet,
    next: usize,
}

impl<'a> Iterator for BitSetIter<'a> {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        let b = self.set.next_set_bit(self.next);
        if b < 0 {
            None
        } else {
            self.next = b as usize + 1;
            Some(b as usize)
        }
    }
}
