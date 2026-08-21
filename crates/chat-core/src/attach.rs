//! Attach file resolution for `-a`.
//!
//! Supports:
//!   -a file.md -a file2.md          (repeat flag)
//!   -a file1.md,file2.md             (comma-separated)
//!   -a "src/**/*.rs"                 (glob)
//!   -a @filelist.txt                 (read list of paths from file)
//!   cat PLAN.md | chat-cli -p "..." (stdin auto-attachment if no -a)

use std::fs;
use std::path::PathBuf;

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
        for entry in
            glob::glob(pattern).map_err(|e| anyhow::anyhow!("invalid glob '{}': {}", pattern, e))?
        {
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
    build_stdin_attachment(buf)
}

/// Pure core of stdin handling — unit-testable without controlling the real stdin.
fn build_stdin_attachment(buf: String) -> anyhow::Result<String> {
    if buf.trim().is_empty() {
        return Ok(String::new());
    }
    if buf.len() as u64 > MAX_TOTAL_BYTES {
        anyhow::bail!(
            "stdin too large ({} bytes > {} limit)",
            buf.len(),
            MAX_TOTAL_BYTES
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn repeated_flag_entries_resolve_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.md", "A");
        let b = write_file(dir.path(), "b.md", "B");
        let raw = vec![
            a.to_string_lossy().to_string(),
            b.to_string_lossy().to_string(),
        ];
        let out = resolve_attachments(&raw).unwrap();
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn comma_separated_single_flag_expands() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.md", "A");
        let b = write_file(dir.path(), "b.md", "B");
        let combined = format!("{},{}", a.display(), b.display());
        let out = resolve_attachments(&[combined]).unwrap();
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn at_list_file_supports_comments_and_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.md", "A");
        let b = write_file(dir.path(), "b.md", "B");
        let list = format!("# comment line\n\n{}\n  \n{}\n", a.display(), b.display());
        let list_path = write_file(dir.path(), "list.txt", &list);

        let out = resolve_attachments(&[format!("@{}", list_path.display())]).unwrap();
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn at_list_missing_file_bails_with_clear_error() {
        let err = resolve_attachments(&["@/nonexistent/path/list.txt".to_string()]).unwrap_err();
        assert!(err.to_string().contains("failed to read file list"));
    }

    #[test]
    fn glob_pattern_matches_and_skips_subdirs_extension_filter() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "x.rs", "fn main() {}");
        write_file(dir.path(), "y.rs", "fn other() {}");
        write_file(dir.path(), "z.md", "not rust");

        let pattern = format!("{}/*.rs", dir.path().display());
        let mut out = resolve_attachments(&[pattern]).unwrap();
        out.sort();
        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .all(|p| p.extension().and_then(|e| e.to_str()) == Some("rs")));
    }

    #[test]
    fn recursive_glob_matches_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        write_file(nested.as_path(), "inner.rs", "mod inner;");

        let pattern = format!("{}/**/*.rs", dir.path().display());
        let out = resolve_attachments(&[pattern]).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("inner.rs"));
    }

    #[test]
    fn glob_matching_nothing_bails_with_hint() {
        let dir = tempfile::tempdir().unwrap();
        let pattern = format!("{}/*.nope", dir.path().display());
        let err = resolve_attachments(&[pattern]).unwrap_err();
        assert!(err.to_string().contains("matched no files"));
    }

    #[test]
    fn plain_path_that_does_not_exist_bails() {
        let err = resolve_attachments(&["/nonexistent/file.md".to_string()]).unwrap_err();
        assert!(err.to_string().contains("attachment not found"));
    }

    #[test]
    fn dedup_preserves_first_occurrence_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.md", "A");
        let b = write_file(dir.path(), "b.md", "B");

        // Same file reached twice via repeat + comma must appear once, in order.
        let raw = vec![
            a.to_string_lossy().to_string(),
            format!("{},{}", b.display(), a.display()),
        ];
        let out = resolve_attachments(&raw).unwrap();
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn empty_and_whitespace_entries_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.md", "A");
        let raw = vec![
            String::new(),
            "   ".to_string(),
            ",".to_string(),
            a.to_string_lossy().to_string(),
        ];
        let out = resolve_attachments(&raw).unwrap();
        assert_eq!(out, vec![a]);
    }

    #[test]
    fn default_prepare_empty_slice_is_empty_string() {
        assert_eq!(default_prepare_attachments(&[]).unwrap(), "");
    }

    #[test]
    fn default_prepare_injects_file_fences() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.md", "content A");

        let out = default_prepare_attachments(std::slice::from_ref(&a)).unwrap();
        let expected = format!("<<<FILE: {}>>>\n```\ncontent A\n```\n\n", a.display());
        assert_eq!(out, expected);
    }

    #[test]
    fn default_prepare_multiple_files_concatenates_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a.md", "AAA-content");
        let b = write_file(dir.path(), "b.md", "BBB-content");

        let out = default_prepare_attachments(&[a.clone(), b.clone()]).unwrap();
        let fence_a = format!("<<<FILE: {}>>>", a.display());
        let fence_b = format!("<<<FILE: {}>>>", b.display());
        assert!(out.contains(&fence_a));
        assert!(out.contains(&fence_b));
        assert!(
            out.find(&fence_a).unwrap() < out.find(&fence_b).unwrap(),
            "files must be injected in argument order"
        );
        assert!(out.find("AAA-content").unwrap() < out.find("BBB-content").unwrap());
    }

    #[test]
    fn per_file_guard_rejects_over_1mb() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.txt");
        std::fs::write(&big, vec![b'x'; MAX_FILE_BYTES as usize + 1]).unwrap();

        let err = default_prepare_attachments(&[big]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("too large"), "{msg}");
        assert!(msg.contains("1000000"), "should name the limit: {msg}");
    }

    #[test]
    fn total_guard_rejects_over_5mb_across_files() {
        let dir = tempfile::tempdir().unwrap();
        // Six files of ~900KB each: every one under the 1MB per-file guard,
        // but together over the 5MB total guard.
        let chunk = vec![b'a'; 900_000];
        let files: Vec<PathBuf> = (0..6)
            .map(|i| {
                let p = dir.path().join(format!("chunk{i}.txt"));
                std::fs::write(&p, &chunk).unwrap();
                p
            })
            .collect();

        let err = default_prepare_attachments(&files).unwrap_err();
        assert!(err.to_string().contains("total attachments too large"));
    }

    #[test]
    fn non_utf8_file_bails_with_path_named() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("binary.bin");
        std::fs::write(&bin, [0xFF, 0xFE, 0x00, 0xC0]).unwrap();

        let err = default_prepare_attachments(std::slice::from_ref(&bin)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("UTF-8"), "{msg}");
        assert!(msg.contains(bin.display().to_string().as_str()), "{msg}");
    }

    #[test]
    fn missing_file_stat_bails() {
        let err = default_prepare_attachments(&[PathBuf::from("/nonexistent/f.md")]).unwrap_err();
        assert!(err.to_string().contains("cannot stat"));
    }

    #[test]
    fn stdin_with_attachments_present_returns_empty_without_reading() {
        // When -a was given, stdin must never be consumed.
        assert_eq!(maybe_read_stdin_as_attachment(true).unwrap(), "");
    }

    #[test]
    fn stdin_builder_wraps_piped_text_as_virtual_file() {
        let out = build_stdin_attachment("piped plan content".to_string()).unwrap();
        assert_eq!(out, "<<<FILE: stdin>>>\n```\npiped plan content\n```\n\n");
    }

    #[test]
    fn stdin_builder_whitespace_only_is_empty() {
        assert_eq!(build_stdin_attachment("   \n\t".to_string()).unwrap(), "");
    }

    #[test]
    fn stdin_builder_rejects_over_total_limit() {
        let big = "x".repeat(MAX_TOTAL_BYTES as usize + 1);
        let err = build_stdin_attachment(big).unwrap_err();
        assert!(err.to_string().contains("stdin too large"));
    }
}
