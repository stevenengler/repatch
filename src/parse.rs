use bstr::ByteSlice;

pub fn lines_with_pos(bytes: &[u8]) -> impl Iterator<Item = (&[u8], usize)> {
    bytes.lines().scan(0, |line_start, line| {
        let x = *line_start;
        *line_start += line.len() + 1;
        Some((line, x))
    })
}

pub fn bytes_as_u64(bytes: &[u8]) -> Option<u64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

pub fn patch_block_header(bytes: &[u8]) -> Option<((u64, u64), (u64, u64))> {
    let header = bytes.strip_prefix(b"@@ ")?.strip_suffix(b" @@")?;

    let (range_1, range_2) = header.split_at(header.find_byte(b' ')?);
    let range_1 = range_1.strip_prefix(b"-")?;
    let range_2 = range_2.strip_prefix(b" +")?;

    let mut range_1 = range_1.split_at(range_1.find_byte(b',')?);
    let mut range_2 = range_2.split_at(range_2.find_byte(b',')?);

    range_1.1 = range_1.1.strip_prefix(b",")?;
    range_2.1 = range_2.1.strip_prefix(b",")?;

    let range_1 = (
        crate::parse::bytes_as_u64(range_1.0)?,
        crate::parse::bytes_as_u64(range_1.1)?,
    );
    let range_2 = (
        crate::parse::bytes_as_u64(range_2.0)?,
        crate::parse::bytes_as_u64(range_2.1)?,
    );

    Some((range_1, range_2))
}
