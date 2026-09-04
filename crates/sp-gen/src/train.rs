//! Pulse-train synthesis (`docs/DESIGN.md` §8.5, §6.6).
//!
//! An import carries **pulse records** — a time of arrival plus a fixed set of
//! numeric fields — and a train is several groups of them. Waveform synthesis
//! (§8.1) produces sampled signals, which is a different shape entirely, so a
//! generated waveform can never stand in for a captured train. This module is
//! the other half: it emits exactly what an import does, so the two are
//! interchangeable as pipeline input.
//!
//! Everything is a function of the **pulse index within the train**, for the
//! same reason the sample renderer is a function of the sample index: the same
//! spec and seed reproduce the same numbers, and a group can be built without
//! building the ones before it (goal G3).

use serde::{Deserialize, Serialize};
use sp_core::TimeUnit;

use crate::noise::{self, Stream};

/// How the time of arrival advances from pulse to pulse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum Pri {
    /// A constant pulse repetition interval.
    Fixed { pri_s: f64 },
    /// A repeating stagger sequence: the intervals cycle through `positions`.
    Stagger { positions: Vec<f64> },
    /// A constant PRI with the arrival time dithered by up to
    /// `fraction` of it. The jitter is applied to each arrival rather than
    /// accumulated, which is what a measurement jitter looks like.
    Jitter { pri_s: f64, fraction: f64 },
    /// A PRI that changes by `per_pulse_s` every pulse.
    Drift { pri_s: f64, per_pulse_s: f64 },
}

impl Default for Pri {
    fn default() -> Self {
        Self::Fixed { pri_s: 1e-3 }
    }
}

impl Pri {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Fixed { .. } => "Fixed",
            Self::Stagger { .. } => "Stagger",
            Self::Jitter { .. } => "Jitter",
            Self::Drift { .. } => "Drift",
        }
    }

    /// The mean interval, for the summary line and for sizing a capture.
    #[must_use]
    pub fn mean_s(&self) -> f64 {
        match self {
            Self::Fixed { pri_s } | Self::Jitter { pri_s, .. } | Self::Drift { pri_s, .. } => {
                *pri_s
            }
            Self::Stagger { positions } => {
                if positions.is_empty() {
                    0.0
                } else {
                    positions.iter().sum::<f64>() / positions.len() as f64
                }
            }
        }
    }

    /// Time from the train's start to pulse `index`, before jitter.
    fn elapsed_s(&self, index: u64) -> f64 {
        let n = index as f64;
        match self {
            Self::Fixed { pri_s } | Self::Jitter { pri_s, .. } => n * pri_s,
            // The intervals sum in closed form: whole cycles plus a partial.
            Self::Stagger { positions } => {
                if positions.is_empty() {
                    return 0.0;
                }
                let cycle: f64 = positions.iter().sum();
                let len = positions.len() as u64;
                let whole = index / len;
                let partial: f64 = positions[..(index % len) as usize].iter().sum();
                whole as f64 * cycle + partial
            }
            // Sum of an arithmetic series: i·pri + per_pulse·i(i−1)/2.
            Self::Drift { pri_s, per_pulse_s } => n * pri_s + per_pulse_s * n * (n - 1.0) / 2.0,
        }
    }
}

/// How one pulse field varies across the train.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "shape")]
pub enum FieldValue {
    Constant {
        value: f64,
    },
    /// Uniform in `[lo, hi)`, drawn per pulse.
    Uniform {
        lo: f64,
        hi: f64,
    },
    Gaussian {
        mean: f64,
        sigma: f64,
    },
    /// A straight line across the whole train.
    Ramp {
        start: f64,
        end: f64,
    },
    /// A repeating list, one value per pulse.
    Sequence {
        values: Vec<f64>,
    },
    /// A sinusoid across the pulse index — an antenna scanning past, say.
    Scan {
        mean: f64,
        amp: f64,
        period_pulses: f64,
    },
}

impl Default for FieldValue {
    fn default() -> Self {
        Self::Constant { value: 0.0 }
    }
}

impl FieldValue {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Constant { .. } => "Constant",
            Self::Uniform { .. } => "Uniform",
            Self::Gaussian { .. } => "Gaussian",
            Self::Ramp { .. } => "Ramp",
            Self::Sequence { .. } => "Sequence",
            Self::Scan { .. } => "Scan",
        }
    }
}

/// One column of the generated train.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldSpec {
    /// Header text as it would appear in a file, e.g. `pulse width`.
    pub name: String,
    pub unit: Option<String>,
    pub value: FieldValue,
}

impl FieldSpec {
    #[must_use]
    pub fn new(name: impl Into<String>, value: FieldValue) -> Self {
        Self {
            name: name.into(),
            unit: None,
            value,
        }
    }

    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }
}

/// A pulse train to generate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainSpec {
    /// Unit the train's times of arrival are expressed in, as an import would
    /// record it. Storage is always seconds.
    pub toa_unit: TimeUnit,
    /// Where the train starts on the absolute timeline.
    pub t0_s: f64,
    /// Groups the train is divided into — dwells, scans, blocks of a file.
    pub groups: u32,
    pub pulses_per_group: u32,
    /// Makes the jittered and random fields reproducible (G3).
    pub seed: u64,
    pub pri: Pri,
    pub fields: Vec<FieldSpec>,
}

impl Default for TrainSpec {
    /// The shape of `sample/sample.csv`, scaled up to something worth looking
    /// at: a 1 kHz PRF train of pulse width, power and angle.
    fn default() -> Self {
        Self {
            toa_unit: TimeUnit::Microseconds,
            t0_s: 0.0,
            groups: 4,
            pulses_per_group: 256,
            seed: 0,
            pri: Pri::Fixed { pri_s: 1e-3 },
            fields: vec![
                FieldSpec::new(
                    "pulse width",
                    FieldValue::Gaussian {
                        mean: 1.0,
                        sigma: 0.02,
                    },
                )
                .with_unit("us"),
                FieldSpec::new(
                    "power",
                    FieldValue::Scan {
                        mean: -40.0,
                        amp: 8.0,
                        period_pulses: 256.0,
                    },
                )
                .with_unit("dBm"),
                FieldSpec::new(
                    "angle",
                    FieldValue::Ramp {
                        start: 0.0,
                        end: 360.0,
                    },
                )
                .with_unit("deg"),
            ],
        }
    }
}

impl TrainSpec {
    /// Total pulses across the train.
    #[must_use]
    pub fn pulses(&self) -> u64 {
        u64::from(self.groups) * u64::from(self.pulses_per_group)
    }

    /// How long the train lasts, from its first arrival to its last.
    #[must_use]
    pub fn duration_s(&self) -> f64 {
        match self.pulses() {
            0 => 0.0,
            n => self.pri.elapsed_s(n - 1),
        }
    }

    /// The global pulse index range one group covers.
    #[must_use]
    pub fn group_range(&self, group: u32) -> std::ops::Range<u64> {
        let per = u64::from(self.pulses_per_group);
        let start = u64::from(group) * per;
        start..start + per
    }

    /// Times of arrival, in seconds, for one group.
    #[must_use]
    pub fn toa_seconds(&self, group: u32) -> Vec<f64> {
        let range = self.group_range(group);
        let mut jitter = match &self.pri {
            Pri::Jitter { .. } => Some(Stream::new(noise::stream_key(self.seed, "/pri"))),
            _ => None,
        };
        if let Some(stream) = &mut jitter {
            stream.seek(range.start);
        }
        range
            .map(|index| {
                let base = self.t0_s + self.pri.elapsed_s(index);
                match (&self.pri, &mut jitter) {
                    (Pri::Jitter { pri_s, fraction }, Some(stream)) => {
                        let (bits, _) = stream.pair_at(index);
                        base + (noise::unit_signed(bits) * 0.5) * fraction * pri_s
                    }
                    _ => base,
                }
            })
            .collect()
    }

    /// One field's values for one group.
    #[must_use]
    pub fn field_values(&self, field: usize, group: u32) -> Vec<f64> {
        let Some(spec) = self.fields.get(field) else {
            return Vec::new();
        };
        let range = self.group_range(group);
        let total = self.pulses().max(1);

        // A random field gets its own stream, keyed by the field's position,
        // so adding a field cannot move an existing one's numbers.
        let mut stream = match spec.value {
            FieldValue::Uniform { .. } | FieldValue::Gaussian { .. } => Some(Stream::new(
                noise::stream_key(self.seed, &format!("/fields/{field}")),
            )),
            _ => None,
        };
        if let Some(stream) = &mut stream {
            stream.seek(range.start);
        }

        range
            .map(|index| match (&spec.value, &mut stream) {
                (FieldValue::Constant { value }, _) => *value,
                (FieldValue::Uniform { lo, hi }, Some(stream)) => {
                    let (bits, _) = stream.pair_at(index);
                    lo + (hi - lo) * noise::unit(bits)
                }
                (FieldValue::Gaussian { mean, sigma }, Some(stream)) => {
                    let (a, b) = stream.pair_at(index);
                    mean + sigma * noise::gaussian(a, b)
                }
                (FieldValue::Ramp { start, end }, _) => {
                    if total <= 1 {
                        *start
                    } else {
                        start + (end - start) * (index as f64 / (total - 1) as f64)
                    }
                }
                (FieldValue::Sequence { values }, _) => {
                    if values.is_empty() {
                        f64::NAN
                    } else {
                        values[(index % values.len() as u64) as usize]
                    }
                }
                (
                    FieldValue::Scan {
                        mean,
                        amp,
                        period_pulses,
                    },
                    _,
                ) => {
                    if *period_pulses <= 0.0 {
                        *mean
                    } else {
                        mean + amp * (std::f64::consts::TAU * index as f64 / period_pulses).sin()
                    }
                }
                // Unreachable: a random shape always has its stream.
                (_, None) => f64::NAN,
            })
            .collect()
    }

    /// Everything one group holds: its arrivals and one column per field.
    #[must_use]
    pub fn group(&self, group: u32) -> GroupData {
        GroupData {
            index: group,
            toa_s: self.toa_seconds(group),
            fields: (0..self.fields.len())
                .map(|field| self.field_values(field, group))
                .collect(),
        }
    }
}

/// One rendered group of a train.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupData {
    pub index: u32,
    pub toa_s: Vec<f64>,
    /// One column per [`TrainSpec::fields`] entry, in the same order.
    pub fields: Vec<Vec<f64>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TrainSpec {
        TrainSpec {
            groups: 3,
            pulses_per_group: 8,
            ..TrainSpec::default()
        }
    }

    #[test]
    fn a_fixed_pri_arrives_on_a_regular_grid_across_group_boundaries() {
        let spec = TrainSpec {
            pri: Pri::Fixed { pri_s: 1e-3 },
            ..spec()
        };
        // The train is continuous: group 1 picks up where group 0 stopped.
        let g0 = spec.toa_seconds(0);
        let g1 = spec.toa_seconds(1);
        assert_eq!(g0.len(), 8);
        assert!((g0[0] - 0.0).abs() < 1e-15);
        assert!((g0[7] - 0.007).abs() < 1e-12);
        assert!((g1[0] - 0.008).abs() < 1e-12);
        assert!((spec.duration_s() - 0.023).abs() < 1e-12);
    }

    #[test]
    fn a_stagger_cycles_its_positions() {
        let spec = TrainSpec {
            pri: Pri::Stagger {
                positions: vec![1e-3, 2e-3, 3e-3],
            },
            ..spec()
        };
        let toa = spec.toa_seconds(0);
        let intervals: Vec<f64> = toa.windows(2).map(|w| w[1] - w[0]).collect();
        for (index, interval) in intervals.iter().enumerate() {
            let expected = [1e-3, 2e-3, 3e-3][index % 3];
            assert!((interval - expected).abs() < 1e-12, "{index}: {interval}");
        }
    }

    #[test]
    fn a_drifting_pri_lengthens_every_interval() {
        let spec = TrainSpec {
            pri: Pri::Drift {
                pri_s: 1e-3,
                per_pulse_s: 1e-5,
            },
            ..spec()
        };
        let toa = spec.toa_seconds(0);
        let intervals: Vec<f64> = toa.windows(2).map(|w| w[1] - w[0]).collect();
        for (index, interval) in intervals.iter().enumerate() {
            let expected = 1e-3 + 1e-5 * index as f64;
            assert!((interval - expected).abs() < 1e-12, "{index}: {interval}");
        }
    }

    #[test]
    fn jitter_dithers_arrivals_without_accumulating() {
        let spec = TrainSpec {
            pri: Pri::Jitter {
                pri_s: 1e-3,
                fraction: 0.1,
            },
            groups: 1,
            pulses_per_group: 2_000,
            ..spec()
        };
        let toa = spec.toa_seconds(0);
        // Every arrival stays within half the jitter window of the grid, so
        // the train does not walk away from its nominal PRF.
        for (index, t) in toa.iter().enumerate() {
            let nominal = index as f64 * 1e-3;
            assert!(
                (t - nominal).abs() <= 0.5 * 0.1 * 1e-3 + 1e-15,
                "{index}: {t} vs {nominal}"
            );
        }
    }

    #[test]
    fn a_group_renders_the_same_whether_or_not_the_ones_before_it_did() {
        // The property the whole module is shaped by: a group is addressable.
        let spec = spec();
        let jittered = TrainSpec {
            pri: Pri::Jitter {
                pri_s: 1e-3,
                fraction: 0.2,
            },
            ..spec.clone()
        };
        for spec in [spec, jittered] {
            let all: Vec<f64> = (0..spec.groups).flat_map(|g| spec.toa_seconds(g)).collect();
            let alone = spec.toa_seconds(2);
            assert_eq!(&all[16..24], &alone[..]);

            for field in 0..spec.fields.len() {
                let all: Vec<f64> = (0..spec.groups)
                    .flat_map(|g| spec.field_values(field, g))
                    .collect();
                assert_eq!(&all[16..24], &spec.field_values(field, 2)[..]);
            }
        }
    }

    #[test]
    fn the_same_seed_reproduces_a_train_and_a_different_one_moves_it() {
        let spec = TrainSpec {
            fields: vec![FieldSpec::new(
                "power",
                FieldValue::Gaussian {
                    mean: 0.0,
                    sigma: 1.0,
                },
            )],
            ..spec()
        };
        let other = TrainSpec {
            seed: 1,
            ..spec.clone()
        };
        assert_eq!(spec.field_values(0, 0), spec.field_values(0, 0));
        assert_ne!(spec.field_values(0, 0), other.field_values(0, 0));
    }

    #[test]
    fn adding_a_field_leaves_the_existing_ones_alone() {
        let noisy = || {
            FieldSpec::new(
                "power",
                FieldValue::Gaussian {
                    mean: 0.0,
                    sigma: 1.0,
                },
            )
        };
        let before = TrainSpec {
            fields: vec![noisy()],
            ..spec()
        };
        let after = TrainSpec {
            fields: vec![noisy(), noisy()],
            ..spec()
        };
        assert_eq!(before.field_values(0, 1), after.field_values(0, 1));
        // The new field draws its own numbers rather than echoing the first.
        assert_ne!(after.field_values(0, 1), after.field_values(1, 1));
    }

    #[test]
    fn field_shapes_produce_what_they_say() {
        let spec = TrainSpec {
            groups: 1,
            pulses_per_group: 4,
            fields: vec![
                FieldSpec::new("k", FieldValue::Constant { value: 7.0 }),
                FieldSpec::new(
                    "r",
                    FieldValue::Ramp {
                        start: 0.0,
                        end: 3.0,
                    },
                ),
                FieldSpec::new(
                    "s",
                    FieldValue::Sequence {
                        values: vec![1.0, 2.0],
                    },
                ),
                FieldSpec::new("u", FieldValue::Uniform { lo: -1.0, hi: 1.0 }),
            ],
            ..spec()
        };
        assert_eq!(spec.field_values(0, 0), [7.0; 4]);
        assert_eq!(spec.field_values(1, 0), [0.0, 1.0, 2.0, 3.0]);
        assert_eq!(spec.field_values(2, 0), [1.0, 2.0, 1.0, 2.0]);
        assert!(spec
            .field_values(3, 0)
            .iter()
            .all(|v| (-1.0..1.0).contains(v)));
    }

    #[test]
    fn a_group_bundles_its_arrivals_with_one_column_per_field() {
        let spec = spec();
        let group = spec.group(1);
        assert_eq!(group.index, 1);
        assert_eq!(group.toa_s.len(), 8);
        assert_eq!(group.fields.len(), spec.fields.len());
        assert!(group.fields.iter().all(|column| column.len() == 8));
    }

    #[test]
    fn a_spec_round_trips_through_json() {
        let spec = spec();
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<TrainSpec>(&json).unwrap(), spec);
    }

    #[test]
    fn an_empty_train_has_no_duration_and_no_pulses() {
        let spec = TrainSpec {
            groups: 0,
            ..spec()
        };
        assert_eq!(spec.pulses(), 0);
        assert_eq!(spec.duration_s(), 0.0);
    }
}
