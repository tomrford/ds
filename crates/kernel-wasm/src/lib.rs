use devspace_kernel::{ObjectKind, validate};

#[unsafe(no_mangle)]
pub extern "C" fn kernel_alloc(length: u32) -> u32 {
    leak(vec![0; length as usize])
}

/// # Safety
///
/// `pointer` and `length` must identify a buffer returned by `kernel_alloc` or
/// `kernel_validate`, and the buffer must not have been freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kernel_dealloc(pointer: u32, length: u32) {
    if length == 0 {
        return;
    }
    let slice = std::ptr::slice_from_raw_parts_mut(pointer as *mut u8, length as usize);
    // SAFETY: The caller contract requires the exact allocation returned by this module.
    unsafe { drop(Box::from_raw(slice)) };
}

/// Validates bytes held in a buffer allocated with `kernel_alloc`.
///
/// The returned `u64` packs the response length into the high 32 bits and its
/// pointer into the low 32 bits. The caller owns that response buffer.
///
/// # Safety
///
/// `pointer` and `length` must identify a live buffer returned by
/// `kernel_alloc` whose contents have been initialized by the caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kernel_validate(kind: u32, pointer: u32, length: u32) -> u64 {
    let input = if length == 0 {
        &[]
    } else {
        // SAFETY: The caller contract requires a live, initialized input allocation.
        unsafe { std::slice::from_raw_parts(pointer as *const u8, length as usize) }
    };
    let response = match u8::try_from(kind)
        .ok()
        .map(ObjectKind::try_from)
        .transpose()
    {
        Ok(Some(kind)) => encode(validate(kind, input)),
        Ok(None) | Err(_) => encode_error("unknown object kind"),
    };
    let length = response.len() as u32;
    let pointer = leak(response);
    (u64::from(length) << 32) | u64::from(pointer)
}

fn encode(
    result: Result<devspace_kernel::ValidatedObject, devspace_kernel::ValidationError>,
) -> Vec<u8> {
    match result {
        Ok(object) => {
            let mut output = Vec::with_capacity(69 + object.references.len() * 65);
            output.push(0);
            output.extend_from_slice(&object.id);
            output.extend_from_slice(&(object.references.len() as u32).to_le_bytes());
            for reference in object.references {
                output.push(reference.kind as u8);
                output.extend_from_slice(&reference.id);
            }
            output
        }
        Err(error) => encode_error(&error.to_string()),
    }
}

fn encode_error(message: &str) -> Vec<u8> {
    let mut output = Vec::with_capacity(message.len() + 1);
    output.push(1);
    output.extend_from_slice(message.as_bytes());
    output
}

fn leak(bytes: Vec<u8>) -> u32 {
    if bytes.is_empty() {
        return 0;
    }
    Box::into_raw(bytes.into_boxed_slice()) as *mut u8 as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_format_carries_id_and_references() {
        let response = encode(validate(ObjectKind::File, b"hello"));
        assert_eq!(response[0], 0);
        assert_eq!(&response[65..69], &[0, 0, 0, 0]);
    }
}
