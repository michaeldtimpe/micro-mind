use serde_json::Value;
use std::collections::HashMap;

use crate::tools::ToolFn;

/// Per-session memoization for read-only tools.
/// Mirrors luxe's `ToolCache` in `tools/base.py`.
#[derive(Default)]
pub struct ToolCache {
    store: HashMap<String, (String, Option<String>)>,
    pub hits: usize,
    pub misses: usize,
}

impl ToolCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(name: &str, args: &Value) -> String {
        // Sort keys for stable hash.
        let canonical = canonicalize(args);
        format!("{name}:{canonical}")
    }

    /// Look up a cached result, or run `fn_` and store. Returns (result, error, cache_hit).
    pub fn get_or_run(
        &mut self,
        name: &str,
        args: &Value,
        fn_: ToolFn,
    ) -> (String, Option<String>, bool) {
        let k = Self::key(name, args);
        if let Some((res, err)) = self.store.get(&k) {
            self.hits += 1;
            return (res.clone(), err.clone(), true);
        }
        self.misses += 1;
        let (res, err) = match fn_(args) {
            Ok(s) => (s, None),
            Err(e) => (String::new(), Some(e)),
        };
        self.store.insert(k, (res.clone(), err.clone()));
        (res, err, false)
    }

    pub fn entry_count(&self) -> usize {
        self.store.len()
    }
}

/// Serialize a JSON value with object keys sorted, so functionally-identical
/// argument sets hash to the same cache key regardless of key order.
fn canonicalize(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{:?}:{}", k, canonicalize(&map[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonicalize).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn caches_repeat_calls() {
        let mut cache = ToolCache::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let fn_: ToolFn = Arc::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            Ok("hit".to_string())
        });
        let args = json!({"x": 1});
        let (r1, _, h1) = cache.get_or_run("t", &args, fn_.clone());
        let (r2, _, h2) = cache.get_or_run("t", &args, fn_.clone());
        assert_eq!(r1, "hit");
        assert_eq!(r2, "hit");
        assert!(!h1);
        assert!(h2);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn key_order_independent() {
        let a = json!({"a": 1, "b": 2});
        let b = json!({"b": 2, "a": 1});
        assert_eq!(ToolCache::key("t", &a), ToolCache::key("t", &b));
    }
}
