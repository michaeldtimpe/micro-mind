use regex::Regex;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::tools::ToolDef;
use crate::tools::fs_utils::{safe_path, walk_gitignore};

/// Build the `read_file` tool. Defaults: 24 KB read window from byte offset 0.
pub fn read_file(cwd: PathBuf) -> ToolDef {
    let params = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Relative or absolute path, must stay inside the working directory."},
            "offset": {"type": "integer", "description": "Byte offset to start reading from. Default 0."},
            "max_bytes": {"type": "integer", "description": "Max bytes to read. Default 24576, hard cap 65536."}
        },
        "required": ["path"]
    });

    ToolDef::new(
        "read_file",
        "Read a file's contents. Use offset/max_bytes for large files.",
        params,
        move |args| -> Result<String, String> {
            let path_str = args.get("path").and_then(|v| v.as_str()).ok_or("path must be a string")?;
            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let max_req = args
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(config::READ_FILE_DEFAULT_MAX);
            let max_bytes = max_req.min(config::READ_FILE_HARD_MAX);

            let abs = safe_path(&cwd, path_str)?;
            let meta = fs::metadata(&abs).map_err(|e| format!("Cannot stat {}: {}", abs.display(), e))?;
            if !meta.is_file() {
                return Err(format!("Not a file: {}", path_str));
            }
            let total = meta.len() as usize;

            if total > config::READ_FILE_REFUSAL_THRESHOLD && offset == 0 && max_req >= total {
                return Ok(format!(
                    "File is {} KB. Use grep or read_file with offset/max_bytes to retrieve a slice.",
                    total / 1024
                ));
            }

            let raw = fs::read(&abs).map_err(|e| format!("Read failed: {}", e))?;
            let end = (offset + max_bytes).min(raw.len());
            let start = offset.min(raw.len());
            let slice = &raw[start..end];
            let text = String::from_utf8_lossy(slice).to_string();
            let truncated_tail = end < raw.len();

            let header = format!("[{} bytes={}..{} of {}]\n", path_str, start, end, raw.len());
            let mut out = header;
            out.push_str(&text);
            if truncated_tail {
                out.push_str(&format!(
                    "\n[truncated: {} bytes remain after offset {}.]",
                    raw.len() - end,
                    end
                ));
            }
            Ok(out)
        },
    )
    .cacheable()
}

/// Build the `list_dir` tool.
pub fn list_dir(cwd: PathBuf) -> ToolDef {
    let params = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Directory path. Defaults to '.'"}
        }
    });

    ToolDef::new(
        "list_dir",
        "List entries in a directory (non-recursive).",
        params,
        move |args| -> Result<String, String> {
            let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let abs = safe_path(&cwd, path_str)?;
            let meta =
                fs::metadata(&abs).map_err(|e| format!("Cannot stat {}: {}", abs.display(), e))?;
            if !meta.is_dir() {
                return Err(format!("Not a directory: {}", path_str));
            }
            let mut entries: Vec<(String, bool)> = fs::read_dir(&abs)
                .map_err(|e| format!("Read dir failed: {}", e))?
                .flatten()
                .map(|d| {
                    let name = d.file_name().to_string_lossy().to_string();
                    let is_dir = d.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    (name, is_dir)
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let total = entries.len();
            let capped = entries
                .into_iter()
                .take(config::LIST_DIR_CAP)
                .collect::<Vec<_>>();
            let mut out = String::new();
            for (name, is_dir) in &capped {
                if *is_dir {
                    out.push_str(&format!("{}/\n", name));
                } else {
                    out.push_str(&format!("{}\n", name));
                }
            }
            if total > config::LIST_DIR_CAP {
                out.push_str(&format!(
                    "[truncated: {} more entries. Refine with list_dir on a subdir.]\n",
                    total - config::LIST_DIR_CAP
                ));
            }
            Ok(out)
        },
    )
    .cacheable()
}

/// Build the `list_files_recursive` tool. Respects .gitignore.
pub fn list_files_recursive(cwd: PathBuf) -> ToolDef {
    let params = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Root path. Defaults to '.'"},
            "max_depth": {"type": "integer", "description": "Max walk depth. Default 3."}
        }
    });

    ToolDef::new(
        "list_files_recursive",
        "Recursive file listing, respects .gitignore. Use this for repo orientation.",
        params,
        move |args| -> Result<String, String> {
            let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let depth = args
                .get("max_depth")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(config::LIST_RECURSIVE_DEFAULT_DEPTH);
            let abs = safe_path(&cwd, path_str)?;
            if !abs.is_dir() {
                return Err(format!("Not a directory: {}", path_str));
            }
            let entries = walk_gitignore(&abs, depth);
            let total = entries.len();
            let capped: Vec<_> = entries
                .into_iter()
                .take(config::LIST_RECURSIVE_CAP)
                .collect();
            let mut out = String::new();
            for path in &capped {
                let rel = path.strip_prefix(&cwd).unwrap_or(path);
                out.push_str(&rel.display().to_string());
                out.push('\n');
            }
            if total > config::LIST_RECURSIVE_CAP {
                out.push_str(&format!(
                    "[truncated: {} more entries. Use list_dir on a subdir, or grep.]\n",
                    total - config::LIST_RECURSIVE_CAP
                ));
            }
            Ok(out)
        },
    )
    .cacheable()
}

/// Build the `grep` tool: regex search across files under `path`.
pub fn grep(cwd: PathBuf) -> ToolDef {
    let params = json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Regular expression."},
            "path": {"type": "string", "description": "File or directory to search. Defaults to '.'"},
            "max_matches": {"type": "integer", "description": "Max matches to return. Default 50."}
        },
        "required": ["pattern"]
    });

    ToolDef::new(
        "grep",
        "Search files for a regex pattern. Returns file:line:match.",
        params,
        move |args| -> Result<String, String> {
            let pattern = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or("pattern required")?;
            let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let max_matches = args
                .get("max_matches")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(config::GREP_MAX_MATCHES_DEFAULT);
            let abs = safe_path(&cwd, path_str)?;
            let re = Regex::new(pattern).map_err(|e| format!("Invalid regex: {}", e))?;

            let files: Vec<PathBuf> = if abs.is_dir() {
                walk_gitignore(&abs, 32)
                    .into_iter()
                    .filter(|p| p.is_file())
                    .collect()
            } else {
                vec![abs.clone()]
            };

            let mut matches: Vec<String> = Vec::new();
            'outer: for f in &files {
                let Ok(bytes) = fs::read(f) else { continue };
                let Ok(text) = std::str::from_utf8(&bytes) else {
                    continue;
                };
                let rel = f.strip_prefix(&cwd).unwrap_or(f);
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        matches.push(format!("{}:{}:{}", rel.display(), i + 1, line.trim_end()));
                        if matches.len() >= max_matches {
                            break 'outer;
                        }
                    }
                }
            }

            if matches.is_empty() {
                return Ok(format!("No matches for /{pattern}/ in {path_str}\n"));
            }
            let mut out = matches.join("\n");
            out.push('\n');
            if matches.len() >= max_matches {
                out.push_str(&format!("[truncated at {} matches]\n", max_matches));
            }
            Ok(out)
        },
    )
    .cacheable()
}

/// Path is unused warning suppressor: read_file/list_dir/etc. all close over cwd.
#[allow(dead_code)]
fn _unused(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> tempfile_lite::TempDir {
        tempfile_lite::TempDir::new()
    }

    #[test]
    fn read_file_returns_contents() {
        let dir = tmpdir();
        let p = dir.path().join("hello.txt");
        std::fs::write(&p, "hello world").unwrap();
        let tool = read_file(dir.path().to_path_buf());
        let out = (tool.function)(&json!({"path": "hello.txt"})).unwrap();
        assert!(out.contains("hello world"));
    }

    #[test]
    fn read_file_respects_offset_and_max_bytes() {
        let dir = tmpdir();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "abcdefghij").unwrap();
        let tool = read_file(dir.path().to_path_buf());
        let out = (tool.function)(&json!({"path": "a.txt", "offset": 3, "max_bytes": 4})).unwrap();
        assert!(out.contains("defg"));
        assert!(!out.contains("abc"));
    }

    #[test]
    fn read_file_rejects_traversal() {
        let dir = tmpdir();
        let tool = read_file(dir.path().to_path_buf());
        let err = (tool.function)(&json!({"path": "../../../etc/passwd"})).unwrap_err();
        assert!(err.contains("escapes"));
    }

    #[test]
    fn list_dir_lists_entries() {
        let dir = tmpdir();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::write(dir.path().join("b.txt"), "y").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let tool = list_dir(dir.path().to_path_buf());
        let out = (tool.function)(&json!({"path": "."})).unwrap();
        assert!(out.contains("a.txt"));
        assert!(out.contains("b.txt"));
        assert!(out.contains("sub/"));
    }

    #[test]
    fn grep_finds_matches() {
        let dir = tmpdir();
        std::fs::write(dir.path().join("a.rs"), "fn main() { /* TODO */ }").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn other() {}").unwrap();
        let tool = grep(dir.path().to_path_buf());
        let out = (tool.function)(&json!({"pattern": "TODO", "path": "."})).unwrap();
        assert!(out.contains("a.rs"));
        assert!(!out.contains("b.rs"));
    }

    #[test]
    fn grep_invalid_regex_errors() {
        let dir = tmpdir();
        let tool = grep(dir.path().to_path_buf());
        let err = (tool.function)(&json!({"pattern": "[unclosed"})).unwrap_err();
        assert!(err.contains("Invalid regex"));
    }

    #[test]
    fn list_files_recursive_basic() {
        let dir = tmpdir();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "y").unwrap();
        let tool = list_files_recursive(dir.path().to_path_buf());
        let out = (tool.function)(&json!({"path": "."})).unwrap();
        assert!(out.contains("a.txt"));
        assert!(out.contains("sub/b.txt") || out.contains("sub\\b.txt"));
    }

    // Minimal tempfile shim — avoids pulling the full `tempfile` crate.
    mod tempfile_lite {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        pub struct TempDir {
            path: PathBuf,
        }

        impl TempDir {
            pub fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                let pid = std::process::id();
                let path = std::env::temp_dir().join(format!("microm-{}-{}", pid, n));
                std::fs::create_dir_all(&path).unwrap();
                Self { path }
            }
            pub fn path(&self) -> &Path {
                &self.path
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }
}
