use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Canonicalize a logical path for dedup / read-before-write comparison.
///
/// - Strips a leading `./`.
/// - Collapses repeated slashes (`src//main.rs` → `src/main.rs`).
/// - Trims surrounding whitespace.
/// - Removes a single trailing slash (unless the result would be empty).
///
/// NOTE: this is a *logical* canonicalization, not a filesystem-resolving one.
/// We deliberately don't follow symlinks or convert to absolute paths — the
/// goal is to recognize equivalent strings the model might emit, not to verify
/// the path exists.
pub fn canonicalize_path(p: &str) -> String {
    let trimmed = p.trim();
    let stripped = trimmed.strip_prefix("./").unwrap_or(trimmed);
    let mut out = String::with_capacity(stripped.len());
    let mut prev_slash = false;
    for c in stripped.chars() {
        if c == '/' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            out.push(c);
            prev_slash = false;
        }
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// Reject paths that escape the CWD via `..` traversal. Returns Ok(abs) if safe.
///
/// - Relative paths: walk components, decrement on `..`, fail if depth ever < 0.
/// - Absolute paths: must be a descendant of `cwd`.
pub fn safe_path(cwd: &Path, p: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let candidate = Path::new(p);

    if candidate.is_absolute() {
        if candidate.starts_with(cwd) {
            return Ok(candidate.to_path_buf());
        }
        // Small-model accommodation: 1.5B models routinely emit "/src/main.rs"
        // when they mean "src/main.rs". If the leading-slash interpretation
        // points outside the cwd, try the relative interpretation as a
        // fallback — only if the resulting path stays inside the cwd.
        let trimmed = p.trim_start_matches('/');
        if !trimmed.is_empty() && trimmed != p {
            if let Ok(p) = safe_path(cwd, trimmed) {
                return Ok(p);
            }
        }
        return Err(format!("Path escapes the working directory: {}", p));
    }

    let mut depth: i32 = 0;
    for comp in candidate.components() {
        match comp {
            Component::CurDir | Component::Prefix(_) => {}
            Component::RootDir => return Err(format!("Unexpected root in relative path: {}", p)),
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("Path escapes the working directory: {}", p));
                }
            }
            Component::Normal(_) => depth += 1,
        }
    }
    Ok(cwd.join(candidate))
}

/// Fuzzy-find a substring inside text, tolerant of:
///   - CRLF vs LF line endings,
///   - Trailing whitespace on each line.
///
/// Returns the byte range of the *first* fuzzy match in `text` (in the
/// original text's coordinates) and the number of *additional* matches found.
/// Returns None if no match exists.
///
/// This is the small-model-survival primitive for `edit_file`: tiny models
/// frequently emit a snippet with a missing or extra trailing space, or with
/// CRLF where the file has LF. Pure exact-string replace fails; this function
/// finds the match anyway.
pub struct FuzzyMatch {
    pub start: usize,
    pub end: usize,
    pub extra_matches: usize,
}

pub fn fuzzy_find(text: &str, needle: &str) -> Option<FuzzyMatch> {
    if needle.is_empty() {
        return None;
    }
    let text_norm = normalize_for_match(text);
    let needle_norm = normalize_for_match(needle);

    // Find all match start positions in normalized text.
    let mut starts = Vec::new();
    let mut search_from = 0;
    while let Some(idx) = text_norm[search_from..].find(&needle_norm) {
        let abs = search_from + idx;
        starts.push(abs);
        search_from = abs + 1; // overlap-tolerant
    }

    let first_norm = *starts.first()?;
    let extra = starts.len().saturating_sub(1);

    // Map the normalized start position back to the original text.
    // normalize_for_match performs only character-preserving edits per line
    // (it strips trailing whitespace per line and CRLF→LF). We rebuild a
    // mapping by walking both strings side-by-side.
    let (start, _) = map_norm_to_orig(text, first_norm)
        .ok_or_else(|| "map failure")
        .ok()?;
    let needle_orig_len = match_length_in_orig(text, start, &needle_norm)?;

    Some(FuzzyMatch {
        start,
        end: start + needle_orig_len,
        extra_matches: extra,
    })
}

/// Normalize text for fuzzy matching: CRLF→LF, strip trailing whitespace per line.
fn normalize_for_match(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        // Drop a trailing \r before a \n.
        let (body, terminator) = if line.ends_with('\n') {
            let body = &line[..line.len() - 1];
            let body = body.strip_suffix('\r').unwrap_or(body);
            (body, "\n")
        } else {
            (line, "")
        };
        // Strip trailing whitespace on the body.
        let trimmed = body.trim_end_matches([' ', '\t']);
        out.push_str(trimmed);
        out.push_str(terminator);
    }
    out
}

/// Walk `text` and `normalized` in lockstep until we've consumed `norm_pos`
/// bytes of normalized output. Return (orig_byte_pos, _).
fn map_norm_to_orig(text: &str, norm_pos: usize) -> Option<(usize, usize)> {
    let mut consumed_norm = 0usize;
    let mut orig_idx = 0usize;
    let bytes = text.as_bytes();

    while orig_idx < bytes.len() {
        if consumed_norm == norm_pos {
            return Some((orig_idx, consumed_norm));
        }
        // Detect line boundary semantics: at the start of each line, we
        // need to mirror the normalization (skip trailing-whitespace bytes,
        // skip \r before \n).
        // Simpler equivalent: walk one byte at a time and skip the bytes
        // that normalization would have removed.
        let b = bytes[orig_idx];
        if b == b'\r' && orig_idx + 1 < bytes.len() && bytes[orig_idx + 1] == b'\n' {
            // Skip the \r — normalization drops it.
            orig_idx += 1;
            continue;
        }
        // Trailing-whitespace-before-newline skip: if we're at a space/tab
        // and the next non-space/tab byte (within the line) is a newline,
        // then this byte is dropped by normalize.
        if b == b' ' || b == b'\t' {
            let mut peek = orig_idx;
            while peek < bytes.len() && (bytes[peek] == b' ' || bytes[peek] == b'\t') {
                peek += 1;
            }
            let next_is_eol = peek >= bytes.len() || bytes[peek] == b'\n' || bytes[peek] == b'\r';
            if next_is_eol {
                orig_idx = peek;
                continue;
            }
        }
        // Otherwise this byte is preserved.
        consumed_norm += 1;
        orig_idx += 1;
    }
    if consumed_norm == norm_pos {
        Some((orig_idx, consumed_norm))
    } else {
        None
    }
}

/// Given that the needle matched at `orig_start` in `text`, return how many
/// original bytes the needle covers (which may differ from its normalized
/// length if the original text has CRLF or trailing whitespace).
fn match_length_in_orig(text: &str, orig_start: usize, needle_norm: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let needle_len = needle_norm.len();
    let mut consumed = 0usize;
    let mut idx = orig_start;
    while idx < bytes.len() && consumed < needle_len {
        let b = bytes[idx];
        if b == b'\r' && idx + 1 < bytes.len() && bytes[idx + 1] == b'\n' {
            idx += 1;
            continue;
        }
        if b == b' ' || b == b'\t' {
            let mut peek = idx;
            while peek < bytes.len() && (bytes[peek] == b' ' || bytes[peek] == b'\t') {
                peek += 1;
            }
            let next_is_eol = peek >= bytes.len() || bytes[peek] == b'\n' || bytes[peek] == b'\r';
            if next_is_eol {
                idx = peek;
                continue;
            }
        }
        consumed += 1;
        idx += 1;
    }
    if consumed == needle_len {
        Some(idx - orig_start)
    } else {
        None
    }
}

/// Walk the filesystem rooted at `root`, respecting `.gitignore` and
/// returning entries up to `max_depth`. Files only by default.
/// Caller is responsible for any output capping; this returns everything found.
pub fn walk_gitignore(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .max_depth(Some(max_depth))
        .build();
    for dent in walker.flatten() {
        if dent.depth() == 0 {
            continue;
        }
        let path = dent.path();
        if path == root {
            continue;
        }
        out.push(path.to_path_buf());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_strips_dot_slash() {
        assert_eq!(canonicalize_path("./src/main.rs"), "src/main.rs");
    }

    #[test]
    fn canonicalize_collapses_repeated_slashes() {
        assert_eq!(canonicalize_path("src//main.rs"), "src/main.rs");
        assert_eq!(canonicalize_path("src///foo////bar.rs"), "src/foo/bar.rs");
    }

    #[test]
    fn canonicalize_trims_whitespace() {
        assert_eq!(canonicalize_path("  src/main.rs  "), "src/main.rs");
    }

    #[test]
    fn canonicalize_strips_trailing_slash() {
        assert_eq!(canonicalize_path("src/"), "src");
        assert_eq!(canonicalize_path("/"), "/");
    }

    #[test]
    fn fuzzy_find_exact() {
        let text = "hello world";
        let m = fuzzy_find(text, "world").unwrap();
        assert_eq!(&text[m.start..m.end], "world");
        assert_eq!(m.extra_matches, 0);
    }

    #[test]
    fn fuzzy_find_handles_crlf_vs_lf() {
        let text = "line1\r\nline2\r\nline3";
        let needle = "line1\nline2";
        let m = fuzzy_find(text, needle).unwrap();
        assert_eq!(&text[m.start..m.end], "line1\r\nline2");
    }

    #[test]
    fn fuzzy_find_handles_trailing_whitespace() {
        let text = "fn foo() {  \n    body\n}"; // trailing spaces after `{`
        let needle = "fn foo() {\n    body";
        let m = fuzzy_find(text, needle).unwrap();
        let slice = &text[m.start..m.end];
        assert!(slice.contains("fn foo() {"), "got: {slice:?}");
        assert!(slice.contains("body"));
    }

    #[test]
    fn fuzzy_find_counts_extra_matches() {
        let text = "foo\nfoo\nfoo";
        let m = fuzzy_find(text, "foo").unwrap();
        assert_eq!(m.extra_matches, 2);
    }

    #[test]
    fn fuzzy_find_none_when_absent() {
        assert!(fuzzy_find("abc", "xyz").is_none());
    }

    #[test]
    fn safe_path_rejects_traversal() {
        let cwd = PathBuf::from("/tmp/foo");
        assert!(safe_path(&cwd, "../etc/passwd").is_err());
    }

    #[test]
    fn safe_path_accepts_subpath() {
        let cwd = PathBuf::from("/tmp/foo");
        let p = safe_path(&cwd, "bar/baz.txt").unwrap();
        assert!(p.starts_with(&cwd));
    }
}
