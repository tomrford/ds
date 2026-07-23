//! Redacting Git subprocess boundary for lease pushes and identity fetches.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::{Oid, hex};

const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
const MAX_OBSERVATION_BYTES: usize = 1024 * 1024;
const MAX_REMOTE_HEADS: usize = 256;

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QualifiedRef(String);

impl QualifiedRef {
    pub fn from_bookmark(bookmark: &str) -> Result<Self, QualifiedRefError> {
        valid_bookmark(bookmark)
            .then(|| Self(format!("refs/heads/{bookmark}")))
            .ok_or(QualifiedRefError)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_qualified(value: &str) -> Result<Self, QualifiedRefError> {
        Self::from_bookmark(value.strip_prefix("refs/heads/").ok_or(QualifiedRefError)?)
    }
}

impl fmt::Debug for QualifiedRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("QualifiedRef")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for QualifiedRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QualifiedRefError;

impl fmt::Display for QualifiedRefError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bookmark is not a valid Git branch name")
    }
}

impl std::error::Error for QualifiedRefError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaseUpdate {
    pub expected_old_oid: Option<Oid>,
    pub new_oid: Option<Oid>,
}

#[derive(Clone)]
pub struct RemoteUrl(String);

impl RemoteUrl {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RemoteUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<remote-url>")
    }
}

impl fmt::Display for RemoteUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<remote-url>")
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GitProcessMode {
    #[default]
    Foreground,
    Background,
}

#[derive(Clone)]
pub struct GitProcessEnvironment {
    git_executable: PathBuf,
    extra: BTreeMap<OsString, OsString>,
    mode: GitProcessMode,
}

impl GitProcessEnvironment {
    pub fn new(git_executable: impl Into<PathBuf>, mode: GitProcessMode) -> Self {
        Self {
            git_executable: git_executable.into(),
            extra: BTreeMap::new(),
            mode,
        }
    }

    pub fn with_extra_environment(mut self, extra: BTreeMap<OsString, OsString>) -> Self {
        self.extra = extra;
        self
    }
}

impl Default for GitProcessEnvironment {
    fn default() -> Self {
        Self::new(resolve_git_executable(), GitProcessMode::Foreground)
    }
}

impl fmt::Debug for GitProcessEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitProcessEnvironment")
            .field("git_executable", &self.git_executable)
            .field("extra", &"<redacted>")
            .field("mode", &self.mode)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushRefStatus {
    Updated,
    Deleted,
    UpToDate,
    LeaseRejected,
    RemoteRejected,
    OtherRejected,
    NotReported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushRefReport {
    pub status: PushRefStatus,
    pub observed_oid: Option<Oid>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandDiagnostic {
    pub command: String,
    pub exit_code: Option<i32>,
    pub observation_command: String,
    pub observation_exit_code: Option<i32>,
    pub stderr_excerpt: String,
}

impl fmt::Display for CommandDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}; {}; {}",
            self.command, self.observation_command, self.stderr_excerpt
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushReport {
    pub refs: BTreeMap<QualifiedRef, PushRefReport>,
    pub diagnostic: CommandDiagnostic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushErrorKind {
    InvalidInput,
    PushFailed,
    ObservationFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushError {
    pub kind: PushErrorKind,
    pub report: PushReport,
}

impl fmt::Display for PushError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Git push failed: {}", self.report.diagnostic)
    }
}

impl std::error::Error for PushError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchReport {
    pub heads: BTreeMap<String, Oid>,
    pub diagnostic: CommandDiagnostic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchError {
    pub report: FetchReport,
}

impl fmt::Display for FetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Git fetch failed: {}", self.report.diagnostic)
    }
}

impl std::error::Error for FetchError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteHeadsError {
    command: String,
    exit_code: Option<i32>,
    stderr_excerpt: String,
}

impl fmt::Display for RemoteHeadsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Git remote observation failed: {}; exit code {:?}; {}",
            self.command, self.exit_code, self.stderr_excerpt
        )
    }
}

impl std::error::Error for RemoteHeadsError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteHead {
    pub branch: String,
    pub oid: Oid,
}

pub fn ls_remote_heads(
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> Result<BTreeMap<String, Oid>, RemoteHeadsError> {
    let spec = remote_heads_command(remote_url, environment);
    let result = run(&spec);
    let parsed = result
        .as_ref()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_remote_heads(&output.stdout));
    if let Some(Ok(heads)) = parsed {
        return Ok(heads);
    }
    let mut stderr = Vec::new();
    append_result_stderr(&mut stderr, &result);
    if let Some(Err(error)) = parsed {
        stderr.extend_from_slice(error.as_bytes());
    }
    Err(RemoteHeadsError {
        command: spec.safe_shape,
        exit_code: result.as_ref().ok().and_then(|output| output.status.code()),
        stderr_excerpt: redact_stderr(&stderr, remote_url, environment),
    })
}

pub fn ls_remote_head(
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> Result<Option<RemoteHead>, RemoteHeadsError> {
    let spec = remote_head_command(remote_url, environment);
    let result = run(&spec);
    let parsed = result
        .as_ref()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_remote_head(&output.stdout));
    if let Some(Ok(head)) = parsed {
        return Ok(head);
    }
    let mut stderr = Vec::new();
    append_result_stderr(&mut stderr, &result);
    if let Some(Err(error)) = parsed {
        stderr.extend_from_slice(error.as_bytes());
    }
    Err(RemoteHeadsError {
        command: spec.safe_shape,
        exit_code: result.as_ref().ok().and_then(|output| output.status.code()),
        stderr_excerpt: redact_stderr(&stderr, remote_url, environment),
    })
}

struct CommandSpec {
    program: PathBuf,
    args: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    safe_shape: String,
}

pub fn push(
    git_dir: &Path,
    remote_url: &RemoteUrl,
    updates: &BTreeMap<QualifiedRef, LeaseUpdate>,
    environment: &GitProcessEnvironment,
) -> Result<PushReport, PushError> {
    let mut refs = updates
        .keys()
        .cloned()
        .map(|reference| {
            (
                reference,
                PushRefReport {
                    status: PushRefStatus::NotReported,
                    observed_oid: None,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let push_spec = push_command(git_dir, remote_url, updates, environment);
    let observation_spec = observation_command(git_dir, remote_url, updates, environment);
    if updates.is_empty() {
        return Err(PushError {
            kind: PushErrorKind::InvalidInput,
            report: PushReport {
                refs,
                diagnostic: empty_diagnostic(
                    &push_spec,
                    &observation_spec,
                    "no refs were requested",
                ),
            },
        });
    }
    let push_result = run(&push_spec);
    if let Ok(output) = &push_result {
        parse_push_porcelain(&output.stdout, &mut refs);
    }
    // Observation is unconditional: the Git exit status is never journal authority.
    let observation_result = run(&observation_spec);
    let observation_parse_error = match &observation_result {
        Ok(output) if output.status.success() => parse_observation(&output.stdout, &mut refs).err(),
        _ => None,
    };
    let diagnostic = diagnostic(
        &push_spec,
        &push_result,
        &observation_spec,
        &observation_result,
        observation_parse_error.as_deref(),
        remote_url,
        environment,
    );
    let report = PushReport { refs, diagnostic };
    if observation_result
        .as_ref()
        .map_or(true, |output| !output.status.success())
        || observation_parse_error.is_some()
    {
        return Err(PushError {
            kind: PushErrorKind::ObservationFailed,
            report,
        });
    }
    if push_result
        .as_ref()
        .map_or(true, |output| !output.status.success())
    {
        return Err(PushError {
            kind: PushErrorKind::PushFailed,
            report,
        });
    }
    Ok(report)
}

pub fn fetch(
    git_dir: &Path,
    remote_name: &str,
    bookmarks: &[String],
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> Result<FetchReport, FetchError> {
    let requested = observation_refs(remote_name, bookmarks).map_err(|message| FetchError {
        report: FetchReport {
            heads: BTreeMap::new(),
            diagnostic: CommandDiagnostic {
                command: "git fetch <remote>".to_owned(),
                exit_code: None,
                observation_command: "git show-ref".to_owned(),
                observation_exit_code: None,
                stderr_excerpt: message,
            },
        },
    })?;
    let fetch_spec = fetch_command(git_dir, remote_name, bookmarks, remote_url, environment);
    let observation_spec = fetched_observation_command(git_dir, &requested, environment);
    let fetch_result = run(&fetch_spec);
    let observation_result = fetch_result
        .as_ref()
        .ok()
        .filter(|output| output.status.success())
        .map(|_| run(&observation_spec));
    let mut heads = BTreeMap::new();
    let mut parse_error = None;
    if let Some(Ok(output)) = &observation_result
        && output.status.success()
    {
        match parse_fetched_observation(&output.stdout, &requested) {
            Ok(value) => heads = value,
            Err(error) => parse_error = Some(error),
        }
    }
    let missing_observation = observation_result.as_ref().is_none_or(|result| {
        result
            .as_ref()
            .map_or(true, |output| !output.status.success())
    });
    let observation_for_diagnostic =
        observation_result.unwrap_or_else(|| Err("fetch did not complete".to_owned()));
    let diagnostic = diagnostic(
        &fetch_spec,
        &fetch_result,
        &observation_spec,
        &observation_for_diagnostic,
        parse_error.as_deref(),
        remote_url,
        environment,
    );
    let report = FetchReport { heads, diagnostic };
    if fetch_result
        .as_ref()
        .map_or(true, |output| !output.status.success())
        || missing_observation
        || parse_error.is_some()
    {
        return Err(FetchError { report });
    }
    Ok(report)
}

fn push_command(
    git_dir: &Path,
    remote_url: &RemoteUrl,
    updates: &BTreeMap<QualifiedRef, LeaseUpdate>,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let mut args = vec![
        git_dir_arg(git_dir),
        "push".into(),
        "--porcelain".into(),
        "--no-verify".into(),
        "--atomic".into(),
    ];
    let mut safe = os_strings(&args);
    for (reference, update) in updates {
        let expected = update.expected_old_oid.map_or_else(String::new, oid_hex);
        let lease = format!("--force-with-lease={reference}:{expected}");
        args.push(lease.clone().into());
        safe.push(lease);
    }
    args.push("--".into());
    args.push(remote_url.expose().into());
    safe.push("--".to_owned());
    safe.push("<remote>".to_owned());
    for (reference, update) in updates {
        let refspec = update.new_oid.map_or_else(
            || format!(":{reference}"),
            |oid| format!("{}:{reference}", oid_hex(oid)),
        );
        args.push(refspec.clone().into());
        safe.push(refspec);
    }
    command_spec(args, safe, environment)
}

fn observation_command(
    git_dir: &Path,
    remote_url: &RemoteUrl,
    updates: &BTreeMap<QualifiedRef, LeaseUpdate>,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let mut args = vec![
        git_dir_arg(git_dir),
        "ls-remote".into(),
        "--refs".into(),
        "--".into(),
        remote_url.expose().into(),
    ];
    let mut safe = vec![
        format!("--git-dir={}", git_dir.display()),
        "ls-remote".to_owned(),
        "--refs".to_owned(),
        "--".to_owned(),
        "<remote>".to_owned(),
    ];
    for reference in updates.keys() {
        args.push(reference.as_str().into());
        safe.push(reference.to_string());
    }
    command_spec(args, safe, environment)
}

fn remote_heads_command(
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let args = vec![
        "ls-remote".into(),
        "--refs".into(),
        "--".into(),
        remote_url.expose().into(),
        "refs/heads/*".into(),
    ];
    let safe = vec![
        "ls-remote".to_owned(),
        "--refs".to_owned(),
        "--".to_owned(),
        "<remote>".to_owned(),
        "refs/heads/*".to_owned(),
    ];
    command_spec(args, safe, environment)
}

fn remote_head_command(remote_url: &RemoteUrl, environment: &GitProcessEnvironment) -> CommandSpec {
    let args = vec![
        "ls-remote".into(),
        "--symref".into(),
        "--".into(),
        remote_url.expose().into(),
        "HEAD".into(),
    ];
    let safe = vec![
        "ls-remote".to_owned(),
        "--symref".to_owned(),
        "--".to_owned(),
        "<remote>".to_owned(),
        "HEAD".to_owned(),
    ];
    command_spec(args, safe, environment)
}

fn fetch_command(
    git_dir: &Path,
    remote_name: &str,
    bookmarks: &[String],
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let mut args = vec![
        git_dir_arg(git_dir),
        "fetch".into(),
        "--".into(),
        remote_url.expose().into(),
    ];
    let mut safe = vec![
        format!("--git-dir={}", git_dir.display()),
        "fetch".to_owned(),
        "--".to_owned(),
        "<remote>".to_owned(),
    ];
    for bookmark in bookmarks {
        let refspec =
            format!("+refs/heads/{bookmark}:refs/devspace/remotes/{remote_name}/{bookmark}");
        args.push(refspec.clone().into());
        safe.push(refspec);
    }
    command_spec(args, safe, environment)
}

fn fetched_observation_command(
    git_dir: &Path,
    requested: &BTreeMap<String, String>,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let mut args = vec![git_dir_arg(git_dir), "show-ref".into(), "--verify".into()];
    let mut safe = os_strings(&args);
    for reference in requested.values() {
        args.push(reference.into());
        safe.push(reference.clone());
    }
    command_spec(args, safe, environment)
}

fn command_spec(
    args: Vec<OsString>,
    safe: Vec<String>,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    CommandSpec {
        program: environment.git_executable.clone(),
        args,
        environment: command_environment(environment),
        safe_shape: safe_command_shape(&environment.git_executable, &safe),
    }
}

fn git_dir_arg(path: &Path) -> OsString {
    format!("--git-dir={}", path.display()).into()
}
fn os_strings(values: &[OsString]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}
fn oid_hex(oid: Oid) -> String {
    hex(&oid.0)
}

fn command_environment(environment: &GitProcessEnvironment) -> BTreeMap<OsString, OsString> {
    let mut values = environment.extra.clone();
    values.insert("LC_ALL".into(), "C".into());
    if environment.mode == GitProcessMode::Background {
        values.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    }
    values
}

fn run(spec: &CommandSpec) -> Result<Output, String> {
    Command::new(&spec.program)
        .args(&spec.args)
        .envs(&spec.environment)
        .output()
        .map_err(|error| format!("could not start Git: {error}"))
}

fn parse_push_porcelain(bytes: &[u8], refs: &mut BTreeMap<QualifiedRef, PushRefReport>) {
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() < 3 || line[1] != b'\t' {
            continue;
        }
        let fields = line[2..].split(|byte| *byte == b'\t').collect::<Vec<_>>();
        let (Some(refspec), Some(summary)) = (fields.first(), fields.get(1)) else {
            continue;
        };
        let refspec = String::from_utf8_lossy(refspec);
        let destination = refspec
            .rsplit_once(':')
            .map_or(refspec.as_ref(), |(_, value)| value);
        let Ok(reference) = QualifiedRef::from_qualified(destination) else {
            continue;
        };
        if let Some(report) = refs.get_mut(&reference) {
            report.status = status_from_porcelain(line[0], summary);
        }
    }
}

fn status_from_porcelain(flag: u8, summary: &[u8]) -> PushRefStatus {
    match flag {
        b' ' | b'+' | b'*' => PushRefStatus::Updated,
        b'-' => PushRefStatus::Deleted,
        b'=' => PushRefStatus::UpToDate,
        b'!' if contains(summary, b"stale info") => PushRefStatus::LeaseRejected,
        b'!' if contains(summary, b"non-fast-forward")
            || contains(summary, b"[remote rejected]") =>
        {
            PushRefStatus::RemoteRejected
        }
        _ => PushRefStatus::OtherRejected,
    }
}

fn parse_observation(
    bytes: &[u8],
    refs: &mut BTreeMap<QualifiedRef, PushRefReport>,
) -> Result<(), String> {
    if bytes.len() > MAX_OBSERVATION_BYTES {
        return Err("Git remote observation exceeded its byte limit".to_owned());
    }
    let mut observed = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let tab = line
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "Git returned a malformed remote observation".to_owned())?;
        let oid = Oid::from_hex(&line[..tab])
            .ok_or_else(|| "Git returned an invalid observed object ID".to_owned())?;
        let name = std::str::from_utf8(&line[tab + 1..])
            .map_err(|_| "Git returned a non-UTF-8 observed ref".to_owned())?;
        let reference = QualifiedRef::from_qualified(name)
            .map_err(|_| "Git returned an invalid observed ref".to_owned())?;
        if refs.contains_key(&reference) {
            observed.insert(reference, oid);
        }
    }
    for (reference, report) in refs {
        report.observed_oid = observed.get(reference).copied();
    }
    Ok(())
}

fn parse_remote_heads(bytes: &[u8]) -> Result<BTreeMap<String, Oid>, String> {
    if bytes.len() > MAX_OBSERVATION_BYTES {
        return Err("Git remote observation exceeded its byte limit".to_owned());
    }
    let mut heads = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if heads.len() >= MAX_REMOTE_HEADS {
            return Err("Git remote has too many branch heads".to_owned());
        }
        let tab = line
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "Git returned a malformed remote observation".to_owned())?;
        let oid = Oid::from_hex(&line[..tab])
            .ok_or_else(|| "Git returned an invalid observed object ID".to_owned())?;
        let reference = std::str::from_utf8(&line[tab + 1..])
            .map_err(|_| "Git returned a non-UTF-8 observed ref".to_owned())?;
        let bookmark = reference
            .strip_prefix("refs/heads/")
            .ok_or_else(|| "Git returned an invalid observed ref".to_owned())?;
        QualifiedRef::from_bookmark(bookmark)
            .map_err(|_| "Git returned an invalid observed ref".to_owned())?;
        if heads.insert(bookmark.to_owned(), oid).is_some() {
            return Err("Git returned a duplicate observed ref".to_owned());
        }
    }
    Ok(heads)
}

fn parse_remote_head(bytes: &[u8]) -> Result<Option<RemoteHead>, String> {
    if bytes.len() > MAX_OBSERVATION_BYTES {
        return Err("Git remote observation exceeded its byte limit".to_owned());
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    let mut branch = None;
    let mut oid = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(line)
            .map_err(|_| "Git returned a non-UTF-8 HEAD observation".to_owned())?;
        if let Some(value) = line.strip_prefix("ref: ") {
            let (reference, name) = value
                .split_once(char::is_whitespace)
                .ok_or_else(|| "Git returned a malformed HEAD symref".to_owned())?;
            if name.trim() != "HEAD" {
                return Err("Git returned a malformed HEAD symref".to_owned());
            }
            let name = reference
                .strip_prefix("refs/heads/")
                .ok_or_else(|| "Git remote HEAD does not point to a branch".to_owned())?;
            QualifiedRef::from_bookmark(name)
                .map_err(|_| "Git remote HEAD points to an invalid branch".to_owned())?;
            branch = Some(name.to_owned());
            continue;
        }
        let (value, name) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| "Git returned a malformed HEAD observation".to_owned())?;
        if name.trim() != "HEAD" {
            return Err("Git returned a malformed HEAD observation".to_owned());
        }
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("SHA-256 Git remotes are not supported".to_owned());
        }
        oid = Some(
            Oid::from_hex(value.as_bytes())
                .ok_or_else(|| "Git returned an invalid HEAD object ID".to_owned())?,
        );
    }
    match (branch, oid) {
        (Some(branch), Some(oid)) => Ok(Some(RemoteHead { branch, oid })),
        (None, None) => Ok(None),
        (None, Some(_)) => Err("Git remote HEAD is detached; it must point to a branch".to_owned()),
        (Some(_), None) => Err("Git remote HEAD did not advertise an object ID".to_owned()),
    }
}

fn observation_refs(
    remote: &str,
    bookmarks: &[String],
) -> Result<BTreeMap<String, String>, String> {
    if !valid_bookmark(remote) {
        return Err("remote name cannot be represented in an observation ref".to_owned());
    }
    let mut requested = BTreeMap::new();
    for bookmark in bookmarks {
        QualifiedRef::from_bookmark(bookmark).map_err(|error| error.to_string())?;
        if requested
            .insert(
                bookmark.clone(),
                format!("refs/devspace/remotes/{remote}/{bookmark}"),
            )
            .is_some()
        {
            return Err(format!(
                "bookmark `{bookmark}` was requested more than once"
            ));
        }
    }
    if requested.is_empty() {
        return Err("no refs were requested".to_owned());
    }
    Ok(requested)
}

fn parse_fetched_observation(
    bytes: &[u8],
    requested: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Oid>, String> {
    if bytes.len() > MAX_OBSERVATION_BYTES {
        return Err("Git fetch observation exceeded its byte limit".to_owned());
    }
    let mut by_ref = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let separator = line
            .iter()
            .position(|byte| byte.is_ascii_whitespace())
            .ok_or_else(|| "Git returned a malformed fetch observation".to_owned())?;
        let oid = Oid::from_hex(&line[..separator])
            .ok_or_else(|| "Git returned an invalid fetched object ID".to_owned())?;
        let name = std::str::from_utf8(&line[separator..])
            .map_err(|_| "Git returned a non-UTF-8 fetched ref".to_owned())?
            .trim();
        by_ref.insert(name.to_owned(), oid);
    }
    requested
        .iter()
        .map(|(bookmark, reference)| {
            by_ref
                .get(reference)
                .copied()
                .map(|oid| (bookmark.clone(), oid))
                .ok_or_else(|| format!("Git did not retain observation ref {reference}"))
        })
        .collect()
}

fn diagnostic(
    command: &CommandSpec,
    result: &Result<Output, String>,
    observation: &CommandSpec,
    observation_result: &Result<Output, String>,
    parse_error: Option<&str>,
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> CommandDiagnostic {
    let mut stderr = Vec::new();
    append_result_stderr(&mut stderr, result);
    append_result_stderr(&mut stderr, observation_result);
    if let Some(error) = parse_error {
        stderr.extend_from_slice(error.as_bytes());
    }
    CommandDiagnostic {
        command: command.safe_shape.clone(),
        exit_code: result.as_ref().ok().and_then(|output| output.status.code()),
        observation_command: observation.safe_shape.clone(),
        observation_exit_code: observation_result
            .as_ref()
            .ok()
            .and_then(|output| output.status.code()),
        stderr_excerpt: redact_stderr(&stderr, remote_url, environment),
    }
}

fn empty_diagnostic(
    command: &CommandSpec,
    observation: &CommandSpec,
    message: &str,
) -> CommandDiagnostic {
    CommandDiagnostic {
        command: command.safe_shape.clone(),
        exit_code: None,
        observation_command: observation.safe_shape.clone(),
        observation_exit_code: None,
        stderr_excerpt: message.to_owned(),
    }
}

fn append_result_stderr(buffer: &mut Vec<u8>, result: &Result<Output, String>) {
    match result {
        Ok(output) => buffer.extend_from_slice(&output.stderr),
        Err(error) => buffer.extend_from_slice(error.as_bytes()),
    }
}

fn redact_stderr(
    bytes: &[u8],
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut sensitive = vec![remote_url.expose().to_owned()];
    if let Some(authority) = remote_authority(remote_url.expose()) {
        sensitive.push(authority.to_owned());
    }
    sensitive.extend(
        environment
            .extra
            .iter()
            .flat_map(|(key, value)| [key, value])
            .filter_map(|value| {
                let value = value.to_string_lossy();
                (!value.is_empty()).then(|| value.into_owned())
            }),
    );
    let mut redacted = String::new();
    for line in text.lines() {
        if sensitive.iter().any(|value| line.contains(value)) {
            continue;
        }
        if redacted.len() + line.len() + 1 > MAX_DIAGNOSTIC_BYTES {
            redacted.push_str("\n<truncated>");
            break;
        }
        if !redacted.is_empty() {
            redacted.push('\n');
        }
        redacted.push_str(line);
    }
    if redacted.is_empty() {
        "<stderr redacted or empty>".to_owned()
    } else {
        redacted
    }
}

fn remote_authority(url: &str) -> Option<&str> {
    if let Some((_, remainder)) = url.split_once("://") {
        return Some(remainder.split(['/', '?', '#']).next().unwrap_or(remainder));
    }
    if let Some(remainder) = url.strip_prefix("//") {
        return Some(remainder.split(['/', '?', '#']).next().unwrap_or(remainder));
    }
    let (authority, _) = url.split_once(':')?;
    authority.contains('@').then_some(authority)
}

fn safe_command_shape(program: &Path, args: &[String]) -> String {
    std::iter::once(program.display().to_string())
        .chain(args.iter().cloned())
        .map(|word| shell_word(&word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_word(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=[]".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("{value:?}")
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn valid_bookmark(bookmark: &str) -> bool {
    !(bookmark.is_empty()
        || bookmark.starts_with('-')
        || bookmark.starts_with('/')
        || bookmark.ends_with('/')
        || bookmark.ends_with('.')
        || bookmark.contains("//")
        || bookmark.contains("..")
        || bookmark.contains("@{")
        || bookmark == "@"
        || bookmark.bytes().any(|byte| {
            byte <= b' '
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        }))
        && bookmark
            .split('/')
            .all(|component| !component.starts_with('.') && !component.ends_with(".lock"))
}

fn resolve_git_executable() -> PathBuf {
    env::var_os("PATH")
        .and_then(|path| {
            env::split_paths(&path)
                .map(|directory| directory.join("git"))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| PathBuf::from("git"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_shape_has_atomic_leases_c_locale_and_no_url() {
        let secret = RemoteUrl::new("https://user:secret@example.invalid/repo.git");
        let reference = QualifiedRef::from_bookmark("main").unwrap();
        let updates = BTreeMap::from([(
            reference,
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: Some(Oid([0x11; 20])),
            },
        )]);
        let environment = GitProcessEnvironment::new("git", GitProcessMode::Foreground);
        let command = push_command(Path::new("repo.git"), &secret, &updates, &environment);
        assert!(command.safe_shape.contains("--porcelain"));
        assert!(command.safe_shape.contains("--no-verify"));
        assert!(command.safe_shape.contains("--atomic"));
        assert!(
            command
                .safe_shape
                .contains("--force-with-lease=refs/heads/main:")
        );
        assert!(command.safe_shape.contains("<remote>"));
        assert!(!command.safe_shape.contains("secret"));
        assert_eq!(
            command.environment.get(&OsString::from("LC_ALL")),
            Some(&OsString::from("C"))
        );
    }

    #[test]
    fn redaction_removes_url_authority_and_environment_values() {
        let secret = RemoteUrl::new("https://user:secret@example.invalid/repo.git");
        let environment = GitProcessEnvironment::new("git", GitProcessMode::Foreground)
            .with_extra_environment(BTreeMap::from([("TOKEN".into(), "top-secret".into())]));
        let stderr = b"safe\nhttps://user:secret@example.invalid/repo.git\nuser:secret@example.invalid\ntop-secret\n";
        assert_eq!(redact_stderr(stderr, &secret, &environment), "safe");
    }
}
