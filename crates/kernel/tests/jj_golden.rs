use devspace_kernel::{ObjectKind, validate};

#[test]
fn ids_match_jj_format_golden_vectors() {
    for (line_number, line) in include_str!("jj_golden.txt").lines().enumerate() {
        let mut fields = line.split('|');
        let kind_name = fields.next().unwrap();
        let expected_id = decode_hex(fields.next().unwrap());
        let bytes = decode_rle(fields.next().unwrap());
        assert!(fields.next().is_none());
        let kind = match kind_name {
            "file" => ObjectKind::File,
            "symlink" => ObjectKind::Symlink,
            "tree" => ObjectKind::Tree,
            "commit" => ObjectKind::Commit,
            "view" => ObjectKind::View,
            "operation" => ObjectKind::Operation,
            _ => panic!("unknown fixture kind on line {}", line_number + 1),
        };
        let object = validate(kind, &bytes)
            .unwrap_or_else(|error| panic!("{} fixture failed: {error}", kind_name));
        assert_eq!(object.id.as_slice(), expected_id, "{kind_name} ID drifted");
    }
}

#[test]
fn every_structured_golden_mutation_returns_without_panicking() {
    for line in include_str!("jj_golden.txt").lines() {
        let mut fields = line.split('|');
        let kind = match fields.next().unwrap() {
            "file" | "symlink" => continue,
            "tree" => ObjectKind::Tree,
            "commit" => ObjectKind::Commit,
            "view" => ObjectKind::View,
            "operation" => ObjectKind::Operation,
            name => panic!("unexpected fixture kind {name}"),
        };
        let _id = fields.next().unwrap();
        let bytes = decode_rle(fields.next().unwrap());
        for length in 0..bytes.len() {
            let _ = validate(kind, &bytes[..length]);
        }
        for index in 0..bytes.len() {
            let mut mutated = bytes.clone();
            mutated[index] ^= 0x80;
            let _ = validate(kind, &mutated);
        }
    }
}

#[test]
fn view_map_entry_order_is_preserved() {
    let sorted = view_with_wc_commit_ids(&[("alpha", 1), ("zeta", 2)]);
    let non_sorted = view_with_wc_commit_ids(&[("zeta", 2), ("alpha", 1)]);

    let sorted_object = validate(ObjectKind::View, &sorted).unwrap();
    let non_sorted_object = validate(ObjectKind::View, &non_sorted).unwrap();
    assert_eq!(sorted_object.id, non_sorted_object.id);
}

#[test]
fn view_head_id_order_is_preserved_and_duplicates_are_rejected() {
    let sorted = view_with_head_ids(&[1, 2]);
    let non_sorted = view_with_head_ids(&[2, 1]);
    assert_eq!(
        validate(ObjectKind::View, &sorted).unwrap().id,
        validate(ObjectKind::View, &non_sorted).unwrap().id
    );

    let duplicate = view_with_head_ids(&[1, 1]);
    let error = validate(ObjectKind::View, &duplicate).unwrap_err();
    assert!(error.to_string().contains("duplicate id"));
}

#[test]
fn view_duplicate_map_keys_are_rejected() {
    let duplicate = view_with_wc_commit_ids(&[("alpha", 1), ("alpha", 2)]);
    let error = validate(ObjectKind::View, &duplicate).unwrap_err();
    assert!(error.to_string().contains("duplicate key \"alpha\""));
}

#[test]
fn view_unknown_fields_and_trailing_bytes_are_rejected() {
    let view = view_with_wc_commit_ids(&[("zeta", 2), ("alpha", 1)]);

    let mut unknown_field = view.clone();
    unknown_field.extend([0x68, 0x01]);
    assert!(validate(ObjectKind::View, &unknown_field).is_err());

    let mut trailing_bytes = view;
    trailing_bytes.push(0x80);
    assert!(validate(ObjectKind::View, &trailing_bytes).is_err());
}

#[test]
fn operation_attribute_order_is_preserved_and_duplicates_are_rejected() {
    let sorted = operation_with_attributes(&[("alpha", "1"), ("zeta", "2")]);
    let non_sorted = operation_with_attributes(&[("zeta", "2"), ("alpha", "1")]);
    assert_eq!(
        validate(ObjectKind::Operation, &sorted).unwrap().id,
        validate(ObjectKind::Operation, &non_sorted).unwrap().id
    );

    let duplicate = operation_with_attributes(&[("alpha", "1"), ("alpha", "2")]);
    let error = validate(ObjectKind::Operation, &duplicate).unwrap_err();
    assert!(error.to_string().contains("duplicate key \"alpha\""));
}

fn view_with_wc_commit_ids(entries: &[(&str, u8)]) -> Vec<u8> {
    view_with_entries(&[], entries)
}

fn view_with_head_ids(head_ids: &[u8]) -> Vec<u8> {
    view_with_entries(head_ids, &[])
}

fn view_with_entries(head_ids: &[u8], entries: &[(&str, u8)]) -> Vec<u8> {
    let mut view = Vec::new();
    for id_byte in head_ids {
        push_length_delimited(&mut view, 1, &[*id_byte; 64]);
    }
    for (key, id_byte) in entries {
        let mut entry = Vec::new();
        push_length_delimited(&mut entry, 1, key.as_bytes());
        push_length_delimited(&mut entry, 2, &[*id_byte; 64]);
        push_length_delimited(&mut view, 8, &entry);
    }

    // A canonical absent git head followed by the migration marker.
    push_length_delimited(&mut view, 9, &[0x1a, 0x02, 0x12, 0x00]);
    view.extend([0x60, 0x01]);
    view
}

fn operation_with_attributes(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut metadata = vec![0x0a, 0x00, 0x12, 0x00];
    for (key, value) in entries {
        let mut entry = Vec::new();
        push_length_delimited(&mut entry, 1, key.as_bytes());
        push_length_delimited(&mut entry, 2, value.as_bytes());
        push_length_delimited(&mut metadata, 6, &entry);
    }

    let mut operation = Vec::new();
    push_length_delimited(&mut operation, 1, &[1; 64]);
    push_length_delimited(&mut operation, 2, &[2; 64]);
    push_length_delimited(&mut operation, 3, &metadata);
    operation
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
    assert!(value.len().is_multiple_of(2));
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).unwrap()
        })
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
