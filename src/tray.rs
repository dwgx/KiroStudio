//! Windows 系统托盘（图标 + 右键菜单：打开网页 / 复制密钥 / 重启服务 / 版本 / 退出）。
//!
//! 架构：`#[tokio::main]` 主线程继续跑 axum 网关；本模块在 Windows 下由 main 另 spawn 一个
//! **专用 std 线程**跑 win32 消息循环 + 托盘图标（tray-icon 要求图标与消息循环同线程，但不必
//! 主线程）。菜单事件通过 `MenuEvent::receiver()` 在消息循环里轮询处理。
//!
//! 「退出」走**优雅关闭**：通知主线程的 tokio graceful shutdown（drain 在途请求、关 SQLite），
//! 不硬杀。跨线程通知用全局 `TRAY_QUIT` Notify。
//!
//! 仅 Windows 编译（`#[cfg(windows)]`），非 Windows target 不含本模块。

#![cfg(windows)]

use std::sync::OnceLock;
use tokio::sync::Notify;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// 托盘「退出」→ 通知主线程优雅关闭。主线程的 shutdown_signal 里 await 这个 Notify。
static TRAY_QUIT: OnceLock<Notify> = OnceLock::new();

/// 取全局退出 Notify（主线程与托盘线程共享）。
pub fn quit_notify() -> &'static Notify {
    TRAY_QUIT.get_or_init(Notify::new)
}

/// 内嵌的 Kiro 托盘图标 svg（24x24，紫底白猫）。运行时用 resvg 渲染成 RGBA。
const ICON_SVG: &str = include_str!("../assets/kiro-color.svg");

/// 托盘退出时用的进程退出码：区别于「面板重启/OTA」的 exit(0)，让 start.bat/run.bat
/// 监督循环识别「用户主动退出」而**不重拉**。裸跑无脚本时此码不影响。
pub const TRAY_QUIT_EXIT_CODE: i32 = 3;

/// 把内嵌 svg 渲染成指定边长的 RGBA 字节（tray-icon 要 straight/非预乘 RGBA）。
/// 失败返回 None（上层降级为无图标托盘或跳过）。
fn render_icon_rgba(size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use resvg::{tiny_skia, usvg};
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_str(ICON_SVG, &opt).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    // svg viewBox 是 24x24，缩放到目标边长。
    let svg_size = tree.size();
    let scale = size as f32 / svg_size.width().max(1.0);
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    // tiny_skia 输出**预乘** RGBA，tray-icon 要**非预乘**——逐像素反预乘。
    let mut rgba = pixmap.data().to_vec();
    unpremultiply_rgba(&mut rgba);
    Some((rgba, size, size))
}

/// 就地把预乘 RGBA 反预乘为直通 RGBA（a=0 时 rgb 归零）。
fn unpremultiply_rgba(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        let a = px[3];
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a != 255 {
            let a_f = a as f32 / 255.0;
            px[0] = ((px[0] as f32 / a_f).round().min(255.0)) as u8;
            px[1] = ((px[1] as f32 / a_f).round().min(255.0)) as u8;
            px[2] = ((px[2] as f32 / a_f).round().min(255.0)) as u8;
        }
    }
}

/// 托盘运行参数（从 main 传入，供菜单动作用）。
pub struct TrayConfig {
    pub host: String,
    pub port: u16,
    pub admin_api_key: String,
    /// exe 路径 + 启动参数，供「重启服务」复用 main 的自重启逻辑（None=不提供重启项）。
    pub relaunch: Option<RelaunchInfo>,
}

/// 「重启服务」所需信息：直接复用 main 里已验证的 .bat 自重启逻辑。
pub struct RelaunchInfo {
    /// 触发自重启的回调（内部调 spawn_windows_relaunch 并优雅退出）。
    pub trigger: Box<dyn Fn() + Send>,
}

/// 在**当前线程**建托盘 + 跑 win32 消息循环（阻塞直到「退出」）。
/// 必须在专用 std 线程调用（main 负责 spawn），不可占 tokio 主线程。
pub fn run(cfg: TrayConfig) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG, WM_QUIT,
    };

    // 构建菜单：打开网页 / 复制密钥 / 重启服务 / 版本(置灰) / 退出。
    let menu = Menu::new();
    let item_open = MenuItem::new("打开网页", true, None);
    let item_copy = MenuItem::new("复制面板密钥", true, None);
    let item_restart = MenuItem::new("重启服务", cfg.relaunch.is_some(), None);
    let version_label = format!("KiroStudio v{}", env!("CARGO_PKG_VERSION"));
    let item_version = MenuItem::new(&version_label, false, None); // 置灰,纯展示
    let item_quit = MenuItem::new("退出", true, None);
    let _ = menu.append(&item_open);
    let _ = menu.append(&item_copy);
    let _ = menu.append(&item_restart);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_version);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_quit);

    // 记录各项 id 供事件匹配。
    let id_open = item_open.id().clone();
    let id_copy = item_copy.id().clone();
    let id_restart = item_restart.id().clone();
    let id_quit = item_quit.id().clone();

    // 构建托盘图标（渲染 svg；失败则无图标仍可用菜单）。
    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("KiroStudio v{} — {}:{}", env!("CARGO_PKG_VERSION"), cfg.host, cfg.port));
    if let Some((rgba, w, h)) = render_icon_rgba(32) {
        if let Ok(icon) = Icon::from_rgba(rgba, w, h) {
            builder = builder.with_icon(icon);
        }
    }
    let _tray = match builder.build() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("[托盘] 创建失败,托盘不可用(不影响网关): {}", e);
            return;
        }
    };
    tracing::info!("[托盘] 已在系统托盘显示(右键菜单:打开网页/复制密钥/重启/退出)");

    let menu_rx = MenuEvent::receiver();
    let admin_url = format!("http://{}:{}/admin", browse_host(&cfg.host), cfg.port);

    // win32 消息循环:GetMessageW 阻塞取消息;每轮 dispatch 后 poll 菜单事件。
    // 用带超时的思路不可行(GetMessageW 阻塞),故靠 tray-icon 内部把菜单点击 post 进本线程
    // 消息队列唤醒 GetMessageW,再 try_recv 取事件。
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if ret <= 0 {
            break; // WM_QUIT(0) 或错误(-1)→退出循环
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // 处理本轮可能到达的菜单事件。
        while let Ok(event) = menu_rx.try_recv() {
            let id = event.id();
            if *id == id_open {
                open_url(&admin_url);
            } else if *id == id_copy {
                copy_to_clipboard(&cfg.admin_api_key);
            } else if *id == id_restart {
                if let Some(r) = &cfg.relaunch {
                    (r.trigger)();
                }
            } else if *id == id_quit {
                tracing::info!("[托盘] 用户点击退出,通知优雅关闭…");
                quit_notify().notify_one();
                return; // 结束消息循环;主线程 graceful shutdown 后退进程
            }
        }
        if msg.message == WM_QUIT {
            break;
        }
    }
}

/// host 为 0.0.0.0 时用 127.0.0.1 打开本机面板。
fn browse_host(host: &str) -> &str {
    if host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        host
    }
}

/// 打开默认浏览器（detached cmd start，不阻塞、不弹窗）。
fn open_url(url: &str) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let r = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn();
    if let Err(e) = r {
        tracing::warn!("[托盘] 打开网页失败: {}", e);
    }
}

/// 复制文本到剪贴板（复制 adminApiKey 给用户登录面板）。
fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new() {
        Ok(mut cb) => {
            if let Err(e) = cb.set_text(text.to_string()) {
                tracing::warn!("[托盘] 复制到剪贴板失败: {}", e);
            } else {
                tracing::info!("[托盘] 面板密钥已复制到剪贴板");
            }
        }
        Err(e) => tracing::warn!("[托盘] 打开剪贴板失败: {}", e),
    }
}

