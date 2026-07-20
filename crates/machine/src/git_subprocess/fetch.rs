use super::*;

const MAX_OBSERVATION_BYTES: usize = 1024 * 1024;
const MAX_REMOTE_HEADS: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchDiagnostic {
    pub fetch_command: String,
    pub fetch_exit_code: Option<i32>,
    pub observation_command: String,
    pub observation_exit_code: Option<i32>,
    pub stderr_excerpt: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchReport {
    pub heads: BTreeMap<String, GitOid>,
    pub diagnostic: FetchDiagnostic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FetchErrorKind {
    InvalidInput,
    FetchFailed,
    ObservationFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchError {
    pub kind: FetchErrorKind,
    pub report: FetchReport,
}

impl fmt::Display for FetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            FetchErrorKind::InvalidInput => "Git fetch input is invalid",
            FetchErrorKind::FetchFailed => "Git fetch failed",
            FetchErrorKind::ObservationFailed => "Git fetch observation failed",
        };
        write!(
            formatter,
            "{message}: {}; {}; {}",
            self.report.diagnostic.fetch_command,
            self.report.diagnostic.observation_command,
            self.report.diagnostic.stderr_excerpt
        )
    }
}

impl std::error::Error for FetchError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteHeadsError {
    pub command: String,
    pub exit_code: Option<i32>,
    pub stderr_excerpt: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteHead {
    pub branch: String,
    pub oid: GitOid,
}

impl fmt::Display for RemoteHeadsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Git remote observation failed: {}; {}",
            self.command, self.stderr_excerpt
        )
    }
}

impl std::error::Error for RemoteHeadsError {}

pub fn fetch(
    sidecar_git_dir: &Path,
    remote_name: &str,
    bookmarks: &[String],
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> Result<FetchReport, FetchError> {
    let requested = match observation_refs(remote_name, bookmarks) {
        Ok(requested) if !requested.is_empty() => requested,
        Ok(_) => {
            return Err(empty_fetch_error(
                sidecar_git_dir,
                remote_name,
                bookmarks,
                remote_url,
                environment,
                "no refs were requested",
            ));
        }
        Err(error) => {
            return Err(empty_fetch_error(
                sidecar_git_dir,
                remote_name,
                bookmarks,
                remote_url,
                environment,
                &error,
            ));
        }
    };
    let fetch_spec = fetch_command(
        sidecar_git_dir,
        remote_name,
        bookmarks,
        remote_url,
        environment,
    );
    let observation_spec = fetched_observation_command(sidecar_git_dir, &requested, environment);
    let fetch_result = run(&fetch_spec);
    let observation_result = fetch_result
        .as_ref()
        .ok()
        .filter(|output| output.status.success())
        .map(|_| run(&observation_spec));
    let (heads, observation_error) = match observation_result.as_ref() {
        Some(Ok(output)) if output.status.success() => {
            match parse_fetched_observation(&output.stdout, &requested) {
                Ok(heads) => (heads, None),
                Err(error) => (BTreeMap::new(), Some(error)),
            }
        }
        _ => (BTreeMap::new(), None),
    };
    let diagnostic = fetch_diagnostic(
        &fetch_spec,
        fetch_result.as_ref().ok(),
        fetch_result.as_ref().err(),
        &observation_spec,
        observation_result
            .as_ref()
            .and_then(|result| result.as_ref().ok()),
        observation_result
            .as_ref()
            .and_then(|result| result.as_ref().err()),
        observation_error.as_deref(),
        remote_url,
        environment,
    );
    let report = FetchReport { heads, diagnostic };
    if fetch_result
        .as_ref()
        .map_or(true, |output| !output.status.success())
    {
        return Err(FetchError {
            kind: FetchErrorKind::FetchFailed,
            report,
        });
    }
    if observation_result.as_ref().is_none_or(|result| {
        result
            .as_ref()
            .map_or(true, |output| !output.status.success())
    }) || observation_error.is_some()
    {
        return Err(FetchError {
            kind: FetchErrorKind::ObservationFailed,
            report,
        });
    }
    Ok(report)
}

pub fn ls_remote_heads(
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> Result<BTreeMap<String, GitOid>, RemoteHeadsError> {
    let spec = ls_remote_heads_command(remote_url, environment);
    let result = run(&spec);
    let (heads, parse_error) = match result.as_ref() {
        Ok(output) if output.status.success() => match parse_remote_heads(&output.stdout) {
            Ok(heads) => (heads, None),
            Err(error) => (BTreeMap::new(), Some(error)),
        },
        _ => (BTreeMap::new(), None),
    };
    if result.as_ref().is_ok_and(|output| output.status.success()) && parse_error.is_none() {
        return Ok(heads);
    }
    let mut stderr = Vec::new();
    if let Ok(output) = &result {
        stderr.extend_from_slice(&output.stderr);
    }
    if let Err(error) = &result {
        stderr.extend_from_slice(error.as_bytes());
    }
    if let Some(error) = parse_error {
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
    let spec = ls_remote_head_command(remote_url, environment);
    let result = run(&spec);
    let (head, parse_error) = match result.as_ref() {
        Ok(output) if output.status.success() => match parse_remote_head(&output.stdout) {
            Ok(head) => (head, None),
            Err(error) => (None, Some(error)),
        },
        _ => (None, None),
    };
    if result.as_ref().is_ok_and(|output| output.status.success()) && parse_error.is_none() {
        return Ok(head);
    }
    let mut stderr = Vec::new();
    if let Ok(output) = &result {
        stderr.extend_from_slice(&output.stderr);
    }
    if let Err(error) = &result {
        stderr.extend_from_slice(error.as_bytes());
    }
    if let Some(error) = parse_error {
        stderr.extend_from_slice(error.as_bytes());
    }
    Err(RemoteHeadsError {
        command: spec.safe_shape,
        exit_code: result.as_ref().ok().and_then(|output| output.status.code()),
        stderr_excerpt: redact_stderr(&stderr, remote_url, environment),
    })
}

fn empty_fetch_error(
    sidecar_git_dir: &Path,
    remote_name: &str,
    bookmarks: &[String],
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
    message: &str,
) -> FetchError {
    let fetch_spec = fetch_command(
        sidecar_git_dir,
        remote_name,
        bookmarks,
        remote_url,
        environment,
    );
    let observation_spec =
        fetched_observation_command(sidecar_git_dir, &BTreeMap::new(), environment);
    FetchError {
        kind: FetchErrorKind::InvalidInput,
        report: FetchReport {
            heads: BTreeMap::new(),
            diagnostic: FetchDiagnostic {
                fetch_command: fetch_spec.safe_shape,
                fetch_exit_code: None,
                observation_command: observation_spec.safe_shape,
                observation_exit_code: None,
                stderr_excerpt: message.to_owned(),
            },
        },
    }
}

fn fetch_command(
    sidecar_git_dir: &Path,
    remote_name: &str,
    bookmarks: &[String],
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let mut args = vec![
        OsString::from(format!("--git-dir={}", sidecar_git_dir.display())),
        OsString::from("fetch"),
        OsString::from("--"),
        OsString::from(remote_url.expose()),
    ];
    let mut safe_args = vec![
        format!("--git-dir={}", sidecar_git_dir.display()),
        "fetch".to_owned(),
        "--".to_owned(),
        "<remote>".to_owned(),
    ];
    for bookmark in bookmarks {
        let refspec =
            format!("+refs/heads/{bookmark}:refs/devspace/remotes/{remote_name}/{bookmark}");
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

fn fetched_observation_command(
    sidecar_git_dir: &Path,
    requested: &BTreeMap<String, String>,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let mut args = vec![
        OsString::from(format!("--git-dir={}", sidecar_git_dir.display())),
        OsString::from("show-ref"),
        OsString::from("--verify"),
    ];
    let mut safe_args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for observation_ref in requested.values() {
        args.push(OsString::from(observation_ref));
        safe_args.push(observation_ref.clone());
    }
    CommandSpec {
        program: environment.git_executable.clone(),
        args,
        environment: command_environment(environment),
        safe_shape: safe_command_shape(&environment.git_executable, &safe_args),
    }
}

fn ls_remote_heads_command(
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let args = vec![
        OsString::from("ls-remote"),
        OsString::from("--refs"),
        OsString::from("--"),
        OsString::from(remote_url.expose()),
        OsString::from("refs/heads/*"),
    ];
    let safe_args = vec![
        "ls-remote".to_owned(),
        "--refs".to_owned(),
        "--".to_owned(),
        "<remote>".to_owned(),
        "refs/heads/*".to_owned(),
    ];
    CommandSpec {
        program: environment.git_executable.clone(),
        args,
        environment: command_environment(environment),
        safe_shape: safe_command_shape(&environment.git_executable, &safe_args),
    }
}

fn ls_remote_head_command(
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> CommandSpec {
    let args = vec![
        OsString::from("ls-remote"),
        OsString::from("--symref"),
        OsString::from("--"),
        OsString::from(remote_url.expose()),
        OsString::from("HEAD"),
    ];
    let safe_args = vec![
        "ls-remote".to_owned(),
        "--symref".to_owned(),
        "--".to_owned(),
        "<remote>".to_owned(),
        "HEAD".to_owned(),
    ];
    CommandSpec {
        program: environment.git_executable.clone(),
        args,
        environment: command_environment(environment),
        safe_shape: safe_command_shape(&environment.git_executable, &safe_args),
    }
}

fn observation_refs(
    remote_name: &str,
    bookmarks: &[String],
) -> Result<BTreeMap<String, String>, String> {
    if !valid_bookmark(remote_name) {
        return Err("remote name cannot be represented in a Git observation ref".to_owned());
    }
    if bookmarks.len() > MAX_REMOTE_HEADS {
        return Err(format!(
            "fetch requested {} heads, exceeding the {MAX_REMOTE_HEADS}-head limit",
            bookmarks.len()
        ));
    }
    let mut requested = BTreeMap::new();
    for bookmark in bookmarks {
        QualifiedRef::from_bookmark(bookmark).map_err(|error| error.to_string())?;
        if requested
            .insert(
                bookmark.clone(),
                format!("refs/devspace/remotes/{remote_name}/{bookmark}"),
            )
            .is_some()
        {
            return Err(format!(
                "bookmark `{bookmark}` was requested more than once"
            ));
        }
    }
    Ok(requested)
}

fn parse_fetched_observation(
    bytes: &[u8],
    requested: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, GitOid>, String> {
    let by_ref = parse_oid_refs(bytes)?;
    let mut heads = BTreeMap::new();
    for (bookmark, observation_ref) in requested {
        let oid = by_ref
            .get(observation_ref)
            .copied()
            .ok_or_else(|| format!("Git did not retain observation ref {observation_ref}"))?;
        heads.insert(bookmark.clone(), oid);
    }
    Ok(heads)
}

fn parse_remote_heads(bytes: &[u8]) -> Result<BTreeMap<String, GitOid>, String> {
    let by_ref = parse_oid_refs(bytes)?;
    if by_ref.len() > MAX_REMOTE_HEADS {
        return Err(format!(
            "remote advertised {} heads, exceeding the {MAX_REMOTE_HEADS}-head limit",
            by_ref.len()
        ));
    }
    by_ref
        .into_iter()
        .map(|(name, oid)| {
            let bookmark = name
                .strip_prefix("refs/heads/")
                .ok_or_else(|| "Git returned a non-head remote ref".to_owned())?;
            QualifiedRef::from_bookmark(bookmark).map_err(|error| error.to_string())?;
            Ok((bookmark.to_owned(), oid))
        })
        .collect()
}

fn parse_remote_head(bytes: &[u8]) -> Result<Option<RemoteHead>, String> {
    if bytes.len() > MAX_OBSERVATION_BYTES {
        return Err(format!(
            "Git observation exceeded the {MAX_OBSERVATION_BYTES}-byte limit"
        ));
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
            QualifiedRef::from_bookmark(name).map_err(|error| error.to_string())?;
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
            GitOid::from_hex(value)
                .map_err(|_| "Git returned an invalid HEAD object ID".to_owned())?,
        );
    }
    match (branch, oid) {
        (Some(branch), Some(oid)) => Ok(Some(RemoteHead { branch, oid })),
        (None, None) => Ok(None),
        (None, Some(_)) => Err("Git remote HEAD is detached; it must point to a branch".to_owned()),
        (Some(_), None) => Err("Git remote HEAD did not advertise an object ID".to_owned()),
    }
}

fn parse_oid_refs(bytes: &[u8]) -> Result<BTreeMap<String, GitOid>, String> {
    if bytes.len() > MAX_OBSERVATION_BYTES {
        return Err(format!(
            "Git observation exceeded the {MAX_OBSERVATION_BYTES}-byte limit"
        ));
    }
    let mut refs = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Some(separator) = line.iter().position(|byte| byte.is_ascii_whitespace()) else {
            return Err("Git returned a malformed ref observation".to_owned());
        };
        let oid = std::str::from_utf8(&line[..separator])
            .ok()
            .and_then(|value| GitOid::from_hex(value).ok())
            .ok_or_else(|| "Git returned an invalid observed object ID".to_owned())?;
        let name = std::str::from_utf8(&line[separator..])
            .map_err(|_| "Git returned a non-UTF-8 observed ref".to_owned())?
            .trim_start();
        if name.is_empty() {
            return Err("Git returned a malformed ref observation".to_owned());
        }
        if let Some(existing) = refs.insert(name.to_owned(), oid)
            && existing != oid
        {
            return Err(format!("Git returned conflicting values for {name}"));
        }
    }
    Ok(refs)
}

#[allow(clippy::too_many_arguments)]
fn fetch_diagnostic(
    fetch_spec: &CommandSpec,
    fetch_output: Option<&Output>,
    fetch_error: Option<&String>,
    observation_spec: &CommandSpec,
    observation_output: Option<&Output>,
    observation_error: Option<&String>,
    observation_parse_error: Option<&str>,
    remote_url: &RemoteUrl,
    environment: &GitProcessEnvironment,
) -> FetchDiagnostic {
    let mut stderr = Vec::new();
    if let Some(output) = fetch_output {
        stderr.extend_from_slice(&output.stderr);
    }
    if let Some(error) = fetch_error {
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
    FetchDiagnostic {
        fetch_command: fetch_spec.safe_shape.clone(),
        fetch_exit_code: fetch_output.and_then(|output| output.status.code()),
        observation_command: observation_spec.safe_shape.clone(),
        observation_exit_code: observation_output.and_then(|output| output.status.code()),
        stderr_excerpt: redact_stderr(&stderr, remote_url, environment),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_forced_observation_refspecs_and_redacts_the_url() {
        let bookmarks = vec!["main".to_owned(), "release".to_owned()];
        let secret_url = RemoteUrl::new("https://user:secret@example.invalid/repo.git");
        let spec = fetch_command(
            Path::new("sidecar.git"),
            "origin",
            &bookmarks,
            &secret_url,
            &GitProcessEnvironment::new("git", GitProcessMode::Foreground),
        );
        let arguments = spec
            .args
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments.iter().filter(|value| *value == "fetch").count(),
            1
        );
        assert!(
            arguments
                .iter()
                .any(|value| { value == "+refs/heads/main:refs/devspace/remotes/origin/main" })
        );
        assert!(
            arguments.iter().any(|value| {
                value == "+refs/heads/release:refs/devspace/remotes/origin/release"
            })
        );
        assert!(!spec.safe_shape.contains("secret"));
        assert!(spec.safe_shape.contains("<remote>"));
        assert_eq!(
            spec.environment.get(std::ffi::OsStr::new("LC_ALL")),
            Some(&OsString::from("C"))
        );
    }

    #[test]
    fn parses_only_bounded_remote_heads() {
        let bytes = format!(
            "{}\trefs/heads/main\n{} refs/heads/topic/nested\r\n",
            "11".repeat(20),
            "22".repeat(20)
        );
        let heads = parse_remote_heads(bytes.as_bytes()).unwrap();
        assert_eq!(heads["main"], GitOid([0x11; 20]));
        assert_eq!(heads["topic/nested"], GitOid([0x22; 20]));
        assert!(parse_remote_heads(&vec![b'x'; MAX_OBSERVATION_BYTES + 1]).is_err());
    }

    #[test]
    fn parses_symbolic_remote_head_and_rejects_sha256() {
        let bytes = format!("ref: refs/heads/main\tHEAD\n{}\tHEAD\n", "11".repeat(20));
        assert_eq!(
            parse_remote_head(bytes.as_bytes()).unwrap(),
            Some(RemoteHead {
                branch: "main".to_owned(),
                oid: GitOid([0x11; 20]),
            })
        );
        let sha256 = format!("ref: refs/heads/main\tHEAD\n{}\tHEAD\n", "11".repeat(32));
        assert_eq!(
            parse_remote_head(sha256.as_bytes()).unwrap_err(),
            "SHA-256 Git remotes are not supported"
        );
        assert_eq!(parse_remote_head(b"").unwrap(), None);
    }
}
