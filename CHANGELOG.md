# Changelog

## v0.1.15

- 标题栏新增独立的“最小化到托盘”按钮（位于 `-` 与 `X` 左侧），Windows / Linux / macOS 三端均支持。点击后窗口隐藏到系统托盘（不再占用任务栏），单击托盘图标或托盘菜单“打开窗口”即可恢复。原有的 `-` 按钮恢复为常规的“最小化到任务栏/Dock”，不再自动进入托盘。
- 新增 `idcflare.com` 及其子域的支持，与 `linux.do` 统一纳入代理、hosts 和证书管理范围。
- macOS / Linux 新增特权辅助守护进程：首次安装时输入一次密码（macOS LaunchDaemon / Linux systemd service），之后"开始加速/停止加速"不再需要密码。辅助进程通过 Unix domain socket 与 GUI 通信；socket 连接失败时自动回退到原有的提权方式。
- 修复部分 DoH（如私人 DoH `query-dns`）返回 HTTPS/SVCB 记录的 SvcParams 可读格式（如 `1 . alpn=h3,h2 ech=AEX+...`）时解析失败、ECH 配置为空的问题；现在同时兼容 RFC 3597 `\# <长度> <hex>` 与 RFC 9460 SvcParams 展示格式，还原 ECH 强制/可选模式的正常握手。

## v0.1.14

- 修复 Windows 上前端仍在运行时被误判为“前端异常退出”的问题；守护进程现在仅在 UI lease 真正过期或持续缺失后才自动停止。
- 修复 UI lease / 状态文件在 Windows 上的原子替换稳定性，降低共享占用或瞬时文件缺失导致的误判概率。
- 修复 Edge 预发布包内显示的版本号始终停留在 `0.1.12` 的问题；现在会正确显示实际的 edge / release 构建版本。
- 调整 GitHub Actions 的 release / edge 构建流程，把版本号注入到桌面端构建环境中，保持包名、界面显示和发布版本一致。
- macOS 打包流程改为先构建 `.app` 再生成 `.dmg`，并修正 DMG 安装界面的背景布局与图标大小。

## v0.1.10

- 增加桌面端和 Android 端的 TTL-based DoH 缓存，减少重复解析并加快 `linux.do`、`cdn.linux.do`、`cdn3.linux.do`、`ping.linux.do` 等域名的二次访问。
- Android 非 Root 版真机验证通过，`linux.do` 相关 DNS 会继续走自定义 DoH，普通域名仍走系统默认 DNS。
- 修复 Android 停止加速后的状态展示，正常停止后会显示“已停止”，不再误显示“服务已销毁”。
- 版本号更新到 `0.1.10`，Android APK 版本更新到 `0.1.10-android` / `versionCode=3`。

## v0.1.9

- 增加 Android 非 Root 版，基于 Android VPN DNS 接管 `linux.do` 及其子域名，无需 Root、无需安装证书。
- 增加 Android 配置文件落地到用户可直接修改的位置：`/storage/emulated/0/Android/media/io.linuxdo.accelerator.android/linuxdo-accelerator.toml`。
- 增加 Android 快捷磁贴、桌面图标与主界面入口，统一使用 Linux.do 风格图标资源。
- 增加 GitHub Actions Android 构建，自动输出 `arm64-v8a` 和 `x86_64` 两个 APK。
- README 补充 Android 实现方式说明：当前为 DNS 代理接管方案，推荐 Chrome / Edge，系统浏览器和 WebView 兼容性有限，后续可能继续提供 Root 版。
