use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
#[cfg(target_os = "linux")]
use std::{fs::OpenOptions, io::Write};

use anyhow::{bail, Context, Error, Result};
use eframe::egui::{self, FontDefinitions, FontFamily, FontId, RichText};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tray_icon::TrayIcon;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tray_icon::TrayIconBuilder;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

use crate::autostart;
use crate::branding;
use crate::config::AppConfig;
#[cfg(target_os = "windows")]
use crate::paths::AppPaths;
use crate::platform::run_elevated;
use crate::platform::spawn_detached;
#[cfg(target_os = "windows")]
use crate::platform::{apply_app_window_icon, update_windows_shortcuts_for_exe};
#[cfg(target_os = "windows")]
use crate::platform::{hide_app_window, restore_app_window};
use crate::runtime_log::{append as append_runtime_log, read_recent_lines};
use crate::service;
use crate::state::{self, ServiceState};

const APP_WINDOW_TITLE: &str = "Linux.do Accelerator";
const APP_ID: &str = "linuxdo-accelerator";
const APP_VERSION: &str = match option_env!("LINUXDO_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
const ACTIVE_REPAINT_INTERVAL: Duration = Duration::from_millis(100);
const IDLE_REPAINT_INTERVAL: Duration = Duration::from_secs(5);
const TRAY_REPAINT_INTERVAL: Duration = Duration::from_secs(15);
const EMBEDDED_CJK_FONT: &[u8] = include_bytes!("../assets/fonts/DroidSansFallbackFull.ttf");
const LAUNCHER_CONTENT_SIZE: [f32; 2] = [620.0, 186.0];
const DETAILS_WINDOW_SIZE: [f32; 2] = [760.0, 520.0];
const TITLE_BAR_HEIGHT: f32 = 52.0;

#[cfg(target_os = "linux")]
fn use_native_wayland_frame() -> bool {
    // Always use the custom title bar on Linux so the in-window controls —
    // including the dedicated minimize-to-tray (↧) button — are available on
    // Wayland as well. Previously Wayland fell back to the native frame, which
    // has no tray button. Set LINUXDO_NATIVE_FRAME=1 to opt back into the
    // native Wayland frame.
    std::env::var_os("LINUXDO_NATIVE_FRAME").is_some()
}

#[cfg(not(target_os = "linux"))]
fn use_native_wayland_frame() -> bool {
    false
}

fn launcher_window_size() -> egui::Vec2 {
    let base = egui::vec2(LAUNCHER_CONTENT_SIZE[0], LAUNCHER_CONTENT_SIZE[1]);
    if use_native_wayland_frame() {
        base
    } else {
        egui::vec2(base.x, base.y + TITLE_BAR_HEIGHT + 2.0)
    }
}

pub fn run(config_path: PathBuf, auto_start: bool) -> Result<()> {
    let native_wayland_frame = use_native_wayland_frame();
    let launcher_size = launcher_window_size();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let restored_window_position: Option<egui::Pos2> = read_saved_window_position(&config_path);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let restored_window_position: Option<egui::Pos2> = None;
    let restored_window_position_available = restored_window_position.is_some();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_WINDOW_TITLE)
        .with_app_id(APP_ID)
        .with_icon(branding::icon_data(256))
        .with_decorations(native_wayland_frame)
        .with_inner_size(launcher_size)
        .with_min_inner_size(launcher_size)
        .with_max_inner_size(launcher_size)
        .with_minimize_button(true)
        .with_maximize_button(false)
        .with_resizable(false);
    if let Some(position) = restored_window_position {
        viewport = viewport.with_position(position);
    }
    let native_options = eframe::NativeOptions {
        renderer: default_renderer(),
        viewport,
        #[cfg(target_os = "linux")]
        event_loop_builder: linux_event_loop_builder(),
        persist_window: false,
        ..Default::default()
    };

    eframe::run_native(
        "Linux.do Accelerator",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(AcceleratorApp::new(
                config_path.clone(),
                auto_start,
                !restored_window_position_available,
                cc,
            )))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn default_renderer() -> eframe::Renderer {
    eframe::Renderer::Glow
}

#[cfg(target_os = "linux")]
fn linux_event_loop_builder() -> Option<eframe::EventLoopBuilderHook> {
    let has_x11 = std::env::var("DISPLAY")
        .map(|display| !display.is_empty())
        .unwrap_or(false);
    if !has_x11 || std::env::var_os("LINUXDO_WAYLAND").is_some() {
        return None;
    }

    Some(Box::new(|event_loop_builder| {
        use winit::platform::x11::EventLoopBuilderExtX11;
        event_loop_builder.with_x11();
    }))
}

#[cfg(target_os = "linux")]
pub fn run_tray_shell(config_path: PathBuf, _ready_file: Option<PathBuf>) -> Result<()> {
    use ksni::blocking::TrayMethods;

    enum LinuxTrayShellCommand {
        Restore(&'static str),
        Quit(&'static str),
    }

    struct LinuxTrayShell {
        command_tx: mpsc::Sender<LinuxTrayShellCommand>,
        icon: ksni::Icon,
    }

    impl LinuxTrayShell {
        fn send_command(&self, command: LinuxTrayShellCommand) {
            let _ = self.command_tx.send(command);
        }
    }

    impl ksni::Tray for LinuxTrayShell {
        fn id(&self) -> String {
            "linuxdo-accelerator-tray-shell".into()
        }

        fn title(&self) -> String {
            APP_WINDOW_TITLE.into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![self.icon.clone()]
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: APP_WINDOW_TITLE.into(),
                icon_pixmap: vec![self.icon.clone()],
                ..Default::default()
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.send_command(LinuxTrayShellCommand::Restore(
                "tray-shell restore activated",
            ));
        }

        fn secondary_activate(&mut self, _x: i32, _y: i32) {
            self.send_command(LinuxTrayShellCommand::Restore(
                "tray-shell restore secondary activated",
            ));
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::{MenuItem as KsniMenuItem, StandardItem};

            vec![
                StandardItem {
                    label: "打开窗口".into(),
                    activate: Box::new(|tray: &mut Self| {
                        tray.send_command(LinuxTrayShellCommand::Restore(
                            "tray-shell restore menu clicked",
                        ))
                    }),
                    ..Default::default()
                }
                .into(),
                KsniMenuItem::Separator,
                StandardItem {
                    label: "退出程序".into(),
                    activate: Box::new(|tray: &mut Self| {
                        tray.send_command(LinuxTrayShellCommand::Quit(
                            "tray-shell quit menu clicked",
                        ))
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    log_linux_tray_event(&format!(
        "tray-shell start config={}",
        config_path.display()
    ));

    // Single-instance guard: if another tray-shell is already showing an icon,
    // exit instead of adding a duplicate. Each minimize spawns a tray-shell and
    // there is otherwise nothing capping how many run at once, so repeated
    // minimize/restore cycles can leave several live icons (each restoring its
    // own window). Fail-open: any read/parse failure falls through and shows the
    // icon, so this can never break minimize-to-tray.
    let tray_lock_path = std::env::temp_dir().join("linuxdo-accelerator-tray-shell.pid");
    if let Ok(content) = std::fs::read_to_string(&tray_lock_path) {
        if let Ok(existing_pid) = content.trim().parse::<u32>() {
            if existing_pid != std::process::id() && linux_tray_shell_pid_alive(existing_pid) {
                log_linux_tray_event(&format!(
                    "another tray-shell (pid {existing_pid}) is already running; exiting to avoid a duplicate icon"
                ));
                return Ok(());
            }
        }
    }
    let _ = std::fs::write(&tray_lock_path, std::process::id().to_string());

    let (command_tx, command_rx) = mpsc::channel();
    let tray_lease_stop = spawn_ui_lease_heartbeat(config_path.clone());
    let tray = LinuxTrayShell {
        command_tx,
        icon: linux_tray_icon_pixmap(),
    };
    let _tray_handle = tray.spawn().context("failed to create Linux tray icon")?;
    log_linux_tray_event("tray-shell icon visible");

    let restore_error = match command_rx.recv() {
        Ok(LinuxTrayShellCommand::Restore(source)) => {
            log_linux_tray_event(source);
            tray_lease_stop.store(true, Ordering::Relaxed);
            match spawn_ui_process(&config_path) {
                Ok(()) => None,
                Err(error) => {
                    log_linux_tray_event(&format!("tray-shell restore failed: {error:#}"));
                    Some(error)
                }
            }
        }
        Ok(LinuxTrayShellCommand::Quit(source)) => {
            log_linux_tray_event(source);
            tray_lease_stop.store(true, Ordering::Relaxed);
            None
        }
        Err(error) => {
            tray_lease_stop.store(true, Ordering::Relaxed);
            log_linux_tray_event(&format!("Linux tray command channel closed: {error}"));
            None
        }
    };

    // Drop our single-instance lock, then force the process to exit so the
    // tray-shell — and its tray icon — actually goes away after the handoff.
    // ksni's graceful shutdown can block on some desktops, which would otherwise
    // leave the process alive with a lingering, duplicate icon.
    let _ = std::fs::remove_file(&tray_lock_path);
    log_linux_tray_event("tray-shell exit");
    if let Some(error) = restore_error {
        return Err(error);
    }
    std::process::exit(0)
}

#[cfg(target_os = "linux")]
fn linux_tray_shell_pid_alive(pid: u32) -> bool {
    // Confirm the PID belongs to a live linuxdo-accelerator tray-shell (not a
    // recycled PID). /proc/<pid>/cmdline holds the NUL-separated argv.
    std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
        .map(|cmdline| cmdline.contains("linuxdo-accelerator") && cmdline.contains("tray-shell"))
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
pub fn run_tray_shell(config_path: PathBuf, _ready_file: Option<PathBuf>) -> Result<()> {
    use winit::application::ApplicationHandler;
    use winit::event::StartCause;
    use winit::event_loop::{ActiveEventLoop, EventLoop};

    #[derive(Debug)]
    enum TrayShellEvent {
        Restore,
        Quit,
    }

    struct TrayShellApp {
        config_path: PathBuf,
        lease_stop: Arc<AtomicBool>,
        tray_icon: Option<TrayIcon>,
        show_item: MenuItem,
        quit_item: MenuItem,
    }

    impl TrayShellApp {
        fn ensure_tray_icon(&mut self, event_loop: &ActiveEventLoop, source: &'static str) {
            if let Err(error) = self.create_tray_icon() {
                #[cfg(target_os = "macos")]
                log_macos_tray_event(&format!(
                    "tray-shell create icon failed at {source}: {error:#}"
                ));
                event_loop.exit();
            }
        }

        fn create_tray_icon(&mut self) -> Result<()> {
            if self.tray_icon.is_some() {
                return Ok(());
            }

            let menu = Menu::new();
            menu.append_items(&[
                &self.show_item,
                &PredefinedMenuItem::separator(),
                &self.quit_item,
            ])?;

            let mut tray_icon_builder = TrayIconBuilder::new()
                .with_id("linuxdo-accelerator-tray-shell")
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_tooltip("Linux.do Accelerator");
            #[cfg(not(target_os = "macos"))]
            {
                tray_icon_builder = tray_icon_builder.with_icon(tray_window_icon()?);
            }
            #[cfg(target_os = "macos")]
            {
                tray_icon_builder = tray_icon_builder.with_title("LDO");
            }

            let tray_icon = tray_icon_builder
                .build()
                .context("failed to create tray shell icon")?;
            #[cfg(target_os = "macos")]
            log_macos_tray_event(&format!("tray-shell icon rect={:?}", tray_icon.rect()));
            self.tray_icon = Some(tray_icon);
            Ok(())
        }
    }

    impl ApplicationHandler<TrayShellEvent> for TrayShellApp {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            self.ensure_tray_icon(event_loop, "resumed");
        }

        fn window_event(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _window_id: winit::window::WindowId,
            _event: winit::event::WindowEvent,
        ) {
        }

        fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
            if cause == StartCause::Init {
                self.ensure_tray_icon(event_loop, "init");
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            self.ensure_tray_icon(event_loop, "about_to_wait");
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: TrayShellEvent) {
            match event {
                TrayShellEvent::Restore => {
                    self.lease_stop.store(true, Ordering::Relaxed);
                    let _ = spawn_ui_process(&self.config_path);
                    event_loop.exit();
                }
                TrayShellEvent::Quit => {
                    self.lease_stop.store(true, Ordering::Relaxed);
                    event_loop.exit();
                }
            }
        }
    }

    let show_item = MenuItem::with_id("tray-show", "打开窗口", true, None);
    let quit_item = MenuItem::with_id("tray-quit", "退出程序", true, None);
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    let tray_lease_stop = spawn_ui_lease_heartbeat(config_path.clone());

    let mut event_loop_builder = EventLoop::<TrayShellEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

        event_loop_builder.with_activation_policy(ActivationPolicy::Accessory);
    }

    let event_loop = event_loop_builder
        .build()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event| match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => {
            let _ = proxy.send_event(TrayShellEvent::Restore);
        }
        _ => {}
    }));

    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            let _ = proxy.send_event(TrayShellEvent::Restore);
        } else if event.id == quit_id {
            let _ = proxy.send_event(TrayShellEvent::Quit);
        }
    }));

    let mut app = TrayShellApp {
        config_path,
        lease_stop: tray_lease_stop.clone(),
        tray_icon: None,
        show_item,
        quit_item,
    };

    let result = event_loop
        .run_app(&mut app)
        .map_err(|error| anyhow::anyhow!(error.to_string()));
    tray_lease_stop.store(true, Ordering::Relaxed);
    TrayIconEvent::set_event_handler::<fn(TrayIconEvent)>(None);
    MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
    result
}

#[cfg(target_os = "macos")]
static MACOS_TRAY_CONFIG_PATH: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(target_os = "macos")]
static MACOS_TRAY_COMMAND_TX: std::sync::OnceLock<
    std::sync::Mutex<Option<mpsc::Sender<TrayCommand>>>,
> = std::sync::OnceLock::new();

#[cfg(target_os = "macos")]
static MACOS_MAIN_WINDOW_HANDLE: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct MacosNsPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct MacosNsSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct MacosNsRect {
    origin: MacosNsPoint,
    size: MacosNsSize,
}

#[cfg(target_os = "macos")]
pub fn run_tray_shell(config_path: PathBuf, ready_file: Option<PathBuf>) -> Result<()> {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    let tray_lock = match macos_acquire_tray_shell_lock(ready_file.as_deref())? {
        Some(lock) => lock,
        None => return Ok(()),
    };
    let heartbeat_config_path = config_path.clone();
    let config_slot = MACOS_TRAY_CONFIG_PATH.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut slot) = config_slot.lock() {
        *slot = Some(config_path);
    }

    let tray_lease_stop = spawn_ui_lease_heartbeat(heartbeat_config_path);

    unsafe {
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, finishLaunching];

        let status_bar: *mut Object = msg_send![class!(NSStatusBar), systemStatusBar];
        let status_item = macos_create_status_item(status_bar);
        if status_item.is_null() {
            tray_lease_stop.store(true, Ordering::Relaxed);
            bail!("failed to create macOS status item");
        }
        macos_configure_status_item_visibility(status_item);

        let button: *mut Object = msg_send![status_item, button];
        if button.is_null() {
            tray_lease_stop.store(true, Ordering::Relaxed);
            bail!("failed to get macOS status item button");
        }

        let title = macos_nsstring("");
        let fallback_title = macos_nsstring("LDO");
        let tooltip = macos_nsstring("Linux.do Accelerator");
        let _: () = msg_send![button, setTitle: title];
        let _: () = msg_send![button, setToolTip: tooltip];
        let _: () = msg_send![button, setAccessibilityLabel: tooltip];
        let image = macos_create_status_logo_image();
        if !image.is_null() {
            let _: () = msg_send![image, setTemplate: true];
            let _: () = msg_send![button, setImage: image];
            let _: () = msg_send![button, setImagePosition: 1isize];
            let _: () = msg_send![button, setImageScaling: 3usize];
            log_macos_tray_event("native status item configured template app logo");
        } else {
            let _: () = msg_send![button, setTitle: fallback_title];
            log_macos_tray_event("native status item image unavailable, using compact title=LDO");
        }
        let _: () = msg_send![button, setHidden: false];
        let _: () = msg_send![button, setNeedsDisplay: true];

        let target_class = macos_tray_target_class();
        let target: *mut Object = msg_send![target_class, new];
        let _: () = msg_send![button, setTarget: target];
        let _: () = msg_send![button, setAction: sel!(restore:)];
        let _: () = msg_send![button, setEnabled: true];

        macos_log_status_item_state(status_item, button);
        log_macos_tray_event("native status item created compact icon");
        macos_signal_tray_ready(ready_file.as_deref(), "native status item ready");
        let _: () = msg_send![app, run];

        tray_lease_stop.store(true, Ordering::Relaxed);
        let _: () = msg_send![status_bar, removeStatusItem: status_item];
    }

    drop(tray_lock);
    log_macos_tray_event("native tray-shell exit");
    std::process::exit(0)
}

#[cfg(target_os = "macos")]
struct MacosTrayShellLock {
    path: Option<PathBuf>,
}

#[cfg(target_os = "macos")]
impl Drop for MacosTrayShellLock {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_acquire_tray_shell_lock(ready_file: Option<&Path>) -> Result<Option<MacosTrayShellLock>> {
    use std::io::{ErrorKind, Write as _};

    let lock_path = macos_tray_shell_lock_path();
    for _ in 0..2 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                if let Err(error) = writeln!(file, "{}", std::process::id()) {
                    let _ = fs::remove_file(&lock_path);
                    log_macos_tray_event(&format!(
                        "native tray-shell lock write failed, continuing without lock: {error}"
                    ));
                    return Ok(Some(MacosTrayShellLock { path: None }));
                }
                log_macos_tray_event(&format!(
                    "native tray-shell lock acquired pid={}",
                    std::process::id()
                ));
                return Ok(Some(MacosTrayShellLock {
                    path: Some(lock_path),
                }));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if let Some(existing_pid) = macos_live_tray_shell_pid_from_lock(&lock_path) {
                    log_macos_tray_event(&format!(
                        "native tray-shell pid {existing_pid} is already running; exiting duplicate helper"
                    ));
                    macos_signal_tray_ready(ready_file, "duplicate helper acknowledged");
                    return Ok(None);
                }
                let _ = fs::remove_file(&lock_path);
            }
            Err(error) => {
                log_macos_tray_event(&format!(
                    "native tray-shell lock unavailable, continuing without lock: {error}"
                ));
                return Ok(Some(MacosTrayShellLock { path: None }));
            }
        }
    }

    log_macos_tray_event(
        "native tray-shell stale lock could not be replaced; continuing without lock",
    );
    Ok(Some(MacosTrayShellLock { path: None }))
}

#[cfg(target_os = "macos")]
fn macos_existing_tray_shell_pid() -> Option<u32> {
    let lock_path = macos_tray_shell_lock_path();
    let pid = macos_live_tray_shell_pid_from_lock(&lock_path);
    if pid.is_none() {
        let _ = fs::remove_file(lock_path);
    }
    pid
}

#[cfg(target_os = "macos")]
fn macos_live_tray_shell_pid_from_lock(path: &Path) -> Option<u32> {
    let existing_pid = fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse::<u32>().ok())?;
    if existing_pid != std::process::id() && macos_tray_shell_pid_alive(existing_pid) {
        Some(existing_pid)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn macos_tray_shell_pid_alive(pid: u32) -> bool {
    let pid_string = pid.to_string();
    let output = std::process::Command::new("/bin/ps")
        .args(["-ww", "-p", &pid_string, "-o", "command="])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|command| {
            command.contains("tray-shell")
                && (command.contains("linuxdo-accelerator")
                    || command.contains("LinuxdoAcceleratorTray"))
        })
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn macos_tray_shell_lock_path() -> PathBuf {
    std::env::temp_dir().join("linuxdo-accelerator-macos-tray-shell.pid")
}

#[cfg(target_os = "macos")]
unsafe fn macos_create_status_logo_image() -> *mut objc::runtime::Object {
    use objc::{class, msg_send, sel, sel_impl};

    let icon = branding::icon_data(64);
    let width = icon.width as usize;
    let height = icon.height as usize;
    let bytes_per_row = width * 4;
    let color_space_name = unsafe { macos_nsstring("NSDeviceRGBColorSpace") };

    let bitmap_rep: *mut objc::runtime::Object =
        unsafe { msg_send![class!(NSBitmapImageRep), alloc] };
    let bitmap_rep: *mut objc::runtime::Object = unsafe {
        msg_send![
            bitmap_rep,
            initWithBitmapDataPlanes: std::ptr::null_mut::<*mut u8>()
            pixelsWide: width
            pixelsHigh: height
            bitsPerSample: 8usize
            samplesPerPixel: 4usize
            hasAlpha: true
            isPlanar: false
            colorSpaceName: color_space_name
            bitmapFormat: 0usize
            bytesPerRow: bytes_per_row
            bitsPerPixel: 32usize
        ]
    };
    if bitmap_rep.is_null() {
        return std::ptr::null_mut();
    }

    let bitmap_data: *mut u8 = unsafe { msg_send![bitmap_rep, bitmapData] };
    if bitmap_data.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        std::ptr::copy_nonoverlapping(icon.rgba.as_ptr(), bitmap_data, icon.rgba.len());
    }

    let image: *mut objc::runtime::Object = unsafe { msg_send![class!(NSImage), alloc] };
    let image: *mut objc::runtime::Object = unsafe {
        msg_send![
            image,
            initWithSize: MacosNsSize {
                width: 18.0,
                height: 18.0,
            }
        ]
    };
    if image.is_null() {
        return std::ptr::null_mut();
    }

    let _: () = unsafe { msg_send![image, addRepresentation: bitmap_rep] };
    let _: () = unsafe {
        msg_send![
            image,
            setSize: MacosNsSize {
                width: 18.0,
                height: 18.0,
            }
        ]
    };
    image
}

#[cfg(target_os = "macos")]
unsafe fn macos_nsstring(value: &str) -> *mut objc::runtime::Object {
    use objc::{class, msg_send, sel, sel_impl};

    let ns_string: *mut objc::runtime::Object = msg_send![class!(NSString), alloc];
    msg_send![
        ns_string,
        initWithBytes: value.as_ptr()
        length: value.len()
        encoding: 4usize
    ]
}

#[cfg(target_os = "macos")]
unsafe fn macos_create_status_item(
    status_bar: *mut objc::runtime::Object,
) -> *mut objc::runtime::Object {
    use objc::{msg_send, sel, sel_impl};

    let supports_insert_order: bool = unsafe {
        msg_send![status_bar, respondsToSelector: sel!(_statusItemWithLength:systemInsertOrder:)]
    };
    if supports_insert_order {
        // New status items are otherwise inserted at the left edge of the
        // status-item area. On crowded menu bars that can put the item under
        // the foreground app's menu text. A high system insert order keeps the
        // tray item with the visible menu extras on the right.
        let status_item: *mut objc::runtime::Object = unsafe {
            msg_send![status_bar, _statusItemWithLength: -2.0f64 systemInsertOrder: 10_000isize]
        };
        log_macos_tray_event("native status item using systemInsertOrder=10000");
        return status_item;
    }

    unsafe { msg_send![status_bar, statusItemWithLength: -2.0f64] }
}

#[cfg(target_os = "macos")]
unsafe fn macos_configure_status_item_visibility(status_item: *mut objc::runtime::Object) {
    use objc::{msg_send, sel, sel_impl};

    let autosave_name = unsafe { macos_nsstring("io.linuxdo.accelerator.tray") };
    let supports_autosave: bool =
        unsafe { msg_send![status_item, respondsToSelector: sel!(setAutosaveName:)] };
    if supports_autosave {
        let _: () = unsafe { msg_send![status_item, setAutosaveName: autosave_name] };
    }

    let supports_behavior: bool =
        unsafe { msg_send![status_item, respondsToSelector: sel!(setBehavior:)] };
    if supports_behavior {
        // NSStatusItemBehaviorRemovalAllowed. This also makes the autosaved
        // visible/preferred-position state meaningful to SystemUIServer.
        let _: () = unsafe { msg_send![status_item, setBehavior: 2usize] };
    }

    let supports_drop_priority: bool =
        unsafe { msg_send![status_item, respondsToSelector: sel!(_setDropPriority:)] };
    if supports_drop_priority {
        let _: () = unsafe { msg_send![status_item, _setDropPriority: 1_000_000isize] };
    }

    let supports_overflow_priority: bool =
        unsafe { msg_send![status_item, respondsToSelector: sel!(_setOverflowSpecifierPriority:)] };
    if supports_overflow_priority {
        let _: () =
            unsafe { msg_send![status_item, _setOverflowSpecifierPriority: 1_000_000isize] };
    }

    let supports_visible: bool =
        unsafe { msg_send![status_item, respondsToSelector: sel!(setVisible:)] };
    if supports_visible {
        let _: () = unsafe { msg_send![status_item, setVisible: true] };
    }
}

#[cfg(target_os = "macos")]
unsafe fn macos_set_status_item_visible(status_item: *mut objc::runtime::Object, visible: bool) {
    use objc::{msg_send, sel, sel_impl};

    if status_item.is_null() {
        return;
    }

    let supports_visible: bool =
        unsafe { msg_send![status_item, respondsToSelector: sel!(setVisible:)] };
    if supports_visible {
        let _: () = unsafe { msg_send![status_item, setVisible: visible] };
        return;
    }

    let button: *mut objc::runtime::Object = unsafe { msg_send![status_item, button] };
    if !button.is_null() {
        let _: () = unsafe { msg_send![button, setHidden: !visible] };
        let _: () = unsafe { msg_send![button, setNeedsDisplay: true] };
    }
}

#[cfg(target_os = "macos")]
fn macos_store_tray_command_tx(sender: mpsc::Sender<TrayCommand>) {
    let slot = MACOS_TRAY_COMMAND_TX.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(sender);
    }
}

#[cfg(target_os = "macos")]
fn macos_clear_tray_command_tx() {
    if let Some(slot) = MACOS_TRAY_COMMAND_TX.get() {
        if let Ok(mut slot) = slot.lock() {
            *slot = None;
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_send_tray_command(command: TrayCommand) -> bool {
    MACOS_TRAY_COMMAND_TX
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|slot| slot.clone()))
        .is_some_and(|sender| sender.send(command).is_ok())
}

#[cfg(target_os = "macos")]
fn build_macos_tray_state(ctx: &egui::Context) -> (Option<TrayState>, Receiver<TrayCommand>) {
    let (event_tx, event_rx) = mpsc::channel();
    macos_store_tray_command_tx(event_tx.clone());

    let menu = Menu::new();
    let show_item = MenuItem::with_id("tray-show", "打开窗口", true, None);
    let quit_item = MenuItem::with_id("tray-quit", "退出程序", true, None);
    if menu
        .append_items(&[&show_item, &PredefinedMenuItem::separator(), &quit_item])
        .is_err()
    {
        macos_clear_tray_command_tx();
        return (None, event_rx);
    }

    let tray_icon = match tray_window_icon() {
        Ok(icon) => TrayIconBuilder::new()
            .with_id("linuxdo-accelerator-macos-tray")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Linux.do Accelerator")
            .with_icon(icon)
            .with_icon_as_template(false)
            .build()
            .ok(),
        Err(_) => None,
    };

    let Some(tray_icon) = tray_icon else {
        macos_clear_tray_command_tx();
        log_macos_tray_event("tray-icon status item creation failed");
        return (None, event_rx);
    };

    let _ = tray_icon.set_visible(true);

    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    let event_tx_click = event_tx.clone();
    let ctx_menu = ctx.clone();
    let ctx_tray = ctx.clone();

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            let _ = event_tx.send(TrayCommand::Restore);
            ctx_menu.request_repaint();
        } else if event.id == quit_id {
            let _ = event_tx.send(TrayCommand::Quit);
            ctx_menu.request_repaint();
        }
    }));

    TrayIconEvent::set_event_handler(Some(move |event| match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => {
            let _ = event_tx_click.send(TrayCommand::Restore);
            ctx_tray.request_repaint();
        }
        _ => {}
    }));

    log_macos_tray_event("tray-icon status item created visible color icon");
    (
        Some(TrayState {
            tray_icon,
            visible: true,
        }),
        event_rx,
    )
}

#[cfg(target_os = "macos")]
fn macos_write_tray_ready_file(path: &Path) -> Result<()> {
    fs::write(path, format!("ready pid={}\n", std::process::id()))
        .with_context(|| format!("failed to write macOS tray ready file {}", path.display()))
}

#[cfg(target_os = "macos")]
fn macos_signal_tray_ready(path: Option<&Path>, source: &str) {
    let Some(path) = path else {
        return;
    };

    match macos_write_tray_ready_file(path) {
        Ok(()) => log_macos_tray_event(&format!("{source}: {}", path.display())),
        Err(error) => log_macos_tray_event(&format!("{source} signal failed: {error:#}")),
    }
}

#[cfg(target_os = "macos")]
unsafe fn macos_log_status_item_state(
    status_item: *mut objc::runtime::Object,
    button: *mut objc::runtime::Object,
) {
    use objc::{msg_send, sel, sel_impl};

    let length: f64 = unsafe { msg_send![status_item, length] };
    let frame: MacosNsRect = unsafe { msg_send![button, frame] };
    let hidden: bool = unsafe { msg_send![button, isHidden] };
    let window: *mut objc::runtime::Object = unsafe { msg_send![button, window] };
    let window_null = window.is_null();
    let window_frame = if window_null {
        "none".to_string()
    } else {
        let frame: MacosNsRect = unsafe { msg_send![window, frame] };
        format!(
            "({:.1},{:.1},{:.1},{:.1})",
            frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
        )
    };

    log_macos_tray_event(&format!(
        "native status item state length={length:.1} button_frame=({:.1},{:.1},{:.1},{:.1}) hidden={hidden} window_null={window_null} window_frame={window_frame}",
        frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
    ));
}

#[cfg(target_os = "macos")]
fn macos_tray_target_class() -> &'static objc::runtime::Class {
    use objc::declare::ClassDecl;
    use objc::runtime::{Class, Object, Sel};
    use objc::{class, sel, sel_impl};

    if let Some(class) = Class::get("LinuxdoAcceleratorTrayTarget") {
        return class;
    }

    extern "C" fn restore(_this: &Object, _cmd: Sel, _sender: *mut Object) {
        macos_restore_from_native_tray();
    }

    let superclass = class!(NSObject);
    let mut declaration =
        ClassDecl::new("LinuxdoAcceleratorTrayTarget", superclass).expect("unique class name");
    unsafe {
        declaration.add_method(
            sel!(restore:),
            restore as extern "C" fn(&Object, Sel, *mut Object),
        );
    }
    declaration.register()
}

#[cfg(target_os = "macos")]
fn macos_restore_from_native_tray() {
    use objc::{class, msg_send, sel, sel_impl};

    log_macos_tray_event("native status item restore clicked");
    if macos_send_tray_command(TrayCommand::Restore) {
        macos_set_activation_policy_regular();
        macos_restore_app_window();
        return;
    }

    if let Some(config_path) = MACOS_TRAY_CONFIG_PATH
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|slot| slot.clone()))
    {
        let _ = spawn_ui_process(&config_path);
    }

    unsafe {
        let app: *mut objc::runtime::Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, terminate: std::ptr::null_mut::<objc::runtime::Object>()];
    }
}

struct AcceleratorApp {
    config_path: PathBuf,
    config: AppConfig,
    edge_node_input: String,
    owns_ui_lease: bool,
    ui_lease_stop: Option<Arc<AtomicBool>>,
    status: ServiceState,
    recent_logs: Vec<String>,
    feedback: String,
    busy: bool,
    action_rx: Option<Receiver<Result<String, String>>>,
    pending_action: Option<GuiAction>,
    confirm_action: Option<GuiAction>,
    optimistic_running: Option<(bool, Instant)>,
    drag_blockers: Vec<egui::Rect>,
    center_window_pending: bool,
    last_refresh: Instant,
    config_modified_at: Option<SystemTime>,
    runtime_log_modified_at: Option<SystemTime>,
    current_page: UiPage,
    logo: egui::TextureHandle,
    autostart_enabled: bool,
    autostart_pending: bool,
    #[cfg(target_os = "linux")]
    hidden_to_tray: bool,
    #[cfg(target_os = "linux")]
    last_minimized: bool,
    #[cfg(target_os = "macos")]
    hidden_to_tray: bool,
    #[cfg(target_os = "macos")]
    last_minimized: bool,
    #[cfg(target_os = "macos")]
    tray: Option<TrayState>,
    #[cfg(target_os = "macos")]
    tray_rx: Receiver<TrayCommand>,
    #[cfg(target_os = "macos")]
    window_handle: Option<usize>,
    #[cfg(target_os = "windows")]
    tray: Option<TrayState>,
    #[cfg(target_os = "windows")]
    tray_rx: Receiver<TrayCommand>,
    #[cfg(target_os = "windows")]
    window_handle: Option<isize>,
    #[cfg(target_os = "windows")]
    hidden_to_tray: bool,
    #[cfg(target_os = "windows")]
    last_minimized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiPage {
    Launcher,
    Details,
}

#[cfg(target_os = "windows")]
struct TrayState {
    tray_icon: TrayIcon,
}

#[cfg(target_os = "macos")]
struct TrayState {
    tray_icon: TrayIcon,
    visible: bool,
}

#[cfg(target_os = "macos")]
impl TrayState {
    fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        let _ = self.tray_icon.set_visible(visible);
        self.visible = visible;
    }
}

#[cfg(target_os = "macos")]
impl Drop for TrayState {
    fn drop(&mut self) {
        let _ = self.tray_icon.set_visible(false);
        TrayIconEvent::set_event_handler::<fn(TrayIconEvent)>(None);
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
        macos_clear_tray_command_tx();
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Debug, Clone, Copy)]
enum TrayCommand {
    Restore,
    Quit,
}

impl AcceleratorApp {
    fn new(
        config_path: PathBuf,
        auto_start: bool,
        center_window_pending: bool,
        cc: &eframe::CreationContext<'_>,
    ) -> Self {
        install_fonts(&cc.egui_ctx);
        install_theme(&cc.egui_ctx);

        let config = AppConfig::load_or_create(&config_path).unwrap_or_default();
        let autostart_enabled = autostart::is_enabled();
        let edge_node_input = config.edge_node_override().unwrap_or_default().to_string();
        let config_modified_at = file_modified_at(&config_path);
        let status = service::status(Some(config_path.clone())).unwrap_or_default();
        let owns_ui_lease = service::resolve_paths(Some(config_path.clone()))
            .ok()
            .and_then(|paths| state::read_ui_lease(&paths).ok().flatten())
            .is_some();
        let ui_lease_stop = if owns_ui_lease {
            Some(spawn_ui_lease_heartbeat(config_path.clone()))
        } else {
            None
        };
        let recent_logs = load_recent_runtime_logs(&config_path);
        let runtime_log_modified_at = runtime_log_file_modified_at(&config_path);
        #[cfg(target_os = "windows")]
        schedule_windows_shortcut_icon_refresh(&config_path);
        let logo = cc.egui_ctx.load_texture(
            "linuxdo-logo",
            branding::logo_image(96),
            egui::TextureOptions::LINEAR,
        );
        #[cfg(target_os = "windows")]
        let window_handle = capture_native_window_handle(cc);
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = window_handle {
            let _ = apply_app_window_icon(hwnd);
        }
        #[cfg(target_os = "windows")]
        let (tray, tray_rx) = build_windows_tray_state(&cc.egui_ctx, window_handle);
        #[cfg(target_os = "macos")]
        let window_handle = capture_macos_window_handle(cc);
        #[cfg(target_os = "macos")]
        macos_store_main_window_handle(window_handle);
        #[cfg(target_os = "macos")]
        let (tray, tray_rx) = build_macos_tray_state(&cc.egui_ctx);
        Self {
            config_path,
            config,
            edge_node_input,
            owns_ui_lease,
            ui_lease_stop,
            status,
            recent_logs,
            feedback: String::new(),
            busy: false,
            action_rx: None,
            pending_action: None,
            confirm_action: None,
            optimistic_running: None,
            drag_blockers: Vec::new(),
            center_window_pending,
            last_refresh: Instant::now() - Duration::from_secs(2),
            config_modified_at,
            runtime_log_modified_at,
            current_page: UiPage::Launcher,
            logo,
            autostart_enabled,
            autostart_pending: auto_start,
            #[cfg(target_os = "linux")]
            hidden_to_tray: false,
            #[cfg(target_os = "linux")]
            last_minimized: false,
            #[cfg(target_os = "macos")]
            hidden_to_tray: false,
            #[cfg(target_os = "macos")]
            last_minimized: false,
            #[cfg(target_os = "macos")]
            tray,
            #[cfg(target_os = "macos")]
            tray_rx,
            #[cfg(target_os = "macos")]
            window_handle,
            #[cfg(target_os = "windows")]
            tray,
            #[cfg(target_os = "windows")]
            tray_rx,
            #[cfg(target_os = "windows")]
            window_handle,
            #[cfg(target_os = "windows")]
            hidden_to_tray: false,
            #[cfg(target_os = "windows")]
            last_minimized: false,
        }
    }

    fn refresh_status(&mut self) {
        if let Ok(status) = service::status(Some(self.config_path.clone())) {
            self.status = self.apply_optimistic_state(status);
        }
        let current_log_modified_at = runtime_log_file_modified_at(&self.config_path);
        if current_log_modified_at != self.runtime_log_modified_at {
            self.recent_logs = load_recent_runtime_logs(&self.config_path);
            self.runtime_log_modified_at = current_log_modified_at;
        }
        let current_config_modified_at = file_modified_at(&self.config_path);
        if current_config_modified_at != self.config_modified_at {
            if let Ok(config) = AppConfig::load_or_create(&self.config_path) {
                self.config = config;
                self.edge_node_input = self
                    .config
                    .edge_node_override()
                    .unwrap_or_default()
                    .to_string();
            }
            self.config_modified_at = current_config_modified_at;
        }
    }

    fn apply_optimistic_state(&mut self, mut status: ServiceState) -> ServiceState {
        if let Some((running, deadline)) = self.optimistic_running {
            if Instant::now() >= deadline {
                self.optimistic_running = None;
                return status;
            }

            if running && !status.running && status.last_error.is_none() {
                status.running = true;
                status.status_text = "加速中".to_string();
            }

            if !running && status.running && status.last_error.is_none() {
                status.running = false;
                status.pid = None;
                status.status_text = "已停止".to_string();
            }
        }

        status
    }

    fn trigger_action(&mut self, action: GuiAction) {
        if self.busy {
            return;
        }

        if matches!(action, GuiAction::Start) {
            self.owns_ui_lease = self.touch_ui_lease().is_ok();
            if self.owns_ui_lease {
                if let Some(stop) = self.ui_lease_stop.take() {
                    stop.store(true, Ordering::Relaxed);
                }
                self.ui_lease_stop = Some(spawn_ui_lease_heartbeat(self.config_path.clone()));
            }
        }

        self.busy = true;
        self.feedback = action.pending_message().to_string();

        let config_path = self.config_path.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result =
                execute_action(&config_path, action).map_err(|error| format_error_chain(&error));
            let _ = tx.send(result);
        });
        self.action_rx = Some(rx);
        self.pending_action = Some(action);
    }

    fn poll_action(&mut self) {
        if let Some(rx) = &self.action_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.busy = false;
                    match result {
                        Ok(message) => {
                            self.feedback = message;
                            let deadline = Instant::now() + Duration::from_secs(4);
                            match self.pending_action {
                                Some(GuiAction::Start) => {
                                    self.status.running = true;
                                    self.status.status_text = "加速中".to_string();
                                    self.status.last_error = None;
                                    self.optimistic_running = Some((true, deadline));
                                }
                                Some(GuiAction::Stop) => {
                                    self.status.running = false;
                                    self.status.status_text = "已停止".to_string();
                                    self.status.last_error = None;
                                    if let Some(stop) = self.ui_lease_stop.take() {
                                        stop.store(true, Ordering::Relaxed);
                                    }
                                    self.clear_ui_lease();
                                    self.owns_ui_lease = false;
                                    self.optimistic_running = Some((false, deadline));
                                }
                                None => {}
                            }
                        }
                        Err(message) => {
                            self.optimistic_running = None;
                            match self.pending_action {
                                Some(GuiAction::Start) => {
                                    if let Some(stop) = self.ui_lease_stop.take() {
                                        stop.store(true, Ordering::Relaxed);
                                    }
                                    self.clear_ui_lease();
                                    self.owns_ui_lease = false;
                                    self.status.running = false;
                                    self.status.pid = None;
                                    self.status.status_text = "启动失败".to_string();
                                    self.status.last_error = Some(message.clone());
                                }
                                _ => {
                                    self.refresh_status();
                                    self.status.last_error = Some(message.clone());
                                }
                            }
                            self.feedback = format!("操作失败: {message}");
                        }
                    }
                    self.last_refresh = Instant::now() - Duration::from_secs(2);
                    self.action_rx = None;
                    self.pending_action = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.busy = false;
                    self.optimistic_running = None;
                    self.feedback = "后台任务意外中断".to_string();
                    self.action_rx = None;
                    self.pending_action = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn headline_status(&self) -> (&'static str, egui::Color32) {
        if self.busy {
            return ("处理中", egui::Color32::from_rgb(250, 196, 92));
        }
        if self.status.running {
            return ("已接管", egui::Color32::from_rgb(106, 220, 155));
        }
        if self.status.last_error.is_some() {
            return ("异常", egui::Color32::from_rgb(255, 120, 100));
        }
        ("待启动", egui::Color32::from_rgb(162, 173, 184))
    }

    fn launcher_status_summary(&self) -> String {
        if self.busy {
            return "正在申请权限并准备环境".to_string();
        }
        if let Some(error) = self
            .status
            .last_error
            .as_deref()
            .or_else(|| self.feedback.strip_prefix("操作失败: "))
        {
            return summarize_launcher_error(error);
        }
        if self.status.running {
            return "本地加速已启用".to_string();
        }
        "点击左侧按钮即可开始".to_string()
    }

    fn recent_logs_or_placeholder(&self) -> Vec<String> {
        if self.recent_logs.is_empty() {
            vec!["暂无运行日志。执行开始、停止、恢复等操作后会在这里显示。".to_string()]
        } else {
            self.recent_logs.clone()
        }
    }

    fn http_listen_address(&self) -> String {
        format!(
            "http://{}:{}",
            self.config.listen_host, self.config.http_port
        )
    }

    fn https_listen_address(&self) -> String {
        format!(
            "https://{}:{}",
            self.config.listen_host, self.config.https_port
        )
    }

    fn listen_state_label(&self) -> &'static str {
        if self.status.running {
            "已监听"
        } else {
            "未监听"
        }
    }

    fn ip_preference_label(&self) -> &'static str {
        if self.config.managed_prefer_ipv6 {
            "IPv6 优先"
        } else {
            "IPv4 优先"
        }
    }

    fn edge_node_label(&self) -> &str {
        self.config.edge_node_override().unwrap_or("自动")
    }

    fn save_current_config(&mut self) -> Result<()> {
        let serialized =
            toml::to_string_pretty(&self.config).context("failed to serialize config")?;
        fs::write(&self.config_path, serialized)
            .with_context(|| format!("failed to write config {}", self.config_path.display()))?;
        self.config_modified_at = file_modified_at(&self.config_path);
        Ok(())
    }

    fn set_edge_node_override(&mut self) {
        if self.status.running {
            self.feedback = "请先停止加速，再修改边缘节点".to_string();
            return;
        }

        let next_value = self.edge_node_input.trim();
        let next_value = if next_value.is_empty() {
            None
        } else {
            Some(next_value.to_string())
        };

        if self.config.edge_node_override() == next_value.as_deref() {
            self.feedback = "边缘节点未变化".to_string();
            return;
        }

        self.config.edge_node = next_value;
        match self.save_current_config() {
            Ok(()) => {
                self.edge_node_input = self
                    .config
                    .edge_node_override()
                    .unwrap_or_default()
                    .to_string();
                self.feedback = if self.config.edge_node_override().is_some() {
                    format!("已设置边缘节点：{}", self.edge_node_label())
                } else {
                    "已恢复自动选择边缘节点".to_string()
                };
            }
            Err(error) => {
                self.feedback = format!("保存配置失败: {}", format_error_chain(&error));
            }
        }
    }

    fn set_ip_preference(&mut self, prefer_ipv6: bool) {
        if self.status.running {
            self.feedback = "请先停止加速，再切换 IPv4 / IPv6 优先级".to_string();
            return;
        }
        if self.config.managed_prefer_ipv6 == prefer_ipv6 {
            return;
        }
        self.config.managed_prefer_ipv6 = prefer_ipv6;
        match self.save_current_config() {
            Ok(()) => {
                self.feedback = if self.status.running {
                    format!("已切换为{}，重启加速后生效", self.ip_preference_label())
                } else {
                    format!("已切换为{}", self.ip_preference_label())
                };
            }
            Err(error) => {
                self.feedback = format!("保存配置失败: {}", format_error_chain(&error));
            }
        }
    }

    fn render_ip_preference_toggle(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 0.0);
            let ip_toggle_enabled = !self.busy && !self.status.running;
            let ipv4_response = ui.add(ip_priority_button(
                "IPv4",
                !self.config.managed_prefer_ipv6,
                ip_toggle_enabled,
            ));
            self.register_drag_blocker(ipv4_response.rect);
            if ipv4_response.clicked() {
                self.set_ip_preference(false);
            }

            let ipv6_response = ui.add(ip_priority_button(
                "IPv6",
                self.config.managed_prefer_ipv6,
                ip_toggle_enabled,
            ));
            self.register_drag_blocker(ipv6_response.rect);
            if ipv6_response.clicked() {
                self.set_ip_preference(true);
            }

            ui.add_space(4.0);
            let details_response = ui.add(launcher_secondary_button(
                "查看详情",
                egui::vec2(80.0, 26.0),
                true,
            ));
            self.register_drag_blocker(details_response.rect);
            if details_response.clicked() {
                self.navigate_to(ctx, UiPage::Details);
            }
        });
    }

    fn register_drag_blocker(&mut self, rect: egui::Rect) {
        self.drag_blockers.push(rect);
    }

    fn drag_area(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, rect: egui::Rect, id: &str) {
        if use_native_wayland_frame() {
            let _ = (ui, ctx, rect, id);
            return;
        }
        let _ = (ui, id);
        let pressed_on_drag_area = ctx.input(|i| {
            i.pointer.primary_pressed()
                && i.pointer
                    .interact_pos()
                    .map(|pos| {
                        rect.contains(pos)
                            && !self
                                .drag_blockers
                                .iter()
                                .any(|blocked| blocked.contains(pos))
                    })
                    .unwrap_or(false)
        });
        if pressed_on_drag_area {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
    }

    fn render_window_title_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let width = ui.available_width();
        let inner = ui.allocate_ui_with_layout(
            egui::vec2(width, TITLE_BAR_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 0.0);
                let drag_width = (ui.available_width() - 134.0).max(180.0);
                let drag_area = ui.allocate_ui_with_layout(
                    egui::vec2(drag_width, TITLE_BAR_HEIGHT),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add_space(12.0);
                        ui.add(egui::Image::new((self.logo.id(), egui::vec2(30.0, 30.0))));
                        ui.label(
                            RichText::new(APP_WINDOW_TITLE)
                                .font(FontId::proportional(17.0))
                                .strong()
                                .color(egui::Color32::from_rgb(244, 245, 247)),
                        );
                    },
                );
                self.drag_area(ui, ctx, drag_area.response.rect, "title_bar_drag");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(6.0);
                    let close_response = ui
                        .add(title_bar_button(
                            "X",
                            egui::vec2(38.0, 28.0),
                            true,
                            true,
                            false,
                        ))
                        .on_hover_text("关闭");
                    self.register_drag_blocker(close_response.rect);
                    if close_response.clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.add_space(2.0);
                    let minimize_response = ui
                        .add(title_bar_button(
                            "_",
                            egui::vec2(38.0, 28.0),
                            false,
                            true,
                            false,
                        ))
                        .on_hover_text("最小化到任务栏");
                    self.register_drag_blocker(minimize_response.rect);
                    if minimize_response.clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    }
                    ui.add_space(2.0);
                    let tray_response = ui
                        .add(title_bar_button(
                            "↧",
                            egui::vec2(38.0, 28.0),
                            false,
                            true,
                            true,
                        ))
                        .on_hover_text("最小化到托盘");
                    self.register_drag_blocker(tray_response.rect);
                    if tray_response.clicked() {
                        self.minimize_to_tray(ctx);
                    }
                });
            },
        );

        ui.painter().line_segment(
            [
                inner.response.rect.left_bottom(),
                inner.response.rect.right_bottom(),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(48, 53, 61)),
        );
    }

    fn render_brand_banner(&self, ui: &mut egui::Ui, title: &str, summary: &str) {
        let (headline, accent) = self.headline_status();
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(22, 26, 32))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 56, 64)))
            .inner_margin(egui::Margin::symmetric(16, 14))
            .corner_radius(egui::CornerRadius::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new((self.logo.id(), egui::vec2(32.0, 32.0))));
                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(title)
                                    .font(FontId::proportional(17.0))
                                    .strong()
                                    .color(egui::Color32::from_rgb(244, 245, 247)),
                            );
                            ui.label(
                                RichText::new(format!("v{APP_VERSION}"))
                                    .font(FontId::proportional(10.5))
                                    .color(egui::Color32::from_rgb(140, 150, 160)),
                            );
                        });
                        ui.label(
                            RichText::new(summary)
                                .font(FontId::proportional(11.5))
                                .color(egui::Color32::from_rgb(155, 164, 172)),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        egui::Frame::new()
                            .fill(accent.linear_multiply(0.14))
                            .stroke(egui::Stroke::new(1.0, accent.linear_multiply(0.5)))
                            .inner_margin(egui::Margin::symmetric(12, 5))
                            .corner_radius(egui::CornerRadius::same(255))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(headline)
                                        .font(FontId::proportional(11.0))
                                        .strong()
                                        .color(accent),
                                );
                            });
                    });
                });
            });
    }

    fn render_launcher_status_card(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        min_height: f32,
    ) {
        let (headline, accent) = self.headline_status();
        let summary_text = self.launcher_status_summary();
        let status_title = match headline {
            "已接管" => "加速已生效",
            "处理中" => "正在处理中",
            "异常" => "当前异常",
            _ => "等待启动",
        };

        let outer_rect = ui.available_rect_before_wrap();
        let response = egui::Frame::new()
            .fill(egui::Color32::from_rgb(28, 33, 39))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(66, 72, 82)))
            .inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 10,
                bottom: 10,
            })
            .corner_radius(egui::CornerRadius::same(14))
            .show(ui, |ui| {
                ui.set_min_height(min_height);
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 0.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        egui::Frame::new()
                            .fill(accent.linear_multiply(0.12))
                            .stroke(egui::Stroke::new(1.0, accent.linear_multiply(0.45)))
                            .inner_margin(egui::Margin::symmetric(7, 2))
                            .corner_radius(egui::CornerRadius::same(255))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new("服务状态")
                                        .font(FontId::proportional(9.4))
                                        .strong()
                                        .color(accent),
                                );
                            });
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let (dot_rect, _) =
                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot_rect.center(), 4.0, accent);
                        ui.label(
                            RichText::new(status_title)
                                .font(FontId::proportional(14.2))
                                .strong()
                                .color(egui::Color32::from_rgb(242, 245, 247)),
                        );
                    });
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(summary_text)
                            .font(FontId::proportional(9.8))
                            .color(egui::Color32::from_rgb(208, 214, 219)),
                    );
                });
            });

        self.drag_area(ui, ctx, response.response.rect, "launcher_status_card_drag");

        // Draw accent color bar on the left edge
        let bar_rect = egui::Rect::from_min_size(
            outer_rect.min,
            egui::vec2(4.0, ui.min_rect().height().max(44.0)),
        );
        ui.painter().rect_filled(
            bar_rect,
            egui::CornerRadius {
                nw: 14,
                sw: 14,
                ne: 0,
                se: 0,
            },
            accent,
        );
    }

    fn render_action_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(26, 30, 36))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 56, 64)))
            .inner_margin(egui::Margin::same(8))
            .corner_radius(egui::CornerRadius::same(14))
            .show(ui, |ui| {
                let primary_label = if self.status.running {
                    "停止加速"
                } else {
                    "开始加速"
                };
                let (primary_fill, primary_text, primary_stroke) = if self.status.running {
                    (
                        egui::Color32::from_rgb(186, 63, 21),
                        egui::Color32::from_rgb(248, 245, 243),
                        egui::Color32::from_rgb(223, 109, 51),
                    )
                } else {
                    (
                        egui::Color32::from_rgb(229, 171, 66),
                        egui::Color32::from_rgb(29, 24, 16),
                        egui::Color32::from_rgb(214, 158, 59),
                    )
                };

                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                ui.vertical(|ui| {
                    let gap = 6.0;
                    let right_width = 144.0;
                    let left_width = ui.available_width() - gap - right_width;
                    let stack_height = 92.0;
                    let button_height = 48.0;
                    let footer_height = 38.0;

                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(left_width, stack_height),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                let primary_response = ui.add_sized(
                                    [left_width, button_height],
                                    launcher_primary_button(
                                        primary_label,
                                        primary_fill,
                                        primary_text,
                                        primary_stroke,
                                        egui::vec2(left_width, button_height),
                                        !self.busy,
                                    ),
                                );
                                self.register_drag_blocker(primary_response.rect);
                                if primary_response.clicked() {
                                    let action = if self.status.running {
                                        GuiAction::Stop
                                    } else {
                                        GuiAction::Start
                                    };
                                    self.trigger_action(action);
                                }

                                ui.add_space(6.0);
                                self.render_launcher_footer(ui, ctx, footer_height);
                            },
                        );

                        ui.add_space(gap);
                        ui.allocate_ui_with_layout(
                            egui::vec2(right_width, stack_height),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                self.render_launcher_status_card(ui, ctx, stack_height);
                            },
                        );
                    });
                });
            });
    }

    fn render_details_content(&mut self, ui: &mut egui::Ui) {
        self.render_brand_banner(ui, "详情与设置", "集中查看状态、配置与工具信息");
        ui.add_space(8.0);
        if ui.available_width() >= 680.0 {
            ui.columns(2, |columns| {
                columns[0].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                self.render_status_panel(&mut columns[0]);
                self.render_scope_panel(&mut columns[0]);

                columns[1].spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                self.render_config_panel(&mut columns[1]);
                self.render_autostart_panel(&mut columns[1]);
                self.render_project_panel(&mut columns[1]);
                self.render_tips_panel(&mut columns[1]);
            });
        } else {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            self.render_status_panel(ui);
            self.render_scope_panel(ui);
            self.render_config_panel(ui);
            self.render_autostart_panel(ui);
            self.render_project_panel(ui);
            self.render_tips_panel(ui);
        }
    }

    fn render_autostart_panel(&mut self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(22, 26, 32),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("开机自启")
                    .font(FontId::proportional(13.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(6.0);

            let mut enabled = self.autostart_enabled;
            let toggle = ui.add_enabled(
                !self.busy,
                egui::Checkbox::new(&mut enabled, "开机自动启动加速"),
            );
            self.register_drag_blocker(toggle.rect);
            if toggle.changed() {
                self.set_autostart(enabled);
            }
            ui.add_space(4.0);
            #[cfg(target_os = "windows")]
            let autostart_note =
                "勾选后系统登录时会通过计划任务启动本程序窗口并自动请求加速；首次开启或关闭时需要确认一次管理员/UAC 授权，后续开机无需再次确认。";
            #[cfg(not(target_os = "windows"))]
            let autostart_note =
                "勾选后系统登录时会自动拉起本程序并申请权限启动加速；首次启动仍需要在系统弹窗中确认管理员/UAC 授权。";
            subtle_note(
                ui,
                autostart_note,
            );
        });
    }

    fn render_page_header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, title: &str) {
        ui.horizontal(|ui| {
            let back_response = ui.add(subtle_button("返回", egui::vec2(68.0, 30.0), true));
            self.register_drag_blocker(back_response.rect);
            if back_response.clicked() {
                self.navigate_to(ctx, UiPage::Launcher);
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(title)
                        .font(FontId::proportional(16.0))
                        .strong()
                        .color(egui::Color32::from_rgb(244, 245, 247)),
                );
                ui.label(
                    RichText::new("Linux.do Accelerator")
                        .font(FontId::proportional(10.5))
                        .color(egui::Color32::from_rgb(140, 150, 160)),
                );
            });
        });
        ui.add_space(8.0);
    }

    fn navigate_to(&mut self, ctx: &egui::Context, page: UiPage) {
        self.current_page = page;
        match page {
            UiPage::Launcher => {
                let size = launcher_window_size();
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(size));
                ctx.send_viewport_cmd(egui::ViewportCommand::MaxInnerSize(size));
                ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(false));
            }
            UiPage::Details => {
                let size = egui::vec2(DETAILS_WINDOW_SIZE[0], DETAILS_WINDOW_SIZE[1]);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                    720.0, 520.0,
                )));
                ctx.send_viewport_cmd(egui::ViewportCommand::MaxInnerSize(egui::vec2(
                    1200.0, 900.0,
                )));
                ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(true));
            }
        }
        ctx.request_repaint();
    }

    fn render_launcher_footer(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, min_height: f32) {
        let response = egui::Frame::new()
            .fill(egui::Color32::from_rgb(24, 28, 34))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(52, 58, 68)))
            .inner_margin(egui::Margin::symmetric(14, 8))
            .corner_radius(egui::CornerRadius::same(12))
            .show(ui, |ui| {
                ui.set_min_height(min_height);
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 0.0);
                ui.horizontal(|ui| {
                    let total_width = ui.available_width();
                    let toggle_width = 240.0;
                    let left_width = (total_width - toggle_width - 16.0).max(120.0);

                    ui.allocate_ui_with_layout(
                        egui::vec2(left_width, min_height - 4.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.add(egui::Image::new((self.logo.id(), egui::vec2(20.0, 20.0))));
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new("linux.do专属加速器")
                                    .font(FontId::proportional(11.2))
                                    .strong()
                                    .color(egui::Color32::from_rgb(236, 240, 243)),
                            );
                        },
                    );

                    ui.allocate_ui_with_layout(
                        egui::vec2(toggle_width, min_height - 4.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            self.render_ip_preference_toggle(ui, ctx);
                        },
                    );
                });
            });

        let drag_rect = egui::Rect::from_min_max(
            response.response.rect.left_top(),
            egui::pos2(
                response.response.rect.left() + 240.0,
                response.response.rect.bottom(),
            ),
        );
        self.drag_area(ui, ctx, drag_rect, "launcher_footer_drag");
    }

    fn render_status_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(22, 26, 32),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("状态与日志")
                    .font(FontId::proportional(13.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(2.0);
            ui.painter().line_segment(
                [
                    ui.cursor().left_top(),
                    ui.cursor().left_top() + egui::vec2(ui.available_width(), 0.0),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 56, 64)),
            );
            ui.add_space(6.0);
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(17, 20, 25))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 48, 56)))
                .inner_margin(egui::Margin::symmetric(12, 10))
                .corner_radius(egui::CornerRadius::same(10))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new("当前状态")
                                .font(FontId::proportional(11.0))
                                .strong()
                                .color(egui::Color32::from_rgb(160, 170, 178)),
                        );
                        ui.label(
                            RichText::new(self.status.status_text.as_str())
                                .font(FontId::proportional(12.0))
                                .color(egui::Color32::from_rgb(232, 236, 239)),
                        );
                    });
                });
            ui.add_space(6.0);
            ui.label(
                RichText::new("最近错误")
                    .font(FontId::proportional(11.0))
                    .strong()
                    .color(egui::Color32::from_rgb(160, 170, 178)),
            );
            let details = self
                .status
                .last_error
                .as_deref()
                .unwrap_or("暂无错误；异常会直接显示原因。");
            ui.label(
                RichText::new(details)
                    .font(FontId::proportional(11.8))
                    .color(if self.status.last_error.is_some() {
                        egui::Color32::from_rgb(235, 110, 90)
                    } else {
                        egui::Color32::from_rgb(202, 208, 214)
                    }),
            );
            ui.add_space(7.0);
            ui.label(
                RichText::new("最近日志")
                    .font(FontId::proportional(11.0))
                    .strong()
                    .color(egui::Color32::from_rgb(160, 170, 178)),
            );
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(14, 17, 21))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(38, 44, 52)))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .corner_radius(egui::CornerRadius::same(8))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(228.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for line in self.recent_logs_or_placeholder() {
                                ui.label(
                                    RichText::new(line)
                                        .font(FontId::monospace(10.5))
                                        .color(egui::Color32::from_rgb(190, 198, 206)),
                                );
                            }
                        });
                });
        });
    }

    fn render_scope_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(22, 26, 32),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("接管范围")
                    .font(FontId::proportional(13.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(2.0);
            ui.painter().line_segment(
                [
                    ui.cursor().left_top(),
                    ui.cursor().left_top() + egui::vec2(ui.available_width(), 0.0),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 56, 64)),
            );
            ui.add_space(6.0);
            let hosts_count = self.config.hosts_domains().len().to_string();
            let doh_count = self.config.doh_endpoints.len().to_string();
            let cert_count = self.config.certificate_domains.len().to_string();
            ui.columns(3, |columns| {
                scope_metric_card(&mut columns[0], "域名", &hosts_count);
                scope_metric_card(&mut columns[1], "DoH", &doh_count);
                scope_metric_card(&mut columns[2], "证书", &cert_count);
            });
            ui.add_space(6.0);
            detail_value_row(ui, "监听状态", self.listen_state_label());
            detail_value_row(ui, "HTTP 监听", &self.http_listen_address());
            detail_value_row(ui, "HTTPS 监听", &self.https_listen_address());
            detail_value_row(ui, "解析优先", self.ip_preference_label());
            detail_value_row(ui, "边缘节点", self.edge_node_label());
            detail_value_row(ui, "上游", &self.config.upstream);
            detail_value_row(
                ui,
                "DoH 端点",
                &self
                    .config
                    .doh_endpoints
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "未配置".to_string()),
            );
        });
    }

    fn render_tips_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(22, 26, 32),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("使用提示")
                    .font(FontId::proportional(13.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(2.0);
            ui.painter().line_segment(
                [
                    ui.cursor().left_top(),
                    ui.cursor().left_top() + egui::vec2(ui.available_width(), 0.0),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 56, 64)),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("全部配置都在 linuxdo-accelerator.toml，改这一份即可。")
                    .font(FontId::proportional(11.4))
                    .color(egui::Color32::from_rgb(214, 219, 223)),
            );
            ui.add_space(5.0);
            ui.label(
                RichText::new("更新根证书后，重开浏览器。")
                    .font(FontId::proportional(10.9))
                    .color(egui::Color32::from_rgb(160, 171, 179)),
            );
            ui.add_space(3.0);
            ui.label(
                RichText::new("提权、DoH 或端口失败时，会直接显示原因。")
                    .font(FontId::proportional(10.9))
                    .color(egui::Color32::from_rgb(160, 171, 179)),
            );
        });
    }

    fn render_config_panel(&mut self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(22, 26, 32),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("配置文件")
                    .font(FontId::proportional(13.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(6.0);
            detail_value_row(ui, "主配置", &self.config_path.display().to_string());
            detail_value_row(ui, "当前边缘", self.edge_node_label());
            ui.add_space(6.0);
            ui.label(
                RichText::new("边缘节点")
                    .font(FontId::proportional(11.0))
                    .strong()
                    .color(egui::Color32::from_rgb(160, 170, 178)),
            );
            let input_enabled = !self.busy && !self.status.running;
            let input_response = ui.add_enabled(
                input_enabled,
                egui::TextEdit::singleline(&mut self.edge_node_input)
                    .hint_text("留空为自动，可填 IPv4 / IPv6 / 域名"),
            );
            self.register_drag_blocker(input_response.rect);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let save_response =
                    ui.add(subtle_button("保存", egui::vec2(72.0, 28.0), input_enabled));
                self.register_drag_blocker(save_response.rect);
                if save_response.clicked() {
                    self.set_edge_node_override();
                }

                let clear_enabled = input_enabled && !self.edge_node_input.trim().is_empty();
                let clear_response =
                    ui.add(subtle_button("清空", egui::vec2(72.0, 28.0), clear_enabled));
                self.register_drag_blocker(clear_response.rect);
                if clear_response.clicked() {
                    self.edge_node_input.clear();
                    self.set_edge_node_override();
                }
            });
            subtle_note(ui, "边缘节点仅在停止加速后可修改；改完重新开始加速生效。");
        });
    }

    fn render_project_panel(&self, ui: &mut egui::Ui) {
        panel_frame(
            egui::Color32::from_rgb(22, 26, 32),
            egui::Color32::from_rgb(50, 56, 64),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("工具信息")
                    .font(FontId::proportional(13.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(6.0);
            detail_value_row(ui, "版本", &format!("v{APP_VERSION}"));
            detail_value_row(ui, "名称", "Linux.do Accelerator");
            ui.add_space(8.0);
            about_bullet(ui, "支持证书、hosts、本地 80/443 与 DoH。");
            about_bullet(ui, "配置统一写在 linuxdo-accelerator.toml。");
            about_bullet(ui, "启动时会申请管理员权限并拉起守护进程。");
        });
    }

    fn show_confirm_action_dialog(&mut self, ctx: &egui::Context) {
        let Some(action) = self.confirm_action else {
            return;
        };

        let mut open = true;
        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new(action.confirm_title())
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_width(500.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(500.0);
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("该操作会再次申请管理员权限。")
                                .font(FontId::proportional(13.0))
                                .strong(),
                        );
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new("• 开始加速：准备环境并启动本地代理。")
                                .font(FontId::proportional(11.5))
                                .color(egui::Color32::from_rgb(213, 218, 222)),
                        );
                        ui.label(
                            RichText::new(
                                "• 停止加速：自动停止服务、恢复 hosts，并尝试刷新 DNS 缓存。",
                            )
                            .font(FontId::proportional(11.5))
                            .color(egui::Color32::from_rgb(213, 218, 222)),
                        );

                        ui.add_space(2.0);
                        ui.label(
                            RichText::new("这些操作会再次申请管理员权限。")
                                .font(FontId::proportional(10.8))
                                .color(egui::Color32::from_rgb(165, 174, 182)),
                        );
                    });

                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let cancel_response =
                        ui.add(subtle_button("取消", egui::vec2(92.0, 34.0), true));
                    self.register_drag_blocker(cancel_response.rect);
                    if cancel_response.clicked() {
                        cancelled = true;
                    }
                    let confirm_response = ui.add(filled_button(
                        action.confirm_button(),
                        egui::Color32::from_rgb(243, 180, 66),
                        egui::Color32::from_rgb(24, 24, 22),
                        egui::Color32::from_rgb(216, 158, 58),
                        egui::vec2(172.0, 34.0),
                        !self.busy,
                    ));
                    self.register_drag_blocker(confirm_response.rect);
                    if confirm_response.clicked() {
                        confirmed = true;
                    }
                });
            });

        if confirmed {
            self.confirm_action = None;
            self.trigger_action(action);
        } else if cancelled || !open {
            self.confirm_action = None;
        }
    }

    #[cfg(target_os = "windows")]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        match (&self.tray, self.window_handle) {
            (Some(tray), Some(hwnd)) => {
                let _ = tray.tray_icon.set_visible(true);
                self.hidden_to_tray = true;
                self.last_minimized = true;
                // Hide the window via Win32 (SW_HIDE) so it leaves the taskbar
                // and lives only in the tray. We deliberately do NOT use
                // ViewportCommand::Visible(false): a fully hidden window stops
                // receiving paint messages, so eframe's update loop would never
                // run again and the tray click could not bring it back. The tray
                // handler calls restore_app_window directly to wake it.
                let _ = hide_app_window(hwnd);
                ctx.request_repaint();
            }
            _ => {
                self.feedback = "托盘不可用，已退回系统最小化".to_string();
                self.hidden_to_tray = false;
                self.last_minimized = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                ctx.request_repaint();
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        // Guard against re-entry: once we've handed off to a tray-shell process,
        // ignore further presses. Otherwise, if the window has not finished
        // closing yet, each extra click on the ↧ button spawns another tray-shell
        // (multiple icons, each of which restores its own window).
        if self.hidden_to_tray {
            return;
        }
        self.save_window_position(ctx);
        match spawn_tray_shell(&self.config_path) {
            Ok(()) => {
                self.hidden_to_tray = true;
                self.last_minimized = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(error) => {
                self.hidden_to_tray = false;
                self.last_minimized = true;
                self.feedback = format!("托盘最小化失败，已退回系统最小化: {error}");
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                ctx.request_repaint();
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn save_window_position(&self, ctx: &egui::Context) {
        let position =
            ctx.input(|input| input.viewport().outer_rect.map(|outer_rect| outer_rect.min));
        if let Some(position) = position {
            let _ = write_saved_window_position(&self.config_path, position);
        }
    }

    #[cfg(target_os = "macos")]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        if self.hidden_to_tray {
            return;
        }
        self.save_window_position(ctx);
        if let Some(tray) = &mut self.tray {
            tray.set_visible(true);
            log_macos_tray_event("in-process minimize: tray item visible, hiding window");
            self.hidden_to_tray = true;
            self.last_minimized = true;
            macos_set_activation_policy_accessory();
            if !macos_hide_app_window(self.window_handle) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            ctx.request_repaint();
            return;
        }

        // Fallback for unusual AppKit failures: keep the older helper-based
        // tray path available even though the normal path is now in-process.
        match spawn_macos_tray_shell(&self.config_path) {
            Ok(()) => {
                self.hidden_to_tray = true;
                self.last_minimized = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(error) => {
                self.hidden_to_tray = false;
                self.last_minimized = true;
                self.feedback = format!("托盘最小化失败，已退回系统最小化: {error}");
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                ctx.request_repaint();
            }
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    fn minimize_to_tray(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }

    fn touch_ui_lease(&self) -> Result<()> {
        let paths = service::resolve_paths(Some(self.config_path.clone()))?;
        state::touch_ui_lease(&paths, std::process::id())
    }

    fn clear_ui_lease(&self) {
        if let Ok(paths) = service::resolve_paths(Some(self.config_path.clone())) {
            let _ = state::clear_ui_lease(&paths);
        }
    }

    #[cfg(target_os = "windows")]
    fn restore_from_tray(&mut self, ctx: &egui::Context) {
        self.hidden_to_tray = false;
        self.last_minimized = true;
        if let Some(tray) = &self.tray {
            let _ = tray.tray_icon.set_visible(false);
        }
        // The window was hidden via Win32 (SW_HIDE), so bring it back the same
        // way. The tray handler already woke it; this also covers the menu path
        // and guarantees it ends up shown and focused.
        if let Some(hwnd) = self.window_handle {
            let _ = restore_app_window(hwnd);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    #[cfg(target_os = "macos")]
    fn restore_from_tray(&mut self, ctx: &egui::Context) {
        self.hidden_to_tray = false;
        self.last_minimized = true;
        // The macOS status item is kept resident. Hiding it on restore can make
        // a later tray minimize unrecoverable if SystemUIServer ignores the
        // next setVisible(true).
        log_macos_tray_event("in-process restore: showing window");
        macos_set_activation_policy_regular();
        macos_restore_app_window();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn poll_tray_events(&mut self, ctx: &egui::Context) {
        while let Ok(command) = self.tray_rx.try_recv() {
            match command {
                TrayCommand::Restore => self.restore_from_tray(ctx),
                TrayCommand::Quit => {
                    #[cfg(target_os = "windows")]
                    if let Some(tray) = &self.tray {
                        let _ = tray.tray_icon.set_visible(false);
                    }
                    #[cfg(target_os = "macos")]
                    if let Some(tray) = &mut self.tray {
                        tray.set_visible(false);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    fn sync_minimize_to_tray(&mut self, ctx: &egui::Context) {
        // Native minimize (the `-` button / window manager) now goes to the
        // taskbar/Dock. Only the dedicated tray button hides to the tray, so we
        // just track the minimized state here without redirecting.
        let minimized = ctx.input(|input| input.viewport().minimized.unwrap_or(false));
        self.last_minimized = minimized && !self.hidden_to_tray;
    }

    fn repaint_interval(&self) -> Duration {
        if self.busy || self.action_rx.is_some() || self.confirm_action.is_some() {
            ACTIVE_REPAINT_INTERVAL
        } else if self.hidden_to_tray {
            TRAY_REPAINT_INTERVAL
        } else {
            IDLE_REPAINT_INTERVAL
        }
    }

    fn maybe_autostart(&mut self) {
        if !self.autostart_pending {
            return;
        }
        if self.busy || self.action_rx.is_some() {
            return;
        }
        if self.status.running {
            self.autostart_pending = false;
            return;
        }
        self.autostart_pending = false;
        self.feedback = "自动启动已请求加速...".to_string();
        self.trigger_action(GuiAction::Start);
    }

    fn set_autostart(&mut self, enabled: bool) {
        if enabled == self.autostart_enabled && enabled == self.config.autostart {
            return;
        }
        let result = if enabled {
            autostart::enable(&self.config_path)
        } else {
            autostart::disable()
        };
        match result {
            Ok(()) => {
                self.autostart_enabled = enabled;
                self.config.autostart = enabled;
                if let Err(error) = self.save_current_config() {
                    self.feedback = format!(
                        "已切换开机自启，但保存配置失败: {}",
                        format_error_chain(&error)
                    );
                    return;
                }
                self.feedback = if enabled {
                    "已开启开机自动启动加速".to_string()
                } else {
                    "已关闭开机自动启动加速".to_string()
                };
            }
            Err(error) => {
                self.autostart_enabled = autostart::is_enabled();
                self.feedback = format!("修改开机自启失败: {}", format_error_chain(&error));
            }
        }
    }

    fn ensure_launcher_viewport(&self, ctx: &egui::Context) {
        // Re-issuing these commands every frame makes winit emit resize events
        // every frame, which marks the UI dirty and keeps egui repainting at
        // full speed. Only send them when the window size actually drifted.
        let size = launcher_window_size();
        let current = ctx.input(|input| {
            input
                .viewport()
                .inner_rect
                .map(|rect| rect.size())
        });
        let drifted = match current {
            Some(current) => (current - size).length() > 1.0,
            None => true,
        };
        if !drifted {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::MaxInnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(false));
    }
}

impl eframe::App for AcceleratorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            self.poll_tray_events(ctx);
        }
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        self.sync_minimize_to_tray(ctx);

        self.poll_action();
        self.drag_blockers.clear();

        let repaint_interval = self.repaint_interval();
        if self.last_refresh.elapsed() >= repaint_interval {
            self.refresh_status();
            self.last_refresh = Instant::now();
        }

        self.maybe_autostart();

        if self.current_page == UiPage::Launcher {
            self.ensure_launcher_viewport(ctx);
            if self.center_window_pending {
                if let Some(command) = egui::ViewportCommand::center_on_screen(ctx) {
                    ctx.send_viewport_cmd(command);
                }
                self.center_window_pending = false;
            }
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(17, 20, 24))
                    .inner_margin(egui::Margin::symmetric(4, 4)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                if !use_native_wayland_frame() {
                    self.render_window_title_bar(ui, ctx);
                    ui.add_space(2.0);
                } else {
                    ui.add_space(4.0);
                }

                match self.current_page {
                    UiPage::Launcher => {
                        let panel_width = ui.available_width();
                        let panel_height = 172.0;
                        ui.horizontal(|ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(panel_width, panel_height),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_width(panel_width);
                                    ui.set_min_height(panel_height);
                                    let launcher_response = egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(20, 24, 29))
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            egui::Color32::from_rgb(44, 50, 58),
                                        ))
                                        .corner_radius(egui::CornerRadius::same(14))
                                        .inner_margin(egui::Margin::same(8))
                                        .show(ui, |ui| {
                                            self.render_action_panel(ui, ctx);
                                        });
                                    self.drag_area(
                                        ui,
                                        ctx,
                                        launcher_response.response.rect,
                                        "launcher_frame_drag",
                                    );
                                },
                            );
                        });
                        let remaining_drag_rect = ui.available_rect_before_wrap();
                        self.drag_area(ui, ctx, remaining_drag_rect, "launcher_remaining_drag");
                    }
                    UiPage::Details => {
                        panel_frame(
                            egui::Color32::from_rgb(20, 24, 29),
                            egui::Color32::from_rgb(44, 50, 58),
                        )
                        .show(ui, |ui| {
                            let scroll_output = egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    self.render_page_header(ui, ctx, "详情与设置");
                                    self.render_details_content(ui);
                                });
                            self.register_drag_blocker(scroll_output.inner_rect);
                        });
                    }
                }

                self.drag_area(ui, ctx, ui.max_rect(), "window_full_drag");
            });

        self.show_confirm_action_dialog(ctx);

        ctx.request_repaint_after(repaint_interval);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(stop) = self.ui_lease_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if !self.status.running {
            self.clear_ui_lease();
            self.owns_ui_lease = false;
        }
    }
}

#[derive(Clone, Copy)]
enum GuiAction {
    Start,
    Stop,
}

impl GuiAction {
    fn pending_message(self) -> &'static str {
        match self {
            Self::Start => "正在申请权限并启动加速...",
            Self::Stop => "正在停止加速并恢复 hosts...",
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::Start => "helper-start",
            Self::Stop => "helper-stop",
        }
    }

    fn confirm_title(self) -> &'static str {
        "确认操作"
    }

    fn confirm_button(self) -> &'static str {
        "确认"
    }

    fn error_context(self) -> &'static str {
        match self {
            Self::Start => "elevation or command execution failed",
            Self::Stop => "failed to stop acceleration from GUI",
        }
    }
}

fn execute_action(config_path: &Path, action: GuiAction) -> Result<String> {
    #[cfg(target_os = "macos")]
    if matches!(action, GuiAction::Start) {
        service::prepare_certificate(Some(config_path.to_path_buf()))
            .with_context(|| "macOS certificate preparation failed")?;
    }

    let before_status = service::status(Some(config_path.to_path_buf())).unwrap_or_default();
    if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
        let _ = append_runtime_log(
            &paths,
            "INFO",
            action.subcommand(),
            "GUI 已发起管理员操作请求",
        );
    }

    // On macOS and Linux, try the privileged helper first (no password needed after initial install).
    #[cfg(unix)]
    {
        use crate::autostart;
        use crate::helper_ipc;

        let cli_binary = locate_action_binary()?;
        if !autostart::is_privileged_helper_installed() {
            autostart::install_privileged_helper(&cli_binary, config_path)
                .context("failed to install privileged helper")?;
        } else {
            // Plist exists but the service might not be loaded (e.g. after reboot).
            // Ensure the socket exists by loading the daemon if needed.
            let socket = helper_ipc::socket_path();
            if !socket.exists() {
                let _ = autostart::ensure_privileged_helper_running(config_path);
            }
        }

        // Wait for the socket to appear (helper may take a moment to start).
        let socket = helper_ipc::socket_path();
        if !socket.exists() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !socket.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(200));
            }
        }
        let request = match action {
            GuiAction::Start => helper_ipc::HelperRequest::Start {
                config_path: config_path.to_path_buf(),
            },
            GuiAction::Stop => helper_ipc::HelperRequest::Stop {
                config_path: config_path.to_path_buf(),
            },
        };

        match helper_ipc::send_request(&socket, &request) {
            Ok(resp) if resp.success => {
                // Fall through to the polling loop below.
            }
            Ok(resp) => {
                if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
                    let _ = append_runtime_log(
                        &paths,
                        "ERROR",
                        action.subcommand(),
                        &format!("helper 操作失败：{}", resp.message),
                    );
                }
                bail!("{}", resp.message);
            }
            Err(error) => {
                // Socket communication failed — fall back to platform elevation.
                if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
                    let _ = append_runtime_log(
                        &paths,
                        "WARN",
                        action.subcommand(),
                        &format!("helper socket 连接失败，回退到提权：{error}"),
                    );
                }
                let args = vec![
                    "--config".to_string(),
                    config_path.to_string_lossy().into_owned(),
                    action.subcommand().to_string(),
                ];
                run_elevated(&cli_binary, &args)?;
            }
        }
    }

    // Non-Unix (Windows): use the existing elevation path.
    #[cfg(not(unix))]
    {
        let cli_binary = locate_action_binary()?;
        let args = vec![
            "--config".to_string(),
            config_path.to_string_lossy().into_owned(),
            action.subcommand().to_string(),
        ];
        if let Err(error) = run_elevated(&cli_binary, &args) {
            if let Ok(status) = service::status(Some(config_path.to_path_buf())) {
                if let Some(last_error) = status.last_error.clone() {
                    if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
                        let _ = append_runtime_log(
                            &paths,
                            "ERROR",
                            action.subcommand(),
                            &format!("GUI 操作失败：{last_error}"),
                        );
                    }
                    return Err(Error::msg(last_error)).with_context(|| action.error_context());
                }
                if !matches!(action, GuiAction::Start)
                    && service_state_changed(&before_status, &status)
                {
                    if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
                        let _ = append_runtime_log(
                            &paths,
                            "WARN",
                            action.subcommand(),
                            &format!("GUI 检测到状态已变化：{}", status.status_text),
                        );
                    }
                    return Err(Error::msg(status.status_text))
                        .with_context(|| action.error_context());
                }
            }
            if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
                let _ = append_runtime_log(
                    &paths,
                    "ERROR",
                    action.subcommand(),
                    &format!("GUI 提权执行失败：{error}"),
                );
            }
            return Err(error).with_context(|| action.error_context());
        }
    }

    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let status = service::status(Some(config_path.to_path_buf()))?;
        match action {
            GuiAction::Start if status.running => {
                return Ok("加速已启动，可以直接最小化窗口".to_string());
            }
            GuiAction::Stop if !status.running => {
                return Ok(status.status_text);
            }
            _ => {
                if let Some(error) = status.last_error.clone() {
                    bail!(error);
                }
                if Instant::now() >= deadline {
                    bail!(
                        "service state did not update in time (status: {})",
                        status.status_text
                    );
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn service_state_changed(before: &ServiceState, after: &ServiceState) -> bool {
    before.running != after.running
        || before.pid != after.pid
        || before.status_text != after.status_text
        || before.last_error != after.last_error
        || before.updated_at != after.updated_at
}

fn load_recent_runtime_logs(config_path: &Path) -> Vec<String> {
    service::resolve_paths(Some(config_path.to_path_buf()))
        .ok()
        .and_then(|paths| read_recent_lines(&paths, 12).ok())
        .unwrap_or_default()
}

fn runtime_log_file_modified_at(config_path: &Path) -> Option<SystemTime> {
    service::resolve_paths(Some(config_path.to_path_buf()))
        .ok()
        .and_then(|paths| file_modified_at(&paths.runtime_log_path))
}

fn file_modified_at(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn ui_lease_exists(config_path: &Path) -> bool {
    service::resolve_paths(Some(config_path.to_path_buf()))
        .ok()
        .and_then(|paths| state::read_ui_lease(&paths).ok().flatten())
        .is_some()
}

fn touch_ui_lease_for_config(config_path: &Path) {
    if let Ok(paths) = service::resolve_paths(Some(config_path.to_path_buf())) {
        let _ = state::touch_ui_lease(&paths, std::process::id());
    }
}

fn spawn_ui_lease_heartbeat(config_path: PathBuf) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    if !ui_lease_exists(&config_path) {
        return stop;
    }

    let stop_flag = stop.clone();
    thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            touch_ui_lease_for_config(&config_path);
            thread::sleep(Duration::from_secs(2));
        }
    });
    stop
}

#[cfg(target_os = "linux")]
fn spawn_tray_shell(config_path: &Path) -> Result<()> {
    let gui_binary = locate_gui_binary()?;
    let args = vec![
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "tray-shell".to_string(),
    ];
    #[cfg(target_os = "linux")]
    log_linux_tray_event(&format!(
        "spawn tray-shell exe={} config={}",
        gui_binary.display(),
        config_path.display()
    ));
    spawn_detached(&gui_binary, &args).context("failed to start tray shell")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_macos_tray_shell(config_path: &Path) -> Result<()> {
    let helper_app = macos_tray_helper_app_path()
        .context("failed to locate LinuxdoAcceleratorTray.app in this application bundle")?;
    if !helper_app.exists() {
        bail!("macOS tray helper is missing: {}", helper_app.display());
    }
    if let Some(existing_pid) = macos_existing_tray_shell_pid() {
        log_macos_tray_event(&format!(
            "macOS tray helper already running pid={existing_pid}; reusing existing tray icon"
        ));
        return Ok(());
    }

    let ready_file = macos_tray_ready_file_path();
    let _ = fs::remove_file(&ready_file);
    let open_binary = PathBuf::from("/usr/bin/open");
    let args = vec![
        "-n".to_string(),
        helper_app.to_string_lossy().into_owned(),
        "--args".to_string(),
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "--tray-ready-file".to_string(),
        ready_file.to_string_lossy().into_owned(),
        "tray-shell".to_string(),
    ];
    log_macos_tray_event(&format!(
        "spawn helper tray app={} config={} ready_file={}",
        helper_app.display(),
        config_path.display(),
        ready_file.display()
    ));
    spawn_detached(&open_binary, &args).context("failed to start macOS tray helper")?;
    wait_for_macos_tray_ready(&ready_file)?;
    let _ = fs::remove_file(&ready_file);
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_tray_ready_file_path() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "linuxdo-macos-tray-ready-{}-{timestamp}.txt",
        std::process::id()
    ))
}

#[cfg(target_os = "macos")]
fn wait_for_macos_tray_ready(path: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if path.exists() {
            log_macos_tray_event(&format!(
                "helper tray ready file observed={}",
                path.display()
            ));
            return Ok(());
        }
        thread::sleep(Duration::from_millis(30));
    }

    bail!(
        "macOS tray helper did not become ready within 3 seconds: {}",
        path.display()
    )
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn spawn_ui_process(config_path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    if use_native_wayland_frame() {
        let desktop_launcher = PathBuf::from("/usr/bin/gtk-launch");
        let args = vec!["linuxdo-accelerator".to_string()];
        log_linux_tray_event(&format!(
            "spawn ui via launcher exe={} config={}",
            desktop_launcher.display(),
            config_path.display()
        ));
        spawn_detached(&desktop_launcher, &args).context("failed to reopen UI")?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let main_app = macos_main_app_bundle_path()
            .context("failed to locate main macOS application bundle")?;
        let open_binary = PathBuf::from("/usr/bin/open");
        let args = vec![
            "-n".to_string(),
            main_app.to_string_lossy().into_owned(),
            "--args".to_string(),
            "--config".to_string(),
            config_path.to_string_lossy().into_owned(),
            "gui".to_string(),
        ];
        log_macos_tray_event(&format!(
            "spawn ui via main app={} config={}",
            main_app.display(),
            config_path.display()
        ));
        spawn_detached(&open_binary, &args).context("failed to reopen UI")?;
        return Ok(());
    }

    #[cfg(not(target_os = "macos"))]
    {
        let gui_binary = locate_gui_binary()?;
        let args = vec![
            "--config".to_string(),
            config_path.to_string_lossy().into_owned(),
            "gui".to_string(),
        ];
        #[cfg(target_os = "linux")]
        log_linux_tray_event(&format!(
            "spawn ui exe={} config={}",
            gui_binary.display(),
            config_path.display()
        ));
        spawn_detached(&gui_binary, &args).context("failed to reopen UI")?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn log_linux_tray_event(message: &str) {
    let path = std::env::temp_dir().join("linuxdo-tray.log");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(target_os = "macos")]
fn log_macos_tray_event(message: &str) {
    use std::io::Write as _;

    let path = std::env::temp_dir().join("linuxdo-macos-tray.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_saved_window_position(config_path: &Path) -> Option<egui::Pos2> {
    let window = service::resolve_paths(Some(config_path.to_path_buf()))
        .ok()
        .and_then(|paths| state::read_ui_window(&paths).ok().flatten())?;
    if window.outer_x.is_finite() && window.outer_y.is_finite() {
        Some(egui::pos2(window.outer_x, window.outer_y))
    } else {
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_saved_window_position(config_path: &Path, position: egui::Pos2) -> Result<()> {
    if !position.x.is_finite() || !position.y.is_finite() {
        return Ok(());
    }
    let paths = service::resolve_paths(Some(config_path.to_path_buf()))?;
    state::write_ui_window(&paths, position.x, position.y)
}

fn locate_action_binary() -> Result<PathBuf> {
    locate_current_or_sibling_binary(action_binary_name())
}

fn locate_gui_binary() -> Result<PathBuf> {
    locate_current_or_sibling_binary(gui_binary_name())
}

#[cfg(target_os = "macos")]
fn macos_main_app_bundle_path() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    current
        .ancestors()
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        })
        .last()
        .map(Path::to_path_buf)
}

#[cfg(target_os = "macos")]
fn macos_tray_helper_app_path() -> Option<PathBuf> {
    Some(
        macos_main_app_bundle_path()?
            .join("Contents")
            .join("Helpers")
            .join("LinuxdoAcceleratorTray.app"),
    )
}

fn locate_current_or_sibling_binary(binary_name: &str) -> Result<PathBuf> {
    let current = std::env::current_exe().context("failed to locate current executable")?;
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == binary_name)
    {
        return Ok(current);
    }

    let sibling = current.with_file_name(binary_name);
    if sibling.exists() {
        return Ok(sibling);
    }

    bail!("failed to locate binary {}", sibling.display())
}

fn action_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "linuxdo-accelerator.exe"
    } else {
        "linuxdo-accelerator"
    }
}

fn gui_binary_name() -> &'static str {
    action_binary_name()
}

fn format_error_chain(error: &Error) -> String {
    let mut lines = Vec::new();
    for cause in error.chain() {
        let message = cause.to_string();
        if lines.last() != Some(&message) {
            lines.push(message);
        }
    }
    lines.join("\ncaused by: ")
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let (font_name, font_data) = load_ui_font();
    fonts.font_data.insert(font_name.clone(), font_data.into());
    if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
        family.push(font_name.clone());
    }
    if let Some(family) = fonts.families.get_mut(&FontFamily::Monospace) {
        family.push(font_name);
    }
    ctx.set_fonts(fonts);
}

fn load_ui_font() -> (String, egui::FontData) {
    if let Some((name, data)) = load_system_ui_font() {
        return (name, egui::FontData::from_owned(data));
    }

    (
        "linuxdo_cjk_embedded".to_string(),
        egui::FontData::from_static(EMBEDDED_CJK_FONT),
    )
}

fn load_system_ui_font() -> Option<(String, Vec<u8>)> {
    let mut database = fontdb::Database::new();
    database.load_system_fonts();

    for family_name in preferred_system_font_families() {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family_name)],
            ..fontdb::Query::default()
        };
        let Some(id) = database.query(&query) else {
            continue;
        };
        let Some(info) = database.face(id) else {
            continue;
        };
        let font_name = info
            .families
            .first()
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| family_name.to_string());
        let Some(data) = database.with_face_data(id, |data, _| data.to_vec()) else {
            continue;
        };
        if data.len() <= EMBEDDED_CJK_FONT.len() {
            return Some((font_name, data));
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn preferred_system_font_families() -> &'static [&'static str] {
    &["Microsoft YaHei UI", "Microsoft YaHei", "SimHei"]
}

#[cfg(target_os = "macos")]
fn preferred_system_font_families() -> &'static [&'static str] {
    &["PingFang SC", "Hiragino Sans GB"]
}

#[cfg(target_os = "linux")]
fn preferred_system_font_families() -> &'static [&'static str] {
    &[
        "Noto Sans CJK SC",
        "Noto Sans SC",
        "WenQuanYi Micro Hei",
        "Source Han Sans SC",
        "Droid Sans Fallback",
    ]
}

fn install_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(egui::Color32::from_rgb(232, 236, 239));
    style.visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(24, 28, 34);
    style.visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(232, 236, 239);
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(32, 37, 44);
    style.visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(32, 37, 44);
    style.visuals.widgets.inactive.fg_stroke.color = egui::Color32::from_rgb(232, 236, 239);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(44, 50, 58);
    style.visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(44, 50, 58);
    style.visuals.widgets.hovered.fg_stroke.color = egui::Color32::from_rgb(252, 253, 254);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(244, 184, 72);
    style.visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(244, 184, 72);
    style.visuals.widgets.active.fg_stroke.color = egui::Color32::from_rgb(28, 24, 17);
    style.visuals.widgets.open.bg_fill = egui::Color32::from_rgb(35, 40, 47);
    style.visuals.widgets.open.fg_stroke.color = egui::Color32::from_rgb(232, 236, 239);
    style.visuals.selection.bg_fill = egui::Color32::from_rgb(244, 184, 72);
    style.visuals.selection.stroke.color = egui::Color32::from_rgb(28, 24, 17);
    style.visuals.window_fill = egui::Color32::from_rgb(17, 20, 24);
    style.visuals.panel_fill = egui::Color32::from_rgb(17, 20, 24);
    style.visuals.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(54, 60, 67));
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(12, 16, 20);
    style.visuals.faint_bg_color = egui::Color32::from_rgb(21, 25, 30);
    style.visuals.window_corner_radius = egui::CornerRadius::same(18);
    style.visuals.menu_corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(10);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(10);
    style.visuals.window_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 2,
        color: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 80),
    };
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        FontId::new(11.5, FontFamily::Proportional),
    );
    ctx.set_style(style);
}

fn panel_frame(fill: egui::Color32, stroke: egui::Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .inner_margin(egui::Margin::same(14))
        .corner_radius(egui::CornerRadius::same(14))
}

fn summarize_launcher_error(error: &str) -> String {
    let compact = error.replace('\n', " ");
    let lower = compact.to_lowercase();

    if lower.contains("127.0.0.1:80") || lower.contains("failed to bind http listener") {
        return "80 端口监听失败，请查看详情".to_string();
    }
    if lower.contains("127.0.0.1:443") || lower.contains("failed to bind https listener") {
        return "443 端口监听失败，请查看详情".to_string();
    }
    if lower.contains("elevation") || lower.contains("permission denied") {
        return "权限申请失败，请查看详情".to_string();
    }
    if lower.contains("doh") {
        return "DoH 配置或连接失败，请查看详情".to_string();
    }

    let mut shortened = compact.chars().take(34).collect::<String>();
    if compact.chars().count() > 34 {
        shortened.push_str("...");
    }
    shortened
}

fn title_bar_button(
    label: &'static str,
    min_size: egui::Vec2,
    danger: bool,
    enabled: bool,
    mono: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        if danger {
            (
                egui::Color32::from_rgba_unmultiplied(200, 60, 50, 35),
                egui::Color32::from_rgb(248, 248, 250),
                egui::Color32::from_rgb(110, 55, 50),
            )
        } else {
            (
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12),
                egui::Color32::from_rgb(220, 225, 230),
                egui::Color32::from_rgb(60, 66, 74),
            )
        }
    } else {
        (
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 4),
            egui::Color32::from_rgb(112, 119, 127),
            egui::Color32::from_rgb(42, 48, 56),
        )
    };

    // Window-control glyphs such as the tray icon (↧) live only in egui's
    // bundled monospace font (Hack); the proportional font lacks them and would
    // render tofu. ASCII labels ("X"/"_") render fine in either family.
    let font = if mono {
        FontId::monospace(14.0)
    } else {
        FontId::proportional(14.0)
    };
    egui::Button::new(RichText::new(label).font(font).strong().color(text))
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(egui::CornerRadius::same(10))
        .min_size(min_size)
}

fn launcher_primary_button(
    label: &'static str,
    fill: egui::Color32,
    text: egui::Color32,
    stroke: egui::Color32,
    min_size: egui::Vec2,
    enabled: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        (fill, text, stroke)
    } else {
        (
            fill.linear_multiply(0.62),
            text.linear_multiply(0.88),
            stroke.linear_multiply(0.72),
        )
    };

    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(16.5))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.6, stroke))
    .corner_radius(egui::CornerRadius::same(16))
    .min_size(min_size)
    .sense(if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    })
}

fn launcher_secondary_button(
    label: &'static str,
    min_size: egui::Vec2,
    enabled: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        (
            egui::Color32::from_rgb(34, 39, 46),
            egui::Color32::from_rgb(236, 240, 244),
            egui::Color32::from_rgb(70, 76, 84),
        )
    } else {
        (
            egui::Color32::from_rgb(28, 32, 38),
            egui::Color32::from_rgb(133, 140, 148),
            egui::Color32::from_rgb(56, 61, 69),
        )
    };

    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(10.6))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
    .corner_radius(egui::CornerRadius::same(9))
    .min_size(min_size)
}

fn filled_button(
    label: &'static str,
    fill: egui::Color32,
    text: egui::Color32,
    stroke: egui::Color32,
    min_size: egui::Vec2,
    enabled: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        (fill, text, stroke)
    } else {
        (
            fill.linear_multiply(0.62),
            text.linear_multiply(0.9),
            stroke.linear_multiply(0.68),
        )
    };
    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(12.5))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
    .corner_radius(egui::CornerRadius::same(10))
    .min_size(min_size)
    .sense(if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    })
}

fn subtle_button(
    label: &'static str,
    min_size: egui::Vec2,
    enabled: bool,
) -> egui::Button<'static> {
    let (fill, text, stroke) = if enabled {
        (
            egui::Color32::from_rgb(28, 33, 40),
            egui::Color32::from_rgb(236, 239, 242),
            egui::Color32::from_rgb(62, 68, 76),
        )
    } else {
        (
            egui::Color32::from_rgb(24, 28, 33),
            egui::Color32::from_rgb(126, 133, 141),
            egui::Color32::from_rgb(52, 58, 66),
        )
    };
    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(11.8))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
    .corner_radius(egui::CornerRadius::same(10))
    .min_size(min_size)
    .sense(if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    })
}

fn ip_priority_button(label: &'static str, selected: bool, enabled: bool) -> egui::Button<'static> {
    let (fill, text, stroke) = if selected {
        (
            egui::Color32::from_rgb(229, 171, 66),
            egui::Color32::from_rgb(29, 24, 16),
            egui::Color32::from_rgb(214, 158, 59),
        )
    } else {
        (
            egui::Color32::from_rgb(28, 33, 40),
            egui::Color32::from_rgb(216, 221, 226),
            egui::Color32::from_rgb(62, 68, 76),
        )
    };
    let (fill, text, stroke) = if enabled {
        (fill, text, stroke)
    } else {
        (
            fill.linear_multiply(0.62),
            text.linear_multiply(0.9),
            stroke.linear_multiply(0.68),
        )
    };
    egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(10.6))
            .strong()
            .color(text),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, stroke))
    .corner_radius(egui::CornerRadius::same(9))
    .min_size(egui::vec2(60.0, 26.0))
    .sense(if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    })
}

fn scope_metric_card(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(17, 20, 25))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 48, 56)))
        .inner_margin(egui::Margin::symmetric(10, 12))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(0.0, 64.0));
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(label)
                        .font(FontId::proportional(10.5))
                        .strong()
                        .color(egui::Color32::from_rgb(150, 161, 170)),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(value)
                        .font(FontId::proportional(20.0))
                        .strong()
                        .color(egui::Color32::from_rgb(243, 179, 74)),
                );
            });
        });
}

fn subtle_note(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(17, 20, 25))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 48, 56)))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .font(FontId::proportional(11.2))
                    .color(egui::Color32::from_rgb(186, 194, 201)),
            );
        });
}

fn detail_value_row(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(17, 20, 25))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 48, 56)))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .corner_radius(egui::CornerRadius::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new(label)
                    .font(FontId::proportional(11.0))
                    .strong()
                    .color(egui::Color32::from_rgb(243, 179, 74)),
            );
            ui.add_space(3.0);
            ui.label(
                RichText::new(value)
                    .font(FontId::monospace(11.2))
                    .color(egui::Color32::from_rgb(232, 236, 239)),
            );
        });
}

fn about_bullet(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_wrapped(|ui| {
        let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), egui::Sense::hover());
        ui.painter().circle_filled(
            egui::pos2(dot_rect.center().x, dot_rect.center().y),
            3.5,
            egui::Color32::from_rgb(243, 179, 74),
        );
        ui.label(
            RichText::new(text)
                .font(FontId::proportional(12.0))
                .color(egui::Color32::from_rgb(214, 219, 223)),
        );
    });
}

#[cfg(target_os = "windows")]
fn build_windows_tray_state(
    ctx: &egui::Context,
    window_handle: Option<isize>,
) -> (Option<TrayState>, Receiver<TrayCommand>) {
    let (event_tx, event_rx) = mpsc::channel();

    let menu = Menu::new();
    let show_item = MenuItem::with_id("tray-show", "打开窗口", true, None);
    let quit_item = MenuItem::with_id("tray-quit", "退出程序", true, None);
    if menu
        .append_items(&[&show_item, &PredefinedMenuItem::separator(), &quit_item])
        .is_err()
    {
        return (None, event_rx);
    }

    let tray_icon = match tray_window_icon() {
        Ok(icon) => TrayIconBuilder::new()
            .with_id("linuxdo-accelerator-tray")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Linux.do Accelerator")
            .with_icon(icon)
            .build()
            .ok(),
        Err(_) => None,
    };

    let Some(tray_icon) = tray_icon else {
        return (None, event_rx);
    };
    let _ = tray_icon.set_visible(false);

    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    let event_tx_click = event_tx.clone();
    let ctx_menu = ctx.clone();
    let ctx_tray = ctx.clone();

    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        // Wake the hidden window first: while hidden via SW_HIDE, eframe's
        // update loop is dormant, so the channel command alone would never be
        // processed. restore_app_window makes the window visible again, which
        // resumes the loop so the queued command runs.
        if let Some(hwnd) = window_handle {
            let _ = restore_app_window(hwnd);
        }
        if event.id == show_id {
            let _ = event_tx.send(TrayCommand::Restore);
            ctx_menu.request_repaint();
        } else if event.id == quit_id {
            let _ = event_tx.send(TrayCommand::Quit);
            ctx_menu.request_repaint();
        }
    }));

    TrayIconEvent::set_event_handler(Some(move |event| match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => {
            // See the menu handler above: wake the window so eframe processes
            // the restore command even when it was hidden from the taskbar.
            if let Some(hwnd) = window_handle {
                let _ = restore_app_window(hwnd);
            }
            let _ = event_tx_click.send(TrayCommand::Restore);
            ctx_tray.request_repaint();
        }
        _ => {}
    }));

    (Some(TrayState { tray_icon }), event_rx)
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn tray_window_icon() -> Result<tray_icon::Icon> {
    let icon = branding::icon_data(64);
    tray_icon::Icon::from_rgba(icon.rgba, icon.width, icon.height)
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

#[cfg(target_os = "linux")]
fn linux_tray_icon_pixmap() -> ksni::Icon {
    let icon = branding::icon_data(64);
    let mut data = icon.rgba;
    for pixel in data.chunks_exact_mut(4) {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: icon.width as i32,
        height: icon.height as i32,
        data,
    }
}

#[cfg(target_os = "windows")]
fn capture_native_window_handle(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn capture_macos_window_handle(cc: &eframe::CreationContext<'_>) -> Option<usize> {
    use objc::{msg_send, sel, sel_impl};

    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::AppKit(handle) => unsafe {
            let ns_view = handle.ns_view.as_ptr() as *mut objc::runtime::Object;
            let ns_window: *mut objc::runtime::Object = msg_send![ns_view, window];
            (!ns_window.is_null()).then_some(ns_window as usize)
        },
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn macos_store_main_window_handle(window_handle: Option<usize>) {
    MACOS_MAIN_WINDOW_HANDLE.store(window_handle.unwrap_or_default(), Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
fn macos_main_window_ptr(window_handle: Option<usize>) -> *mut objc::runtime::Object {
    window_handle.unwrap_or_else(|| MACOS_MAIN_WINDOW_HANDLE.load(Ordering::Relaxed))
        as *mut objc::runtime::Object
}

#[cfg(target_os = "macos")]
fn macos_hide_app_window(window_handle: Option<usize>) -> bool {
    use objc::{msg_send, sel, sel_impl};

    let window = macos_main_window_ptr(window_handle);
    if window.is_null() {
        return false;
    }

    unsafe {
        let _: () = msg_send![window, orderOut: std::ptr::null_mut::<objc::runtime::Object>()];
    }
    true
}

#[cfg(target_os = "macos")]
fn macos_restore_app_window() -> bool {
    use objc::{class, msg_send, sel, sel_impl};

    let window = macos_main_window_ptr(None);
    unsafe {
        let app: *mut objc::runtime::Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
        if window.is_null() {
            return false;
        }
        let _: () = msg_send![window, deminiaturize: std::ptr::null_mut::<objc::runtime::Object>()];
        let _: () =
            msg_send![window, makeKeyAndOrderFront: std::ptr::null_mut::<objc::runtime::Object>()];
    }
    true
}

#[cfg(target_os = "macos")]
fn macos_set_activation_policy_regular() {
    macos_set_activation_policy(0);
}

#[cfg(target_os = "macos")]
fn macos_set_activation_policy_accessory() {
    macos_set_activation_policy(1);
}

#[cfg(target_os = "macos")]
fn macos_set_activation_policy(policy: isize) {
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let app: *mut objc::runtime::Object = msg_send![class!(NSApplication), sharedApplication];
        let _: bool = msg_send![app, setActivationPolicy: policy];
    }
}

#[cfg(target_os = "windows")]
fn schedule_windows_shortcut_icon_refresh(config_path: &Path) {
    let result = (|| -> Result<()> {
        let current_exe = std::env::current_exe().context("failed to locate current executable")?;
        let paths = AppPaths::resolve(Some(config_path.to_path_buf()))?;
        std::fs::create_dir_all(&paths.runtime_dir)
            .with_context(|| format!("failed to create {}", paths.runtime_dir.display()))?;

        let stamp_path = paths.runtime_dir.join("windows-shortcut-icon-sync.txt");
        let stamp = format!("{}\n{}", APP_VERSION, current_exe.display());
        if std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str()) {
            return Ok(());
        }

        thread::spawn(move || {
            if update_windows_shortcuts_for_exe(&current_exe).is_ok() {
                let _ = std::fs::write(&stamp_path, stamp);
            }
        });
        Ok(())
    })();

    let _ = result;
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
fn schedule_windows_shortcut_icon_refresh(_config_path: &Path) {}
