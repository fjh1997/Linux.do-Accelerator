use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::AppConfig;
use crate::hosts_store::{
    ensure_hosts_backup, restore_hosts_from_backup, validate_hosts_backup, write_hosts_content,
};
use crate::paths::AppPaths;

const START_MARKER: &str = "# >>> linuxdo-accelerator >>>";
const END_MARKER: &str = "# <<< linuxdo-accelerator <<<";

pub fn apply_hosts(config: &AppConfig, paths: &AppPaths) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        return apply_android_hosts(config, paths);
    }

    #[cfg(not(target_os = "android"))]
    {
        let path = hosts_path();
        let original = fs::read_to_string(&path)
            .with_context(|| format!("failed to read hosts file {}", path.display()))?;
        let backup_baseline = backup_baseline_content(&original);
        ensure_hosts_backup(paths, &path, &backup_baseline)?;
        let content = render_managed_hosts(&original, config);

        if content == original {
            return Ok(());
        }

        // 先生成备份，再用原子替换方式落盘，尽量避免 hosts 被写坏。
        write_hosts_content(&path, &original, &content)
            .with_context(|| format!("failed to update hosts file {}", path.display()))?;
        Ok(())
    }
}

pub fn remove_hosts(_paths: &AppPaths) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        return remove_android_hosts(_paths);
    }

    #[cfg(not(target_os = "android"))]
    {
        let path = hosts_path();
        let original = fs::read_to_string(&path)
            .with_context(|| format!("failed to read hosts file {}", path.display()))?;
        let stripped = strip_managed_block(&original);

        if stripped == original {
            return Ok(());
        }

        write_hosts_content(&path, &original, &stripped)
            .with_context(|| format!("failed to clean hosts file {}", path.display()))?;
        Ok(())
    }
}

pub fn backup_hosts_file(paths: &AppPaths) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        return backup_android_hosts(paths);
    }

    #[cfg(not(target_os = "android"))]
    {
        let path = hosts_path();
        let original = fs::read_to_string(&path)
            .with_context(|| format!("failed to read hosts file {}", path.display()))?;
        let backup_baseline = backup_baseline_content(&original);
        ensure_hosts_backup(paths, &path, &backup_baseline)
    }
}

pub fn restore_hosts_file(paths: &AppPaths) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        return restore_android_hosts(paths);
    }

    #[cfg(not(target_os = "android"))]
    {
        let path = hosts_path();
        let original = fs::read_to_string(&path)
            .with_context(|| format!("failed to read hosts file {}", path.display()))?;

        // 完整恢复会覆盖当前 hosts，因此使用首次备份作为明确回滚点。
        restore_hosts_from_backup(paths, &path, &original)?;
        Ok(())
    }
}

pub fn validate_hosts_backup_file(paths: &AppPaths) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        let _ = paths;
        return Ok(());
    }

    #[cfg(not(target_os = "android"))]
    {
        validate_hosts_backup(paths, &hosts_path())
    }
}

#[cfg(not(target_os = "android"))]
fn render_managed_hosts(original: &str, config: &AppConfig) -> String {
    let newline = detect_newline(original);
    let mut content = strip_managed_block(original);

    if !content.is_empty() && !content.ends_with(newline) {
        content.push_str(newline);
    }

    content.push_str(START_MARKER);
    content.push_str(newline);
    for host in config.hosts_domains() {
        if host.starts_with("*.") {
            continue;
        }
        content.push_str(&format!("{} {}", config.hosts_ip, host));
        content.push_str(newline);
    }
    content.push_str(END_MARKER);
    content.push_str(newline);
    content
}

#[cfg(not(target_os = "android"))]
fn backup_baseline_content(original: &str) -> String {
    strip_managed_block(original)
}

#[cfg(target_os = "android")]
fn apply_android_hosts(_config: &AppConfig, paths: &AppPaths) -> Result<()> {
    let marker_path = android_hosts_marker_path(paths);
    if let Some(parent) = marker_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(
        &marker_path,
        "android uses internal dns_hosts overrides; system hosts is not touched\n",
    )
    .with_context(|| format!("failed to write {}", marker_path.display()))?;
    Ok(())
}

#[cfg(target_os = "android")]
fn remove_android_hosts(paths: &AppPaths) -> Result<()> {
    let marker_path = android_hosts_marker_path(paths);
    if marker_path.exists() {
        fs::remove_file(&marker_path)
            .with_context(|| format!("failed to remove {}", marker_path.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn backup_android_hosts(paths: &AppPaths) -> Result<()> {
    fs::write(
        &paths.hosts_backup_path,
        "android uses internal dns_hosts overrides; no system hosts backup is required\n",
    )
    .with_context(|| format!("failed to write {}", paths.hosts_backup_path.display()))?;
    fs::write(
        &paths.hosts_backup_meta_path,
        "{\"android\":true,\"mode\":\"dns_hosts_override\"}\n",
    )
    .with_context(|| format!("failed to write {}", paths.hosts_backup_meta_path.display()))?;
    Ok(())
}

#[cfg(target_os = "android")]
fn restore_android_hosts(paths: &AppPaths) -> Result<()> {
    remove_android_hosts(paths)
}

#[cfg(target_os = "android")]
fn android_hosts_marker_path(paths: &AppPaths) -> PathBuf {
    paths.runtime_dir.join("android-dns-hosts.txt")
}

#[cfg(not(target_os = "android"))]
fn detect_newline(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

#[cfg(not(target_os = "android"))]
fn strip_managed_block(content: &str) -> String {
    let newline = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut output = Vec::with_capacity(lines.len());
    let mut index = 0;

    while index < lines.len() {
        if lines[index].trim() == START_MARKER {
            if let Some(end_index) = lines[index + 1..]
                .iter()
                .position(|line| line.trim() == END_MARKER)
                .map(|offset| index + 1 + offset)
            {
                index = end_index + 1;
                continue;
            }
        }

        output.push(lines[index]);
        index += 1;
    }

    output.join(newline)
}

#[cfg(not(target_os = "android"))]
fn hosts_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts")
    } else {
        PathBuf::from("/etc/hosts")
    }
}

#[cfg(target_os = "android")]
fn hosts_path() -> PathBuf {
    if cfg!(target_os = "android") {
        PathBuf::from("/system/etc/hosts")
    } else {
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::{backup_baseline_content, render_managed_hosts, strip_managed_block};
    use crate::config::AppConfig;

    #[test]
    fn strip_managed_block_only_removes_owned_lines() {
        let content = [
            "127.0.0.1 localhost",
            "# >>> linuxdo-accelerator >>>",
            "127.211.73.84 linux.do",
            "# <<< linuxdo-accelerator <<<",
            "1.1.1.1 example.com",
        ]
        .join("\n");

        let stripped = strip_managed_block(&content);
        assert_eq!(stripped, "127.0.0.1 localhost\n1.1.1.1 example.com");
    }

    #[test]
    fn render_managed_hosts_skips_wildcards_and_replaces_old_block() {
        let mut config = AppConfig::default();
        config.hosts_ip = "127.211.73.84".to_string();
        config.hosts_domains = vec![
            "linux.do".to_string(),
            "*.linux.do".to_string(),
            "www.linux.do".to_string(),
        ];

        let original = [
            "127.0.0.1 localhost",
            "# >>> linuxdo-accelerator >>>",
            "127.0.0.1 stale.example",
            "# <<< linuxdo-accelerator <<<",
        ]
        .join("\n");

        let rendered = render_managed_hosts(&original, &config);

        assert!(rendered.contains("127.0.0.1 localhost"));
        assert!(rendered.contains("127.211.73.84 linux.do"));
        assert!(rendered.contains("127.211.73.84 www.linux.do"));
        assert!(!rendered.contains("stale.example"));
        assert!(!rendered.contains("*.linux.do"));
    }

    #[test]
    fn strip_managed_block_keeps_tail_when_end_marker_is_missing() {
        let content = [
            "127.0.0.1 localhost",
            "# >>> linuxdo-accelerator >>>",
            "127.211.73.84 linux.do",
            "1.1.1.1 example.com",
        ]
        .join("\n");

        let stripped = strip_managed_block(&content);
        assert_eq!(stripped, content);
    }

    #[test]
    fn backup_baseline_strips_existing_managed_block() {
        let content = [
            "127.0.0.1 localhost",
            "# >>> linuxdo-accelerator >>>",
            "127.211.73.84 linux.do",
            "# <<< linuxdo-accelerator <<<",
            "1.1.1.1 example.com",
        ]
        .join("\n");

        let backup = backup_baseline_content(&content);
        assert_eq!(backup, "127.0.0.1 localhost\n1.1.1.1 example.com");
    }
}
