//! Deterministic, index-addressable noise and PRBS sources
//! (`docs/DESIGN.md` §8.2, goal G3).
//!
//! Two constraints shape everything here. Noise must be reproducible from the
//! spec's seed, and a range rendered in parallel chunks must equal the same
//! range rendered serially. Both are met by making every source a *function of
//! the sample index* rather than a running state:
//!
//! - Each noise node derives its own 32-byte key from `blake3(seed ‖ path)`,
//!   so adding a node elsewhere in the tree cannot perturb an existing node's
//!   stream, and the same node in two specs with the same seed draws the same
//!   numbers.
//! - `ChaCha12Rng` is seekable, so the draw for sample *i* is at a fixed word
//!   position. A chunk starting at *i* seeks there and reads forward.
//! - Pink and brown noise are Voss-McCartney octave sums, `white(k, i >> k)`,
//!   which is addressable. An exact `1/f²` random walk is a prefix sum and is
//!   not, and window-independent rendering is the harder constraint.
//! - A PRBS is a linear recurrence, so its state at index *i* is `M^i · s₀`
//!   over GF(2); repeated squaring reaches any index in `O(log i)` and the
//!   sequence then steps forward one bit per sample.

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha12Rng;

use crate::spec::NoiseKind;

/// Octaves summed for pink and brown noise. Sixteen covers ~5 decades below
/// the sample rate, which is past the resolution of any pyramid the scope
/// draws.
const OCTAVES: usize = 16;

/// 32-bit words each sample consumes, so seeking is `index * WORDS_PER_DRAW`.
const WORDS_PER_DRAW: u128 = 4;

/// The key one noise node draws from.
///
/// `path` is the node's position in the tree, written by the renderer as it
/// descends (`/terms/1/input`, say).
#[must_use]
pub fn stream_key(seed: u64, path: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seed.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(path.as_bytes());
    *hasher.finalize().as_bytes()
}

/// A seekable stream of uniform draws, addressed by sample index.
#[derive(Debug, Clone)]
pub struct Stream {
    rng: ChaCha12Rng,
    /// The index the next [`Stream::next_pair`] returns, so a sequential walk
    /// costs no reseek.
    next_index: u64,
}

impl Stream {
    #[must_use]
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            rng: ChaCha12Rng::from_seed(key),
            next_index: 0,
        }
    }

    /// Positions the stream at `index`.
    pub fn seek(&mut self, index: u64) {
        if index != self.next_index {
            self.rng.set_word_pos(u128::from(index) * WORDS_PER_DRAW);
            self.next_index = index;
        }
    }

    /// The two 64-bit draws belonging to the current index.
    pub fn next_pair(&mut self) -> (u64, u64) {
        self.next_index = self.next_index.wrapping_add(1);
        (self.rng.next_u64(), self.rng.next_u64())
    }

    /// The draw for `index`, seeking only when it is not the next one.
    pub fn pair_at(&mut self, index: u64) -> (u64, u64) {
        self.seek(index);
        self.next_pair()
    }
}

/// A `u64` draw as a float in `[0, 1)`, using the 53 bits an `f64` can hold
/// exactly.
#[must_use]
pub fn unit(bits: u64) -> f64 {
    (bits >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// A `u64` draw as a float in `[-1, 1)`.
#[must_use]
pub fn unit_signed(bits: u64) -> f64 {
    2.0 * unit(bits) - 1.0
}

/// One standard normal draw by the Box-Muller transform. Deterministic given
/// the two draws, which is what G3 asks of it.
#[must_use]
pub fn gaussian(a: u64, b: u64) -> f64 {
    // `ln(0)` is the one input Box-Muller cannot take; nudge it into (0, 1].
    let u1 = 1.0 - unit(a);
    let u2 = unit(b);
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// A noise source positioned by sample index.
///
/// Held across a chunk so a sequential fill never reseeks; constructing one is
/// a ChaCha key schedule per octave, which is why it is not per-sample.
#[derive(Debug, Clone)]
pub struct Noise {
    kind: NoiseKind,
    /// One stream for white and Gaussian; one per octave for pink and brown.
    streams: Vec<Stream>,
    /// The last value drawn per octave, and the octave index it belongs to.
    held: Vec<(u64, f64)>,
    normaliser: f64,
}

impl Noise {
    #[must_use]
    pub fn new(kind: NoiseKind, key: [u8; 32]) -> Self {
        let octaves = match kind {
            NoiseKind::Gaussian | NoiseKind::Uniform => 1,
            NoiseKind::Pink | NoiseKind::Brown => OCTAVES,
        };
        let streams = (0..octaves)
            .map(|octave| {
                let mut octave_key = key;
                // Fold the octave into the key so the octaves are independent
                // streams rather than offsets into one.
                octave_key[0] ^= octave as u8;
                octave_key[1] ^= 0x5b;
                Stream::new(octave_key)
            })
            .collect();
        let normaliser = match kind {
            NoiseKind::Pink => (octaves as f64).sqrt().recip(),
            NoiseKind::Brown => {
                let power: f64 = (0..octaves).map(|o| 4f64.powi(o as i32)).sum();
                power.sqrt().recip()
            }
            _ => 1.0,
        };
        Self {
            kind,
            streams,
            held: vec![(u64::MAX, 0.0); octaves],
            normaliser,
        }
    }

    /// Positions every octave for a fill starting at `index`.
    pub fn seek(&mut self, index: u64) {
        for (octave, stream) in self.streams.iter_mut().enumerate() {
            stream.seek(index >> octave);
        }
        self.held.fill((u64::MAX, 0.0));
    }

    /// The noise value at `index`. Call in increasing index order after a
    /// [`Noise::seek`]; out of order still works, it just reseeks.
    pub fn value(&mut self, index: u64) -> f64 {
        match self.kind {
            NoiseKind::Gaussian => {
                let (a, b) = self.streams[0].pair_at(index);
                gaussian(a, b)
            }
            NoiseKind::Uniform => {
                let (a, _) = self.streams[0].pair_at(index);
                2.0 * unit(a) - 1.0
            }
            NoiseKind::Pink | NoiseKind::Brown => {
                let brown = matches!(self.kind, NoiseKind::Brown);
                let mut sum = 0.0;
                for octave in 0..self.streams.len() {
                    let slot = index >> octave;
                    if self.held[octave].0 != slot {
                        let (a, b) = self.streams[octave].pair_at(slot);
                        self.held[octave] = (slot, gaussian(a, b));
                    }
                    let weight = if brown {
                        f64::from(1u32 << octave)
                    } else {
                        1.0
                    };
                    sum += weight * self.held[octave].1;
                }
                sum * self.normaliser
            }
        }
    }
}

/// Maximal-length feedback taps for a Fibonacci LFSR, indexed by
/// `order - MIN_ORDER`. The polynomials are the standard ones (Xilinx
/// XAPP052); that table numbers register bits from 1 upwards and shifts up,
/// while this register shifts down, so bit `b` of order `n` becomes bit
/// `n - b` here. Every mask therefore has bit 0 set: the bit being shifted
/// out always feeds back.
const MAXIMAL_TAPS: [u32; 31] = [
    0x00000003, //  2: x^2 + x^1 + 1
    0x00000003, //  3: x^3 + x^2 + 1
    0x00000003, //  4: x^4 + x^3 + 1
    0x00000005, //  5: x^5 + x^3 + 1
    0x00000003, //  6: x^6 + x^5 + 1
    0x00000003, //  7: x^7 + x^6 + 1
    0x0000001D, //  8: x^8 + x^6 + x^5 + x^4 + 1
    0x00000011, //  9: x^9 + x^5 + 1
    0x00000009, // 10: x^10 + x^7 + 1
    0x00000005, // 11: x^11 + x^9 + 1
    0x00000941, // 12: x^12 + x^6 + x^4 + x^1 + 1
    0x00001601, // 13: x^13 + x^4 + x^3 + x^1 + 1
    0x00002A01, // 14: x^14 + x^5 + x^3 + x^1 + 1
    0x00000003, // 15: x^15 + x^14 + 1
    0x0000100B, // 16: x^16 + x^15 + x^13 + x^4 + 1
    0x00000009, // 17: x^17 + x^14 + 1
    0x00000081, // 18: x^18 + x^11 + 1
    0x00062001, // 19: x^19 + x^6 + x^2 + x^1 + 1
    0x00000009, // 20: x^20 + x^17 + 1
    0x00000005, // 21: x^21 + x^19 + 1
    0x00000003, // 22: x^22 + x^21 + 1
    0x00000021, // 23: x^23 + x^18 + 1
    0x00000087, // 24: x^24 + x^23 + x^22 + x^17 + 1
    0x00000009, // 25: x^25 + x^22 + 1
    0x03100001, // 26: x^26 + x^6 + x^2 + x^1 + 1
    0x06400001, // 27: x^27 + x^5 + x^2 + x^1 + 1
    0x00000009, // 28: x^28 + x^25 + 1
    0x00000005, // 29: x^29 + x^27 + 1
    0x25000001, // 30: x^30 + x^6 + x^4 + x^1 + 1
    0x00000009, // 31: x^31 + x^28 + 1
    0xC0000401, // 32: x^32 + x^22 + x^2 + x^1 + 1
];

/// The smallest LFSR order the tap table covers.
pub const MIN_ORDER: u8 = 2;
/// The largest LFSR order the tap table covers. A 32-bit state is what the
/// GF(2) transition matrix is sized for.
pub const MAX_ORDER: u8 = 32;

/// The built-in maximal-length taps for `order`, or `None` outside
/// `MIN_ORDER..=MAX_ORDER`.
#[must_use]
pub fn default_taps(order: u8) -> Option<u32> {
    if !(MIN_ORDER..=MAX_ORDER).contains(&order) {
        return None;
    }
    MAXIMAL_TAPS.get(usize::from(order - MIN_ORDER)).copied()
}

/// A Fibonacci LFSR that can be positioned at any index.
#[derive(Debug, Clone)]
pub struct Prbs {
    order: u8,
    taps: u32,
    state: u32,
    next_index: u64,
}

impl Prbs {
    /// A generator of `order` bits with `taps`, seeded to all ones — the one
    /// state a maximal-length LFSR can never reach by accident, and the
    /// conventional starting point.
    #[must_use]
    pub fn new(order: u8, taps: u32) -> Self {
        let order = order.clamp(MIN_ORDER, MAX_ORDER);
        let mask = state_mask(order);
        let taps = if taps & mask == 0 {
            default_taps(order).unwrap_or(0b11)
        } else {
            taps & mask
        };
        Self {
            order,
            taps,
            state: mask,
            next_index: 0,
        }
    }

    /// Positions the generator so the next [`Prbs::next_bit`] is the bit at
    /// `index`. `O(log index)` by squaring the transition matrix.
    pub fn seek(&mut self, index: u64) {
        if index == self.next_index {
            return;
        }
        let mask = state_mask(self.order);
        let mut result = identity(self.order);
        let mut power = step_matrix(self.order, self.taps);
        let mut remaining = index;
        while remaining > 0 {
            if remaining & 1 == 1 {
                result = multiply(&result, &power, self.order);
            }
            power = multiply(&power, &power, self.order);
            remaining >>= 1;
        }
        self.state = apply(&result, mask, self.order);
        self.next_index = index;
    }

    /// The next bit, advancing the register.
    pub fn next_bit(&mut self) -> bool {
        let bit = self.state & 1 == 1;
        let feedback = u32::from((self.state & self.taps).count_ones() & 1 == 1);
        self.state = (self.state >> 1) | (feedback << (self.order - 1));
        self.next_index = self.next_index.wrapping_add(1);
        bit
    }

    /// The bit at `index`, seeking only when it is not the next one.
    pub fn bit_at(&mut self, index: u64) -> bool {
        self.seek(index);
        self.next_bit()
    }
}

const fn state_mask(order: u8) -> u32 {
    if order >= 32 {
        u32::MAX
    } else {
        (1u32 << order) - 1
    }
}

/// One step of the LFSR as a GF(2) matrix: row *i* is the set of state bits
/// that XOR into bit *i* of the next state.
fn step_matrix(order: u8, taps: u32) -> [u32; 32] {
    let mut rows = [0u32; 32];
    for (i, row) in rows.iter_mut().enumerate().take(usize::from(order) - 1) {
        // The register shifts down, so bit i takes bit i + 1.
        *row = 1u32 << (i + 1);
    }
    // The top bit takes the feedback parity.
    rows[usize::from(order) - 1] = taps & state_mask(order);
    rows
}

fn identity(order: u8) -> [u32; 32] {
    let mut rows = [0u32; 32];
    for (i, row) in rows.iter_mut().enumerate().take(usize::from(order)) {
        *row = 1u32 << i;
    }
    rows
}

/// `a · b` over GF(2), for `order`-square matrices held as bit rows.
fn multiply(a: &[u32; 32], b: &[u32; 32], order: u8) -> [u32; 32] {
    let mut out = [0u32; 32];
    for i in 0..usize::from(order) {
        let mut row = 0u32;
        let mut selected = a[i];
        while selected != 0 {
            let k = selected.trailing_zeros() as usize;
            row ^= b[k];
            selected &= selected - 1;
        }
        out[i] = row;
    }
    out
}

/// `m · v` over GF(2).
fn apply(m: &[u32; 32], v: u32, order: u8) -> u32 {
    let mut out = 0u32;
    for (i, row) in m.iter().enumerate().take(usize::from(order)) {
        if (row & v).count_ones() & 1 == 1 {
            out |= 1u32 << i;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_seeks_to_the_same_draws_it_walks_to() {
        let key = stream_key(42, "/root");
        let mut walked = Stream::new(key);
        let sequential: Vec<_> = (0..64).map(|i| walked.pair_at(i)).collect();

        let mut sought = Stream::new(key);
        for (index, expected) in sequential.iter().enumerate().rev() {
            assert_eq!(sought.pair_at(index as u64), *expected, "index {index}");
        }
    }

    #[test]
    fn a_node_path_changes_the_stream_but_the_seed_still_reproduces_it() {
        assert_ne!(stream_key(1, "/a"), stream_key(1, "/b"));
        assert_ne!(stream_key(1, "/a"), stream_key(2, "/a"));
        assert_eq!(stream_key(1, "/a"), stream_key(1, "/a"));
    }

    #[test]
    fn every_noise_kind_is_addressable_by_index() {
        for kind in NoiseKind::ALL {
            let key = stream_key(9, "/noise");
            let mut whole = Noise::new(kind, key);
            whole.seek(0);
            let expected: Vec<_> = (0..300).map(|i| whole.value(i)).collect();

            // The same range as three chunks, each seeking to its own start.
            let mut chunked = Vec::new();
            for range in [(0u64, 100u64), (100, 210), (210, 300)] {
                let mut noise = Noise::new(kind, key);
                noise.seek(range.0);
                chunked.extend((range.0..range.1).map(|i| noise.value(i)));
            }
            assert_eq!(chunked, expected, "{kind:?}");
        }
    }

    #[test]
    fn gaussian_noise_has_roughly_unit_variance() {
        let mut noise = Noise::new(NoiseKind::Gaussian, stream_key(3, "/n"));
        noise.seek(0);
        let values: Vec<_> = (0..20_000).map(|i| noise.value(i)).collect();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
        assert!(mean.abs() < 0.05, "mean {mean}");
        assert!((variance - 1.0).abs() < 0.1, "variance {variance}");
    }

    #[test]
    fn uniform_noise_stays_inside_its_range() {
        let mut noise = Noise::new(NoiseKind::Uniform, stream_key(4, "/n"));
        noise.seek(0);
        for i in 0..10_000 {
            let v = noise.value(i);
            assert!((-1.0..1.0).contains(&v), "{v}");
        }
    }

    #[test]
    fn pink_and_brown_noise_are_tilted_towards_low_frequencies() {
        // The variance of the first difference, over the variance of the
        // signal, measures spectral tilt without caring about scale: white
        // noise sits at 2, and the more energy moves down the spectrum the
        // smaller it gets.
        let tilt = |kind| {
            let mut noise = Noise::new(kind, stream_key(5, "/n"));
            noise.seek(0);
            let values: Vec<f64> = (0..16_384).map(|i| noise.value(i)).collect();
            let variance = |xs: &[f64]| {
                let mean = xs.iter().sum::<f64>() / xs.len() as f64;
                xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / xs.len() as f64
            };
            let differences: Vec<f64> = values.windows(2).map(|w| w[1] - w[0]).collect();
            variance(&differences) / variance(&values)
        };
        let white = tilt(NoiseKind::Gaussian);
        let pink = tilt(NoiseKind::Pink);
        let brown = tilt(NoiseKind::Brown);
        assert!((white - 2.0).abs() < 0.1, "white {white}");
        assert!(pink < white / 2.0, "pink {pink} vs white {white}");
        assert!(brown < pink / 2.0, "brown {brown} vs pink {pink}");
    }

    #[test]
    fn a_prbs_repeats_after_its_maximal_period() {
        for order in [3u8, 5, 7, 9] {
            let taps = default_taps(order).unwrap();
            let period = (1u64 << order) - 1;
            let mut prbs = Prbs::new(order, taps);
            prbs.seek(0);
            let first: Vec<_> = (0..period).map(|_| prbs.next_bit()).collect();
            // Maximal length: every state but zero is visited, so the run has
            // 2^(n-1) ones.
            assert_eq!(
                first.iter().filter(|b| **b).count() as u64,
                1 << (order - 1),
                "order {order}"
            );
            let repeat: Vec<_> = (0..period).map(|_| prbs.next_bit()).collect();
            assert_eq!(first, repeat, "order {order}");
        }
    }

    #[test]
    fn a_prbs_seeks_to_the_same_bits_it_walks_to() {
        let mut walked = Prbs::new(16, default_taps(16).unwrap());
        let sequential: Vec<_> = (0..500).map(|_| walked.next_bit()).collect();
        let mut sought = Prbs::new(16, default_taps(16).unwrap());
        for (index, expected) in sequential.iter().enumerate().rev() {
            assert_eq!(sought.bit_at(index as u64), *expected, "index {index}");
        }
        // And far past the start, where only the matrix power can reach.
        let mut far = Prbs::new(16, default_taps(16).unwrap());
        far.seek(1_000_000);
        let a = far.next_bit();
        let mut stepped = Prbs::new(16, default_taps(16).unwrap());
        stepped.seek(999_999);
        stepped.next_bit();
        assert_eq!(stepped.next_bit(), a);
    }

    #[test]
    fn every_order_in_the_table_is_maximal_length() {
        for order in MIN_ORDER..=20 {
            let taps = default_taps(order).expect("tap table covers the order");
            let period = (1u64 << order) - 1;
            let mut prbs = Prbs::new(order, taps);
            let start = prbs.state;
            for _ in 0..period - 1 {
                prbs.next_bit();
                assert_ne!(prbs.state, start, "order {order} repeats early");
            }
            prbs.next_bit();
            assert_eq!(prbs.state, start, "order {order} does not close its cycle");
        }
    }

    #[test]
    fn taps_outside_the_table_fall_back_rather_than_panicking() {
        assert!(default_taps(1).is_none());
        assert!(default_taps(33).is_none());
        // A zero mask is unusable, so the default takes over.
        let prbs = Prbs::new(9, 0);
        assert_eq!(prbs.taps, default_taps(9).unwrap());
    }
}
