# Changelog

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
