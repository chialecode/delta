// Test-only helper module mounted under #[cfg(test)] in training.rs.
//! Shared panic-safe temporary workspace sandbox for `delta-app` training and regression tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;

use crate::host::AppHost;
use crate::paths::{resolve_app_paths, BuildChannel, ResolvedAppPaths};
use crate::process::apply_process_quiet_guards;
use crate::service::AppService;

static NEXT_TEST_SANDBOX_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct TestSandbox {
    root: PathBuf,
    credential_prefix: String,
}

impl TestSandbox {
    pub(crate) fn new(label: &str) -> Self {
        apply_process_quiet_guards();
        let id = NEXT_TEST_SANDBOX_ID.fetch_add(1, Ordering::Relaxed);
        let base = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let root = base.join(format!(
            "delta-app-{label}-{}-{id}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        fs::create_dir_all(&root).unwrap();
        let credential_prefix = format!("delta-test-{label}-{}-{id}", std::process::id());
        Self {
            root,
            credential_prefix,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn credential_prefix(&self) -> &str {
        &self.credential_prefix
    }

    pub(crate) fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    pub(crate) fn app_paths(&self) -> ResolvedAppPaths {
        let mut paths =
            resolve_app_paths(Some(BuildChannel::Source), Some(&self.data_dir())).unwrap();
        paths.documents_dir = self.root.join("docs");
        paths.default_export_dir = paths.documents_dir.join("exports");
        paths.default_backup_dir = paths.documents_dir.join("backups");
        paths.credential_prefix = self.credential_prefix.clone();
        paths
    }

    pub(crate) fn open_service(&self) -> AppService {
        AppService::open(self.app_paths(), "USD", "USD", "UTC", true).unwrap()
    }

    pub(crate) fn open_host(&self) -> AppHost {
        AppHost::from_service(self.open_service())
    }
}

impl Drop for TestSandbox {
    fn drop(&mut self) {
        for _ in 0..40 {
            if fs::remove_dir_all(&self.root).is_ok() || !self.root.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // Never panic inside Drop (avoids double-panic process abort on test failure).
        let _ = fs::remove_dir_all(&self.root);
    }
}
