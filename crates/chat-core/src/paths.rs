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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;

static OVERRIDE: RwLock<Option<Arc<PathBuf>>> = RwLock::new(None);
// Debug-only tripwire: production paths must never be resolved while an
// override is active AND a flag asserts override-only resolution. Kept simple:
// the flag exists so tests can assert the override was actually consulted.
static OVERRIDE_CONSULTED: AtomicBool = AtomicBool::new(false);

/// Install a process-wide base dir; returns a guard that restores the
/// previous state on drop. Panics if an override is already active — nested
/// overrides would silently restore the inner one first and corrupt the outer
/// test's assumptions.
pub fn set_base_override(base: &Path) -> OverrideGuard {
    let mut slot = OVERRIDE.write();
    assert!(
        slot.is_none(),
        "path base override already active — tests must not nest"
    );
    *slot = Some(Arc::new(base.to_path_buf()));
    OverrideGuard
}

/// Resolve `<default root>/chat-cli` under the active override, or return
/// `None` when no override is set (production behavior).
pub fn redirected(_default_root: &Path) -> Option<PathBuf> {
    let slot = OVERRIDE.read();
    let base = slot.as_ref()?;
    OVERRIDE_CONSULTED.store(true, Ordering::SeqCst);
    Some(base.join("chat-cli"))
}

/// Guard handle: clears the override on drop (or on panic unwind).
#[derive(Debug)]
pub struct OverrideGuard;

impl Drop for OverrideGuard {
    fn drop(&mut self) {
        *OVERRIDE.write() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_override_returns_none_and_override_redirects() {
        // No override in this process yet (each test binary starts clean).
        assert!(redirected(Path::new("/tmp/x")).is_none());

        let dir = tempfile::tempdir().unwrap();
        let guard = set_base_override(dir.path());
        let p = redirected(Path::new("/tmp/ignored"))
            .expect("override must redirect");
        assert_eq!(p, dir.path().join("chat-cli"));
        drop(guard);

        assert!(redirected(Path::new("/tmp/x")).is_none());
    }

    #[test]
    #[should_panic(expected = "already active")]
    fn nesting_panics() {
        let dir = tempfile::tempdir().unwrap();
        let _g1 = set_base_override(dir.path());
        let _g2 = set_base_override(dir.path());
    }
}
