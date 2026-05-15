use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::tools::ToolDef;
use crate::tools::fs_utils::{fuzzy_find, safe_path};

/// Honesty-guard patterns: substrings that strongly suggest the model is
/// emitting placeholder/template content rather than a real edit.
const PLACEHOLDER_MARKERS: &[&str] = &[
    "<your code here>",
    "// your code here",
    "// TODO: fill in",
    "// TODO: implement",
    "/* TODO: fill in */",
    "# TODO: fill in",
    "<insert ",
    "...rest of the code...",
    "// rest of the code",
    "/* rest of the code */",
];

/// Honesty-guard threshold: refuse to overwrite a file >1 KB with <100 B.
/// Catches the "model accidentally wipes a file" failure mode.
const MASS_DELETION_OLD_SIZE: usize = 1024;
const MASS_DELETION_NEW_SIZE: usize = 100;

pub fn write_file(cwd: PathBuf) -> ToolDef {
    let params = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "content": {"type": "string"}
        },
        "required": ["path", "content"]
    });

    ToolDef::new(
        "write_file",
        "Overwrite a file atomically. Use edit_file for partial changes; this replaces the whole file.",
        params,
        move |args| -> Result<String, String> {
            let path_str = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("path required")?;
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or("content required")?;

            // Honesty guard #1: placeholder text.
            for marker in PLACEHOLDER_MARKERS {
                if content.contains(marker) {
                    return Err(format!(
                        "Refused: content contains placeholder text {marker:?}. \
                         Write the real implementation, not a stub."
                    ));
                }
            }

            let abs = safe_path(&cwd, path_str)?;

            // Honesty guard #2: mass deletion (overwriting a real file with near-empty).
            if let Ok(meta) = fs::metadata(&abs) {
                let old_size = meta.len() as usize;
                if old_size > MASS_DELETION_OLD_SIZE && content.len() < MASS_DELETION_NEW_SIZE {
                    return Err(format!(
                        "Refused: overwriting a {} B file with {} B looks like an accidental wipe. \
                         If you really mean this, use edit_file to remove specific content instead.",
                        old_size,
                        content.len()
                    ));
                }
            }

            atomic_write(&abs, content.as_bytes())
                .map_err(|e| format!("Atomic write failed: {e}"))?;

            Ok(format!(
                "write_file ok: {} ({} bytes)",
                path_str,
                content.len()
            ))
        },
    )
}

pub fn edit_file(cwd: PathBuf) -> ToolDef {
    let params = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "old": {"type": "string", "description": "Text to find. Matched fuzzy (whitespace + CRLF tolerant)."},
            "new": {"type": "string", "description": "Replacement text."},
            "replace_all": {"type": "boolean", "description": "Replace every match. Default false."}
        },
        "required": ["path", "old", "new"]
    });

    ToolDef::new(
        "edit_file",
        "Replace `old` with `new` in `path`. Match is fuzzy on whitespace/line-endings. Defaults to a single unique match.",
        params,
        move |args| -> Result<String, String> {
            let path_str = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("path required")?;
            let old = args
                .get("old")
                .and_then(|v| v.as_str())
                .ok_or("old required")?;
            let new = args
                .get("new")
                .and_then(|v| v.as_str())
                .ok_or("new required")?;
            let replace_all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if old.is_empty() {
                return Err("`old` cannot be empty.".into());
            }
            if old == new {
                return Err("`old` and `new` are identical — nothing to change.".into());
            }

            // Honesty guard on the new content too.
            for marker in PLACEHOLDER_MARKERS {
                if new.contains(marker) {
                    return Err(format!(
                        "Refused: replacement contains placeholder text {marker:?}."
                    ));
                }
            }

            let abs = safe_path(&cwd, path_str)?;
            let raw = fs::read(&abs).map_err(|e| format!("Read failed: {e}"))?;
            let text = String::from_utf8_lossy(&raw).to_string();

            let Some(m) = fuzzy_find(&text, old) else {
                return Err(format!(
                    "edit_file: could not find `old` in {}. \
                     The text may differ in whitespace or be missing entirely. \
                     Read the file and try a shorter, unique snippet.",
                    path_str
                ));
            };

            if m.extra_matches > 0 && !replace_all {
                return Err(format!(
                    "edit_file: `old` matched {} times in {}. \
                     Provide a longer/unique snippet, or set replace_all=true.",
                    m.extra_matches + 1,
                    path_str
                ));
            }

            let new_text = if replace_all {
                replace_all_fuzzy(&text, old, new)
            } else {
                let mut s = String::with_capacity(text.len() + new.len());
                s.push_str(&text[..m.start]);
                s.push_str(new);
                s.push_str(&text[m.end..]);
                s
            };

            atomic_write(&abs, new_text.as_bytes())
                .map_err(|e| format!("Atomic write failed: {e}"))?;

            let count = if replace_all { m.extra_matches + 1 } else { 1 };
            Ok(format!(
                "edit_file ok: {} ({} replacement{})",
                path_str,
                count,
                if count == 1 { "" } else { "s" }
            ))
        },
    )
}

fn replace_all_fuzzy(text: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    loop {
        let slice = &text[cursor..];
        let Some(m) = fuzzy_find(slice, old) else {
            out.push_str(slice);
            return out;
        };
        out.push_str(&slice[..m.start]);
        out.push_str(new);
        cursor += m.end;
        if cursor >= text.len() {
            return out;
        }
    }
}

/// Atomic write: write to `<path>.tmp.<pid>.<nano>`, fsync, rename over destination.
/// If anything fails before rename, the original file is untouched.
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let pid = std::process::id();
    let nano = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let fname = format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        pid,
        nano
    );
    let tmp = parent.join(fname);

    let mut f: File = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static T: AtomicUsize = AtomicUsize::new(0);

    fn tdir() -> PathBuf {
        let n = T.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("microm-write-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn write_file_creates_file() {
        let dir = tdir();
        let tool = write_file(dir.clone());
        let out = (tool.function)(&json!({"path": "a.txt", "content": "hello"})).unwrap();
        assert!(out.contains("write_file ok"));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "hello");
    }

    #[test]
    fn write_file_rejects_placeholder() {
        let dir = tdir();
        let tool = write_file(dir.clone());
        let err =
            (tool.function)(&json!({"path": "a.txt", "content": "fn x() { // TODO: fill in\n}"}))
                .unwrap_err();
        assert!(err.contains("placeholder"));
    }

    #[test]
    fn write_file_rejects_mass_deletion() {
        let dir = tdir();
        // Prepopulate a 2 KB file.
        std::fs::write(dir.join("big.txt"), "x".repeat(2048)).unwrap();
        let tool = write_file(dir.clone());
        let err = (tool.function)(&json!({"path": "big.txt", "content": "tiny"})).unwrap_err();
        assert!(err.contains("accidental wipe"));
    }

    #[test]
    fn edit_file_unique_match() {
        let dir = tdir();
        std::fs::write(
            dir.join("a.rs"),
            "fn main() {\n    let x = 1;\n    let y = 2;\n}",
        )
        .unwrap();
        let tool = edit_file(dir.clone());
        let out = (tool.function)(&json!({
            "path": "a.rs",
            "old": "let x = 1;",
            "new": "let x = 42;"
        }))
        .unwrap();
        assert!(out.contains("ok"));
        let after = std::fs::read_to_string(dir.join("a.rs")).unwrap();
        assert!(after.contains("let x = 42;"));
        assert!(after.contains("let y = 2;"));
    }

    #[test]
    fn edit_file_fuzzy_crlf() {
        let dir = tdir();
        std::fs::write(dir.join("a.txt"), "line1\r\nline2\r\nline3").unwrap();
        let tool = edit_file(dir.clone());
        let out = (tool.function)(&json!({
            "path": "a.txt",
            "old": "line2\n",
            "new": "LINE2\n"
        }))
        .unwrap();
        assert!(out.contains("ok"), "got: {out}");
        let after = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert!(after.contains("LINE2"));
    }

    #[test]
    fn edit_file_rejects_multiple_matches_without_replace_all() {
        let dir = tdir();
        std::fs::write(dir.join("a.txt"), "foo\nfoo\nfoo").unwrap();
        let tool = edit_file(dir.clone());
        let err = (tool.function)(&json!({
            "path": "a.txt",
            "old": "foo",
            "new": "bar"
        }))
        .unwrap_err();
        assert!(err.contains("matched"));
        assert!(err.contains("replace_all"));
    }

    #[test]
    fn edit_file_replace_all_works() {
        let dir = tdir();
        std::fs::write(dir.join("a.txt"), "foo\nfoo\nfoo").unwrap();
        let tool = edit_file(dir.clone());
        let out = (tool.function)(&json!({
            "path": "a.txt",
            "old": "foo",
            "new": "bar",
            "replace_all": true
        }))
        .unwrap();
        assert!(out.contains("3 replacements"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "bar\nbar\nbar"
        );
    }

    #[test]
    fn edit_file_no_match_errors() {
        let dir = tdir();
        std::fs::write(dir.join("a.txt"), "abc").unwrap();
        let tool = edit_file(dir.clone());
        let err = (tool.function)(&json!({
            "path": "a.txt",
            "old": "zzz",
            "new": "xxx"
        }))
        .unwrap_err();
        assert!(err.contains("could not find"));
    }

    #[test]
    fn atomic_write_does_not_leave_tmp() {
        let dir = tdir();
        let p = dir.join("atomic.txt");
        atomic_write(&p, b"payload").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "payload");
        // No .tmp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|d| d.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty());
    }
}
