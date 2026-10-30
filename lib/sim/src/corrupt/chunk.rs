//! How a case's bytes are cut up before they are fed.

use crate::prng::Prng;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chunking {
    Whole,
    Fixed(usize),
    Random { max: usize, seed: u64 },
}

impl Chunking {
    pub fn pick(prng: &mut Prng, len: usize) -> Chunking {
        let small = len <= 8 * 1024;
        match prng.below(8) {
            0 | 1 => Chunking::Whole,
            2 if small => Chunking::Fixed(1),
            3 if small => Chunking::Fixed(prng.range(2, 8) as usize),
            4 if small => Chunking::Random { max: 4, seed: prng.next_u64() },
            5 => Chunking::Random { max: 64, seed: prng.next_u64() },
            6 => Chunking::Random { max: 1024, seed: prng.next_u64() },
            _ => Chunking::Random { max: 16 * 1024, seed: prng.next_u64() },
        }
    }

    pub fn pieces<'a>(&self, data: &'a [u8]) -> Chunks<'a> {
        let seed = match *self {
            Chunking::Random { seed, .. } => seed,
            _ => 0,
        };
        Chunks { rest: data, chunking: *self, prng: Prng::new(seed), empty_owed: false }
    }

    /// [`Self::pieces`], except an empty input is one empty chunk.
    pub fn chunks<'a>(&self, data: &'a [u8]) -> Chunks<'a> {
        Chunks { empty_owed: data.is_empty(), ..self.pieces(data) }
    }

    pub fn count(&self, data: &[u8]) -> usize {
        self.chunks(data).count()
    }

    pub fn split<'a>(&self, data: &'a [u8]) -> Vec<&'a [u8]> {
        self.chunks(data).collect()
    }
}

pub struct Chunks<'a> {
    rest: &'a [u8],
    chunking: Chunking,
    prng: Prng,
    empty_owed: bool,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return std::mem::take(&mut self.empty_owed).then_some(self.rest);
        }
        let n = match self.chunking {
            Chunking::Whole => self.rest.len(),
            Chunking::Fixed(n) => n.max(1),
            Chunking::Random { max, .. } => self.prng.range(1, max.max(1) as u64 + 1) as usize,
        };
        let (head, tail) = self.rest.split_at(n.min(self.rest.len()));
        self.rest = tail;
        Some(head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Chunking; 6] = [
        Chunking::Whole,
        Chunking::Fixed(0),
        Chunking::Fixed(1),
        Chunking::Fixed(7),
        Chunking::Random { max: 0, seed: 3 },
        Chunking::Random { max: 5, seed: 4 },
    ];

    #[test]
    fn pieces_cover_the_input_in_order_and_are_never_empty() {
        let data: Vec<u8> = (0..=255).collect();
        for len in [0, 1, 2, 7, 8, 256] {
            for chunking in ALL {
                let pieces: Vec<&[u8]> = chunking.pieces(&data[..len]).collect();
                assert!(pieces.iter().all(|p| !p.is_empty()), "{chunking:?} on {len}");
                assert_eq!(pieces.concat(), data[..len], "{chunking:?} on {len}");
            }
        }
    }

    #[test]
    fn an_empty_input_is_no_piece_and_one_chunk() {
        for chunking in ALL {
            assert_eq!(chunking.pieces(&[]).count(), 0);
            assert_eq!(chunking.split(&[]), [&[][..]]);
            assert_eq!(chunking.count(&[]), 1);
            assert_eq!(chunking.count(&[9]), 1);
        }
    }

    #[test]
    fn chunk_sizes_follow_the_chunking() {
        let data = [0u8; 100];
        assert_eq!(Chunking::Whole.count(&data), 1);
        assert_eq!(Chunking::Fixed(7).count(&data), 15);
        let random = Chunking::Random { max: 5, seed: 1 };
        assert!(random.chunks(&data).all(|c| (1..=5).contains(&c.len())));
        assert_eq!(random.split(&data), random.split(&data));
        assert_ne!(random.split(&data), Chunking::Random { max: 5, seed: 2 }.split(&data));
    }

    #[test]
    fn pick_keeps_tiny_chunks_for_small_inputs() {
        let mut prng = Prng::new(8);
        for _ in 0..2_000 {
            match Chunking::pick(&mut prng, 9_000) {
                Chunking::Fixed(n) => panic!("fixed {n} on a large input"),
                Chunking::Random { max, .. } => assert!(max >= 64),
                Chunking::Whole => {}
            }
        }
    }
}
