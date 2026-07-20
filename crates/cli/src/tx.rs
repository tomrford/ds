use std::error::Error;
use std::fmt;
use std::sync::Arc;

use jj_lib::backend::BackendError;
use jj_lib::commit::Commit;
use jj_lib::op_store::OperationId;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::transaction::{Transaction, TransactionCommitError};
use jj_lib::working_copy::{CheckoutError, CheckoutStats};
use jj_lib::workspace::Workspace;

/// Commits a raw jj-lib transaction without leaving unresolved rewrites.
///
/// jj asserts that every rewrite has had its descendants rebased before the
/// transaction is written. Keep that invariant at this seam instead of at
/// individual call sites.
pub(crate) async fn commit_repo_transaction(
    mut transaction: Transaction,
    description: impl Into<String>,
) -> Result<Arc<ReadonlyRepo>, RepoTransactionError> {
    transaction
        .repo_mut()
        .rebase_descendants()
        .await
        .map_err(RepoTransactionError::Rebase)?;
    transaction
        .commit(description)
        .await
        .map_err(RepoTransactionError::Commit)
}

#[derive(Debug)]
pub(crate) enum RepoTransactionError {
    Rebase(BackendError),
    Commit(TransactionCommitError),
}

impl fmt::Display for RepoTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rebase(source) => source.fmt(formatter),
            Self::Commit(source) => source.fmt(formatter),
        }
    }
}

impl Error for RepoTransactionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rebase(source) => Some(source),
            Self::Commit(source) => Some(source),
        }
    }
}

/// Materializes a commit through the destination workspace's own store.
///
/// jj requires the checkout target and working copy to share the same store
/// `Arc`; re-fetching by ID preserves that identity across workspace seams.
pub(crate) async fn materialize_checkout(
    workspace: &mut Workspace,
    operation_id: OperationId,
    commit: &Commit,
) -> Result<CheckoutStats, MaterializeCheckoutError> {
    let commit = workspace
        .repo_loader()
        .store()
        .get_commit_async(commit.id())
        .await
        .map_err(MaterializeCheckoutError::Reload)?;
    workspace
        .check_out(operation_id, None, &commit)
        .await
        .map_err(MaterializeCheckoutError::Checkout)
}

#[derive(Debug)]
pub(crate) enum MaterializeCheckoutError {
    Reload(BackendError),
    Checkout(CheckoutError),
}

impl fmt::Display for MaterializeCheckoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reload(source) => source.fmt(formatter),
            Self::Checkout(source) => source.fmt(formatter),
        }
    }
}

impl Error for MaterializeCheckoutError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Reload(source) => Some(source),
            Self::Checkout(source) => Some(source),
        }
    }
}
