use std::path::PathBuf;

use anyhow::{Context, Result};

/// Parse series.conf bytes and return the ordered list of patch file paths.
///
/// Rules:
/// - Lines starting with `#` (possibly after leading whitespace) → comment, skip
/// - Lines starting with `+` or `-` → guarded, skip
/// - Blank lines → skip
/// - All other lines: first whitespace-separated token is the patch path (relative to
///   kernel-source root); any trailing tokens (e.g. `-p0`) are ignored since we always
///   use strip=1 per the project spec.
pub fn parse_series(data: &[u8]) -> Result<Vec<PathBuf>> {
    let content = std::str::from_utf8(data).context("series.conf is not UTF-8")?;
    let mut patches = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let first = trimmed.as_bytes()[0];
        if first == b'#' || first == b'+' || first == b'-' {
            continue;
        }
        let patch_path = trimmed.split_whitespace().next().unwrap();
        patches.push(PathBuf::from(patch_path));
    }
    Ok(patches)
}
