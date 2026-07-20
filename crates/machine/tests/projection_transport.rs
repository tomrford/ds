use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use devspace_machine::{
    FetchReceipt, FetchRef, FetchResult, HttpTransport, MachineConfig, MachineId, ProjectionState,
    SharedSecret,
};

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
    let transport = HttpTransport::new(&config, &"ab".repeat(32), [0xcd; 16]).unwrap();

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

#[tokio::test(flavor = "current_thread")]
async fn projection_transport_records_fetch_and_deserializes_result() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        assert!(
            request.starts_with(&format!(
                "POST /repositories/{}/git/fetches HTTP/1.1",
                "ab".repeat(32)
            )),
            "{request}"
        );
        let body: serde_json::Value = serde_json::from_str(request_body(&request)).unwrap();
        assert_eq!(body["incarnation"], "cd".repeat(16));
        assert_eq!(body["fetchId"], "22".repeat(16));
        assert_eq!(body["machineId"], "11".repeat(16));
        assert_eq!(body["remote"], "origin");
        assert_eq!(body["refs"][0]["bookmark"], "main");
        assert_eq!(body["refs"][0]["observedGitOid"], "33".repeat(20));
        assert_eq!(
            body["refs"][0]["expectedCursorOid"],
            serde_json::Value::Null
        );
        assert_eq!(body["refs"][0]["proposedState"], 0);
        assert_eq!(body["receipts"][0]["publicCommitId"], "55".repeat(64));
        respond_json(
            &mut stream,
            &serde_json::json!({
                "fetchId": "22".repeat(16),
                "activationCursor": 9,
            })
            .to_string(),
        );
    });
    let config = MachineConfig::new(
        base_url,
        MachineId::parse("11".repeat(16)).unwrap(),
        SharedSecret::new("fetch-test-secret").unwrap(),
    )
    .unwrap();
    let transport = HttpTransport::new(&config, &"ab".repeat(32), [0xcd; 16]).unwrap();
    let state = ProjectionState {
        git_oid: [0x33; 20],
        canonical_commit_id: [0x44; 64],
        public_commit_id: [0x55; 64],
        hidden_set_id: Some([0x66; 64]),
    };

    let result = transport
        .record_fetch(
            [0x22; 16],
            [0x11; 16],
            "origin",
            &[FetchRef {
                bookmark: "main".to_owned(),
                observed_git_oid: [0x33; 20],
                expected_cursor_oid: None,
                states: vec![state],
                proposed_state: Some(0),
            }],
            &[FetchReceipt {
                git_oid: [0x33; 20],
                public_commit_id: [0x55; 64],
            }],
        )
        .await
        .unwrap();

    assert_eq!(
        result,
        FetchResult {
            fetch_id: [0x22; 16],
            activation_cursor: 9,
        }
    );
    server.join().unwrap();
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8_192];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "client closed before completing its request");
        bytes.extend_from_slice(&buffer[..count]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + content_length {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

fn request_body(request: &str) -> &str {
    request.split_once("\r\n\r\n").unwrap().1
}

fn respond_json(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
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
