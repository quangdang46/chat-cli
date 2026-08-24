//! Process-wide path override for tests.
//!
//! On Windows, `dirs::config_dir()` / `dirs::data_local_dir()` use
//! Known-Folder APIs that IGNORE `$HOME`, so redirecting per-user dirs via
//! env vars (the unix approach) is impossible. Tests previously fell back to
//! touching the REAL user config/history — reading it broke provider
//! resolution, writing it corrupted user state, and `clear_history_dir()`
//! deleted real conversation files (#1).
//!
//! Fix: a process-global override installed by tests. When set, every
//! default config/history path resolves inside it. Production binaries never
//! call `set_base_override`, so runtime behavior is unchanged.

use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;

static OVERRIDE: RwLock<Option<Arc<PathBuf>>> = RwLock::new(None);

/// Install a process-wide base dir; returns a guard that restores the
/// previous state on drop (nesting-safe: the guard remembers what was active
/// before it). Callers that need exclusive isolation should hold a shared
/// lock around `set_base_override` — see chat-cli's `temp_env`.
pub fn set_base_override(base: &Path) -> OverrideGuard {
    let mut slot = OVERRIDE.write();
    let prev = slot.replace(Arc::new(base.to_path_buf()));
    OverrideGuard { prev }
}

/// Resolve `<default root>/chat-cli` under the active override, or return
/// `None` when no override is set (production behavior).
pub fn redirected(_default_root: &Path) -> Option<PathBuf> {
    let slot = OVERRIDE.read();
    let base = slot.as_ref()?;
    Some(base.join("chat-cli"))
}

/// Guard handle: restores the previously-active override (or none) on drop,
/// including during panic unwind.
#[derive(Debug)]
pub struct OverrideGuard {
    prev: Option<Arc<PathBuf>>,
}

impl Drop for OverrideGuard {
    fn drop(&mut self) {
        *OVERRIDE.write() = self.prev.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_override_returns_none_and_override_redirects() {
        // No other test in this module holds an override at this point —
        // this module's tests run in one binary; keep them lock-free simple.
        assert!(redirected(Path::new("/tmp/x")).is_none());

        let dir = tempfile::tempdir().unwrap();
        let guard = set_base_override(dir.path());
        let p = redirected(Path::new("/tmp/ignored")).expect("override must redirect");
        assert_eq!(p, dir.path().join("chat-cli"));
        drop(guard);

        assert!(redirected(Path::new("/tmp/x")).is_none());
    }

    #[test]
    fn nesting_restores_previous_state() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();

        let g1 = set_base_override(d1.path());
        {
            let g2 = set_base_override(d2.path());
            assert_eq!(
                redirected(d1.path()).unwrap(),
                d2.path().join("chat-cli"),
                "inner override wins while active"
            );
            drop(g2);
        }
        assert_eq!(
            redirected(d1.path()).unwrap(),
            d1.path().join("chat-cli"),
            "outer override restored after inner drop"
        );
        drop(g1);
        assert!(redirected(d1.path()).is_none());
    }
}
