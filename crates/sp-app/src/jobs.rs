//! Store jobs as futures for `Task::perform` (`docs/DESIGN.md` §4.2).
//!
//! Store calls block, so they run on the tokio blocking pool rather than on
//! an executor worker; the UI thread only ever sees the resulting message.

use sp_store::{Connection, Store};

/// Runs a read against a pooled connection off the UI thread.
pub async fn read<T, F>(store: Store, job: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> sp_store::Result<T> + Send + 'static,
{
    flatten(tokio::task::spawn_blocking(move || store.read(job)).await)
}

/// Runs a write on the store's writer thread, off the UI thread.
pub async fn write<T, F>(store: Store, job: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> sp_store::Result<T> + Send + 'static,
{
    flatten(tokio::task::spawn_blocking(move || store.write(job)).await)
}

/// Runs a blocking call that does not touch the store — reading a file to
/// preview it, for instance — off the UI thread.
pub async fn blocking<T, F>(job: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(job).await {
        Ok(value) => value,
        Err(error) => panic!("a blocking job did not complete: {error}"),
    }
}

fn flatten<T>(outcome: Result<sp_store::Result<T>, tokio::task::JoinError>) -> Result<T, String> {
    match outcome {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) => Err(format!("store job did not complete: {error}")),
    }
}
