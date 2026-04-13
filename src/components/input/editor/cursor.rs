/// Returns the previous cursor boundary (character boundary) for the given offset.
pub(crate) fn previous_boundary(text: &ropey::Rope, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut prev = offset - 1;
    while prev > 0 && !text.is_char_boundary(prev) {
        prev -= 1;
    }
    prev
}

/// Returns the next cursor boundary (character boundary) for the given offset.
pub(crate) fn next_boundary(text: &ropey::Rope, offset: usize) -> usize {
    let len = text.len();
    if offset >= len {
        return len;
    }
    let mut next = offset + 1;
    while next < len && !text.is_char_boundary(next) {
        next += 1;
    }
    next
}
