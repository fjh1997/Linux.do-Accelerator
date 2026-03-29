use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};

use crate::paths::AppPaths;
use crate::platform::sync_user_ownership;

pub(crate) fn append(paths: &AppPaths, level: &str, action: &str, message: &str) -> Result<()> {
    if let Some(parent) = paths.runtime_log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.runtime_log_path)
        .with_context(|| format!("failed to open {}", paths.runtime_log_path.display()))?;

    writeln!(
        file,
        "[{}] {:<5} {:<14} {}",
        unix_timestamp_secs()?,
        level,
        action,
        sanitize_message(message)
    )
    .with_context(|| format!("failed to write {}", paths.runtime_log_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", paths.runtime_log_path.display()))?;
    sync_user_ownership(&paths.runtime_log_path)?;
    Ok(())
}

pub(crate) fn read_recent_lines(paths: &AppPaths, max_lines: usize) -> Result<Vec<String>> {
    if max_lines == 0 || !paths.runtime_log_path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&paths.runtime_log_path)
        .with_context(|| format!("failed to read {}", paths.runtime_log_path.display()))?;
    let mut lines = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    if lines.len() > max_lines {
        lines.drain(0..lines.len() - max_lines);
    }
    Ok(lines)
}

fn sanitize_message(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn unix_timestamp_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow!("system time is before unix epoch: {error}"))?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{append, read_recent_lines};
    use crate::paths::AppPaths;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn read_recent_lines_only_returns_tail() {
        let root = create_test_dir("read_recent_lines_only_returns_tail");
        let paths = test_paths(&root);
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();

        append(&paths, "INFO", "start", "one").unwrap();
        append(&paths, "INFO", "start", "two").unwrap();
        append(&paths, "INFO", "start", "three").unwrap();

        let lines = read_recent_lines(&paths, 2).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("two"));
        assert!(lines[1].contains("three"));

        cleanup_test_dir(&root);
    }

    fn create_test_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        path.push(format!("linuxdo-accelerator-runtime-log-{name}-{id}"));
        if path.exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn cleanup_test_dir(path: &PathBuf) {
        if path.exists() {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    fn test_paths(root: &PathBuf) -> AppPaths {
        let config_dir = root.join("config");
        let data_dir = root.join("data");
        let runtime_dir = data_dir.join("runtime");
        let cert_dir = data_dir.join("certs");

        AppPaths {
            config_path: config_dir.join("linuxdo-accelerator.toml"),
            config_dir,
            data_dir,
            runtime_dir: runtime_dir.clone(),
            cert_dir,
            state_path: runtime_dir.join("service-state.json"),
            pid_path: runtime_dir.join("linuxdo-accelerator.pid"),
            ui_lease_path: runtime_dir.join("ui-lease.json"),
            runtime_log_path: runtime_dir.join("operations.log"),
            hosts_backup_path: runtime_dir.join("hosts.backup"),
            hosts_backup_meta_path: runtime_dir.join("hosts.backup.json"),
        }
    }
}
