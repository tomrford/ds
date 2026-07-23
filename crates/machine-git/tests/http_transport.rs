use std::error::Error;
use std::process::Command;
use std::time::{Duration, Instant};

use devspace_machine_git::GitHttpTransport;

#[path = "../../cli/tests/support/stalling_server.rs"]
mod stalling_server;
use stalling_server::StallingServer;

const CHILD_ENV: &str = "DEVSPACE_MACHINE_GIT_TIMEOUT_TEST_CHILD";

#[tokio::test(flavor = "current_thread")]
async fn http_transport_times_out_when_worker_stalls() {
    if std::env::var_os(CHILD_ENV).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "http_transport_times_out_when_worker_stalls",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .env("DEVSPACE_HTTP_TEST_HOOKS", "1")
            .env("DEVSPACE_HTTP_TEST_REQUEST_TIMEOUT_MS", "100")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    let server = StallingServer::start();
    let transport = GitHttpTransport::new(
        server.base_url(),
        "timeout-test-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();

    let started = Instant::now();
    let error = transport.list_packs(0, None).await.unwrap_err();

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "Git catalog request took {:?}",
        started.elapsed(),
    );
    assert!(
        error_chain_contains(&error, "operation timed out"),
        "{error:?}"
    );
}

fn error_chain_contains(error: &(dyn Error + 'static), needle: &str) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.to_string().contains(needle) {
            return true;
        }
        current = error.source();
    }
    false
}
