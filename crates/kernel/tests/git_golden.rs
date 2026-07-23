use std::io::Write;
use std::process::{Command, Stdio};

use devspace_kernel::{
    ObjectKind, Oid, ReferenceKind, TreeEntryKind, TreeError, parse_commit, parse_tree, validate,
};

#[derive(Clone, Debug)]
struct Vector {
    kind: ObjectKind,
    expected: Oid,
    payload: Vec<u8>,
}

fn vectors() -> Vec<Vector> {
    [
        include_str!("git_golden.txt"),
        include_str!("git_golden_oracle.txt"),
    ]
    .into_iter()
    .flat_map(str::lines)
    .filter(|line| !line.is_empty() && !line.starts_with('#'))
    .map(|line| {
        let mut fields = line.split('|');
        let kind = match fields.next().expect("vector type") {
            "blob" => ObjectKind::Blob,
            "tree" => ObjectKind::Tree,
            "commit" => ObjectKind::Commit,
            other => panic!("unknown vector type {other}"),
        };
        let expected = Oid::from_hex(fields.next().expect("expected id").as_bytes())
            .expect("20-byte expected id");
        let payload = decode_hex(fields.next().expect("payload hex"));
        assert!(fields.next().is_none(), "extra vector field");
        Vector {
            kind,
            expected,
            payload,
        }
    })
    .collect()
}

fn decode_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "odd hex length");
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("ASCII hex");
            u8::from_str_radix(text, 16).expect("hex byte")
        })
        .collect()
}

fn git_id(kind: ObjectKind, payload: &[u8]) -> Oid {
    let kind = match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
    };
    let mut child = Command::new("git")
        .args(["hash-object", "-t", kind, "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run git hash-object");
    child
        .stdin
        .take()
        .expect("git stdin")
        .write_all(payload)
        .expect("write git payload");
    let output = child.wait_with_output().expect("wait for git hash-object");
    assert!(output.status.success(), "git hash-object failed");
    let hex = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    Oid::from_hex(hex).expect("git emitted a SHA-1")
}

#[test]
fn golden_ids_match_git() {
    for vector in vectors() {
        let validated = validate(vector.kind, &vector.payload).expect("valid starter vector");
        assert_eq!(validated.id, vector.expected);
        assert_eq!(git_id(vector.kind, &vector.payload), vector.expected);
    }
}

#[test]
fn starter_vectors_cover_parsers_and_references() {
    let vectors = vectors();
    assert_eq!(
        devspace_kernel::parse_blob(&vectors[0].payload).data,
        vectors[0].payload
    );
    let tree = parse_tree(&vectors[2].payload).expect("all-mode tree");
    assert_eq!(
        tree.entries
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        [
            TreeEntryKind::Tree,
            TreeEntryKind::File,
            TreeEntryKind::Symlink,
            TreeEntryKind::Executable,
            TreeEntryKind::Gitlink,
        ]
    );
    let tree_refs = validate(ObjectKind::Tree, &vectors[2].payload)
        .expect("all-mode tree")
        .references;
    assert!(
        tree_refs
            .iter()
            .any(|item| item.kind == ReferenceKind::Gitlink)
    );

    let simple = parse_commit(&vectors[3].payload).expect("simple commit");
    assert_eq!(simple.encoding, Some(b"ISO-8859-1".as_slice()));
    assert_eq!(simple.message, b"message\xffbytes\n");
    assert!(
        simple
            .headers
            .iter()
            .any(|header| header.name == b"x-vendor")
    );
    assert_eq!(simple.author.tz_offset_minutes, 90);
    assert_eq!(simple.committer.tz_offset_minutes, -420);

    let merge = parse_commit(&vectors[4].payload).expect("merge commit");
    assert_eq!(merge.parents.len(), 2);

    let signed = parse_commit(&vectors[5].payload).expect("signed commit");
    let gpgsig = signed
        .headers
        .iter()
        .find(|header| header.name == b"gpgsig")
        .expect("gpgsig header");
    assert_eq!(gpgsig.value_lines.len(), 3);
    let mergetag = signed
        .headers
        .iter()
        .find(|header| header.name == b"mergetag")
        .expect("mergetag header");
    assert_eq!(mergetag.value_lines.len(), 6);

    let jj = parse_commit(&vectors[6].payload).expect("jj commit");
    assert!(jj.change_id.is_some());
    assert_eq!(jj.jj_trees.len(), 3);
    assert_eq!(jj.conflict_labels, Some(vec!["left", "base", "right"]));
    let jj_refs = validate(ObjectKind::Commit, &vectors[6].payload)
        .expect("jj commit")
        .references;
    assert_eq!(
        jj_refs
            .iter()
            .filter(|item| item.kind == ReferenceKind::Tree)
            .count(),
        4
    );
    assert_eq!(
        jj_refs
            .iter()
            .filter(|item| item.kind == ReferenceKind::Commit)
            .count(),
        1
    );
}

#[test]
fn every_structured_golden_mutation_returns_without_panicking() {
    for vector in vectors()
        .into_iter()
        .filter(|vector| vector.kind != ObjectKind::Blob)
    {
        for length in 0..vector.payload.len() {
            assert_rejected_or_reidentified(&vector, &vector.payload[..length], "truncation");
        }
        for index in 0..vector.payload.len() {
            let mut mutated = vector.payload.clone();
            mutated[index] ^= 0x80;
            assert_rejected_or_reidentified(&vector, &mutated, "single-byte mutation");
        }
    }
}

#[test]
fn every_blob_golden_mutation_changes_its_id() {
    for vector in vectors()
        .into_iter()
        .filter(|vector| vector.kind == ObjectKind::Blob)
    {
        for length in 0..vector.payload.len() {
            assert_reidentified(&vector, &vector.payload[..length], "truncation");
        }
        for index in 0..vector.payload.len() {
            let mut mutated = vector.payload.clone();
            mutated[index] ^= 0x80;
            assert_reidentified(&vector, &mutated, "single-byte mutation");
        }
    }
}

fn assert_rejected_or_reidentified(vector: &Vector, candidate: &[u8], operation: &str) {
    if let Ok(validated) = validate(vector.kind, candidate) {
        assert_ne!(
            validated.id, vector.expected,
            "{operation} of {:?} was accepted with its original ID",
            vector.kind
        );
    }
}

fn assert_reidentified(vector: &Vector, candidate: &[u8], operation: &str) {
    let validated = validate(vector.kind, candidate)
        .unwrap_or_else(|error| panic!("blob {operation} was rejected: {error}"));
    assert_ne!(
        validated.id, vector.expected,
        "blob {operation} retained its original ID"
    );
}

#[test]
fn invalid_jj_metadata_is_handled_without_panics() {
    let invalid_trees = b"tree 1111111111111111111111111111111111111111\nauthor A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 +0000\njj:trees 2222222222222222222222222222222222222222\n\n";
    assert!(parse_commit(invalid_trees).is_err());

    let invalid_change_id = b"tree 1111111111111111111111111111111111111111\nauthor A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 +0000\nchange-id ordinary-hex-is-not-jj-reverse-hex\n\n";
    let parsed = parse_commit(invalid_change_id).expect("jj tolerates an invalid change-id");
    assert_eq!(parsed.change_id, None);
}

#[test]
fn tree_parser_rejects_noncanonical_order_and_duplicate_names() {
    let first = tree_entry(b"100644", b"a", 0x11);
    let second = tree_entry(b"100644", b"b", 0x22);
    let first_length = first.len();

    let mut swapped = second.clone();
    swapped.extend_from_slice(&first);
    assert_eq!(
        parse_tree(&swapped),
        Err(TreeError::NonCanonicalOrder {
            offset: first_length,
        })
    );

    let mut duplicate = first;
    duplicate.extend_from_slice(&second);
    duplicate.extend_from_slice(&tree_entry(b"100755", b"b", 0x33));
    assert_eq!(
        parse_tree(&duplicate),
        Err(TreeError::DuplicateName {
            offset: first_length * 2,
        })
    );
}

fn tree_entry(mode: &[u8], name: &[u8], oid_byte: u8) -> Vec<u8> {
    let mut entry = mode.to_vec();
    entry.push(b' ');
    entry.extend_from_slice(name);
    entry.push(0);
    entry.extend_from_slice(&[oid_byte; 20]);
    entry
}
