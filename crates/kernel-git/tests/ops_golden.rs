use devspace_kernel_git::ops::{OpObjectKind, OpReferenceKind, validate_op};

#[test]
fn ported_and_git_backend_regenerated_ids_match_jj_golden_vectors() {
    for line in include_str!("ops_golden.txt")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let mut fields = line.split('|');
        let kind = match fields.next() {
            Some("view") => OpObjectKind::View,
            Some("operation") => OpObjectKind::Operation,
            other => panic!("unexpected operation-store kind {other:?}"),
        };
        let expected = decode_hex(fields.next().unwrap());
        let bytes = decode_rle(fields.next().unwrap());
        assert!(fields.next().is_none());
        let object = validate_op(kind, &bytes).unwrap();
        assert_eq!(object.id.as_slice(), expected);
        assert!(
            object
                .references
                .iter()
                .all(|reference| match reference.kind {
                    OpReferenceKind::View | OpReferenceKind::Operation => reference.id.len() == 64,
                    OpReferenceKind::Commit => reference.id.len() == 20,
                })
        );
    }
}

#[test]
fn git_view_ids_are_20_bytes_and_store_ids_remain_64_bytes() {
    let view = view_with_entries(&[1, 2], &[("default", 3)]);
    let validated = validate_op(OpObjectKind::View, &view).unwrap();
    assert_eq!(validated.id.len(), 64);
    assert!(validated.references.iter().all(|reference| {
        reference.kind == OpReferenceKind::Commit && reference.id.len() == 20
    }));

    let mut invalid = view.clone();
    let first_commit_payload = invalid.iter().position(|byte| *byte == 20).unwrap() + 1;
    invalid.insert(first_commit_payload, 0);
    assert!(validate_op(OpObjectKind::View, &invalid).is_err());
}

#[test]
fn every_op_golden_mutation_returns_without_panicking() {
    for line in include_str!("ops_golden.txt")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (kind, rest) = line.split_once('|').unwrap();
        let kind = match kind {
            "view" => OpObjectKind::View,
            "operation" => OpObjectKind::Operation,
            other => panic!("unexpected operation-store kind {other}"),
        };
        let bytes = decode_rle(rest.rsplit_once('|').unwrap().1);
        for length in 0..bytes.len() {
            let _ = validate_op(kind, &bytes[..length]);
        }
        for index in 0..bytes.len() {
            let mut mutated = bytes.clone();
            mutated[index] ^= 0x80;
            let _ = validate_op(kind, &mutated);
        }
    }

    let view = view_with_entries(&[1, 2], &[("default", 3), ("other", 4)]);
    for length in 0..view.len() {
        let _ = validate_op(OpObjectKind::View, &view[..length]);
    }
    for index in 0..view.len() {
        let mut mutated = view.clone();
        mutated[index] ^= 0x80;
        let _ = validate_op(OpObjectKind::View, &mutated);
    }
}

#[test]
fn view_order_is_hash_stable_and_duplicates_are_rejected() {
    let sorted = view_with_entries(&[1, 2], &[("alpha", 3), ("zeta", 4)]);
    let unsorted = view_with_entries(&[2, 1], &[("zeta", 4), ("alpha", 3)]);
    assert_eq!(
        validate_op(OpObjectKind::View, &sorted).unwrap().id,
        validate_op(OpObjectKind::View, &unsorted).unwrap().id
    );

    let duplicate_head = view_with_entries(&[1, 1], &[]);
    assert!(
        validate_op(OpObjectKind::View, &duplicate_head)
            .unwrap_err()
            .to_string()
            .contains("duplicate id")
    );
    let duplicate_key = view_with_entries(&[], &[("alpha", 1), ("alpha", 2)]);
    assert!(
        validate_op(OpObjectKind::View, &duplicate_key)
            .unwrap_err()
            .to_string()
            .contains("duplicate key")
    );
}

fn view_with_entries(head_ids: &[u8], entries: &[(&str, u8)]) -> Vec<u8> {
    let mut view = Vec::new();
    for id_byte in head_ids {
        push_length_delimited(&mut view, 1, &[*id_byte; 20]);
    }
    for (key, id_byte) in entries {
        let mut entry = Vec::new();
        push_length_delimited(&mut entry, 1, key.as_bytes());
        push_length_delimited(&mut entry, 2, &[*id_byte; 20]);
        push_length_delimited(&mut view, 8, &entry);
    }
    push_length_delimited(&mut view, 9, &[0x1a, 0x02, 0x12, 0x00]);
    view.extend([0x60, 0x01]);
    view
}

fn push_length_delimited(output: &mut Vec<u8>, tag: u8, value: &[u8]) {
    output.push((tag << 3) | 2);
    push_varint(output, value.len());
    output.extend_from_slice(value);
}

fn push_varint(output: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn decode_rle(value: &str) -> Vec<u8> {
    let mut output = Vec::new();
    for run in value.split(',') {
        let (byte, count) = run.split_once('*').unwrap();
        output.extend(std::iter::repeat_n(
            u8::from_str_radix(byte, 16).unwrap(),
            count.parse::<usize>().unwrap(),
        ));
    }
    output
}
