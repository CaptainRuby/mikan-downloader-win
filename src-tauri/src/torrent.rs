use sha1::{Digest, Sha1};

pub struct TorrentMetadata {
    pub info_hash: String,
    pub name: String,
    pub total_bytes: u64,
}

pub fn parse_metadata(bytes: &[u8]) -> Result<TorrentMetadata, String> {
    if bytes.first() != Some(&b'd') {
        return Err("Torrent root is not a bencoded dictionary".to_string());
    }
    let mut cursor = 1;
    let mut info_slice = None;
    let mut name = String::new();
    while cursor < bytes.len() && bytes[cursor] != b'e' {
        let (key, next) = parse_bytes(bytes, cursor)?;
        let value_start = next;
        let value_end = skip_value(bytes, value_start)?;
        if key == b"info" {
            info_slice = Some(&bytes[value_start..value_end]);
            name = find_name(&bytes[value_start..value_end]).unwrap_or_default();
        }
        cursor = value_end;
    }
    let info = info_slice.ok_or_else(|| "Torrent is missing info dictionary".to_string())?;
    Ok(TorrentMetadata {
        info_hash: hex::encode(Sha1::digest(info)),
        name,
        total_bytes: find_total_length(info)?,
    })
}

fn find_total_length(info: &[u8]) -> Result<u64, String> {
    if info.first() != Some(&b'd') {
        return Err("Torrent info is not a bencoded dictionary".to_string());
    }
    let mut cursor = 1;
    let mut single_file_length = None;
    while cursor < info.len() && info[cursor] != b'e' {
        let (key, next) = parse_bytes(info, cursor)?;
        if key == b"length" {
            single_file_length = Some(parse_integer(info, next)?.0);
        } else if key == b"files" {
            return sum_file_lengths(info, next);
        }
        cursor = skip_value(info, next)?;
    }
    single_file_length.ok_or_else(|| "Torrent info does not contain file sizes".to_string())
}

fn sum_file_lengths(bytes: &[u8], offset: usize) -> Result<u64, String> {
    if bytes.get(offset) != Some(&b'l') {
        return Err("Torrent files value is not a list".to_string());
    }
    let mut cursor = offset + 1;
    let mut total = 0u64;
    while bytes.get(cursor) != Some(&b'e') {
        if bytes.get(cursor) != Some(&b'd') {
            return Err("Torrent file entry is not a dictionary".to_string());
        }
        cursor += 1;
        let mut length = None;
        while bytes.get(cursor) != Some(&b'e') {
            let (key, next) = parse_bytes(bytes, cursor)?;
            if key == b"length" {
                length = Some(parse_integer(bytes, next)?.0);
            }
            cursor = skip_value(bytes, next)?;
        }
        cursor += 1;
        total = total
            .checked_add(length.ok_or_else(|| "Torrent file is missing length".to_string())?)
            .ok_or_else(|| "Torrent total size is too large".to_string())?;
    }
    Ok(total)
}

fn parse_integer(bytes: &[u8], offset: usize) -> Result<(u64, usize), String> {
    if bytes.get(offset) != Some(&b'i') {
        return Err("Invalid bencode integer".to_string());
    }
    let end = bytes[offset + 1..]
        .iter()
        .position(|byte| *byte == b'e')
        .map(|position| offset + position + 1)
        .ok_or_else(|| "Unterminated bencode integer".to_string())?;
    let value = std::str::from_utf8(&bytes[offset + 1..end])
        .map_err(|_| "Invalid bencode integer".to_string())?
        .parse::<u64>()
        .map_err(|_| "Invalid bencode integer".to_string())?;
    Ok((value, end + 1))
}

fn find_name(info: &[u8]) -> Result<String, String> {
    if info.first() != Some(&b'd') {
        return Ok(String::new());
    }
    let mut cursor = 1;
    while cursor < info.len() && info[cursor] != b'e' {
        let (key, next) = parse_bytes(info, cursor)?;
        if key == b"name" {
            let (value, _) = parse_bytes(info, next)?;
            return Ok(String::from_utf8_lossy(value).into_owned());
        }
        cursor = skip_value(info, next)?;
    }
    Ok(String::new())
}

fn parse_bytes(bytes: &[u8], offset: usize) -> Result<(&[u8], usize), String> {
    let colon = bytes[offset..]
        .iter()
        .position(|byte| *byte == b':')
        .map(|position| position + offset)
        .ok_or_else(|| "Invalid bencode string length".to_string())?;
    let length = std::str::from_utf8(&bytes[offset..colon])
        .map_err(|_| "Invalid bencode string length".to_string())?
        .parse::<usize>()
        .map_err(|_| "Invalid bencode string length".to_string())?;
    let start = colon + 1;
    let end = start + length;
    if end > bytes.len() {
        return Err("Bencode string exceeds buffer length".to_string());
    }
    Ok((&bytes[start..end], end))
}

fn skip_value(bytes: &[u8], offset: usize) -> Result<usize, String> {
    let marker = *bytes
        .get(offset)
        .ok_or_else(|| "Unexpected end of bencode data".to_string())?;
    match marker {
        b'0'..=b'9' => parse_bytes(bytes, offset).map(|(_, next)| next),
        b'i' => bytes[offset + 1..]
            .iter()
            .position(|byte| *byte == b'e')
            .map(|position| offset + position + 2)
            .ok_or_else(|| "Unterminated bencode integer".to_string()),
        b'l' => skip_collection(bytes, offset + 1, false),
        b'd' => skip_collection(bytes, offset + 1, true),
        _ => Err(format!("Invalid bencode marker at {offset}")),
    }
}

fn skip_collection(bytes: &[u8], mut cursor: usize, dictionary: bool) -> Result<usize, String> {
    while bytes.get(cursor) != Some(&b'e') {
        if dictionary {
            cursor = parse_bytes(bytes, cursor)?.1;
        }
        cursor = skip_value(bytes, cursor)?;
    }
    Ok(cursor + 1)
}

#[cfg(test)]
mod tests {
    use super::parse_metadata;
    use sha1::{Digest, Sha1};

    #[test]
    fn returns_canonical_infohash_and_name() {
        let info = b"d6:lengthi12345e4:name11:example.mkv12:piece lengthi16384e6:pieces20:abcdefghijklmnopqrste";
        let mut torrent = b"d8:announce32:https://tracker.example/announce4:info".to_vec();
        torrent.extend(info);
        torrent.push(b'e');
        let metadata = parse_metadata(&torrent).unwrap();
        assert_eq!(metadata.name, "example.mkv");
        assert_eq!(metadata.total_bytes, 12345);
        assert_eq!(metadata.info_hash, hex::encode(Sha1::digest(info)));
    }

    #[test]
    fn sums_multi_file_lengths() {
        let info =
            b"d5:filesld6:lengthi10e4:pathl5:a.mkveed6:lengthi25e4:pathl5:b.mkveee4:name4:showe";
        let mut torrent = b"d4:info".to_vec();
        torrent.extend(info);
        torrent.push(b'e');

        let metadata = parse_metadata(&torrent).unwrap();
        assert_eq!(metadata.total_bytes, 35);
    }
}
