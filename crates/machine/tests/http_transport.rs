use std::error::Error;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use devspace_machine::GitHttpTransport;

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

#[tokio::test(flavor = "current_thread")]
async fn paged_projection_snapshot_keeps_first_page_metadata_during_concurrent_update() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let first = projection_page_json(2, "11", "aa", 1, true, "01");
        let second = projection_page_json(3, "22", "bb", 2, false, "02");
        for body in [first, second] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            if body.contains("\"nextAfter\":1") {
                assert!(request.contains("after=0"));
                assert!(!request.contains("through="));
            } else {
                assert!(request.contains("after=1&through=2"));
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });
    let transport = GitHttpTransport::new(
        &format!("http://{address}"),
        "snapshot-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();

    let snapshot = transport.projection_snapshot_all().await.unwrap();

    assert_eq!(snapshot.activation_cursor, 2);
    assert_eq!(snapshot.through, 2);
    assert_eq!(snapshot.next_after, 2);
    assert!(!snapshot.has_more);
    assert_eq!(
        snapshot.cursors[0].canonical_oid,
        devspace_kernel::Oid([0x11; 20])
    );
    assert_eq!(snapshot.pending[0].owner_machine, [0xaa; 16]);
    assert_eq!(snapshot.mappings.len(), 2);
    server.join().unwrap();
}

fn projection_page_json(
    activation_cursor: u64,
    cursor_byte: &str,
    owner_byte: &str,
    next_after: u64,
    has_more: bool,
    mapping_byte: &str,
) -> String {
    format!(
        r#"{{
          "activationCursor":{activation_cursor},
          "cursors":[{{
            "remote":"origin","bookmark":"main",
            "canonicalOid":"{cursor_oid}","publicOid":"{cursor_oid}",
            "hiddenSetId":null,"activationSequence":{activation_cursor}
          }}],
          "mappings":[{{
            "remote":"origin","bookmark":"main",
            "canonicalOid":"{mapping_oid}","publicOid":"{mapping_oid}",
            "hiddenSetId":null
          }}],
          "nextAfter":{next_after},"through":2,"hasMore":{has_more},
          "pending":[{{
            "batchId":"{batch_id}","remote":"origin",
            "ownerMachine":"{owner_machine}","fence":1,"refs":[]
          }}]
        }}"#,
        cursor_oid = cursor_byte.repeat(20),
        mapping_oid = mapping_byte.repeat(20),
        batch_id = mapping_byte.repeat(16),
        owner_machine = owner_byte.repeat(16),
    )
}
