//! Lease-protected atomic pushes through Git's credential and transport stack.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitOid(pub [u8; 20]);

impl GitOid {
    pub fn from_hex(value: &str) -> Result<Self, GitOidParseError> {
        if value.len() != 40
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(GitOidParseError);
        }
        let mut bytes = [0; 20];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .map_err(|_| GitOidParseError)?;
        }
        Ok(Self(bytes))
    }

    fn hex(self) -> String {
        hex(&self.0)
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("GitOid").field(&self.hex()).finish()
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hex())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitOidParseError;

impl fmt::Display for GitOidParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Git object ID must be 40 lowercase hexadecimal characters")
    }
}

impl std::error::Error for GitOidParseError {}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QualifiedRef(String);

impl QualifiedRef {
    pub fn from_bookmark(bookmark: &str) -> Result<Self, QualifiedRefError> {
        if !valid_bookmark(bookmark) {
            return Err(QualifiedRefError);
        }
        Ok(Self(format!("refs/heads/{bookmark}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
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
    pub expected_old_oid: Option<GitOid>,
    pub new_oid: Option<GitOid>,
}

#[derive(Clone)]
pub struct RemoteUrl(String);

impl RemoteUrl {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn expose(&self) -> &str {
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

    pub fn git_executable(&self) -> &Path {
        &self.git_executable
    }

    pub fn mode(&self) -> GitProcessMode {
        self.mode
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
    pub observed_oid: Option<GitOid>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandDiagnostic {
    pub push_command: String,
    pub push_exit_code: Option<i32>,
    pub observation_command: String,
    pub observation_exit_code: Option<i32>,
    pub stderr_excerpt: String,
}

impl fmt::Display for CommandDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}; {}; {}",
            self.push_command, self.observation_command, self.stderr_excerpt
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
        let message = match self.kind {
            PushErrorKind::InvalidInput => "Git push input is invalid",
            PushErrorKind::PushFailed => "Git push failed",
            PushErrorKind::ObservationFailed => "Git remote observation failed",
        };
        write!(formatter, "{message}: {}", self.report.diagnostic)
    }
}

impl std::error::Error for PushError {}

pub fn push(
    sidecar_git_dir: &Path,
    remote_url: &RemoteUrl,
    updates: &BTreeMap<QualifiedRef, LeaseUpdate>,
    environment: &GitProcessEnvironment,
) -> Result<PushReport, PushError> {
    let mut refs = updates
        .keys()
        .cloned()
        .map(|qualified_ref| {
            (
                qualified_ref,
                PushRefReport {
                    status: PushRefStatus::NotReported,
                    observed_oid: None,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    let push_spec = push_command(sidecar_git_dir, remote_url, updates, environment);
    let observation_spec = observation_command(sidecar_git_dir, remote_url, updates, environment);
    if updates.is_empty() {
        return Err(PushError {
            kind: PushErrorKind::InvalidInput,
            report: PushReport {
                refs,
                diagnostic: CommandDiagnostic {
                    push_command: push_spec.safe_shape,
                    push_exit_code: None,
                    observation_command: observation_spec.safe_shape,
                    observation_exit_code: None,
                    stderr_excerpt: "no refs were requested".to_owned(),
                },
            },
        });
    }
    let push_result = run(&push_spec);
    if let Ok(output) = &push_result {
        parse_push_porcelain(&output.stdout, &mut refs);
    }

    let observation_result = run(&observation_spec);
    let observation_parse_error = match &observation_result {
        Ok(output) if output.status.success() => parse_observation(&output.stdout, &mut refs).err(),
        _ => None,
    };

    let diagnostic = diagnostic(
        &push_spec,
        push_result.as_ref().ok(),
        push_result.as_ref().err(),
        &observation_spec,
        observation_result.as_ref().ok(),
        observation_result.as_ref().err(),
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

struct CommandSpec {
    program: PathBuf,
    args: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    safe_shape: String,
}

fn push_command(
    sidecar_git_dir: &Path,
    remote_url: &RemoteUrl,
    updates: &BTreeMap<QualifiedRef, LeaseUpdate>,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let mut args = vec![
        OsString::from(format!("--git-dir={}", sidecar_git_dir.display())),
        OsString::from("push"),
        OsString::from("--porcelain"),
        OsString::from("--no-verify"),
        OsString::from("--atomic"),
    ];
    let mut safe_args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for (qualified_ref, update) in updates {
        let expected = update
            .expected_old_oid
            .map_or_else(String::new, GitOid::hex);
        let lease = format!("--force-with-lease={qualified_ref}:{expected}");
        args.push(OsString::from(&lease));
        safe_args.push(lease);
    }
    args.push(OsString::from("--"));
    args.push(OsString::from(remote_url.expose()));
    safe_args.push("--".to_owned());
    safe_args.push("<remote>".to_owned());
    for (qualified_ref, update) in updates {
        let refspec = update.new_oid.map_or_else(
            || format!(":{qualified_ref}"),
            |oid| format!("{}:{qualified_ref}", oid.hex()),
        );
        args.push(OsString::from(&refspec));
        safe_args.push(refspec);
    }
    CommandSpec {
        program: environment.git_executable.clone(),
        args,
        environment: command_environment(environment),
        safe_shape: safe_command_shape(&environment.git_executable, &safe_args),
    }
}

fn observation_command(
    sidecar_git_dir: &Path,
    remote_url: &RemoteUrl,
    updates: &BTreeMap<QualifiedRef, LeaseUpdate>,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let mut args = vec![
        OsString::from(format!("--git-dir={}", sidecar_git_dir.display())),
        OsString::from("ls-remote"),
        OsString::from("--refs"),
        OsString::from("--"),
        OsString::from(remote_url.expose()),
    ];
    let mut safe_args = vec![
        format!("--git-dir={}", sidecar_git_dir.display()),
        "ls-remote".to_owned(),
        "--refs".to_owned(),
        "--".to_owned(),
        "<remote>".to_owned(),
    ];
    for qualified_ref in updates.keys() {
        args.push(OsString::from(qualified_ref.as_str()));
        safe_args.push(qualified_ref.to_string());
    }
    CommandSpec {
        program: environment.git_executable.clone(),
        args,
        environment: command_environment(environment),
        safe_shape: safe_command_shape(&environment.git_executable, &safe_args),
    }
}

fn command_environment(environment: &GitProcessEnvironment) -> BTreeMap<OsString, OsString> {
    let mut values = environment.extra.clone();
    values.insert(OsString::from("LC_ALL"), OsString::from("C"));
    if environment.mode == GitProcessMode::Background {
        values.insert(OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0"));
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
        let Some(summary) = fields.get(1) else {
            continue;
        };
        let refspec = String::from_utf8_lossy(fields[0]);
        let destination = refspec
            .rsplit_once(':')
            .map_or(refspec.as_ref(), |(_, value)| value);
        let Ok(qualified_ref) = QualifiedRef::from_qualified(destination) else {
            continue;
        };
        let Some(report) = refs.get_mut(&qualified_ref) else {
            continue;
        };
        report.status = status_from_porcelain(line[0], summary);
    }
}

impl QualifiedRef {
    fn from_qualified(value: &str) -> Result<Self, QualifiedRefError> {
        let Some(bookmark) = value.strip_prefix("refs/heads/") else {
            return Err(QualifiedRefError);
        };
        Self::from_bookmark(bookmark)
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
    let mut observed = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Some(tab) = line.iter().position(|byte| *byte == b'\t') else {
            return Err("Git returned a malformed remote observation".to_owned());
        };
        let oid = std::str::from_utf8(&line[..tab])
            .ok()
            .and_then(|value| GitOid::from_hex(value).ok())
            .ok_or_else(|| "Git returned an invalid observed object ID".to_owned())?;
        let name = std::str::from_utf8(&line[tab + 1..])
            .map_err(|_| "Git returned a non-UTF-8 observed ref".to_owned())?;
        let qualified_ref = QualifiedRef::from_qualified(name)
            .map_err(|_| "Git returned an invalid observed ref".to_owned())?;
        if refs.contains_key(&qualified_ref) {
            observed.insert(qualified_ref, oid);
        }
    }
    for (qualified_ref, report) in refs {
        report.observed_oid = observed.get(qualified_ref).copied();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn diagnostic(
    push_spec: &CommandSpec,
    push_output: Option<&Output>,
    push_error: Option<&String>,
    observation_spec: &CommandSpec,
    observation_output: Option<&Output>,
    observation_error: Option<&String>,
    observation_parse_error: Option<&str>,
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> CommandDiagnostic {
    let mut stderr = Vec::new();
    if let Some(output) = push_output {
        stderr.extend_from_slice(&output.stderr);
    }
    if let Some(error) = push_error {
        stderr.extend_from_slice(error.as_bytes());
    }
    if let Some(output) = observation_output {
        stderr.extend_from_slice(&output.stderr);
    }
    if let Some(error) = observation_error {
        stderr.extend_from_slice(error.as_bytes());
    }
    if let Some(error) = observation_parse_error {
        stderr.extend_from_slice(error.as_bytes());
    }
    CommandDiagnostic {
        push_command: push_spec.safe_shape.clone(),
        push_exit_code: push_output.and_then(|output| output.status.code()),
        observation_command: observation_spec.safe_shape.clone(),
        observation_exit_code: observation_output.and_then(|output| output.status.code()),
        stderr_excerpt: redact_stderr(&stderr, remote_url, environment),
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
        if sensitive
            .iter()
            .any(|sensitive_value| line.contains(sensitive_value))
        {
            continue;
        }
        let separator = usize::from(!redacted.is_empty());
        if redacted.len() + separator + line.len() > MAX_DIAGNOSTIC_BYTES {
            const MARKER: &str = "<truncated>";
            let available = MAX_DIAGNOSTIC_BYTES.saturating_sub(MARKER.len() + separator);
            while redacted.len() > available {
                redacted.pop();
            }
            if !redacted.is_empty() {
                redacted.push('\n');
            }
            redacted.push_str(MARKER);
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
    let mut shape = shell_word(&program.display().to_string());
    for arg in args {
        shape.push(' ');
        shape.push_str(&shell_word(arg));
    }
    shape
}

fn shell_word(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=[]".contains(&byte))
    {
        return value.to_owned();
    }
    format!("{:?}", value)
}

fn valid_bookmark(bookmark: &str) -> bool {
    if bookmark.is_empty()
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
        })
    {
        return false;
    }
    bookmark
        .split('/')
        .all(|component| !component.starts_with('.') && !component.ends_with(".lock"))
}

fn resolve_git_executable() -> PathBuf {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("git"))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_status(line: &[u8]) -> PushRefStatus {
        let qualified_ref = QualifiedRef::from_bookmark("main").unwrap();
        let mut refs = BTreeMap::from([(
            qualified_ref.clone(),
            PushRefReport {
                status: PushRefStatus::NotReported,
                observed_oid: None,
            },
        )]);
        parse_push_porcelain(line, &mut refs);
        refs[&qualified_ref].status
    }

    // Byte fixtures captured from git version 2.54.0.
    #[test]
    fn parses_fixed_push_porcelain_lines() {
        assert_eq!(
            parsed_status(b"*\trefs/heads/main:refs/heads/main\t[new branch]\n"),
            PushRefStatus::Updated
        );
        assert_eq!(
            parsed_status(b" \trefs/heads/main:refs/heads/main\t4bdc3e7..f30ab12\n"),
            PushRefStatus::Updated
        );
        assert_eq!(
            parsed_status(b"-\t:refs/heads/main\t[deleted]\n"),
            PushRefStatus::Deleted
        );
        assert_eq!(
            parsed_status(b"=\trefs/heads/main:refs/heads/main\t[up to date]\n"),
            PushRefStatus::UpToDate
        );
        assert_eq!(
            parsed_status(b"!\trefs/heads/main:refs/heads/main\t[rejected] (stale info)\n"),
            PushRefStatus::LeaseRejected
        );
        assert_eq!(
            parsed_status(b"!\trefs/heads/main:refs/heads/main\t[rejected] (non-fast-forward)\n"),
            PushRefStatus::RemoteRejected
        );
        assert_eq!(
            parsed_status(b"?\trefs/heads/main:refs/heads/main\t[unknown]\n"),
            PushRefStatus::OtherRejected
        );
    }

    #[test]
    fn background_commands_disable_terminal_prompts() {
        let environment =
            GitProcessEnvironment::new("git", GitProcessMode::Background).with_extra_environment(
                BTreeMap::from([(OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("1"))]),
            );
        let updates = BTreeMap::from([(
            QualifiedRef::from_bookmark("main").unwrap(),
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: None,
            },
        )]);
        let spec = push_command(
            Path::new("sidecar.git"),
            &RemoteUrl::new("remote.git"),
            &updates,
            &environment,
        );
        assert_eq!(
            spec.environment
                .get(std::ffi::OsStr::new("GIT_TERMINAL_PROMPT")),
            Some(&OsString::from("0"))
        );
        assert_eq!(
            spec.environment.get(std::ffi::OsStr::new("LC_ALL")),
            Some(&OsString::from("C"))
        );

        let foreground = GitProcessEnvironment::new("git", GitProcessMode::Foreground);
        let spec = push_command(
            Path::new("sidecar.git"),
            &RemoteUrl::new("remote.git"),
            &updates,
            &foreground,
        );
        assert!(
            !spec
                .environment
                .contains_key(std::ffi::OsStr::new("GIT_TERMINAL_PROMPT"))
        );
    }

    #[test]
    fn bookmark_qualification_matches_git_ref_rules() {
        for valid in ["main", "feature/nested", "unicode-ä"] {
            assert!(QualifiedRef::from_bookmark(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "", "-main", ".hidden", "a..b", "a//b", "a.lock", "a@{b", "a:b", "a?b",
        ] {
            assert!(QualifiedRef::from_bookmark(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn command_uses_empty_creation_lease_and_unforced_refspec() {
        let qualified_ref = QualifiedRef::from_bookmark("main").unwrap();
        let updates = BTreeMap::from([(
            qualified_ref,
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: Some(GitOid([0x12; 20])),
            },
        )]);
        let spec = push_command(
            Path::new("sidecar.git"),
            &RemoteUrl::new("remote.git"),
            &updates,
            &GitProcessEnvironment::new("git", GitProcessMode::Foreground),
        );
        let arguments = spec
            .args
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--force-with-lease=refs/heads/main:")
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument == &format!("{}:refs/heads/main", "12".repeat(20)))
        );
    }
}
