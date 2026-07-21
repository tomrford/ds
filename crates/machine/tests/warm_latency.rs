use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use devspace_machine::MachineRepository;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteRefSymbol};
use jj_lib::repo::{Repo as _, RepoLoader, StoreFactories};
use jj_lib::settings::UserSettings;

const WARMUP_BATCHES: usize = 5;
const SAMPLE_BATCHES: usize = 21;
const OPENS_PER_BATCH: usize = 20;
const FIXTURE_OPERATIONS: usize = 64;

mod common;

use common::settings;

async fn stock_jj_batch(path: &Path, settings: &UserSettings) -> Duration {
    let started = Instant::now();
    for _ in 0..OPENS_PER_BATCH {
        let loader =
            RepoLoader::init_from_file_system(settings, path, &StoreFactories::default()).unwrap();
        let repository = loader.load_at_head().await.unwrap();
        black_box(repository.op_id());
    }
    started.elapsed()
}

async fn devspace_batch(path: &Path, settings: &UserSettings) -> Duration {
    let started = Instant::now();
    for _ in 0..OPENS_PER_BATCH {
        let repository = MachineRepository::open(path, settings).await.unwrap();
        black_box(repository.repo().op_id());
    }
    started.elapsed()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn require_release_build() {
    #[cfg(debug_assertions)]
    panic!("the warm-latency acceptance probe must run with --release");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "timing acceptance probe; run explicitly in release mode"]
async fn warm_local_open_stays_within_twice_stock_jj() {
    require_release_build();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo");
    let settings = settings();
    let repository = MachineRepository::init(&path, &settings).await.unwrap();
    // A warm open reads the head operation and its view, so give the probe a
    // repository with real operation depth and a view holding many bookmarks
    // rather than measuring against a freshly-initialized store.
    let mut repo = repository.repo().clone();
    for index in 0..FIXTURE_OPERATIONS {
        let mut transaction = repo.start_transaction();
        transaction.repo_mut().set_remote_bookmark(
            RemoteRefSymbol {
                name: RefName::new(&format!("bookmark-{index:03}")),
                remote: "origin".as_ref(),
            },
            RemoteRef {
                target: RefTarget::normal(repo.store().root_commit_id().clone()),
                state: RemoteRefState::New,
            },
        );
        repo = transaction
            .commit(format!("fixture operation {index}"))
            .await
            .unwrap();
    }
    drop(repo);
    drop(repository);

    for batch in 0..WARMUP_BATCHES {
        if batch % 2 == 0 {
            stock_jj_batch(&path, &settings).await;
            devspace_batch(&path, &settings).await;
        } else {
            devspace_batch(&path, &settings).await;
            stock_jj_batch(&path, &settings).await;
        }
    }

    let mut stock_jj = Vec::with_capacity(SAMPLE_BATCHES);
    let mut devspace = Vec::with_capacity(SAMPLE_BATCHES);
    for batch in 0..SAMPLE_BATCHES {
        if batch % 2 == 0 {
            stock_jj.push(stock_jj_batch(&path, &settings).await);
            devspace.push(devspace_batch(&path, &settings).await);
        } else {
            devspace.push(devspace_batch(&path, &settings).await);
            stock_jj.push(stock_jj_batch(&path, &settings).await);
        }
    }

    let stock_jj = median(stock_jj) / OPENS_PER_BATCH as u32;
    let devspace = median(devspace) / OPENS_PER_BATCH as u32;
    let ratio = devspace.as_secs_f64() / stock_jj.as_secs_f64();
    eprintln!(
        "warm repository open: stock jj {stock_jj:?}, devspace {devspace:?}, ratio {ratio:.3}x"
    );
    assert!(
        ratio <= 2.0,
        "warm Devspace repository open was {ratio:.3}x stock jj"
    );
}
