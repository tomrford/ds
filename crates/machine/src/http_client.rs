use std::time::Duration;

use crate::sync_engine::TransportError;

// Prevent an unreachable cloud from wedging the singleton daemon during connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
// Prevent a stalled Worker from wedging the singleton daemon during a request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

const TEST_HOOKS_ENV: &str = "DEVSPACE_HTTP_TEST_HOOKS";
const TEST_REQUEST_TIMEOUT_MS_ENV: &str = "DEVSPACE_HTTP_TEST_REQUEST_TIMEOUT_MS";

pub(crate) fn hardened_http_client() -> Result<reqwest::Client, TransportError> {
    Ok(reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(request_timeout())
        .build()?)
}

fn request_timeout() -> Duration {
    if std::env::var_os(TEST_HOOKS_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
        && let Some(milliseconds) = std::env::var(TEST_REQUEST_TIMEOUT_MS_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        && milliseconds > 0
    {
        return Duration::from_millis(milliseconds);
    }
    REQUEST_TIMEOUT
}
