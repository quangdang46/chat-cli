//! Attach file resolution for `-a`.
//!
//! Supports:
//!   -a file.md -a file2.md          (repeat flag)
//!   -a file1.md,file2.md             (comma-separated)
//!   -a "src/**/*.rs"                 (glob)
//!   -a @filelist.txt                 (read list of paths from file)
//!   cat PLAN.md | chat-cli -p "..." (stdin auto-attachment if no -a)

use std::fs;
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 1_000_000; // 1 MB per file
const MAX_TOTAL_BYTES: u64 = 5_000_000; // 5 MB total

/// Resolve raw `-a` values (already split by clap) into a deduplicated `Vec<PathBuf>`.
/// Also handles `@file` indirection and glob expansion.
pub fn resolve_attachments(raw: &[String]) -> anyhow::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = vec![];
    for entry in raw {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // comma-separated inside a single -a value
        let parts: Vec<&str> = entry.split(',').map(|s| s.trim()).collect();
        for part in parts {
            if part.is_empty() {
                continue;
            }
            if let Some(list_path) = part.strip_prefix('@') {
                // @filelist.txt
                let content = fs::read_to_string(list_path).map_err(|e| {
                    anyhow::anyhow!("failed to read file list '{}': {}", list_path, e)
                })?;
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    expand_glob_or_push(line, &mut out)?;
                }
            } else {
                expand_glob_or_push(part, &mut out)?;
            }
        }
    }
    // Deduplicate preserving order
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    Ok(out)
}

fn expand_glob_or_push(pattern: &str, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        let mut matched = false;
        for entry in glob::glob(pattern).map_err(|e| anyhow::anyhow!("invalid glob '{}': {}", pattern, e))? {
            let path = entry.map_err(|e| anyhow::anyhow!("glob entry error: {}", e))?;
            out.push(path);
            matched = true;
        }
        if !matched {
            anyhow::bail!("glob pattern '{}' matched no files", pattern);
        }
    } else {
        let p = PathBuf::from(pattern);
        if !p.exists() {
            anyhow::bail!("attachment not found: {}", pattern);
        }
        out.push(p);
    }
    Ok(())
}

/// Default attachment preparation: read UTF-8, inject with clear header.
/// Providers can override `Provider::prepare_attachments` for real upload.
pub fn default_prepare_attachments(files: &[PathBuf]) -> anyhow::Result<String> {
    if files.is_empty() {
        return Ok(String::new());
    }
    let mut total: u64 = 0;
    let mut out = String::new();
    for path in files {
        let meta = fs::metadata(path)
            .map_err(|e| anyhow::anyhow!("cannot stat '{}': {}", path.display(), e))?;
        if meta.len() > MAX_FILE_BYTES {
            anyhow::bail!(
                "file '{}' too large ({} bytes > {} limit)",
                path.display(),
                meta.len(),
                MAX_FILE_BYTES
            );
        }
        total += meta.len();
        if total > MAX_TOTAL_BYTES {
            anyhow::bail!(
                "total attachments too large ({} bytes > {} limit)",
                total,
                MAX_TOTAL_BYTES
            );
        }
        let content = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read '{}' as UTF-8: {}", path.display(), e))?;
        out.push_str(&format!(
            "<<<FILE: {}>>>\n```\n{}\n```\n\n",
            path.display(),
            content
        ));
    }
    Ok(out)
}

/// If stdin is piped and no `-a` given, read stdin as an attachment.
pub fn maybe_read_stdin_as_attachment(has_attachments: bool) -> anyhow::Result<String> {
    if has_attachments {
        return Ok(String::new());
    }
    // Only read stdin if it's not a tty (i.e. piped)
    if atty_check() {
        return Ok(String::new());
    }
    let mut buf = String::new();
    use std::io::Read;
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(String::new());
    }
    if buf.len() as u64 > MAX_TOTAL_BYTES {
        anyhow::bail!("stdin too large ({} bytes > {} limit)", buf.len(), MAX_TOTAL_BYTES);
    }
    Ok(format!("<<<FILE: stdin>>>\n```\n{}\n```\n\n", buf))
}

fn atty_check() -> bool {
    #[cfg(unix)]
    {
        // Use isatty via libc if available; fallback to true (no stdin).
        // We avoid adding `atty` dep — just check if stdin is a tty via std.
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    }
    #[cfg(not(unix))]
    {
        true
    }
}
