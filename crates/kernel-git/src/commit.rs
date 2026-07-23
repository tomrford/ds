use alloc::vec::Vec;

use crate::{CommitError, Oid};

const TREE_HEADER: &[u8] = b"tree";
const PARENT_HEADER: &[u8] = b"parent";
const AUTHOR_HEADER: &[u8] = b"author";
const COMMITTER_HEADER: &[u8] = b"committer";
const ENCODING_HEADER: &[u8] = b"encoding";
const CHANGE_ID_HEADER: &[u8] = b"change-id";
const JJ_TREES_HEADER: &[u8] = b"jj:trees";
const JJ_CONFLICT_LABELS_HEADER: &[u8] = b"jj:conflict-labels";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitHeader<'a> {
    pub name: &'a [u8],
    pub value_lines: Vec<&'a [u8]>,
    pub offset: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Signature<'a> {
    pub name: &'a [u8],
    pub email: &'a [u8],
    pub timestamp: i64,
    pub tz_offset_minutes: i16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Commit<'a> {
    pub headers: Vec<CommitHeader<'a>>,
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature<'a>,
    pub committer: Signature<'a>,
    pub encoding: Option<&'a [u8]>,
    pub change_id: Option<[u8; 16]>,
    pub jj_trees: Vec<Oid>,
    pub conflict_labels: Option<Vec<&'a str>>,
    pub message: &'a [u8],
}

pub fn parse_commit(payload: &[u8]) -> Result<Commit<'_>, CommitError> {
    let (headers, message) = parse_headers(payload)?;
    let mut index = 0_usize;

    let tree_header = required_header(&headers, index, TREE_HEADER, "tree")?;
    let tree = header_oid(tree_header, "tree")?;
    index = index
        .checked_add(1)
        .ok_or(CommitError::MissingHeader("author"))?;

    let mut parents = Vec::new();
    while let Some(header) = headers.get(index) {
        if header.name != PARENT_HEADER {
            break;
        }
        parents.push(header_oid(header, "parent")?);
        index = index
            .checked_add(1)
            .ok_or(CommitError::MissingHeader("author"))?;
    }

    let author_header = required_header(&headers, index, AUTHOR_HEADER, "author")?;
    let author = parse_signature_header(author_header, "author")?;
    index = index
        .checked_add(1)
        .ok_or(CommitError::MissingHeader("committer"))?;

    let committer_header = required_header(&headers, index, COMMITTER_HEADER, "committer")?;
    let committer = parse_signature_header(committer_header, "committer")?;
    index = index
        .checked_add(1)
        .ok_or(CommitError::MissingHeader("commit body"))?;

    let encoding = match headers.get(index) {
        Some(header) if header.name == ENCODING_HEADER => {
            let value = single_line(header).ok_or(CommitError::InvalidHeader {
                offset: header.offset,
            })?;
            index = index
                .checked_add(1)
                .ok_or(CommitError::MissingHeader("commit body"))?;
            Some(value)
        }
        _ => None,
    };

    for header in headers.get(index..).unwrap_or_default() {
        if matches!(
            header.name,
            TREE_HEADER | PARENT_HEADER | AUTHOR_HEADER | COMMITTER_HEADER | ENCODING_HEADER
        ) {
            return Err(CommitError::UnexpectedHeader {
                expected: "extra",
                offset: header.offset,
            });
        }
    }

    let change_id = headers
        .get(index..)
        .unwrap_or_default()
        .iter()
        .find(|header| header.name == CHANGE_ID_HEADER)
        .and_then(|header| single_line(header))
        .and_then(parse_reverse_hex_change_id);

    let jj_trees = match headers
        .get(index..)
        .unwrap_or_default()
        .iter()
        .find(|header| header.name == JJ_TREES_HEADER)
    {
        Some(header) => parse_jj_trees(header)?,
        None => Vec::new(),
    };

    let conflict_labels = match headers
        .get(index..)
        .unwrap_or_default()
        .iter()
        .find(|header| header.name == JJ_CONFLICT_LABELS_HEADER)
    {
        Some(header) => Some(parse_conflict_labels(header, jj_trees.len())?),
        None => None,
    };

    Ok(Commit {
        headers,
        tree,
        parents,
        author,
        committer,
        encoding,
        change_id,
        jj_trees,
        conflict_labels,
        message,
    })
}

fn parse_headers(payload: &[u8]) -> Result<(Vec<CommitHeader<'_>>, &[u8]), CommitError> {
    let mut headers: Vec<CommitHeader<'_>> = Vec::new();
    let mut offset = 0_usize;
    loop {
        let remainder = payload
            .get(offset..)
            .ok_or(CommitError::MissingHeaderTerminator)?;
        let line_length = remainder
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or(CommitError::MissingHeaderTerminator)?;
        let line = remainder
            .get(..line_length)
            .ok_or(CommitError::InvalidHeader { offset })?;
        let line_offset = offset;
        offset = offset
            .checked_add(line_length)
            .and_then(|value| value.checked_add(1))
            .ok_or(CommitError::InvalidHeader {
                offset: line_offset,
            })?;

        if line.is_empty() {
            let message = payload
                .get(offset..)
                .ok_or(CommitError::MissingHeaderTerminator)?;
            return Ok((headers, message));
        }
        if line.first() == Some(&b' ') {
            let value = line.get(1..).ok_or(CommitError::UnexpectedContinuation {
                offset: line_offset,
            })?;
            let header = headers
                .last_mut()
                .ok_or(CommitError::UnexpectedContinuation {
                    offset: line_offset,
                })?;
            header.value_lines.push(value);
            continue;
        }

        let separator =
            line.iter()
                .position(|byte| *byte == b' ')
                .ok_or(CommitError::InvalidHeader {
                    offset: line_offset,
                })?;
        let name = line.get(..separator).ok_or(CommitError::InvalidHeader {
            offset: line_offset,
        })?;
        let value = line
            .get(
                separator.checked_add(1).ok_or(CommitError::InvalidHeader {
                    offset: line_offset,
                })?..,
            )
            .ok_or(CommitError::InvalidHeader {
                offset: line_offset,
            })?;
        if name.is_empty()
            || name
                .iter()
                .any(|byte| *byte == 0 || *byte == b' ' || byte.is_ascii_control())
        {
            return Err(CommitError::InvalidHeader {
                offset: line_offset,
            });
        }
        headers.push(CommitHeader {
            name,
            value_lines: alloc::vec![value],
            offset: line_offset,
        });
    }
}

fn required_header<'headers, 'payload>(
    headers: &'headers [CommitHeader<'payload>],
    index: usize,
    name: &[u8],
    label: &'static str,
) -> Result<&'headers CommitHeader<'payload>, CommitError> {
    let header = headers
        .get(index)
        .ok_or(CommitError::MissingHeader(label))?;
    if header.name != name {
        return Err(CommitError::UnexpectedHeader {
            expected: label,
            offset: header.offset,
        });
    }
    Ok(header)
}

fn single_line<'payload>(header: &CommitHeader<'payload>) -> Option<&'payload [u8]> {
    if header.value_lines.len() == 1 {
        header.value_lines.first().copied()
    } else {
        None
    }
}

fn header_oid(header: &CommitHeader<'_>, label: &'static str) -> Result<Oid, CommitError> {
    let value = single_line(header).ok_or(CommitError::InvalidObjectId {
        header: label,
        offset: header.offset,
    })?;
    Oid::from_hex(value).ok_or(CommitError::InvalidObjectId {
        header: label,
        offset: header.offset,
    })
}

fn parse_signature_header<'payload>(
    header: &CommitHeader<'payload>,
    label: &'static str,
) -> Result<Signature<'payload>, CommitError> {
    let value = single_line(header).ok_or(CommitError::InvalidSignature {
        header: label,
        offset: header.offset,
    })?;
    parse_signature(value).ok_or(CommitError::InvalidSignature {
        header: label,
        offset: header.offset,
    })
}

fn parse_signature(value: &[u8]) -> Option<Signature<'_>> {
    let timezone_separator = value.iter().rposition(|byte| *byte == b' ')?;
    let timezone = value.get(timezone_separator.checked_add(1)?..)?;
    let before_timezone = value.get(..timezone_separator)?;
    let timestamp_separator = before_timezone.iter().rposition(|byte| *byte == b' ')?;
    let timestamp = parse_i64(before_timezone.get(timestamp_separator.checked_add(1)?..)?)?;
    let actor = before_timezone.get(..timestamp_separator)?;
    if actor.last() != Some(&b'>') {
        return None;
    }
    let email_start = actor.iter().rposition(|byte| *byte == b'<')?;
    if email_start == 0 || actor.get(email_start.checked_sub(1)?) != Some(&b' ') {
        return None;
    }
    let name = actor.get(..email_start.checked_sub(1)?)?;
    let email = actor.get(email_start.checked_add(1)?..actor.len().checked_sub(1)?)?;
    let tz_offset_minutes = parse_timezone(timezone)?;
    Some(Signature {
        name,
        email,
        timestamp,
        tz_offset_minutes,
    })
}

fn parse_i64(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, bytes.get(1..)?),
        Some(b'+') => (false, bytes.get(1..)?),
        Some(_) => (false, bytes),
        None => return None,
    };
    if digits.is_empty() {
        return None;
    }
    let mut value = 0_i64;
    for digit in digits {
        let digit = digit.checked_sub(b'0')?;
        if digit > 9 {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(digit))?;
    }
    if negative {
        value.checked_neg()
    } else {
        Some(value)
    }
}

fn parse_timezone(bytes: &[u8]) -> Option<i16> {
    if bytes.len() != 5 {
        return None;
    }
    let sign = match bytes.first()? {
        b'+' => 1_i16,
        b'-' => -1_i16,
        _ => return None,
    };
    let hour_tens = decimal_digit(*bytes.get(1)?)?;
    let hour_ones = decimal_digit(*bytes.get(2)?)?;
    let minute_tens = decimal_digit(*bytes.get(3)?)?;
    let minute_ones = decimal_digit(*bytes.get(4)?)?;
    let hours = hour_tens.checked_mul(10)?.checked_add(hour_ones)?;
    let minutes = minute_tens.checked_mul(10)?.checked_add(minute_ones)?;
    if minutes >= 60 {
        return None;
    }
    hours
        .checked_mul(60)?
        .checked_add(minutes)?
        .checked_mul(sign)
}

fn decimal_digit(byte: u8) -> Option<i16> {
    let digit = byte.checked_sub(b'0')?;
    (digit <= 9).then_some(i16::from(digit))
}

fn parse_reverse_hex_change_id(value: &[u8]) -> Option<[u8; 16]> {
    if value.len() != 32 {
        return None;
    }
    let mut output = [0_u8; 16];
    for (index, pair) in value.chunks_exact(2).enumerate() {
        let high = reverse_hex_digit(*pair.first()?)?;
        let low = reverse_hex_digit(*pair.get(1)?)?;
        let slot = output.get_mut(index)?;
        *slot = high.checked_shl(4)? | low;
    }
    Some(output)
}

fn reverse_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'k'..=b'z' => Some(b'z' - byte),
        b'K'..=b'Z' => Some(b'Z' - byte),
        _ => None,
    }
}

fn parse_jj_trees(header: &CommitHeader<'_>) -> Result<Vec<Oid>, CommitError> {
    let value = single_line(header).ok_or(CommitError::InvalidJjTrees)?;
    let mut trees = Vec::new();
    for value in value.split(|byte| *byte == b' ') {
        trees.push(Oid::from_hex(value).ok_or(CommitError::InvalidJjTrees)?);
    }
    if trees.len() == 1 || trees.len().is_multiple_of(2) {
        return Err(CommitError::InvalidJjTrees);
    }
    Ok(trees)
}

fn parse_conflict_labels<'payload>(
    header: &CommitHeader<'payload>,
    tree_count: usize,
) -> Result<Vec<&'payload str>, CommitError> {
    let mut labels = header
        .value_lines
        .iter()
        .map(|line| core::str::from_utf8(line).map_err(|_| CommitError::InvalidConflictLabels))
        .collect::<Result<Vec<_>, _>>()?;
    if labels.last() == Some(&"") {
        labels.pop();
    }
    if labels.is_empty()
        || labels.len().is_multiple_of(2)
        || tree_count == 0
        || labels.len() != tree_count
    {
        return Err(CommitError::InvalidConflictLabels);
    }
    Ok(labels)
}
