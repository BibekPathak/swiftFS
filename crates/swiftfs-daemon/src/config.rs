//! Minimal configuration-file loader.
//!
//! SwiftFS config files are a flat list of `key: value` pairs with `#` comments
//! (a small YAML subset). This avoids pulling in a full YAML dependency for a
//! fixed, known key set. Unknown keys are ignored with a warning so that a
//! config written for a newer version does not break an older binary.

use std::path::Path;
use std::time::Duration;

use crate::mount::{Backend, DiskCacheConfig, MountConfig};

/// Settings loaded from a config file. Each field is optional so that CLI flags
/// can override only what the user supplied.
#[derive(Debug, Default, Clone)]
pub struct FileConfig {
    pub chunk_size: Option<u32>,
    pub ram_cache_bytes: Option<u64>,
    pub disk_cache_dir: Option<String>,
    pub disk_cache_bytes: Option<u64>,
    pub max_concurrent_fetches: Option<usize>,
    pub storage_timeout_ms: Option<u64>,
    pub read_ahead: Option<bool>,
    pub http_base_url: Option<String>,
    /// Present for forward-compatibility; recorded but not yet acted on.
    pub metrics_listen: Option<String>,
}

fn parse_value(raw: &str) -> String {
    let v = raw.trim();
    // Strip a trailing comment that is not inside quotes.
    let v = if let Some(idx) = v.find(" #") {
        v[..idx].trim()
    } else {
        v
    };
    // Strip surrounding quotes.
    let v = v.trim_matches('"').trim_matches('\'');
    v.to_string()
}

fn parse_u64(v: &str) -> Result<u64, String> {
    v.parse::<u64>()
        .map_err(|e| format!("invalid integer {v:?}: {e}"))
}

/// Parse a config file's contents into a [`FileConfig`].
pub fn parse(contents: &str) -> Result<FileConfig, String> {
    let mut cfg = FileConfig::default();
    for (lineno, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("line {}: expected `key: value`", lineno + 1));
        };
        let key = key.trim();
        let value = parse_value(value);
        let err = |e: String| format!("line {}: {e}", lineno + 1);
        match key {
            "chunk_size" => cfg.chunk_size = Some(parse_u64(&value).map_err(err)? as u32),
            "ram_cache_bytes" => cfg.ram_cache_bytes = Some(parse_u64(&value).map_err(err)?),
            "disk_cache_dir" => {
                cfg.disk_cache_dir = if value == "null" || value.is_empty() {
                    None
                } else {
                    Some(value)
                }
            }
            "disk_cache_bytes" => cfg.disk_cache_bytes = Some(parse_u64(&value).map_err(err)?),
            "max_concurrent_fetches" => {
                cfg.max_concurrent_fetches = Some(parse_u64(&value).map_err(err)? as usize)
            }
            "storage_timeout_ms" => cfg.storage_timeout_ms = Some(parse_u64(&value).map_err(err)?),
            "read_ahead" => cfg.read_ahead = Some(value == "true" || value == "1"),
            "http_base_url" => {
                cfg.http_base_url = if value == "null" || value.is_empty() {
                    None
                } else {
                    Some(value)
                }
            }
            "metrics_listen" => cfg.metrics_listen = Some(value),
            other => {
                tracing::warn!(key = other, "ignoring unknown config key");
            }
        }
    }
    Ok(cfg)
}

/// Load and parse a config file.
pub fn load(path: &Path) -> Result<FileConfig, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
    parse(&contents)
}

/// Apply file settings onto a `MountConfig`, leaving already-set mount location
/// fields (image/target) alone.
pub fn apply(cfg: &FileConfig, mount: &mut MountConfig) {
    if let Some(b) = cfg.ram_cache_bytes {
        mount.ram_cache_bytes = b;
    }
    if let Some(n) = cfg.max_concurrent_fetches {
        mount.max_concurrent_fetches = n;
    }
    if let Some(ms) = cfg.storage_timeout_ms {
        mount.storage_timeout = Duration::from_millis(ms);
    }
    if let Some(ra) = cfg.read_ahead {
        mount.read_ahead = ra;
    }
    if let Some(dir) = &cfg.disk_cache_dir {
        mount.disk_cache = Some(DiskCacheConfig {
            dir: dir.into(),
            capacity_bytes: cfg.disk_cache_bytes.unwrap_or(1024 * 1024 * 1024),
        });
    }
    if let Some(url) = &cfg.http_base_url {
        mount.backend = Backend::Http {
            base_url: url.clone(),
        };
    }
    if let Some(addr) = &cfg.metrics_listen {
        if !addr.is_empty() {
            mount.metrics_listen = Some(addr.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_config() {
        let cfg = parse(
            "# comment\n\
             chunk_size: 1048576\n\
             ram_cache_bytes: 268435456\n\
             disk_cache_dir: null\n\
             read_ahead: true\n\
             metrics_listen: \"127.0.0.1:9898\"\n",
        )
        .unwrap();
        assert_eq!(cfg.chunk_size, Some(1048576));
        assert_eq!(cfg.ram_cache_bytes, Some(268435456));
        assert_eq!(cfg.disk_cache_dir, None);
        assert_eq!(cfg.read_ahead, Some(true));
        assert_eq!(cfg.metrics_listen.as_deref(), Some("127.0.0.1:9898"));
    }

    #[test]
    fn strips_trailing_comments() {
        let cfg = parse("ram_cache_bytes: 1024 # one KiB\n").unwrap();
        assert_eq!(cfg.ram_cache_bytes, Some(1024));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        assert!(parse("future_key: 42\n").is_ok());
    }

    #[test]
    fn malformed_line_errors() {
        assert!(parse("no_colon_here\n").is_err());
    }

    #[test]
    fn apply_sets_disk_cache_and_backend() {
        let cfg = parse(
            "disk_cache_dir: /tmp/x\n\
             disk_cache_bytes: 2048\n\
             http_base_url: http://example/base\n",
        )
        .unwrap();
        let mut m = MountConfig::new("/img", "/mnt");
        apply(&cfg, &mut m);
        let dc = m.disk_cache.expect("disk cache");
        assert_eq!(dc.dir.to_str().unwrap(), "/tmp/x");
        assert_eq!(dc.capacity_bytes, 2048);
        assert!(matches!(m.backend, Backend::Http { .. }));
    }
}
