use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use devspace_machine::{
    GitOid, GitProcessEnvironment, GitProcessMode, LeaseUpdate, PushErrorKind, PushRefStatus,
    QualifiedRef, RemoteUrl, ls_remote_head, push,
};
use tempfile::TempDir;

#[test]
fn atomic_create_observes_both_refs() {
    let fixture = Fixture::new();
    let first = qualified("first");
    let second = qualified("second");
    let updates = BTreeMap::from([
        (
            first.clone(),
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: Some(fixture.commits[0]),
            },
        ),
        (
            second.clone(),
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: Some(fixture.commits[1]),
            },
        ),
    ]);

    let report = fixture.push(&updates).unwrap();

    assert_eq!(report.refs[&first].status, PushRefStatus::Updated);
    assert_eq!(report.refs[&first].observed_oid, Some(fixture.commits[0]));
    assert_eq!(report.refs[&second].status, PushRefStatus::Updated);
    assert_eq!(report.refs[&second].observed_oid, Some(fixture.commits[1]));
    assert_eq!(fixture.remote_ref(&first), Some(fixture.commits[0]));
    assert_eq!(fixture.remote_ref(&second), Some(fixture.commits[1]));
}

#[test]
fn stale_lease_rejects_the_whole_batch_atomically() {
    let fixture = Fixture::new();
    let stale = qualified("stale");
    let other = qualified("other");
    fixture.seed(&[(&stale, fixture.commits[0]), (&other, fixture.commits[0])]);
    fixture.seed(&[(&stale, fixture.commits[2])]);
    let updates = BTreeMap::from([
        (
            stale.clone(),
            LeaseUpdate {
                expected_old_oid: Some(fixture.commits[0]),
                new_oid: Some(fixture.commits[1]),
            },
        ),
        (
            other.clone(),
            LeaseUpdate {
                expected_old_oid: Some(fixture.commits[0]),
                new_oid: Some(fixture.commits[1]),
            },
        ),
    ]);

    let error = fixture.push(&updates).unwrap_err();

    assert_eq!(error.kind, PushErrorKind::PushFailed);
    assert_eq!(
        error.report.refs[&stale].status,
        PushRefStatus::LeaseRejected
    );
    assert_eq!(
        error.report.refs[&stale].observed_oid,
        Some(fixture.commits[2])
    );
    assert_eq!(fixture.remote_ref(&stale), Some(fixture.commits[2]));
    assert_eq!(fixture.remote_ref(&other), Some(fixture.commits[0]));
    assert_eq!(
        error.report.refs[&other].observed_oid,
        Some(fixture.commits[0])
    );
}

#[test]
fn updates_and_deletes_in_one_atomic_batch() {
    let fixture = Fixture::new();
    let updated = qualified("updated");
    let deleted = qualified("deleted");
    fixture.seed(&[
        (&updated, fixture.commits[0]),
        (&deleted, fixture.commits[0]),
    ]);
    let updates = BTreeMap::from([
        (
            updated.clone(),
            LeaseUpdate {
                expected_old_oid: Some(fixture.commits[0]),
                new_oid: Some(fixture.commits[1]),
            },
        ),
        (
            deleted.clone(),
            LeaseUpdate {
                expected_old_oid: Some(fixture.commits[0]),
                new_oid: None,
            },
        ),
    ]);

    let report = fixture.push(&updates).unwrap();

    assert_eq!(report.refs[&updated].status, PushRefStatus::Updated);
    assert_eq!(report.refs[&updated].observed_oid, Some(fixture.commits[1]));
    assert_eq!(report.refs[&deleted].status, PushRefStatus::Deleted);
    assert_eq!(report.refs[&deleted].observed_oid, None);
    assert_eq!(fixture.remote_ref(&updated), Some(fixture.commits[1]));
    assert_eq!(fixture.remote_ref(&deleted), None);
}

#[test]
fn transport_failure_is_typed_complete_and_structurally_redacted() {
    let fixture = Fixture::new();
    let first = qualified("first");
    let second = qualified("second");
    let updates = BTreeMap::from([
        (
            first.clone(),
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: Some(fixture.commits[0]),
            },
        ),
        (
            second.clone(),
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: Some(fixture.commits[1]),
            },
        ),
    ]);
    let sentinel = "sentinel-secret-remote";
    let remote_path = fixture.root.path().join(format!("{sentinel}.git"));
    let remote = RemoteUrl::new(remote_path.to_string_lossy());
    let environment = GitProcessEnvironment::default().with_extra_environment(BTreeMap::from([(
        OsString::from("DEVSPACE_TEST_SECRET"),
        OsString::from("sentinel-environment-secret"),
    )]));

    let error = push(&fixture.sidecar, &remote, &updates, &environment).unwrap_err();

    assert_eq!(error.kind, PushErrorKind::ObservationFailed);
    assert_eq!(error.report.refs[&first].status, PushRefStatus::NotReported);
    assert_eq!(
        error.report.refs[&second].status,
        PushRefStatus::NotReported
    );
    let rendered = format!("{error:?}\n{error}\n{remote:?}\n{environment:?}");
    assert!(!rendered.contains(sentinel), "{rendered}");
    assert!(
        !rendered.contains("sentinel-environment-secret"),
        "{rendered}"
    );
    assert!(rendered.contains("<remote>"), "{rendered}");
}

#[test]
fn remote_head_observation_reports_the_symbolic_branch_and_empty_remote() {
    let fixture = Fixture::new();
    let remote_url = RemoteUrl::new(fixture.remote.to_string_lossy());
    assert_eq!(
        ls_remote_head(&remote_url, &fixture.environment).unwrap(),
        None
    );

    let main = qualified("main");
    fixture.seed(&[(&main, fixture.commits[0])]);
    run_git(
        &fixture.environment,
        [
            "--git-dir",
            path(&fixture.remote),
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ],
    );
    let head = ls_remote_head(&remote_url, &fixture.environment)
        .unwrap()
        .unwrap();
    assert_eq!(head.branch, "main");
    assert_eq!(head.oid, fixture.commits[0]);
}

struct Fixture {
    root: TempDir,
    sidecar: PathBuf,
    remote: PathBuf,
    commits: [GitOid; 3],
    environment: GitProcessEnvironment,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let sidecar = root.path().join("sidecar.git");
        let remote = root.path().join("remote.git");
        let environment = GitProcessEnvironment::new(resolved_git(), GitProcessMode::Background);

        run_git(&environment, ["init", "-q", path(&source)]);
        run_git(
            &environment,
            ["-C", path(&source), "config", "user.name", "Devspace test"],
        );
        run_git(
            &environment,
            [
                "-C",
                path(&source),
                "config",
                "user.email",
                "devspace@example.invalid",
            ],
        );
        let mut commits = [GitOid([0; 20]); 3];
        for (index, oid) in commits.iter_mut().enumerate() {
            fs::write(source.join("content.txt"), format!("commit {index}\n")).unwrap();
            run_git(&environment, ["-C", path(&source), "add", "content.txt"]);
            run_git(
                &environment,
                ["-C", path(&source), "commit", "-q", "-m", "fixture"],
            );
            *oid = parse_oid(&run_git(
                &environment,
                ["-C", path(&source), "rev-parse", "HEAD"],
            ));
        }
        run_git(
            &environment,
            ["clone", "-q", "--bare", path(&source), path(&sidecar)],
        );
        run_git(&environment, ["init", "-q", "--bare", path(&remote)]);

        Self {
            root,
            sidecar,
            remote,
            commits,
            environment,
        }
    }

    fn push(
        &self,
        updates: &BTreeMap<QualifiedRef, LeaseUpdate>,
    ) -> Result<devspace_machine::PushReport, devspace_machine::PushError> {
        push(
            &self.sidecar,
            &RemoteUrl::new(self.remote.to_string_lossy()),
            updates,
            &self.environment,
        )
    }

    fn seed(&self, refs: &[(&QualifiedRef, GitOid)]) {
        let mut arguments = vec![
            format!("--git-dir={}", self.sidecar.display()),
            "push".to_owned(),
            self.remote.display().to_string(),
        ];
        arguments.extend(
            refs.iter()
                .map(|(qualified_ref, oid)| format!("{oid}:{qualified_ref}")),
        );
        run_git_owned(&self.environment, arguments);
    }

    fn remote_ref(&self, qualified_ref: &QualifiedRef) -> Option<GitOid> {
        let output = git_output(
            &self.environment,
            [
                format!("--git-dir={}", self.remote.display()),
                "rev-parse".to_owned(),
                "--verify".to_owned(),
                qualified_ref.to_string(),
            ],
        );
        output.status.success().then(|| parse_oid(&output.stdout))
    }
}

fn qualified(bookmark: &str) -> QualifiedRef {
    QualifiedRef::from_bookmark(bookmark).unwrap()
}

fn resolved_git() -> PathBuf {
    GitProcessEnvironment::default().git_executable().to_owned()
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn run_git<const N: usize>(environment: &GitProcessEnvironment, arguments: [&str; N]) -> Vec<u8> {
    let output = Command::new(environment.git_executable())
        .args(arguments)
        .output()
        .unwrap();
    require_success(output)
}

fn run_git_owned(environment: &GitProcessEnvironment, arguments: Vec<String>) -> Vec<u8> {
    let output = Command::new(environment.git_executable())
        .args(arguments)
        .output()
        .unwrap();
    require_success(output)
}

fn git_output<const N: usize>(
    environment: &GitProcessEnvironment,
    arguments: [String; N],
) -> Output {
    Command::new(environment.git_executable())
        .args(arguments)
        .output()
        .unwrap()
}

fn require_success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

fn parse_oid(bytes: &[u8]) -> GitOid {
    GitOid::from_hex(String::from_utf8_lossy(bytes).trim()).unwrap()
}
