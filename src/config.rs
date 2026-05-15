use std::path::PathBuf;

pub const DEFAULT_MODEL_PATH: &str = "~/models/Qwen2.5-1.5B-Instruct-Q8_0.gguf";

pub const N_CTX: usize = 8192;
pub const N_GPU_LAYERS: u32 = 99;
pub const N_THREADS: u32 = 6;
pub const N_BATCH: u32 = 512;
pub const N_UBATCH: u32 = 512;

pub const TEMPERATURE: f32 = 0.0;
pub const TOP_P: f32 = 1.0;
pub const REPEAT_PENALTY: f32 = 1.1;
pub const SEED: u32 = 42;
pub const MAX_TOKENS: u32 = 2048;

pub const MAX_TURNS: usize = 8;
pub const PRESSURE_THRESHOLD: f32 = 0.7;
pub const KEEP_RECENT_TOOLS: usize = 4;
pub const WRITE_PRESSURE_ZERO_BYTE_LIMIT: usize = 3;
pub const DEDUP_CONSECUTIVE_LIMIT: usize = 3;

pub const TOOL_OUTPUT_HARD_CAP: usize = 8 * 1024;
pub const READ_FILE_DEFAULT_MAX: usize = 24 * 1024;
pub const READ_FILE_HARD_MAX: usize = 64 * 1024;
pub const READ_FILE_REFUSAL_THRESHOLD: usize = 256 * 1024;
pub const LIST_DIR_CAP: usize = 200;
pub const LIST_RECURSIVE_CAP: usize = 500;
pub const LIST_RECURSIVE_DEFAULT_DEPTH: usize = 3;
pub const GREP_MAX_MATCHES_DEFAULT: usize = 50;

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(stripped) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    PathBuf::from(p)
}
