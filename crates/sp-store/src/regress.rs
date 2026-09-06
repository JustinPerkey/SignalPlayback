//! Assertions and baselines: what turns a run into a test
//! (`docs/DESIGN.md` §9.7, §10.4).
//!
//! Two things are persisted here, and they are deliberately different shapes:
//!
//! - An **assertion** belongs to a pipeline as source text and is evaluated
//!   per group. Its outcome is recorded per `(run, group)` together with the
//!   text it was evaluated from, so a run says what it actually tested even
//!   after the pipeline has been edited.
//! - A **baseline** is a run promoted to golden, under a name and a set of
//!   tolerances. It is a pointer, not a copy: the run's own rows are the
//!   golden data, which is why promoting is instant and why deleting a run
//!   that a baseline names is refused.
//!
//! Like the rest of the crate, nothing here knows what an expression *means* —
//! parsing and evaluation are `sp-proc`'s.

use rusqlite::{params, Connection, Row};
use sp_core::run::AssertStatus;
use sp_core::time::{now_utc, Timestamp};
use sp_core::{BaselineId, GroupId, PipelineId, RunId, Tolerances};

use crate::error::{Result, StoreError};
use crate::library::{format_timestamp, parse_timestamp};

// ---------------------------------------------------------------------------
// Assertion definitions
// ---------------------------------------------------------------------------

/// One assertion of a pipeline, as the user wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertionRow {
    pub ordinal: u32,
    /// Source text, e.g. `metrics.snr_db > 12.0`.
    pub expression: String,
    pub enabled: bool,
}

impl AssertionRow {
    #[must_use]
    pub fn new(ordinal: u32, expression: impl Into<String>) -> Self {
        Self {
            ordinal,
            expression: expression.into(),
            enabled: true,
        }
    }

    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// Replaces a pipeline's assertions wholesale, for the same reason
/// [`crate::runs::set_pipeline_stages`] does: ordinals are positions.
pub fn set_pipeline_assertions(
    conn: &Connection,
    id: PipelineId,
    assertions: &[AssertionRow],
) -> Result<()> {
    conn.execute(
        "DELETE FROM pipeline_assertion WHERE pipeline_id = ?1",
        [id.get()],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO pipeline_assertion (pipeline_id, ordinal, expression, enabled)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for assertion in assertions {
        stmt.execute(params![
            id.get(),
            assertion.ordinal,
            assertion.expression,
            assertion.enabled,
        ])?;
    }
    Ok(())
}

pub fn pipeline_assertions(conn: &Connection, id: PipelineId) -> Result<Vec<AssertionRow>> {
    let mut stmt = conn.prepare(
        "SELECT ordinal, expression, enabled FROM pipeline_assertion
         WHERE pipeline_id = ?1 ORDER BY ordinal",
    )?;
    let rows = stmt.query_and_then([id.get()], |row| {
        Ok(AssertionRow {
            ordinal: row.get(0)?,
            expression: row.get(1)?,
            enabled: row.get(2)?,
        })
    })?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Assertion outcomes
// ---------------------------------------------------------------------------

/// How one assertion turned out on one group of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct AssertionResultRow {
    pub group_id: GroupId,
    pub ordinal: u32,
    pub expression: String,
    pub status: AssertStatus,
    /// What the subject resolved to, when it resolved to a number.
    pub actual: Option<f64>,
    /// What it was tested against, when that is a number — a literal, or the
    /// baseline's value for the same subject.
    pub expected: Option<f64>,
    pub message: Option<String>,
}

impl AssertionResultRow {
    #[must_use]
    pub fn new(
        group_id: GroupId,
        ordinal: u32,
        expression: impl Into<String>,
        status: AssertStatus,
    ) -> Self {
        Self {
            group_id,
            ordinal,
            expression: expression.into(),
            status,
            actual: None,
            expected: None,
            message: None,
        }
    }

    #[must_use]
    pub fn with_values(mut self, actual: Option<f64>, expected: Option<f64>) -> Self {
        self.actual = actual;
        self.expected = expected;
        self
    }

    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// One line naming the assertion and what it saw, which is what a failure
    /// is read for.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut line = format!("{} — {}", self.expression, self.status.label());
        if let Some(message) = &self.message {
            line.push_str(": ");
            line.push_str(message);
        }
        line
    }
}

/// Records one assertion outcome, overwriting any earlier one for the same
/// `(run, group, ordinal)` — a re-evaluation replaces its predecessor rather
/// than accumulating.
pub fn record_assertion(conn: &Connection, run: RunId, result: &AssertionResultRow) -> Result<()> {
    conn.execute(
        "INSERT INTO run_assertion (run_id, group_id, ordinal, expression, status,
                                    actual, expected, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(run_id, group_id, ordinal) DO UPDATE SET
             expression = excluded.expression,
             status = excluded.status,
             actual = excluded.actual,
             expected = excluded.expected,
             message = excluded.message",
        params![
            run.get(),
            result.group_id.get(),
            result.ordinal,
            result.expression,
            result.status.as_str(),
            finite(result.actual),
            finite(result.expected),
            result.message,
        ],
    )?;
    Ok(())
}

const RESULT_COLUMNS: &str = "group_id, ordinal, expression, status, actual, expected, message";

fn result_from_row(row: &Row<'_>) -> Result<AssertionResultRow> {
    Ok(AssertionResultRow {
        group_id: GroupId::new(row.get(0)?),
        ordinal: row.get(1)?,
        expression: row.get(2)?,
        status: row.get::<_, String>(3)?.parse()?,
        actual: row.get(4)?,
        expected: row.get(5)?,
        message: row.get(6)?,
    })
}

/// Every assertion outcome of a run, in group then assertion order.
pub fn run_assertions(conn: &Connection, run: RunId) -> Result<Vec<AssertionResultRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RESULT_COLUMNS} FROM run_assertion WHERE run_id = ?1
         ORDER BY group_id, ordinal"
    ))?;
    let rows = stmt.query_and_then([run.get()], result_from_row)?;
    rows.collect()
}

/// One group's assertion outcomes.
pub fn group_assertions(
    conn: &Connection,
    run: RunId,
    group: GroupId,
) -> Result<Vec<AssertionResultRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RESULT_COLUMNS} FROM run_assertion WHERE run_id = ?1 AND group_id = ?2
         ORDER BY ordinal"
    ))?;
    let rows = stmt.query_and_then(params![run.get(), group.get()], result_from_row)?;
    rows.collect()
}

/// How many assertions of a run failed, without reading them all back — what
/// the run list shows next to a run.
pub fn failed_assertions(conn: &Connection, run: RunId) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM run_assertion WHERE run_id = ?1 AND status IN ('fail','error')",
        [run.get()],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

// ---------------------------------------------------------------------------
// Baselines
// ---------------------------------------------------------------------------

/// A run promoted to golden.
#[derive(Debug, Clone, PartialEq)]
pub struct BaselineRow {
    pub id: BaselineId,
    pub name: String,
    pub run_id: RunId,
    pub created_utc: Timestamp,
    pub tolerances: Tolerances,
}

/// Promotes `run` to the baseline called `name`, replacing whatever that name
/// pointed at.
///
/// Re-promoting under an existing name is the normal way to accept a change:
/// the name is what a pipeline and a CI job refer to, so it has to outlive the
/// run it currently names.
pub fn promote(
    conn: &Connection,
    name: &str,
    run: RunId,
    tolerances: &Tolerances,
) -> Result<BaselineId> {
    let name = name.trim();
    if name.is_empty() {
        return Err(StoreError::invalid("a baseline needs a name"));
    }
    // Naming a run that does not exist would leave a baseline nothing can be
    // compared against, so it is refused here rather than at comparison time.
    crate::runs::get_run(conn, run)?;

    conn.execute(
        "INSERT INTO baseline (name, run_id, created_utc, tolerance_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
             run_id = excluded.run_id,
             created_utc = excluded.created_utc,
             tolerance_json = excluded.tolerance_json",
        params![
            name,
            run.get(),
            format_timestamp(now_utc())?,
            serde_json::to_string(tolerances)?,
        ],
    )?;
    get_baseline_by_name(conn, name).map(|row| row.id)
}

const BASELINE_COLUMNS: &str = "id, name, run_id, created_utc, tolerance_json";

fn baseline_from_row(row: &Row<'_>) -> Result<BaselineRow> {
    Ok(BaselineRow {
        id: BaselineId::new(row.get(0)?),
        name: row.get(1)?,
        run_id: RunId::new(row.get(2)?),
        created_utc: parse_timestamp(&row.get::<_, String>(3)?)?,
        tolerances: serde_json::from_str(&row.get::<_, String>(4)?)?,
    })
}

pub fn get_baseline(conn: &Connection, id: BaselineId) -> Result<BaselineRow> {
    conn.query_row_and_then(
        &format!("SELECT {BASELINE_COLUMNS} FROM baseline WHERE id = ?1"),
        [id.get()],
        baseline_from_row,
    )
    .map_err(|error| as_not_found(error, "baseline", id.get()))
}

/// The baseline a name refers to. Names are what the CLI and a pipeline's
/// assertions use, so this is the usual way in.
pub fn get_baseline_by_name(conn: &Connection, name: &str) -> Result<BaselineRow> {
    conn.query_row_and_then(
        &format!("SELECT {BASELINE_COLUMNS} FROM baseline WHERE name = ?1"),
        [name.trim()],
        baseline_from_row,
    )
    .map_err(|error| match error {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
            StoreError::Invalid(format!("no baseline is called '{}'", name.trim()))
        }
        other => other,
    })
}

/// Baselines by name, which is the order the picker lists them in.
pub fn list_baselines(conn: &Connection) -> Result<Vec<BaselineRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {BASELINE_COLUMNS} FROM baseline ORDER BY name"
    ))?;
    let rows = stmt.query_and_then([], baseline_from_row)?;
    rows.collect()
}

/// The baselines that point at `run`. A run named by one cannot be deleted
/// without breaking the comparison, so callers check this first.
pub fn baselines_of_run(conn: &Connection, run: RunId) -> Result<Vec<BaselineRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {BASELINE_COLUMNS} FROM baseline WHERE run_id = ?1 ORDER BY name"
    ))?;
    let rows = stmt.query_and_then([run.get()], baseline_from_row)?;
    rows.collect()
}

/// Updates a baseline's tolerances in place, leaving the run it names alone.
pub fn set_tolerances(conn: &Connection, id: BaselineId, tolerances: &Tolerances) -> Result<()> {
    let changed = conn.execute(
        "UPDATE baseline SET tolerance_json = ?1 WHERE id = ?2",
        params![serde_json::to_string(tolerances)?, id.get()],
    )?;
    if changed == 0 {
        return Err(StoreError::not_found("baseline", id.get()));
    }
    Ok(())
}

/// Forgets a baseline. The run it named is untouched: promoting is a label,
/// and removing the label is not a reason to lose the results.
pub fn delete_baseline(conn: &Connection, id: BaselineId) -> Result<()> {
    let deleted = conn.execute("DELETE FROM baseline WHERE id = ?1", [id.get()])?;
    if deleted == 0 {
        return Err(StoreError::not_found("baseline", id.get()));
    }
    Ok(())
}

/// SQLite has no `REAL` for NaN or infinity, so a non-finite value is stored
/// as NULL — "there was no number here", which is what it means.
fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite())
}

fn as_not_found(error: StoreError, what: &'static str, id: i64) -> StoreError {
    match error {
        StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => StoreError::not_found(what, id),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::{RunStatus, SourceKind};

    use crate::library::{self, NewDataset, NewGroup};
    use crate::runs::{self, NewPipeline, NewRun};
    use crate::trains::{self, NewTrain};
    use crate::Store;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        pipeline: PipelineId,
        group: GroupId,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("library.db")).unwrap();
        let (pipeline, group) = store
            .write(|conn| {
                let dataset =
                    library::insert_dataset(conn, &NewDataset::new("d", SourceKind::Generated))?;
                let train = trains::insert_train(conn, &NewTrain::new(dataset, 0))?;
                let group = library::insert_group(conn, &NewGroup::new(train, 0, 0))?;
                let pipeline = runs::insert_pipeline(conn, &NewPipeline::new("p"))?;
                Ok((pipeline, group))
            })
            .unwrap();
        Fixture {
            _dir: dir,
            store,
            pipeline,
            group,
        }
    }

    fn a_run(fixture: &Fixture) -> RunId {
        let pipeline = fixture.pipeline;
        fixture
            .store
            .write(move |conn| {
                let run = runs::begin_run(conn, &NewRun::new(pipeline, "hash"))?;
                runs::finish_run(conn, run, RunStatus::Ok)?;
                Ok(run)
            })
            .unwrap()
    }

    #[test]
    fn a_pipelines_assertions_are_replaced_wholesale() {
        let fixture = fixture();
        let id = fixture.pipeline;
        let saved = fixture
            .store
            .write(move |conn| {
                set_pipeline_assertions(
                    conn,
                    id,
                    &[
                        AssertionRow::new(0, "metrics.snr_db > 12.0"),
                        AssertionRow::new(1, "detections.count == 4").disabled(),
                    ],
                )?;
                pipeline_assertions(conn, id)
            })
            .unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].expression, "metrics.snr_db > 12.0");
        assert!(saved[0].enabled);
        assert!(!saved[1].enabled);

        let replaced = fixture
            .store
            .write(move |conn| {
                set_pipeline_assertions(conn, id, &[AssertionRow::new(0, "metrics.rms < 1.0")])?;
                pipeline_assertions(conn, id)
            })
            .unwrap();
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].expression, "metrics.rms < 1.0");
    }

    #[test]
    fn an_outcome_records_what_it_saw_and_counts_towards_the_run() {
        let fixture = fixture();
        let run = a_run(&fixture);
        let group = fixture.group;

        let (results, failed) = fixture
            .store
            .write(move |conn| {
                record_assertion(
                    conn,
                    run,
                    &AssertionResultRow::new(group, 0, "metrics.snr_db > 12.0", AssertStatus::Fail)
                        .with_values(Some(9.5), Some(12.0))
                        .with_message("9.5 is not > 12"),
                )?;
                record_assertion(
                    conn,
                    run,
                    &AssertionResultRow::new(group, 1, "detections.count == 4", AssertStatus::Pass)
                        .with_values(Some(4.0), Some(4.0)),
                )?;
                Ok((run_assertions(conn, run)?, failed_assertions(conn, run)?))
            })
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].status, AssertStatus::Fail);
        assert_eq!(results[0].actual, Some(9.5));
        assert!(results[0].describe().contains("9.5 is not > 12"));
        assert_eq!(failed, 1);

        // Re-evaluating replaces rather than accumulates.
        let again = fixture
            .store
            .write(move |conn| {
                record_assertion(
                    conn,
                    run,
                    &AssertionResultRow::new(group, 0, "metrics.snr_db > 12.0", AssertStatus::Pass)
                        .with_values(Some(13.0), Some(12.0)),
                )?;
                Ok((
                    group_assertions(conn, run, group)?,
                    failed_assertions(conn, run)?,
                ))
            })
            .unwrap();
        assert_eq!(again.0.len(), 2);
        assert_eq!(again.1, 0);
    }

    #[test]
    fn a_non_finite_value_is_recorded_as_no_value_at_all() {
        let fixture = fixture();
        let run = a_run(&fixture);
        let group = fixture.group;
        let stored = fixture
            .store
            .write(move |conn| {
                record_assertion(
                    conn,
                    run,
                    &AssertionResultRow::new(
                        group,
                        0,
                        "metrics.snr_db > 12.0",
                        AssertStatus::Error,
                    )
                    .with_values(Some(f64::NAN), Some(12.0)),
                )?;
                run_assertions(conn, run)
            })
            .unwrap();
        assert_eq!(stored[0].actual, None);
        assert_eq!(stored[0].expected, Some(12.0));
    }

    #[test]
    fn promoting_under_an_existing_name_moves_it_to_the_new_run() {
        let fixture = fixture();
        let first = a_run(&fixture);
        let second = a_run(&fixture);

        let tolerances = Tolerances {
            sample_abs: 1e-9,
            ..Tolerances::EXACT
        };
        let listed = fixture
            .store
            .write(move |conn| {
                promote(conn, "golden", first, &Tolerances::EXACT)?;
                promote(conn, "golden", second, &tolerances)?;
                list_baselines(conn)
            })
            .unwrap();

        assert_eq!(listed.len(), 1, "a name refers to exactly one run");
        assert_eq!(listed[0].run_id, second);
        assert_eq!(listed[0].tolerances.sample_abs, 1e-9);

        let by_name = fixture
            .store
            .read(|conn| get_baseline_by_name(conn, " golden "))
            .unwrap();
        assert_eq!(by_name.id, listed[0].id);
        assert_eq!(
            fixture
                .store
                .read(move |conn| baselines_of_run(conn, second))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_baseline_needs_a_name_and_a_run_that_exists() {
        let fixture = fixture();
        let run = a_run(&fixture);
        let blank = fixture
            .store
            .write(move |conn| promote(conn, "  ", run, &Tolerances::EXACT));
        assert!(blank.is_err());

        let missing = fixture
            .store
            .write(|conn| promote(conn, "golden", RunId::new(9999), &Tolerances::EXACT));
        assert!(missing.unwrap_err().is_not_found());

        let unknown = fixture
            .store
            .read(|conn| get_baseline_by_name(conn, "none"));
        assert!(unknown.unwrap_err().to_string().contains("no baseline"));
    }

    #[test]
    fn a_run_a_baseline_names_cannot_be_deleted() {
        // The baseline's golden data is the run's own rows, so deleting the
        // run would leave every later comparison with nothing to compare to.
        let fixture = fixture();
        let run = a_run(&fixture);
        let refused = fixture.store.write(move |conn| {
            promote(conn, "golden", run, &Tolerances::EXACT)?;
            runs::delete_run(conn, run)
        });
        assert!(refused.unwrap_err().to_string().contains("golden"));

        let allowed = fixture.store.write(move |conn| {
            let id = get_baseline_by_name(conn, "golden")?.id;
            delete_baseline(conn, id)?;
            runs::delete_run(conn, run)
        });
        assert!(allowed.is_ok());
    }

    #[test]
    fn tolerances_can_be_retuned_without_re_promoting() {
        let fixture = fixture();
        let run = a_run(&fixture);
        let loosened = Tolerances {
            metric_rel: 0.05,
            ..Tolerances::EXACT
        };
        let row = fixture
            .store
            .write(move |conn| {
                let id = promote(conn, "golden", run, &Tolerances::EXACT)?;
                set_tolerances(conn, id, &loosened)?;
                get_baseline(conn, id)
            })
            .unwrap();
        assert_eq!(row.run_id, run);
        assert_eq!(row.tolerances.metric_rel, 0.05);

        // Forgetting the label leaves the run itself alone.
        let id = row.id;
        fixture
            .store
            .write(move |conn| delete_baseline(conn, id))
            .unwrap();
        assert!(fixture
            .store
            .read(move |conn| runs::get_run(conn, run))
            .is_ok());
    }
}
