use std::path::PathBuf;
use chrono::DateTime;
use memchr::memchr_iter;

#[derive(Clone)]
pub struct PatchMeta {
    pub source_path: PathBuf,
    pub author_name: String,
    pub author_email: String,
    /// Unix timestamp from Date: header, or 0 if missing
    pub author_timestamp: i64,
    /// Timezone string e.g. "+0100"
    pub author_tz: String,
    pub subject: String,
    pub references: String,
    pub patch_mainline: String,
    pub git_commit: String,
    pub git_repo: String,
    pub alt_commit: String,
    /// Description body: text after the blank line following RFC 2822 headers,
    /// up to (but not including) the `---` separator line.
    pub body: String,
}

/// Parse email-style headers from the raw bytes of a patch file's header section
/// (everything before the first `---` separator line).
pub fn parse_header(raw: &[u8], source_path: PathBuf) -> PatchMeta {
    let mut author_name = String::new();
    let mut author_email = String::new();
    let mut author_timestamp: i64 = 0;
    let mut author_tz = "+0000".to_string();
    let mut subject = String::new();
    let mut references = String::new();
    let mut patch_mainline = String::new();
    let mut git_commit = String::new();
    let mut git_repo = String::new();
    let mut alt_commit = String::new();
    let mut body = String::new();

    // Split into lines for header parsing
    let lines = split_lines(raw);
    let mut i = 0;

    // Phase 1: parse RFC 2822-style headers until the first blank line.
    while i < lines.len() {
        let line = lines[i];
        if line.is_empty() {
            i += 1;
            break; // blank line ends the header section
        }
        if line.starts_with(b"From: ") || line.starts_with(b"From:") {
            let val = header_value(line, b"From:");
            parse_from(val, &mut author_name, &mut author_email);
        } else if line.starts_with(b"Date: ") || line.starts_with(b"Date:") {
            let val = header_value(line, b"Date:");
            if let Ok(s) = std::str::from_utf8(val) {
                parse_date(s.trim(), &mut author_timestamp, &mut author_tz);
            }
        } else if line.starts_with(b"Subject:") {
            let val = header_value(line, b"Subject:");
            let mut s = to_str(val).trim().to_string();
            // Collect continuation lines (start with whitespace)
            i += 1;
            while i < lines.len() && !lines[i].is_empty() && (lines[i][0] == b' ' || lines[i][0] == b'\t') {
                s.push(' ');
                s.push_str(to_str(lines[i]).trim());
                i += 1;
            }
            subject = strip_patch_prefix(&s);
            continue; // i already advanced
        } else if line.starts_with(b"References:") {
            let val = header_value(line, b"References:");
            let mut s = to_str(val).trim().to_string();
            i += 1;
            while i < lines.len() && !lines[i].is_empty() && (lines[i][0] == b' ' || lines[i][0] == b'\t') {
                s.push(' ');
                s.push_str(to_str(lines[i]).trim());
                i += 1;
            }
            references = s;
            continue;
        } else if line.starts_with(b"Patch-mainline:") {
            let val = header_value(line, b"Patch-mainline:");
            patch_mainline = to_str(val).trim().to_string();
        } else if line.starts_with(b"Git-commit:") {
            let val = header_value(line, b"Git-commit:");
            git_commit = to_str(val).trim().to_string();
        } else if line.starts_with(b"Git-repo:") {
            let val = header_value(line, b"Git-repo:");
            git_repo = to_str(val).trim().to_string();
        } else if line.starts_with(b"Alt-commit:") {
            let val = header_value(line, b"Alt-commit:");
            alt_commit = to_str(val).trim().to_string();
        }
        i += 1;
    }

    // Phase 2: collect the body — everything after the blank line up to `---`.
    let mut body_lines: Vec<&[u8]> = Vec::new();
    while i < lines.len() {
        let line = lines[i];
        if line == b"---" {
            break;
        }
        body_lines.push(line);
        i += 1;
    }
    // Trim trailing blank lines from the body.
    while body_lines.last().map_or(false, |l| l.is_empty()) {
        body_lines.pop();
    }
    if !body_lines.is_empty() {
        body = body_lines.iter()
            .map(|l| to_str(l))
            .collect::<Vec<_>>()
            .join("\n");
    }

    PatchMeta {
        source_path,
        author_name,
        author_email,
        author_timestamp,
        author_tz,
        subject,
        references,
        patch_mainline,
        git_commit,
        git_repo,
        alt_commit,
        body,
    }
}

fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for end in memchr_iter(b'\n', data) {
        let line = &data[start..end];
        // Strip trailing \r if present
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        lines.push(line);
        start = end + 1;
    }
    if start < data.len() {
        lines.push(&data[start..]);
    }
    lines
}

fn header_value<'a>(line: &'a [u8], prefix: &[u8]) -> &'a [u8] {
    if line.len() > prefix.len() {
        &line[prefix.len()..]
    } else {
        b""
    }
}

fn to_str(s: &[u8]) -> &str {
    std::str::from_utf8(s).unwrap_or("")
}

/// Parse "Name <email>" or just "email" from From: header.
/// Handles malformed headers where the closing `>` is missing.
fn parse_from(val: &[u8], name: &mut String, email: &mut String) {
    let s = to_str(val).trim();
    if let Some(lt) = s.find('<') {
        *name = s[..lt].trim().to_string();
        // Take everything after '<', strip optional '>'
        let rest = s[lt + 1..].trim_end_matches('>').trim();
        // Sanitize: strip any embedded '<' or '>' from the email
        *email = rest.chars().filter(|&c| c != '<' && c != '>').collect();
        return;
    }
    // No angle brackets: entire string is the email
    *email = s.chars().filter(|&c| c != '<' && c != '>').collect();
}

/// Parse RFC 2822 date into Unix timestamp + tz string.
/// Produces timezone in ±HHMM format as required by git fast-import.
fn parse_date(s: &str, timestamp: &mut i64, tz: &mut String) {
    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        *timestamp = dt.timestamp();
        // utc_minus_local: negative for east-of-UTC (e.g. UTC+1 → -3600)
        let utc_minus_local = dt.offset().utc_minus_local();
        let (sign, abs_secs) = if utc_minus_local <= 0 {
            ('+', (-utc_minus_local) as u32)
        } else {
            ('-', utc_minus_local as u32)
        };
        let hours = abs_secs / 3600;
        let minutes = (abs_secs % 3600) / 60;
        // Clamp to valid range (±14:00 max; git fast-import rejects larger values)
        if hours <= 14 {
            *tz = format!("{}{:02}{:02}", sign, hours, minutes);
        } else {
            *tz = "+0000".to_string();
        }
    }
}

/// Strip [PATCH], [PATCH n/m], [PATCH v2], etc. prefix from Subject
fn strip_patch_prefix(s: &str) -> String {
    let mut rest = s;
    // Strip leading bracketed tokens like [PATCH], [PATCH 1/5], [RFC PATCH v2], etc.
    loop {
        let r = rest.trim_start();
        if r.starts_with('[') {
            if let Some(end) = r.find(']') {
                rest = &r[end + 1..];
            } else {
                break;
            }
        } else {
            break;
        }
    }
    rest.trim().to_string()
}
