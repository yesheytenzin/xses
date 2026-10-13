//! JSONL record helpers shared by the store scanners and the extractors.
use std::path::Path;

/// Head of a file, for provenance markers stored in the first record.
pub fn head_bytes(path: &Path, limit: usize) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; limit];
    let n = f.read(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}
