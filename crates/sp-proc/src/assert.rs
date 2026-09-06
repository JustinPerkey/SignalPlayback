//! Assertions: the thing that turns a run into a test (`docs/DESIGN.md` §9.7).
//!
//! A pipeline may carry assertions, evaluated per group after the last stage.
//! They are written as one line each, in terms of what the run recorded:
//!
//! ```text
//! detections.count == 4
//! metrics.snr_db > 12.0
//! signals["envelope"].rms within 5% of baseline
//! stage[3].wall_ms < 250
//! ```
//!
//! ## What can be named
//!
//! | Subject | Reads |
//! |---------|-------|
//! | `metrics.<name>` | a metric any stage recorded; the last stage to record it wins |
//! | `stage[n].metrics.<name>` | that metric from stage `n` specifically |
//! | `stage[n].wall_ms` | how long stage `n` took on this group |
//! | `stage[n].status` | `ok`, `failed`, `skipped` or `cached` |
//! | `signals["name"].<field>` | `min`, `max`, `mean`, `rms`, `peak_to_peak` or `samples` of a signal as it left the pipeline |
//! | `<port>.count` | how many rows the artifact published on `<port>` has |
//! | `<port>.<field>.<agg>` | `min`, `max`, `mean` or `sum` over one numeric field of that artifact |
//!
//! A metric name may contain dots (`rf.rms` is what a per-signal metric is
//! called), so `metrics.` takes everything up to the comparison operator.
//!
//! ## Tests
//!
//! `== != < <= > >=` compare against a number, a bare token (for `status`), or
//! `baseline`. `within X of <ref>` and `within X% of <ref>` bound the
//! difference from a number or from the baseline's value for the same subject.
//!
//! ## Why an unresolvable subject is not a pass
//!
//! An assertion about an artifact a group never produced is
//! [`AssertStatus::NotApplicable`], not a pass: a suite whose subjects have
//! quietly stopped existing would otherwise go green while testing nothing.
//! An assertion that *cannot* be evaluated — a comparison against a baseline
//! that has no such value, a text value under `<` — is
//! [`AssertStatus::Error`], which fails the run for the same reason.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;
use sp_core::artifact::{self, ArtifactData};
use sp_core::run::{AssertStatus, StageStatus};
use sp_core::{GroupId, RunId, SignalStats};
use sp_store::runs::{self, SOURCE_STAGE};
use sp_store::Store;

use crate::error::Result;

/// An assertion that would not parse, with the position it gave up at so the
/// editor can point at it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} (at character {position})")]
pub struct AssertError {
    pub message: String,
    pub position: usize,
}

impl AssertError {
    fn at(position: usize, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            position,
        }
    }
}

/// One parsed assertion, keeping the text it was written as: that is what the
/// editor shows, what the run records, and what a failure quotes.
#[derive(Debug, Clone, PartialEq)]
pub struct Assertion {
    source: String,
    subject: Subject,
    test: Test,
}

/// What an assertion is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// A metric, optionally pinned to one stage.
    Metric { stage: Option<i32>, name: String },
    /// How long a stage took on this group.
    StageWall { stage: i32 },
    /// What became of a stage on this group.
    StageStatus { stage: i32 },
    /// A statistic of one signal as it left the pipeline.
    Signal { name: String, field: SignalField },
    /// Something about the artifact published on a port.
    Artifact { port: String, of: ArtifactOf },
}

impl Subject {
    /// Whether this subject reads a signal's samples, which a run under a
    /// `never` retention policy will not have kept.
    #[must_use]
    pub const fn is_signal(&self) -> bool {
        matches!(self, Self::Signal { .. })
    }
}

/// A statistic of a recorded signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalField {
    Min,
    Max,
    Mean,
    Rms,
    PeakToPeak,
    /// How many samples the signal has.
    Samples,
}

impl SignalField {
    const NAMES: [(&'static str, Self); 6] = [
        ("min", Self::Min),
        ("max", Self::Max),
        ("mean", Self::Mean),
        ("rms", Self::Rms),
        ("peak_to_peak", Self::PeakToPeak),
        ("samples", Self::Samples),
    ];

    fn parse(token: &str) -> Option<Self> {
        Self::NAMES
            .iter()
            .find(|(name, _)| *name == token)
            .map(|(_, field)| *field)
    }

    fn of(self, stats: &SignalStats, sample_count: u64) -> Option<f64> {
        match self {
            Self::Min => stats.min(),
            Self::Max => stats.max(),
            Self::Mean => stats.mean(),
            Self::Rms => stats.rms(),
            Self::PeakToPeak => Some(stats.max()? - stats.min()?),
            Self::Samples => Some(sample_count as f64),
        }
    }
}

/// What is read out of an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactOf {
    /// How many rows it has.
    Count,
    /// An aggregate over one of its numeric fields.
    Aggregate { field: String, aggregate: Aggregate },
}

/// How a column of an artifact is reduced to one number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    Min,
    Max,
    Mean,
    Sum,
}

impl Aggregate {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            "mean" => Some(Self::Mean),
            "sum" => Some(Self::Sum),
            _ => None,
        }
    }

    fn apply(self, values: &[f64]) -> Option<f64> {
        if values.is_empty() {
            return None;
        }
        Some(match self {
            Self::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
            Self::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            Self::Mean => values.iter().sum::<f64>() / values.len() as f64,
            Self::Sum => values.iter().sum(),
        })
    }
}

/// What the subject is tested against.
#[derive(Debug, Clone, PartialEq)]
pub enum Test {
    Compare {
        op: CompareOp,
        operand: Operand,
    },
    /// `within X of <ref>`, or `within X% of <ref>` when `relative`.
    Within {
        amount: f64,
        relative: bool,
        reference: Operand,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }

    fn holds(self, left: f64, right: f64) -> bool {
        match self {
            Self::Eq => left == right,
            Self::Ne => left != right,
            Self::Lt => left < right,
            Self::Le => left <= right,
            Self::Gt => left > right,
            Self::Ge => left >= right,
        }
    }
}

/// The right-hand side of a test.
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Number(f64),
    /// A bare token, which is what a `status` comparison reads.
    Text(String),
    /// The same subject, evaluated over the baseline run's matching group.
    Baseline,
}

/// A value a subject resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    Number(f64),
    Text(String),
}

impl Resolved {
    fn number(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Text(_) => None,
        }
    }
}

impl fmt::Display for Resolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(f, "{value}"),
            Self::Text(text) => f.write_str(text),
        }
    }
}

/// How one assertion turned out, in the terms the run records (§9.7).
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub status: AssertStatus,
    /// What the subject resolved to, when that is a number.
    pub actual: Option<f64>,
    /// What it was tested against, when that is a number.
    pub expected: Option<f64>,
    /// Why, in the user's terms. Always present for anything but a pass.
    pub message: Option<String>,
}

impl Outcome {
    fn pass(actual: Option<f64>, expected: Option<f64>) -> Self {
        Self {
            status: AssertStatus::Pass,
            actual,
            expected,
            message: None,
        }
    }

    fn of(status: AssertStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            actual: None,
            expected: None,
            message: Some(message.into()),
        }
    }

    fn with_values(mut self, actual: Option<f64>, expected: Option<f64>) -> Self {
        self.actual = actual;
        self.expected = expected;
        self
    }
}

impl Assertion {
    /// Parses one line of the assertion language.
    pub fn parse(source: &str) -> std::result::Result<Self, AssertError> {
        parse::assertion(source)
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub fn subject(&self) -> &Subject {
        &self.subject
    }

    #[must_use]
    pub fn test(&self) -> &Test {
        &self.test
    }

    /// Whether this assertion can say anything without a baseline to compare
    /// against.
    #[must_use]
    pub fn needs_baseline(&self) -> bool {
        matches!(
            &self.test,
            Test::Compare {
                operand: Operand::Baseline,
                ..
            } | Test::Within {
                reference: Operand::Baseline,
                ..
            }
        )
    }

    /// Evaluates the assertion over one group, optionally against the same
    /// group of a baseline run.
    #[must_use]
    pub fn evaluate(&self, facts: &GroupFacts, baseline: Option<&GroupFacts>) -> Outcome {
        let Some(actual) = facts.resolve(&self.subject) else {
            return Outcome::of(
                AssertStatus::NotApplicable,
                format!("{} is not recorded for this group", describe(&self.subject)),
            );
        };

        let reference = match self.test.operand() {
            Operand::Number(value) => Some(Resolved::Number(*value)),
            Operand::Text(text) => Some(Resolved::Text(text.clone())),
            Operand::Baseline => match baseline {
                None => {
                    return Outcome::of(
                        AssertStatus::NotApplicable,
                        "no baseline was given to compare against",
                    )
                    .with_values(actual.number(), None);
                }
                Some(baseline) => match baseline.resolve(&self.subject) {
                    Some(value) => Some(value),
                    None => {
                        return Outcome::of(
                            AssertStatus::Error,
                            format!(
                                "the baseline has no {} for this group",
                                describe(&self.subject)
                            ),
                        )
                        .with_values(actual.number(), None);
                    }
                },
            },
        };
        let reference = reference.expect("every operand resolves or returns above");
        let values = (actual.number(), reference.number());

        match &self.test {
            Test::Compare { op, .. } => self.compare(*op, &actual, &reference),
            Test::Within {
                amount, relative, ..
            } => {
                let (Some(actual), Some(reference)) = values else {
                    return Outcome::of(
                        AssertStatus::Error,
                        format!("'within' needs numbers, and got {actual} and {reference}"),
                    );
                };
                let bound = if *relative {
                    amount / 100.0 * reference.abs()
                } else {
                    *amount
                };
                let error = (actual - reference).abs();
                if error <= bound {
                    Outcome::pass(Some(actual), Some(reference))
                } else {
                    let unit = if *relative { "%" } else { "" };
                    Outcome::of(
                        AssertStatus::Fail,
                        format!(
                            "{actual} differs from {reference} by {error}, more than {amount}{unit} ({bound})"
                        ),
                    )
                    .with_values(Some(actual), Some(reference))
                }
            }
        }
    }

    fn compare(&self, op: CompareOp, actual: &Resolved, reference: &Resolved) -> Outcome {
        let numeric = (actual.number(), reference.number());
        // Text compares only for equality: `stage[2].status == ok` is
        // meaningful, `< ok` is not.
        let holds = match (numeric, op) {
            ((Some(left), Some(right)), _) => op.holds(left, right),
            ((_, _), CompareOp::Eq) => actual.to_string() == reference.to_string(),
            ((_, _), CompareOp::Ne) => actual.to_string() != reference.to_string(),
            _ => {
                return Outcome::of(
                    AssertStatus::Error,
                    format!(
                        "'{}' needs numbers on both sides, and got {actual} and {reference}",
                        op.as_str()
                    ),
                )
                .with_values(numeric.0, numeric.1);
            }
        };
        if holds {
            Outcome::pass(numeric.0, numeric.1)
        } else {
            Outcome::of(
                AssertStatus::Fail,
                format!("{actual} is not {} {reference}", op.as_str()),
            )
            .with_values(numeric.0, numeric.1)
        }
    }
}

impl Test {
    fn operand(&self) -> &Operand {
        match self {
            Self::Compare { operand, .. } => operand,
            Self::Within { reference, .. } => reference,
        }
    }
}

impl fmt::Display for Assertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// A subject in words, for a message that has to explain itself.
fn describe(subject: &Subject) -> String {
    match subject {
        Subject::Metric { stage: None, name } => format!("metric '{name}'"),
        Subject::Metric {
            stage: Some(stage),
            name,
        } => format!("metric '{name}' of stage {stage}"),
        Subject::StageWall { stage } => format!("the wall time of stage {stage}"),
        Subject::StageStatus { stage } => format!("the status of stage {stage}"),
        Subject::Signal { name, field } => {
            format!("the {} of signal '{name}'", field_name(*field))
        }
        Subject::Artifact {
            port,
            of: ArtifactOf::Count,
        } => format!("the row count of '{port}'"),
        Subject::Artifact {
            port,
            of: ArtifactOf::Aggregate { field, aggregate },
        } => format!("the {aggregate:?} of '{port}.{field}'").to_lowercase(),
    }
}

fn field_name(field: SignalField) -> &'static str {
    SignalField::NAMES
        .iter()
        .find(|(_, value)| *value == field)
        .map_or("value", |(name, _)| *name)
}

// ---------------------------------------------------------------------------
// The facts one group offers
// ---------------------------------------------------------------------------

/// One signal as it left the pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct SignalFacts {
    pub name: String,
    pub stats: SignalStats,
    pub sample_count: u64,
}

/// Everything about one group of one run that an assertion can name.
///
/// Built from what the run *recorded*, not from the live frame, so the same
/// assertions can be re-evaluated over an old run — which is what makes a
/// baseline comparison possible at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupFacts {
    /// Metrics by `(stage ordinal, name)`.
    pub metrics: BTreeMap<(i32, String), f64>,
    pub wall_ms: BTreeMap<i32, u64>,
    pub statuses: BTreeMap<i32, StageStatus>,
    /// The signals at the output of the last stage that produced any.
    pub signals: Vec<SignalFacts>,
    /// Artifact payloads by port. A port published by more than one stage
    /// resolves to the last one, which is the value that survived the
    /// pipeline.
    pub artifacts: BTreeMap<String, Value>,
}

impl GroupFacts {
    /// Reads back everything one group of `run` recorded.
    pub fn from_run(store: &Store, run: RunId, group: GroupId) -> Result<Self> {
        let (stages, artifacts) = store.read(|conn| {
            let stages = runs::group_stages(conn, run, group)?;
            let rows = runs::run_artifacts(conn, run, Some(group))?;
            let mut artifacts = Vec::with_capacity(rows.len());
            for row in rows {
                let payload = runs::artifact_payload(conn, &row)?;
                artifacts.push((row.stage_ordinal, row.port, payload));
            }
            Ok((stages, artifacts))
        })?;

        let mut facts = Self::default();
        for stage in &stages {
            facts.statuses.insert(stage.stage_ordinal, stage.status);
            if let Some(wall) = stage.wall_ms {
                facts.wall_ms.insert(stage.stage_ordinal, wall);
            }
            for (name, value) in &stage.metrics {
                facts
                    .metrics
                    .insert((stage.stage_ordinal, name.clone()), *value);
            }
        }

        // Artifacts arrive in stage order, so a later stage's port wins.
        for (_, port, payload) in artifacts {
            match serde_json::from_str::<Value>(&payload) {
                Ok(value) => {
                    facts.artifacts.insert(port, value);
                }
                Err(error) => {
                    tracing::warn!(%error, port, "an artifact payload would not parse");
                }
            }
        }

        // The final frame is whatever the last stage that produced output
        // left behind; a run with no stages at all leaves the source signals.
        let last = stages
            .iter()
            .filter(|stage| stage.status.produced_output())
            .map(|stage| stage.stage_ordinal)
            .max()
            .unwrap_or(SOURCE_STAGE);
        let signals = store.read(|conn| runs::stage_signals(conn, run, group, last))?;
        facts.signals = signals
            .into_iter()
            .filter(|row| row.disposition.flows_onward())
            .map(|row| SignalFacts {
                name: row.name,
                stats: row.stats,
                sample_count: row.sample_count,
            })
            .collect();
        Ok(facts)
    }

    /// The value a subject names, or `None` when this group has no such thing.
    #[must_use]
    pub fn resolve(&self, subject: &Subject) -> Option<Resolved> {
        match subject {
            Subject::Metric { stage: None, name } => self
                .metrics
                .iter()
                .rfind(|((_, metric), _)| metric == name)
                .map(|(_, value)| Resolved::Number(*value)),
            Subject::Metric {
                stage: Some(stage),
                name,
            } => self
                .metrics
                .get(&(*stage, name.clone()))
                .map(|value| Resolved::Number(*value)),
            Subject::StageWall { stage } => self
                .wall_ms
                .get(stage)
                .map(|ms| Resolved::Number(*ms as f64)),
            Subject::StageStatus { stage } => self
                .statuses
                .get(stage)
                .map(|status| Resolved::Text(status.as_str().to_owned())),
            Subject::Signal { name, field } => {
                let signal = self.signals.iter().find(|signal| &signal.name == name)?;
                field
                    .of(&signal.stats, signal.sample_count)
                    .map(Resolved::Number)
            }
            Subject::Artifact { port, of } => {
                let payload = self.artifacts.get(port)?;
                let data =
                    ArtifactData::from_value(artifact::infer_schema(payload), payload).ok()?;
                match of {
                    ArtifactOf::Count => Some(Resolved::Number(data.rows() as f64)),
                    ArtifactOf::Aggregate { field, aggregate } => {
                        let column = data.column(field)?;
                        let values: Vec<f64> = (0..column.len())
                            .filter_map(|row| column.number_at(row))
                            .collect();
                        aggregate.apply(&values).map(Resolved::Number)
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

mod parse {
    use super::{
        Aggregate, ArtifactOf, AssertError, Assertion, CompareOp, Operand, SignalField, Subject,
        Test,
    };

    /// One line of the language, tokenised on the fly: the grammar is small
    /// enough that a cursor over the characters is clearer than a token
    /// stream, and it keeps every error pinned to a real character offset.
    pub(super) fn assertion(source: &str) -> Result<Assertion, AssertError> {
        let mut cursor = Cursor::new(source);
        let subject = subject(&mut cursor)?;
        let test = test(&mut cursor)?;
        cursor.skip_space();
        if !cursor.at_end() {
            return Err(AssertError::at(
                cursor.position,
                format!("unexpected '{}' after the assertion", cursor.rest().trim()),
            ));
        }
        Ok(Assertion {
            source: source.trim().to_owned(),
            subject,
            test,
        })
    }

    fn subject(cursor: &mut Cursor<'_>) -> Result<Subject, AssertError> {
        let head = cursor.identifier("a subject")?;
        match head.as_str() {
            "metrics" => {
                cursor.expect('.')?;
                Ok(Subject::Metric {
                    stage: None,
                    name: cursor.dotted_name()?,
                })
            }
            "stage" => {
                let ordinal = cursor.index()?;
                cursor.expect('.')?;
                let what = cursor.identifier("'wall_ms', 'status' or 'metrics'")?;
                match what.as_str() {
                    "wall_ms" => Ok(Subject::StageWall { stage: ordinal }),
                    "status" => Ok(Subject::StageStatus { stage: ordinal }),
                    "metrics" => {
                        cursor.expect('.')?;
                        Ok(Subject::Metric {
                            stage: Some(ordinal),
                            name: cursor.dotted_name()?,
                        })
                    }
                    other => Err(AssertError::at(
                        cursor.position,
                        format!("a stage has no '{other}'; try wall_ms, status or metrics.<name>"),
                    )),
                }
            }
            "signals" => {
                let name = cursor.subscript_name()?;
                cursor.expect('.')?;
                let field = cursor.identifier("a signal field")?;
                let field = SignalField::parse(&field).ok_or_else(|| {
                    AssertError::at(
                        cursor.position,
                        format!(
                            "a signal has no '{field}'; try min, max, mean, rms, \
                             peak_to_peak or samples"
                        ),
                    )
                })?;
                Ok(Subject::Signal { name, field })
            }
            // Anything else is the port an artifact was published on.
            port => {
                cursor.expect('.')?;
                let what = cursor.identifier("'count' or a field name")?;
                if what == "count" {
                    return Ok(Subject::Artifact {
                        port: port.to_owned(),
                        of: ArtifactOf::Count,
                    });
                }
                cursor.expect('.')?;
                let aggregate = cursor.identifier("an aggregate")?;
                let aggregate = Aggregate::parse(&aggregate).ok_or_else(|| {
                    AssertError::at(
                        cursor.position,
                        format!("'{aggregate}' is not an aggregate; try min, max, mean or sum"),
                    )
                })?;
                Ok(Subject::Artifact {
                    port: port.to_owned(),
                    of: ArtifactOf::Aggregate {
                        field: what,
                        aggregate,
                    },
                })
            }
        }
    }

    fn test(cursor: &mut Cursor<'_>) -> Result<Test, AssertError> {
        cursor.skip_space();
        if cursor.take_word("within") {
            let amount = cursor.number()?;
            let relative = cursor.take_char('%');
            cursor.skip_space();
            if !cursor.take_word("of") {
                return Err(AssertError::at(
                    cursor.position,
                    "'within' reads as 'within X of <value>'",
                ));
            }
            return Ok(Test::Within {
                amount,
                relative,
                reference: operand(cursor)?,
            });
        }

        let op = cursor.compare_op()?;
        Ok(Test::Compare {
            op,
            operand: operand(cursor)?,
        })
    }

    fn operand(cursor: &mut Cursor<'_>) -> Result<Operand, AssertError> {
        cursor.skip_space();
        match cursor.peek() {
            Some(c) if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => {
                Ok(Operand::Number(cursor.number()?))
            }
            Some('"') | Some('\'') => Ok(Operand::Text(cursor.quoted()?)),
            Some(c) if c.is_alphabetic() || c == '_' => {
                let word = cursor.identifier("a value")?;
                if word == "baseline" {
                    Ok(Operand::Baseline)
                } else {
                    Ok(Operand::Text(word))
                }
            }
            _ => Err(AssertError::at(
                cursor.position,
                "expected a number, a name or 'baseline'",
            )),
        }
    }

    struct Cursor<'a> {
        source: &'a str,
        position: usize,
    }

    impl<'a> Cursor<'a> {
        fn new(source: &'a str) -> Self {
            Self {
                source,
                position: 0,
            }
        }

        fn rest(&self) -> &'a str {
            &self.source[self.position..]
        }

        fn at_end(&self) -> bool {
            self.rest().is_empty()
        }

        fn peek(&self) -> Option<char> {
            self.rest().chars().next()
        }

        fn bump(&mut self, c: char) {
            self.position += c.len_utf8();
        }

        fn skip_space(&mut self) {
            while let Some(c) = self.peek() {
                if c.is_whitespace() {
                    self.bump(c);
                } else {
                    break;
                }
            }
        }

        fn take_char(&mut self, expected: char) -> bool {
            if self.peek() == Some(expected) {
                self.bump(expected);
                true
            } else {
                false
            }
        }

        fn expect(&mut self, expected: char) -> Result<(), AssertError> {
            self.skip_space();
            if self.take_char(expected) {
                Ok(())
            } else {
                Err(AssertError::at(
                    self.position,
                    format!("expected '{expected}'"),
                ))
            }
        }

        /// Consumes `word` when it is next and is not the prefix of a longer
        /// identifier, so `ofsted` is not read as `of`.
        fn take_word(&mut self, word: &str) -> bool {
            let rest = self.rest();
            if !rest.starts_with(word) {
                return false;
            }
            let next = rest[word.len()..].chars().next();
            if next.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                return false;
            }
            self.position += word.len();
            true
        }

        fn identifier(&mut self, expected: &str) -> Result<String, AssertError> {
            self.skip_space();
            let start = self.position;
            while let Some(c) = self.peek() {
                if c.is_alphanumeric() || c == '_' {
                    self.bump(c);
                } else {
                    break;
                }
            }
            if self.position == start {
                return Err(AssertError::at(start, format!("expected {expected}")));
            }
            Ok(self.source[start..self.position].to_owned())
        }

        /// A metric name, which may carry dots: `rf.rms` is what a per-signal
        /// metric is called. Runs to the operator.
        fn dotted_name(&mut self) -> Result<String, AssertError> {
            self.skip_space();
            let start = self.position;
            while let Some(c) = self.peek() {
                if c.is_alphanumeric() || c == '_' || c == '.' {
                    self.bump(c);
                } else {
                    break;
                }
            }
            if self.position == start {
                return Err(AssertError::at(start, "expected a metric name"));
            }
            Ok(self.source[start..self.position].to_owned())
        }

        /// `[3]`, the stage an assertion is about.
        fn index(&mut self) -> Result<i32, AssertError> {
            self.expect('[')?;
            self.skip_space();
            let start = self.position;
            if self.peek() == Some('-') {
                self.bump('-');
            }
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.bump(c);
                } else {
                    break;
                }
            }
            let text = &self.source[start..self.position];
            let ordinal = text
                .parse::<i32>()
                .map_err(|_| AssertError::at(start, "expected a stage ordinal"))?;
            self.expect(']')?;
            Ok(ordinal)
        }

        /// `["envelope"]` or `[envelope]` — a signal named by subscript.
        fn subscript_name(&mut self) -> Result<String, AssertError> {
            self.expect('[')?;
            self.skip_space();
            let name = match self.peek() {
                Some('"') | Some('\'') => self.quoted()?,
                _ => self.identifier("a signal name")?,
            };
            self.expect(']')?;
            Ok(name)
        }

        fn quoted(&mut self) -> Result<String, AssertError> {
            self.skip_space();
            let quote = self
                .peek()
                .filter(|c| *c == '"' || *c == '\'')
                .ok_or_else(|| AssertError::at(self.position, "expected a quoted name"))?;
            self.bump(quote);
            let start = self.position;
            while let Some(c) = self.peek() {
                if c == quote {
                    let text = self.source[start..self.position].to_owned();
                    self.bump(quote);
                    return Ok(text);
                }
                self.bump(c);
            }
            Err(AssertError::at(start, "the quoted name is not closed"))
        }

        fn number(&mut self) -> Result<f64, AssertError> {
            self.skip_space();
            let start = self.position;
            if matches!(self.peek(), Some('-') | Some('+')) {
                let sign = self.peek().expect("just matched");
                self.bump(sign);
            }
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || c == '.' {
                    self.bump(c);
                } else if c == 'e' || c == 'E' {
                    self.bump(c);
                    if matches!(self.peek(), Some('-') | Some('+')) {
                        let sign = self.peek().expect("just matched");
                        self.bump(sign);
                    }
                } else {
                    break;
                }
            }
            self.source[start..self.position]
                .parse::<f64>()
                .map_err(|_| AssertError::at(start, "expected a number"))
        }

        fn compare_op(&mut self) -> Result<CompareOp, AssertError> {
            self.skip_space();
            for (token, op) in [
                ("==", CompareOp::Eq),
                ("!=", CompareOp::Ne),
                ("<=", CompareOp::Le),
                (">=", CompareOp::Ge),
                ("<", CompareOp::Lt),
                (">", CompareOp::Gt),
                // A single '=' is what everyone types first; reading it as
                // equality costs nothing and is never ambiguous, since the
                // language has no assignment.
                ("=", CompareOp::Eq),
            ] {
                if self.rest().starts_with(token) {
                    self.position += token.len();
                    return Ok(op);
                }
            }
            Err(AssertError::at(
                self.position,
                "expected a comparison (==, !=, <, <=, >, >=) or 'within'",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary as the database hands one back.
    fn stats(min: f64, max: f64, mean: f64, rms: f64, count: u64) -> SignalStats {
        SignalStats::from_stored(min, max, mean, rms, count, 0)
    }

    fn facts() -> GroupFacts {
        let mut facts = GroupFacts::default();
        facts.metrics.insert((1, "snr_db".to_owned()), 14.5);
        facts.metrics.insert((2, "snr_db".to_owned()), 9.0);
        facts.metrics.insert((1, "rf.rms".to_owned()), 3.0);
        facts.wall_ms.insert(3, 120);
        facts.statuses.insert(1, StageStatus::Ok);
        facts.statuses.insert(2, StageStatus::Ok);
        facts.statuses.insert(3, StageStatus::Cached);
        facts.signals.push(SignalFacts {
            name: "envelope".to_owned(),
            stats: stats(-2.0, 4.0, 1.0, 2.0, 16),
            sample_count: 16,
        });
        facts.artifacts.insert(
            "detections".to_owned(),
            serde_json::json!({
                "spans": [[0.0, 1.0], [2.0, 3.0], [4.0, 5.0], [6.0, 7.0]],
                "peak": [0.9, 0.4, 0.5, 0.7],
                "signal": ["rf", "rf", "rf", "rf"]
            }),
        );
        facts
    }

    fn evaluate(source: &str) -> Outcome {
        Assertion::parse(source).unwrap().evaluate(&facts(), None)
    }

    #[test]
    fn the_four_shapes_from_the_design_document_parse() {
        // §9.7 lists these; they are the contract this module owes.
        let count = Assertion::parse("detections.count == 4").unwrap();
        assert_eq!(
            count.subject(),
            &Subject::Artifact {
                port: "detections".to_owned(),
                of: ArtifactOf::Count
            }
        );

        let metric = Assertion::parse("metrics.snr_db > 12.0").unwrap();
        assert_eq!(
            metric.subject(),
            &Subject::Metric {
                stage: None,
                name: "snr_db".to_owned()
            }
        );

        let within = Assertion::parse(r#"signals["envelope"].rms within 5% of baseline"#).unwrap();
        assert_eq!(
            within.subject(),
            &Subject::Signal {
                name: "envelope".to_owned(),
                field: SignalField::Rms
            }
        );
        assert!(within.needs_baseline());

        let wall = Assertion::parse("stage[3].wall_ms < 250").unwrap();
        assert_eq!(wall.subject(), &Subject::StageWall { stage: 3 });
        assert!(!wall.needs_baseline());
    }

    #[test]
    fn an_assertion_keeps_the_text_it_was_written_as() {
        let assertion = Assertion::parse("  metrics.snr_db  >  12  ").unwrap();
        assert_eq!(assertion.source(), "metrics.snr_db  >  12");
        assert_eq!(assertion.to_string(), "metrics.snr_db  >  12");
    }

    #[test]
    fn a_metric_name_may_carry_dots_and_a_stage_may_be_pinned() {
        assert_eq!(evaluate("metrics.rf.rms == 3.0").status, AssertStatus::Pass);
        // Unpinned, the last stage to record the metric wins.
        assert_eq!(evaluate("metrics.snr_db == 9.0").status, AssertStatus::Pass);
        assert_eq!(
            evaluate("stage[1].metrics.snr_db == 14.5").status,
            AssertStatus::Pass
        );
        assert_eq!(
            evaluate("stage[2].metrics.snr_db > 12").status,
            AssertStatus::Fail
        );
    }

    #[test]
    fn a_failure_says_what_it_saw() {
        let outcome = evaluate("stage[2].metrics.snr_db > 12");
        assert_eq!(outcome.actual, Some(9.0));
        assert_eq!(outcome.expected, Some(12.0));
        assert_eq!(outcome.message.unwrap(), "9 is not > 12");
    }

    #[test]
    fn signal_statistics_and_artifact_aggregates_resolve() {
        assert_eq!(
            evaluate(r#"signals["envelope"].rms == 2"#).status,
            AssertStatus::Pass
        );
        assert_eq!(
            evaluate("signals[envelope].peak_to_peak == 6").status,
            AssertStatus::Pass
        );
        assert_eq!(
            evaluate("signals[envelope].samples == 16").status,
            AssertStatus::Pass
        );
        assert_eq!(evaluate("detections.count == 4").status, AssertStatus::Pass);
        assert_eq!(
            evaluate("detections.peak.max >= 0.9").status,
            AssertStatus::Pass
        );
        assert_eq!(
            evaluate("detections.peak.mean < 0.7").status,
            AssertStatus::Pass
        );
    }

    #[test]
    fn a_stage_status_compares_as_a_token() {
        assert_eq!(
            evaluate("stage[3].status == cached").status,
            AssertStatus::Pass
        );
        assert_eq!(
            evaluate("stage[1].status != failed").status,
            AssertStatus::Pass
        );
        // Ordering a status is meaningless, and saying so beats guessing.
        assert_eq!(evaluate("stage[1].status < ok").status, AssertStatus::Error);
    }

    #[test]
    fn a_subject_the_group_does_not_have_is_not_applicable() {
        // Not a pass: a suite whose subjects have quietly stopped existing
        // would otherwise go green while testing nothing.
        let outcome = evaluate("metrics.missing > 0");
        assert_eq!(outcome.status, AssertStatus::NotApplicable);
        assert!(outcome.message.unwrap().contains("metric 'missing'"));
        assert_eq!(
            evaluate("spectrum.count == 1").status,
            AssertStatus::NotApplicable
        );
        assert_eq!(
            evaluate(r#"signals["absent"].rms > 0"#).status,
            AssertStatus::NotApplicable
        );
        assert!(!AssertStatus::NotApplicable.is_failure());
    }

    #[test]
    fn a_baseline_comparison_needs_a_baseline() {
        let assertion =
            Assertion::parse(r#"signals["envelope"].rms within 5% of baseline"#).unwrap();
        let outcome = assertion.evaluate(&facts(), None);
        assert_eq!(outcome.status, AssertStatus::NotApplicable);
        assert_eq!(outcome.actual, Some(2.0));

        // Within tolerance of the baseline, then outside it.
        let mut baseline = facts();
        baseline.signals[0].stats = stats(-2.0, 4.0, 1.0, 2.05, 16);
        assert_eq!(
            assertion.evaluate(&facts(), Some(&baseline)).status,
            AssertStatus::Pass
        );
        baseline.signals[0].stats = stats(-2.0, 4.0, 1.0, 3.0, 16);
        let failed = assertion.evaluate(&facts(), Some(&baseline));
        assert_eq!(failed.status, AssertStatus::Fail);
        assert_eq!(failed.expected, Some(3.0));

        // A baseline that does not have the subject at all cannot be
        // compared against, and saying "pass" would be a lie.
        baseline.signals.clear();
        assert_eq!(
            assertion.evaluate(&facts(), Some(&baseline)).status,
            AssertStatus::Error
        );
    }

    #[test]
    fn within_reads_both_absolute_and_relative_bounds() {
        let absolute = Assertion::parse("metrics.snr_db within 0.5 of 9.4").unwrap();
        assert_eq!(absolute.evaluate(&facts(), None).status, AssertStatus::Pass);
        let tight = Assertion::parse("metrics.snr_db within 0.1 of 9.4").unwrap();
        assert_eq!(tight.evaluate(&facts(), None).status, AssertStatus::Fail);

        let relative = Assertion::parse("metrics.snr_db within 10% of 9.5").unwrap();
        assert_eq!(relative.evaluate(&facts(), None).status, AssertStatus::Pass);
    }

    #[test]
    fn a_malformed_assertion_says_where_it_gave_up() {
        let error = Assertion::parse("metrics.snr_db >>> 4").unwrap_err();
        assert!(error.message.contains("expected a number"), "{error}");

        let error = Assertion::parse("stage[x].wall_ms < 1").unwrap_err();
        assert!(error.message.contains("stage ordinal"), "{error}");

        let error = Assertion::parse("signals[rf].loudness > 1").unwrap_err();
        assert!(error.message.contains("peak_to_peak"), "{error}");

        let error = Assertion::parse("metrics.snr_db > 12 and more").unwrap_err();
        assert!(error.message.contains("unexpected"), "{error}");

        let error = Assertion::parse("").unwrap_err();
        assert_eq!(error.position, 0);
    }

    #[test]
    fn a_single_equals_reads_as_equality() {
        // Nothing in the language assigns, so the sign everyone types first
        // is unambiguous.
        assert_eq!(evaluate("detections.count = 4").status, AssertStatus::Pass);
    }
}
