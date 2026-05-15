use std::path::Path;

pub fn system_prompt(cwd: &Path) -> String {
    format!(
        "You are micro-mind, a development assistant operating inside {cwd}.\n\
         \n\
         Tool-use rules:\n\
         - To invoke a function on N inputs, emit N separate tool calls. \
         Do not pack multiple inputs into array arguments.\n\
         - If the available tools cannot satisfy the user's request, do not \
         call any tool — answer in plain text.\n\
         - Use Python operator syntax for math: `x**2`, `3*x`. Not `^`.\n\
         \n\
         Behaviour:\n\
         - Prefer the smallest action that directly answers the user.\n\
         - If a tool call is required, emit it immediately. Do not apologize \
         or narrate before the call. Explain only after the result is in, \
         and only if the user benefits.\n\
         - Read a file before modifying it.\n\
         - After a successful write, verify with ONE concise read or test command, then stop. \
         Do not continue searching once the answer is known.\n\
         \n\
         Working directory: {cwd}\n",
        cwd = cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn includes_anti_overcall_rule() {
        let p = PathBuf::from("/tmp/foo");
        let s = system_prompt(&p);
        assert!(s.contains("do not"));
        assert!(s.contains("call any tool"));
    }

    #[test]
    fn includes_parallel_rule() {
        let p = PathBuf::from("/tmp/foo");
        let s = system_prompt(&p);
        assert!(s.contains("N separate tool calls"));
    }

    #[test]
    fn includes_read_before_write() {
        let p = PathBuf::from("/tmp/foo");
        let s = system_prompt(&p);
        assert!(s.contains("Read a file before modifying it"));
    }

    #[test]
    fn stays_compact() {
        let p = PathBuf::from("/tmp/foo");
        let s = system_prompt(&p);
        // Rough token estimate: ~300 chars/token? No — ~4 chars/token.
        // Should be well under 300 tokens => well under 1200 chars.
        assert!(s.len() < 1500, "prompt grew to {} chars", s.len());
    }
}
