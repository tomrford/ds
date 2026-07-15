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
