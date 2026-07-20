use std::process::Command;
use std::time::{Duration, Instant};

use devspace_machine::{MachineConfig, MachineId, ProjectionTransport, SharedSecret};

#[path = "../../cli/tests/support/stalling_server.rs"]
mod stalling_server;
use stalling_server::StallingServer;

const CHILD_ENV: &str = "DEVSPACE_PROJECTION_TIMEOUT_TEST_CHILD";

#[tokio::test(flavor = "current_thread")]
async fn projection_transport_times_out_when_worker_stalls() {
    if std::env::var_os(CHILD_ENV).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "projection_transport_times_out_when_worker_stalls",
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
    let config = MachineConfig::new(
        server.base_url(),
        MachineId::parse("11".repeat(16)).unwrap(),
        SharedSecret::new("timeout-test-secret").unwrap(),
    )
    .unwrap();
    let transport = ProjectionTransport::new(&config, &"ab".repeat(32), [0xcd; 16]).unwrap();

    let started = Instant::now();
    let error = transport.get(0, None).await.unwrap_err();

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "projection request took {:?}",
        started.elapsed(),
    );
    assert!(
        error_chain_contains(error.as_ref(), "operation timed out"),
        "{error:?}"
    );
}

fn error_chain_contains(mut error: &(dyn std::error::Error + 'static), needle: &str) -> bool {
    loop {
        if error.to_string().contains(needle) {
            return true;
        }
        let Some(source) = error.source() else {
            return false;
        };
        error = source;
    }
}
