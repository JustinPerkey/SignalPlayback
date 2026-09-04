//! Signal identity, sample storage and the fixed core of a signal's metadata.
//!
//! See `docs/DESIGN.md` §6: every signal has a fixed core (name, dtype, rate,
//! t0, count, statistics), a declared [`Domain`] that drives rendering and port
//! typing, a [`Provenance`], and any number of user-defined properties.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::group::GroupId;
use crate::props::Attributes;
use crate::stats::SignalStats;
use crate::time::{SampleIndex, TimeRange, Timebase};

crate::id_newtype! {
    /// Identifies a signal row.
    SignalId
}

crate::id_newtype! {
    /// Identifies one execution of a pipeline.
    RunId
}

/// Sample element type. The name encodes the width in bits, so `C64` is a
/// 64-bit complex value: two `f32` components (NumPy's `complex64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DType {
    F32,
    F64,
    I16,
    I32,
    C64,
    U8,
}

impl DType {
    /// Every dtype, in the order used by the `.sigbin` header codes.
    pub const ALL: [Self; 6] = [
        Self::F32,
        Self::F64,
        Self::I16,
        Self::I32,
        Self::C64,
        Self::U8,
    ];

    /// The code written into the `.sigbin` header (§5.3).
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::F32 => 0,
            Self::F64 => 1,
            Self::I16 => 2,
            Self::I32 => 3,
            Self::C64 => 4,
            Self::U8 => 5,
        }
    }

    /// Inverse of [`DType::code`].
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::F32),
            1 => Some(Self::F64),
            2 => Some(Self::I16),
            3 => Some(Self::I32),
            4 => Some(Self::C64),
            5 => Some(Self::U8),
            _ => None,
        }
    }

    /// Bytes per sample.
    #[must_use]
    pub const fn size_bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::I16 => 2,
            Self::F32 | Self::I32 => 4,
            Self::F64 | Self::C64 => 8,
        }
    }

    /// The token stored in the database, matching the `signal.dtype` check
    /// constraint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::C64 => "c64",
            Self::U8 => "u8",
        }
    }

    #[must_use]
    pub const fn is_complex(self) -> bool {
        matches!(self, Self::C64)
    }

    /// Integer dtypes carry a scale/offset so raw counts can be read back in
    /// engineering units.
    #[must_use]
    pub const fn is_integer(self) -> bool {
        matches!(self, Self::I16 | Self::I32 | Self::U8)
    }
}

impl fmt::Display for DType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Failure to parse an enum token that came from the database or a file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} '{token}'")]
pub struct ParseTokenError {
    pub kind: &'static str,
    pub token: String,
}

impl FromStr for DType {
    type Err = ParseTokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|d| d.as_str() == s)
            .ok_or_else(|| ParseTokenError {
                kind: "dtype",
                token: s.to_owned(),
            })
    }
}

/// What kind of thing the samples represent.
///
/// `Domain` does real work: it selects the default renderer, gates which
/// pipeline stages accept a signal (§9.3 port typing), and drives per-domain
/// defaults such as a discrete y-axis for logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    /// Continuous-valued real; drawn as a trace.
    #[default]
    Analog,
    /// Two- or multi-level; drawn as logic lanes with transitions.
    DigitalLogic,
    /// Complex; drawn as I/Q traces, magnitude, or a constellation.
    BasebandIq,
    /// Discrete decisions at symbol instants; drawn as stems plus labels.
    Symbols,
    /// Packed bitstream; drawn as a bit ribbon.
    Bits,
}

impl Domain {
    pub const ALL: [Self; 5] = [
        Self::Analog,
        Self::DigitalLogic,
        Self::BasebandIq,
        Self::Symbols,
        Self::Bits,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Analog => "analog",
            Self::DigitalLogic => "digital_logic",
            Self::BasebandIq => "baseband_iq",
            Self::Symbols => "symbols",
            Self::Bits => "bits",
        }
    }

    /// Human-readable label for menus and the inspector.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Analog => "Analog",
            Self::DigitalLogic => "Digital logic",
            Self::BasebandIq => "Baseband I/Q",
            Self::Symbols => "Symbols",
            Self::Bits => "Bits",
        }
    }

    /// Whether the domain's natural sample type is complex.
    #[must_use]
    pub const fn is_complex(self) -> bool {
        matches!(self, Self::BasebandIq)
    }

    /// Whether values are drawn from a small discrete set, which the renderer
    /// uses to pick a stepped y-axis instead of an autoscaled one.
    #[must_use]
    pub const fn is_discrete(self) -> bool {
        matches!(self, Self::DigitalLogic | Self::Symbols | Self::Bits)
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Domain {
    type Err = ParseTokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|d| d.as_str() == s)
            .ok_or_else(|| ParseTokenError {
                kind: "domain",
                token: s.to_owned(),
            })
    }
}

/// Where a signal came from. A derived signal names the run and stage that
/// produced it, so any result is traceable back to its algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Provenance {
    #[default]
    Imported,
    Generated,
    Derived {
        run_id: RunId,
        stage_ordinal: u16,
    },
}

impl Provenance {
    /// The token stored in `signal.provenance`; the run/stage detail lives in
    /// the run tables rather than in this column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Imported => "imported",
            Self::Generated => "generated",
            Self::Derived { .. } => "derived",
        }
    }
}

/// A 64-bit complex sample: two `f32` components.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct C64 {
    pub re: f32,
    pub im: f32,
}

impl C64 {
    #[must_use]
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }

    #[must_use]
    pub fn magnitude(self) -> f64 {
        f64::from(self.re).hypot(f64::from(self.im))
    }

    #[must_use]
    pub fn phase_rad(self) -> f64 {
        f64::from(self.im).atan2(f64::from(self.re))
    }
}

/// Affine mapping from stored counts to engineering units, applied on read.
/// Identity for floating-point dtypes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Scaling {
    pub scale: f64,
    pub offset: f64,
}

impl Scaling {
    pub const IDENTITY: Self = Self {
        scale: 1.0,
        offset: 0.0,
    };

    #[must_use]
    pub const fn new(scale: f64, offset: f64) -> Self {
        Self { scale, offset }
    }

    #[must_use]
    pub fn is_identity(self) -> bool {
        self.scale == 1.0 && self.offset == 0.0
    }

    #[must_use]
    pub fn apply(self, raw: f64) -> f64 {
        raw * self.scale + self.offset
    }
}

impl Default for Scaling {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Typed sample storage, one variant per [`DType`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Samples {
    F32(Vec<f32>),
    F64(Vec<f64>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    C64(Vec<C64>),
    U8(Vec<u8>),
}

impl Samples {
    #[must_use]
    pub fn dtype(&self) -> DType {
        match self {
            Self::F32(_) => DType::F32,
            Self::F64(_) => DType::F64,
            Self::I16(_) => DType::I16,
            Self::I32(_) => DType::I32,
            Self::C64(_) => DType::C64,
            Self::U8(_) => DType::U8,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
            Self::I16(v) => v.len(),
            Self::I32(v) => v.len(),
            Self::C64(v) => v.len(),
            Self::U8(v) => v.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// An empty buffer of the given dtype with room for `capacity` samples.
    #[must_use]
    pub fn with_capacity(dtype: DType, capacity: usize) -> Self {
        match dtype {
            DType::F32 => Self::F32(Vec::with_capacity(capacity)),
            DType::F64 => Self::F64(Vec::with_capacity(capacity)),
            DType::I16 => Self::I16(Vec::with_capacity(capacity)),
            DType::I32 => Self::I32(Vec::with_capacity(capacity)),
            DType::C64 => Self::C64(Vec::with_capacity(capacity)),
            DType::U8 => Self::U8(Vec::with_capacity(capacity)),
        }
    }
}

/// Samples plus the scaling needed to read them in engineering units.
///
/// Processing is done in `f64` internally and narrowed on write (§17.10), so
/// [`SampleBuffer::value`] is the single place raw storage becomes a real
/// number for the renderer, statistics and stages alike.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleBuffer {
    samples: Samples,
    scaling: Scaling,
}

impl SampleBuffer {
    #[must_use]
    pub fn new(samples: Samples) -> Self {
        Self {
            samples,
            scaling: Scaling::IDENTITY,
        }
    }

    /// Attaches a scale/offset. Ignored for float dtypes, which are always
    /// stored in engineering units already.
    #[must_use]
    pub fn with_scaling(mut self, scaling: Scaling) -> Self {
        if self.samples.dtype().is_integer() {
            self.scaling = scaling;
        }
        self
    }

    #[must_use]
    pub fn samples(&self) -> &Samples {
        &self.samples
    }

    #[must_use]
    pub fn into_samples(self) -> Samples {
        self.samples
    }

    #[must_use]
    pub fn scaling(&self) -> Scaling {
        self.scaling
    }

    #[must_use]
    pub fn dtype(&self) -> DType {
        self.samples.dtype()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The scaled real value of one sample; the magnitude for complex samples,
    /// which is what a single-axis trace and the render pyramid want. Use
    /// [`SampleBuffer::complex`] for I/Q access.
    #[must_use]
    pub fn value(&self, index: SampleIndex) -> Option<f64> {
        let i = usize::try_from(index).ok()?;
        let raw = match &self.samples {
            Samples::F32(v) => f64::from(*v.get(i)?),
            Samples::F64(v) => *v.get(i)?,
            Samples::I16(v) => f64::from(*v.get(i)?),
            Samples::I32(v) => f64::from(*v.get(i)?),
            Samples::C64(v) => v.get(i)?.magnitude(),
            Samples::U8(v) => f64::from(*v.get(i)?),
        };
        Some(self.scaling.apply(raw))
    }

    /// The complex value of one sample, or `None` for a real dtype.
    #[must_use]
    pub fn complex(&self, index: SampleIndex) -> Option<C64> {
        let i = usize::try_from(index).ok()?;
        match &self.samples {
            Samples::C64(v) => v.get(i).copied(),
            _ => None,
        }
    }

    /// Scaled values in order.
    pub fn values(&self) -> impl Iterator<Item = f64> + '_ {
        (0..self.len() as u64).filter_map(|i| self.value(i))
    }

    /// Builds a buffer from `f64` values, narrowing to `dtype`. Non-finite
    /// values narrow to zero for integer dtypes, which cannot represent them.
    #[must_use]
    pub fn from_f64(dtype: DType, values: &[f64]) -> Self {
        let samples = match dtype {
            DType::F32 => Samples::F32(values.iter().map(|&v| v as f32).collect()),
            DType::F64 => Samples::F64(values.to_vec()),
            DType::I16 => Samples::I16(values.iter().map(|&v| saturate::<i16>(v)).collect()),
            DType::I32 => Samples::I32(values.iter().map(|&v| saturate::<i32>(v)).collect()),
            DType::C64 => Samples::C64(values.iter().map(|&v| C64::new(v as f32, 0.0)).collect()),
            DType::U8 => Samples::U8(values.iter().map(|&v| saturate::<u8>(v)).collect()),
        };
        Self::new(samples)
    }

    /// Materialises every scaled value. Allocates; prefer
    /// [`SampleBuffer::values`] on large buffers.
    #[must_use]
    pub fn to_f64(&self) -> Vec<f64> {
        self.values().collect()
    }
}

/// Rounds and clamps into an integer dtype's range; NaN becomes zero.
fn saturate<T>(value: f64) -> T
where
    T: num_bounds::Bounded,
{
    if value.is_nan() {
        return T::from_f64_saturating(0.0);
    }
    T::from_f64_saturating(value.round())
}

/// Minimal saturating-conversion helper, kept private so `sp-core` stays
/// dependency-light.
mod num_bounds {
    pub trait Bounded {
        fn from_f64_saturating(value: f64) -> Self;
    }

    macro_rules! impl_bounded {
        ($($t:ty),*) => {
            $(impl Bounded for $t {
                fn from_f64_saturating(value: f64) -> Self {
                    if value <= <$t>::MIN as f64 {
                        <$t>::MIN
                    } else if value >= <$t>::MAX as f64 {
                        <$t>::MAX
                    } else {
                        value as $t
                    }
                }
            })*
        };
    }

    impl_bounded!(i16, i32, u8);
}

/// The metadata core of a stored signal.
///
/// Sample bytes are not here: they live in a content-addressed blob and are
/// mapped lazily by `sp-store`, so a library of 10 000 signals is browsable
/// without reading a single sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub id: SignalId,
    pub group_id: GroupId,
    /// Position within the group; matches the CSV row order on import.
    pub ordinal: u32,
    pub name: String,
    pub units: Option<String>,
    pub dtype: DType,
    pub domain: Domain,
    pub provenance: Provenance,
    pub timebase: Timebase,
    pub sample_count: u64,
    /// Cached summary statistics; `None` until the summariser has run.
    pub stats: Option<SignalStats>,
    /// Typed properties plus any unrecognised attributes, preserved verbatim.
    pub attributes: Attributes,
}

impl Signal {
    /// Absolute time of sample `index`, for regular signals.
    #[must_use]
    pub fn time_of(&self, index: SampleIndex) -> Option<f64> {
        self.timebase.time_of(index)
    }

    /// Timeline span the signal occupies, for regular signals.
    #[must_use]
    pub fn time_range(&self) -> Option<TimeRange> {
        self.timebase.time_range(self.sample_count)
    }

    #[must_use]
    pub fn duration_s(&self) -> Option<f64> {
        self.timebase.duration_s(self.sample_count)
    }

    /// Bytes the samples occupy on disk, excluding the blob header.
    #[must_use]
    pub fn payload_bytes(&self) -> u64 {
        self.sample_count * self.dtype.size_bytes() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtype_codes_round_trip() {
        for dtype in DType::ALL {
            assert_eq!(DType::from_code(dtype.code()), Some(dtype));
            assert_eq!(dtype.as_str().parse::<DType>().unwrap(), dtype);
        }
        assert_eq!(DType::from_code(9), None);
        assert!("f128".parse::<DType>().is_err());
    }

    #[test]
    fn domain_tokens_match_the_schema_check_constraint() {
        let tokens: Vec<_> = Domain::ALL.iter().map(|d| d.as_str()).collect();
        assert_eq!(
            tokens,
            ["analog", "digital_logic", "baseband_iq", "symbols", "bits"]
        );
        for domain in Domain::ALL {
            assert_eq!(domain.as_str().parse::<Domain>().unwrap(), domain);
        }
    }

    #[test]
    fn integer_samples_read_back_in_engineering_units() {
        let buf = SampleBuffer::new(Samples::I16(vec![0, 100, -100]))
            .with_scaling(Scaling::new(0.01, 1.0));
        assert_eq!(buf.value(0), Some(1.0));
        assert_eq!(buf.value(1), Some(2.0));
        assert_eq!(buf.value(2), Some(0.0));
        assert_eq!(buf.value(3), None);
    }

    #[test]
    fn float_buffers_ignore_scaling() {
        let buf = SampleBuffer::new(Samples::F32(vec![1.0])).with_scaling(Scaling::new(10.0, 5.0));
        assert!(buf.scaling().is_identity());
        assert_eq!(buf.value(0), Some(1.0));
    }

    #[test]
    fn complex_samples_read_as_magnitude() {
        let buf = SampleBuffer::new(Samples::C64(vec![C64::new(3.0, 4.0)]));
        assert_eq!(buf.value(0), Some(5.0));
        assert_eq!(buf.complex(0), Some(C64::new(3.0, 4.0)));
    }

    #[test]
    fn narrowing_to_an_integer_dtype_saturates() {
        let buf = SampleBuffer::from_f64(DType::I16, &[0.4, -0.6, 1e9, -1e9, f64::NAN]);
        assert_eq!(
            buf.samples(),
            &Samples::I16(vec![0, -1, i16::MAX, i16::MIN, 0])
        );
    }

    #[test]
    fn nan_survives_a_float_round_trip() {
        let buf = SampleBuffer::from_f64(DType::F64, &[f64::NAN, 1.5]);
        assert!(buf.value(0).unwrap().is_nan());
        assert_eq!(buf.value(1), Some(1.5));
    }
}
