use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[cfg(target_os = "macos")]
const AUTOSTART_LABEL: &str = "io.linuxdo.accelerator";
#[cfg(any(target_os = "windows", target_os = "linux"))]
const AUTOSTART_DISPLAY_NAME: &str = "Linux.do Accelerator";
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
const AUTOSTART_FLAG: &str = "--autostart";

pub fn enable(config_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate current executable")?;
    enable_for_exe(&exe, config_path)
}

pub fn disable() -> Result<()> {
    platform_disable()
}

pub fn is_enabled() -> bool {
    platform_is_enabled().unwrap_or(false)
}

fn enable_for_exe(exe: &Path, config_path: &Path) -> Result<()> {
    platform_enable(exe, config_path)
}

#[cfg(target_os = "windows")]
fn platform_enable(exe: &Path, config_path: &Path) -> Result<()> {
    use std::process::Command;

    let exe = exe
        .canonicalize()
        .unwrap_or_else(|_| exe.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let config = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let value = format!(
        "\"{}\" --config \"{}\" {} gui",
        exe, config, AUTOSTART_FLAG
    );

    let mut command = Command::new("reg");
    command.args([
        "add",
        "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
        "/v",
        AUTOSTART_DISPLAY_NAME,
        "/t",
        "REG_SZ",
        "/d",
        &value,
        "/f",
    ]);
    hide_windows_window(&mut command);
    let output = command.output().context("failed to invoke reg.exe")?;
    if !output.status.success() {
        bail!(
            "reg add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_disable() -> Result<()> {
    use std::process::Command;

    let mut command = Command::new("reg");
    command.args([
        "delete",
        "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
        "/v",
        AUTOSTART_DISPLAY_NAME,
        "/f",
    ]);
    hide_windows_window(&mut command);
    let output = command.output().context("failed to invoke reg.exe")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        if stderr.contains("unable to find") || stderr.contains("找不到") {
            return Ok(());
        }
        bail!(
            "reg delete failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_is_enabled() -> Result<bool> {
    use std::process::Command;

    let mut command = Command::new("reg");
    command.args([
        "query",
        "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run",
        "/v",
        AUTOSTART_DISPLAY_NAME,
    ]);
    hide_windows_window(&mut command);
    let output = command.output().context("failed to invoke reg.exe")?;
    Ok(output.status.success())
}

#[cfg(target_os = "windows")]
fn hide_windows_window(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(target_os = "macos")]
fn platform_enable(exe: &Path, config_path: &Path) -> Result<()> {
    use std::fs;

    let plist_path = macos_plist_path()?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let exe = exe
        .canonicalize()
        .unwrap_or_else(|_| exe.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let config = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--config</string>
        <string>{config}</string>
        <string>{flag}</string>
        <string>gui</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        label = AUTOSTART_LABEL,
        exe = xml_escape(&exe),
        config = xml_escape(&config),
        flag = AUTOSTART_FLAG,
    );

    fs::write(&plist_path, plist)
        .with_context(|| format!("failed to write {}", plist_path.display()))?;

    let _ = std::process::Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .output();
    let _ = std::process::Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .output();
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_disable() -> Result<()> {
    use std::fs;

    let plist_path = macos_plist_path()?;
    if plist_path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .output();
        fs::remove_file(&plist_path)
            .with_context(|| format!("failed to remove {}", plist_path.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_is_enabled() -> Result<bool> {
    Ok(macos_plist_path()
        .map(|path| path.exists())
        .unwrap_or(false))
}

#[cfg(target_os = "macos")]
fn macos_plist_path() -> Result<PathBuf> {
    let home = home_dir().context("failed to resolve user home directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{AUTOSTART_LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "linux")]
fn platform_enable(exe: &Path, config_path: &Path) -> Result<()> {
    use std::fs;

    let desktop_path = linux_desktop_path()?;
    if let Some(parent) = desktop_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let exec = format!(
        "{} --config {} {} gui",
        desktop_quote(&exe.to_string_lossy()),
        desktop_quote(&config_path.to_string_lossy()),
        AUTOSTART_FLAG,
    );

    let contents = format!(
        "[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\nIcon=linuxdo-accelerator\nTerminal=false\nStartupNotify=false\nX-GNOME-Autostart-enabled=true\n",
        name = AUTOSTART_DISPLAY_NAME,
        exec = exec,
    );

    fs::write(&desktop_path, contents)
        .with_context(|| format!("failed to write {}", desktop_path.display()))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_disable() -> Result<()> {
    use std::fs;

    let desktop_path = linux_desktop_path()?;
    if desktop_path.exists() {
        fs::remove_file(&desktop_path)
            .with_context(|| format!("failed to remove {}", desktop_path.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_is_enabled() -> Result<bool> {
    Ok(linux_desktop_path()
        .map(|path| path.exists())
        .unwrap_or(false))
}

#[cfg(target_os = "linux")]
fn linux_desktop_path() -> Result<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .context("failed to resolve user config directory")?;
    Ok(config_home
        .join("autostart")
        .join("linuxdo-accelerator.desktop"))
}

#[cfg(target_os = "linux")]
fn desktop_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_enable(_exe: &Path, _config_path: &Path) -> Result<()> {
    bail!("autostart is not supported on this platform")
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_disable() -> Result<()> {
    bail!("autostart is not supported on this platform")
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_is_enabled() -> Result<bool> {
    Ok(false)
}
