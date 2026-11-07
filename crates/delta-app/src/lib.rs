pub mod credentials;
pub mod paths;
pub mod process;
pub mod service;

pub use credentials::*;
pub use paths::*;
pub use process::*;
pub use service::*;

#[cfg(test)]
mod regression;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_s10_control_chars_and_window_sentinel() {
        apply_process_quiet_guards();
        let raw = "Bad\x07Bell and \x1b[31mANSI\x1b[0m and \x00Null\nNextLine";
        let sanitized = sanitize_terminal_output(raw);
        assert!(!sanitized.contains('\x07'));
        assert!(!sanitized.contains('\x1b'));
        assert!(sanitized.contains("\\x07"));
        assert!(sanitized.contains("\\x1b[31m"));
        assert_eq!(count_visible_windows_for_current_process(), 0);
    }

    #[test]
    fn test_s11_channel_resolution_and_non_writable_no_fallback() {
        let tmp_root = std::env::temp_dir().join(format!("delta_app_s11_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_root);
        std::fs::create_dir_all(&tmp_root).unwrap();

        // Create a regular file and pass it as cli_data_dir: must fail with DataDirNotWritable
        let file_as_dir = tmp_root.join("not_a_directory.txt");
        std::fs::write(&file_as_dir, b"file").unwrap();
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&file_as_dir)).unwrap();
        let err = ensure_writable_data_dir(&paths.data_dir);
        assert!(err.is_err());

        // Exclusive lock contention in-process check
        let valid_dir = tmp_root.join("valid_data");
        let lock1 = InstanceLock::acquire(&valid_dir).unwrap();
        let lock2 = InstanceLock::acquire(&valid_dir);
        assert!(lock2.is_err());
        drop(lock1);
        let lock3 = InstanceLock::acquire(&valid_dir);
        assert!(lock3.is_ok());
        drop(lock3);

        let _ = std::fs::remove_dir_all(&tmp_root);
    }

    #[test]
    fn test_s02_background_job_panic_timeout_and_cancel() {
        let controller = JobController::new();

        // 1. Panic isolation
        let panic_job = controller.spawn_job("panic_test", Duration::from_secs(2), |_cancel| {
            panic!("simulated worker panic");
        });
        let snap1 = controller
            .wait_job(panic_job, Duration::from_secs(2))
            .unwrap();
        match snap1.state {
            JobState::Failed(env) => assert_eq!(env.code, "job_panicked"),
            other => panic!("expected Failed(job_panicked), got {other:?}"),
        }

        // 2. Timeout isolation
        let timeout_job =
            controller.spawn_job("timeout_test", Duration::from_millis(20), |_cancel| {
                std::thread::sleep(Duration::from_millis(120));
                Ok(())
            });
        let snap2 = controller
            .wait_job(timeout_job, Duration::from_secs(1))
            .unwrap();
        match snap2.state {
            JobState::TimedOut(env) => assert_eq!(env.code, "job_timed_out"),
            other => panic!("expected TimedOut, got {other:?}"),
        }

        // 3. Cancellation
        let cancel_job =
            controller.spawn_job("cancel_test", Duration::from_secs(5), |cancel_flag| {
                while !cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            });
        let snap3 = controller.cancel_job(cancel_job).unwrap();
        match snap3.state {
            JobState::Cancelled(env) => assert_eq!(env.code, "job_cancelled"),
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[test]
    fn test_s04_credential_store_and_redaction() {
        let prefix = format!("delta-test-unit-{}", std::process::id());
        let store = CredentialStore::with_custom_prefix(&prefix);
        let secret = "unit-test-cred-value-123";
        let opaque = store.set_secret("tiingo", "unit_user", secret).unwrap();
        assert!(opaque.starts_with("keyring://"));
        assert!(!opaque.contains(secret));
        let fetched = store.get_secret("tiingo", "unit_user").unwrap();
        assert_eq!(fetched, secret);
        store.delete_secret("tiingo", "unit_user").unwrap();
        assert!(store.get_secret("tiingo", "unit_user").is_err());

        let redacted = redact_secrets("Bearer unit-test-cred-value-123 failed", &[secret]);
        assert!(!redacted.contains(secret));
        assert!(redacted.contains("[REDACTED_SECRET]"));
    }

    #[test]
    #[ignore = "live network probes only run when explicitly invoked via verify-stage --live or cargo test -- --ignored"]
    fn test_m1_live_network_probes() {
        let tmp = std::env::temp_dir().join(format!("delta_live_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap();
        let svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let probes = svc.run_live_network_probes();
        assert!(!probes.is_empty());
        drop(svc);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
