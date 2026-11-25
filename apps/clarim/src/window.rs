//! `clarim window`: the dashboard in Clarim's own native window (tao + WebView2 through wry) with the Windows 11
//! Acrylic backdrop behind the page. It is a separate process from the agent (the agent owns its main thread for the
//! tray), started by `open::open_dashboard`; closing the window ends only this process.
//!
//! The page is told which backdrop it got (`<html data-backdrop="acrylic|mica|solid">`) and styles itself by it: with
//! an OS backdrop its background is transparent and its cards are translucent glass; `solid` keeps today's opaque look.
//! Acrylic first, Mica when Windows refuses it, solid when both fail, and solid at once when the user switched
//! "Transparency effects" off or uses a high-contrast theme (the OS would draw a flat colour behind translucent cards).
//!
//! The title bar is the system one (dark or light as the system apps theme, which also decides the backdrop tint and the
//! page's `prefers-color-scheme`). If WebView2 is missing or cannot start, the old Edge / browser path opens instead.
//!
//! Dev-only environment, for looking at the page without the agent: `CLARIM_WINDOW_URL` (open this URL, e.g. the
//! `clarim-web` mock on http://127.0.0.1:47801/; no agent check) and `CLARIM_WINDOW_TRANSPARENCY=1` (treat
//! "Transparency effects" as on).

use std::ffi::{c_void, OsStr};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
use tao::window::{Icon, Theme, Window, WindowBuilder};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsIconic, IsZoomed, SetForegroundWindow, ShowWindow, SystemParametersInfoW, SPI_GETHIGHCONTRAST,
    SW_RESTORE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};
use wry::{NewWindowResponse, WebContext, WebViewBuilder};

use crate::{instance, logging, open, tray};

/// Window class name: how a second `clarim window` finds the first one.
const WINDOW_CLASS: &str = "ClarimWindow";
/// Default inner size in logical pixels (what the Edge app window used).
const DEFAULT_SIZE: (f64, f64) = (1180.0, 820.0);
const MIN_SIZE: (f64, f64) = (480.0, 360.0);
/// Room left around a window that is cut down to a screen (taskbar, window shadow).
const SCREEN_MARGIN: u32 = 48;
/// Height below the window's top edge whose horizontal centre must be on a screen for the saved place to count.
const TITLE_PROBE: i32 = 16;

// ------------------------------------------------------------------------------------------ backdrop

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backdrop {
    Acrylic,
    Mica,
    Solid,
}

impl Backdrop {
    /// The `data-backdrop` value the dashboard styles itself by.
    fn name(self) -> &'static str {
        match self {
            Backdrop::Acrylic => "acrylic",
            Backdrop::Mica => "mica",
            Backdrop::Solid => "solid",
        }
    }
}

/// Which backdrop the window ends up with. `apply` asks Windows for one kind and says whether it took it. The OS
/// build is not an input: window-vibrancy checks it itself and answers an `Err` Windows 10 before 1809 (acrylic) or
/// before Windows 11 (mica) cannot take. (Between 1809 and Windows 11 22H2 acrylic is the old accent-policy kind: it
/// works but drags and resizes slowly; 22H2 and later use the system backdrop, which does not.)
fn choose_backdrop(transparency_on: bool, high_contrast: bool, mut apply: impl FnMut(Backdrop) -> bool) -> Backdrop {
    if !transparency_on || high_contrast {
        return Backdrop::Solid;
    }
    [Backdrop::Acrylic, Backdrop::Mica].into_iter().find(|&b| apply(b)).unwrap_or(Backdrop::Solid)
}

/// "Transparency effects" in Windows Settings: `EnableTransparency` (DWORD, 0 = off). A missing value means on.
fn transparency_enabled() -> bool {
    if std::env::var_os("CLARIM_WINDOW_TRANSPARENCY").is_some_and(|v| v == "1") {
        return true;
    }
    let mut value = 0u32;
    let mut bytes = size_of::<u32>() as u32;
    // SAFETY: the key and value names are static NUL-terminated strings; `value` is valid for `bytes` bytes and
    // `bytes` is a valid in/out pointer.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("EnableTransparency"),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut value).cast::<c_void>()),
            Some(&mut bytes),
        )
    };
    status != ERROR_SUCCESS || value != 0
}

fn high_contrast() -> bool {
    let mut hc = HIGHCONTRASTW { cbSize: size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    // SAFETY: `hc` is a HIGHCONTRASTW with its size set, valid for the whole call.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            Some(std::ptr::from_mut(&mut hc).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    read.is_ok() && hc.dwFlags.contains(HCF_HIGHCONTRASTON)
}

// ------------------------------------------------------------------------------------------ remembered place

/// Where the window was when it was last closed (physical pixels; the place and size of the normal, not maximized,
/// window). `window.json` next to `config.json`: the agent rewrites `config.json`, so this process leaves it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    #[serde(default)]
    maximized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Rect {
    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && i64::from(x) < i64::from(self.x) + i64::from(self.w) && i64::from(y) < i64::from(self.y) + i64::from(self.h)
    }
}

/// `w` x `h` cut down so it fits `screen` with some room around it.
fn fit(w: u32, h: u32, screen: Rect) -> (u32, u32) {
    (w.min(screen.w.saturating_sub(SCREEN_MARGIN)), h.min(screen.h.saturating_sub(SCREEN_MARGIN)))
}

/// The saved place if it still shows on one of the `screens` (the middle of its title bar is on one), with its size cut
/// down to that screen; `None` (use the default) when the monitor it was on is gone.
fn restore(saved: Saved, screens: &[Rect]) -> Option<Saved> {
    let (cx, cy) = (saved.x.saturating_add((saved.w / 2) as i32), saved.y.saturating_add(TITLE_PROBE));
    let screen = screens.iter().find(|s| s.contains(cx, cy))?;
    let (w, h) = fit(saved.w, saved.h, *screen);
    Some(Saved { w, h, ..saved })
}

fn state_path() -> PathBuf {
    clarim_core::Config::path().with_file_name("window.json")
}

fn load_saved() -> Option<Saved> {
    serde_json::from_slice(&std::fs::read(state_path()).ok()?).ok().filter(|s: &Saved| s.w > 0 && s.h > 0)
}

fn save(saved: &Saved) {
    let written = serde_json::to_vec(saved).map_err(std::io::Error::from).and_then(|json| std::fs::write(state_path(), json));
    if let Err(e) = written {
        log::warn!("cannot remember the window place: {e}");
    }
}

/// Asks Windows, not tao: tao raises `Moved` before it updates its own maximized flag, so `Window::is_maximized` is
/// stale exactly when the place must not be taken.
fn is_maximized(window: &Window) -> bool {
    // SAFETY: a plain query of the window handle, which lives as long as `window`.
    unsafe { IsZoomed(HWND(window.hwnd() as _)) }.as_bool()
}

/// The window's current normal place, or `None` while it is minimized or maximized, or has no size.
fn current_place(window: &Window) -> Option<Saved> {
    if window.is_minimized() || is_maximized(window) {
        return None;
    }
    let (pos, size) = (window.outer_position().ok()?, window.inner_size());
    (size.width > 0 && size.height > 0).then_some(Saved { x: pos.x, y: pos.y, w: size.width, h: size.height, maximized: false })
}

// ------------------------------------------------------------------------------------------ the window

/// A new-window request (a `target="_blank"` link of the page) goes to the default browser, web links only.
fn is_web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn open_external(url: &str) {
    if !is_web_url(url) {
        log::warn!("not opening a non-web link from the dashboard");
    } else if let Err(e) = open::shell_execute(OsStr::new(url), None) {
        log::warn!("cannot open {url}: {e:#}");
    }
}

/// Brings the window of a running `clarim window` to the front. The agent hands this process the right to do it
/// (`open::start_window`); without it Windows only flashes the taskbar button.
fn focus_running_window() {
    let class: Vec<u16> = WINDOW_CLASS.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `class` is a live NUL-terminated string; the other calls only take the handle FindWindowW returned.
    unsafe {
        if let Ok(hwnd) = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Entry point of `clarim window`. Returns when the window is closed.
pub fn run(port: u16) -> Result<()> {
    let dev_url = std::env::var("CLARIM_WINDOW_URL").ok().filter(|u| !u.is_empty());
    if dev_url.is_none() && !instance::agent_running() {
        bail!("Clarim is not running: start it with `clarim` (or install the service) first");
    }
    let Some(_slot) = instance::acquire_window()? else {
        log::info!("the dashboard window is already open; bringing it to the front");
        focus_running_window();
        return Ok(());
    };
    let url = dev_url.unwrap_or_else(|| open::dashboard_url(port));
    match show(&url) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::warn!("cannot open the dashboard window: {e:#}; using the browser");
            open::open_in_browser(&url)
        }
    }
}

/// Everything up to and including the first frame can fail (no WebView2 runtime, no window); once the window is
/// shown this only returns when it is closed. The window stays hidden until the page view exists, so a failure leaves
/// no empty window flashing before the browser fallback.
fn show(url: &str) -> Result<()> {
    let mut event_loop = EventLoop::new();
    let screens: Vec<Rect> = event_loop
        .available_monitors()
        .map(|m| Rect { x: m.position().x, y: m.position().y, w: m.size().width, h: m.size().height })
        .collect();
    let primary = event_loop.primary_monitor();
    let scale = primary.as_ref().map_or(1.0, |m| m.scale_factor());
    let primary_rect = primary.map(|m| Rect { x: m.position().x, y: m.position().y, w: m.size().width, h: m.size().height });

    let transparency = transparency_enabled();
    let high_contrast = high_contrast();
    let glass = transparency && !high_contrast;
    let icon = Icon::from_rgba(tray::glyph_rgba(false), tray::ICON_SIZE, tray::ICON_SIZE).ok();
    let builder = WindowBuilder::new()
        .with_title("Clarim")
        .with_transparent(glass)
        // Measured on Windows 11 22631 (system title bar on): without this Windows keeps an opaque surface behind the
        // page (white) and neither the desktop nor the backdrop shows through, whatever `with_transparent` says.
        .with_no_redirection_bitmap(glass)
        .with_visible(false)
        .with_min_inner_size(LogicalSize::new(MIN_SIZE.0, MIN_SIZE.1))
        .with_window_icon(icon.clone())
        .with_window_classname(WINDOW_CLASS)
        .with_taskbar_icon(icon);
    let restored = load_saved().and_then(|s| restore(s, &screens));
    let builder = match restored {
        Some(s) => builder
            .with_inner_size(PhysicalSize::new(s.w, s.h))
            .with_position(PhysicalPosition::new(s.x, s.y))
            .with_maximized(s.maximized),
        None => {
            let (w, h) = (DEFAULT_SIZE.0 * scale, DEFAULT_SIZE.1 * scale);
            let (w, h) = primary_rect.map_or((w as u32, h as u32), |r| fit(w as u32, h as u32, r));
            builder.with_inner_size(PhysicalSize::new(w, h))
        }
    };
    let window = builder.build(&event_loop).map_err(|e| anyhow!("cannot create the window: {e}"))?;

    let dark = window.theme() == Theme::Dark;
    // Only the old accent-policy acrylic (before Windows 11 22H2) takes a tint; the system backdrop ignores it.
    let tint = if dark { (16, 18, 24, 150) } else { (243, 245, 250, 150) };
    let backdrop = choose_backdrop(transparency, high_contrast, |kind| {
        let applied = match kind {
            Backdrop::Acrylic => window_vibrancy::apply_acrylic(&window, Some(tint)),
            _ => window_vibrancy::apply_mica(&window, None),
        };
        applied.inspect_err(|e| log::warn!("backdrop {} refused: {e}", kind.name())).is_ok()
    });
    log::info!("dashboard window: backdrop {}", backdrop.name());

    // The page's own --bg (dashboard.html), for the instant before it paints: WebView2 would show white.
    let background = if dark { (14, 16, 20, 255) } else { (241, 243, 248, 255) };
    // `document.documentElement` may not exist yet when the script runs: wait for it.
    let init = format!(
        "(()=>{{const s=()=>{{document.documentElement.dataset.backdrop='{}'}};\
         if(document.documentElement)s();else new MutationObserver((_,o)=>{{o.disconnect();s()}}).observe(document,{{childList:true}})}})()",
        backdrop.name()
    );
    // The default user data folder sits next to the exe, in Program Files: not writable for the user.
    let data_dir = logging::log_path().with_file_name("WebView2");
    let _ = std::fs::create_dir_all(&data_dir);
    let mut context = WebContext::new(Some(data_dir));
    let _webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(url)
        .with_transparent(glass)
        .with_background_color(background)
        .with_initialization_script(init)
        .with_new_window_req_handler(|url, _| {
            open_external(&url);
            NewWindowResponse::Deny
        })
        .build(&window)
        .map_err(|e| anyhow!("cannot start WebView2: {e}"))?;

    window.set_visible(true);
    window.set_focus();
    // A window that opens maximized keeps the saved normal place: what it reports now is the maximized one.
    let mut place = restored.or_else(|| current_place(&window));
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::Wait;
        let Event::WindowEvent { event, .. } = event else { return };
        match event {
            // A minimized or maximized window has no normal place to take: the last one stays (`maximized` is saved apart).
            WindowEvent::Moved(_) | WindowEvent::Resized(_) => place = current_place(&window).or(place),
            WindowEvent::CloseRequested => {
                if let Some(p) = place {
                    save(&Saved { maximized: is_maximized(&window), ..p });
                }
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };
    /// A portrait monitor to the left of the main one (negative x, like Windows lays them out).
    const LEFT: Rect = Rect { x: -900, y: 65, w: 900, h: 1440 };

    fn saved(x: i32, y: i32, w: u32, h: u32) -> Saved {
        Saved { x, y, w, h, maximized: false }
    }

    #[test]
    fn solid_when_transparency_is_off_or_high_contrast_without_asking_windows() {
        for (transparency, contrast) in [(false, false), (true, true), (false, true)] {
            let backdrop = choose_backdrop(transparency, contrast, |_| panic!("must not even ask"));
            assert_eq!(backdrop, Backdrop::Solid, "transparency {transparency}, high contrast {contrast}");
        }
    }

    #[test]
    fn acrylic_first_then_mica_then_solid() {
        let mut asked = Vec::new();
        let accepts = |ok: &'static [Backdrop], asked: &mut Vec<Backdrop>| {
            choose_backdrop(true, false, |b| {
                asked.push(b);
                ok.contains(&b)
            })
        };
        assert_eq!(accepts(&[Backdrop::Acrylic, Backdrop::Mica], &mut asked), Backdrop::Acrylic);
        assert_eq!(asked, [Backdrop::Acrylic], "mica is not tried when acrylic took");
        asked.clear();
        assert_eq!(accepts(&[Backdrop::Mica], &mut asked), Backdrop::Mica);
        assert_eq!(asked, [Backdrop::Acrylic, Backdrop::Mica]);
        asked.clear();
        assert_eq!(accepts(&[], &mut asked), Backdrop::Solid);
        assert_eq!(asked, [Backdrop::Acrylic, Backdrop::Mica], "both refused");
    }

    #[test]
    fn the_page_gets_the_names_its_css_matches() {
        let names: Vec<_> = [Backdrop::Acrylic, Backdrop::Mica, Backdrop::Solid].map(Backdrop::name).into();
        assert_eq!(names, ["acrylic", "mica", "solid"]);
        let html = clarim_web::DASHBOARD_HTML;
        for name in names {
            assert!(html.contains(&format!("data-backdrop={name}")), "dashboard.html has no data-backdrop={name} rule");
        }
    }

    #[test]
    fn a_saved_place_on_a_screen_is_kept() {
        let s = saved(100, 80, 1180, 820);
        assert_eq!(restore(s, &[SCREEN]), Some(s));
        let s = saved(-800, 100, 700, 900);
        assert_eq!(restore(s, &[SCREEN, LEFT]), Some(s), "a monitor at negative coordinates counts");
        let s = Saved { maximized: true, ..saved(10, 10, 800, 600) };
        assert_eq!(restore(s, &[SCREEN]), Some(s), "the maximized flag is kept");
    }

    #[test]
    fn a_saved_place_on_a_gone_monitor_falls_back_to_the_default() {
        assert_eq!(restore(saved(-800, 100, 700, 900), &[SCREEN]), None, "the left monitor was unplugged");
        assert_eq!(restore(saved(1920, 0, 1180, 820), &[SCREEN]), None, "the right monitor was unplugged");
        assert_eq!(restore(saved(100, 80, 1180, 820), &[]), None, "no screens known");
        assert_eq!(restore(saved(100, 5000, 1180, 820), &[SCREEN]), None, "far below every screen");
    }

    #[test]
    fn the_middle_of_the_title_bar_decides_whether_a_hanging_window_stays() {
        let s = saved(1500, 900, 1180, 820); // hangs off the right and bottom edges
        assert_eq!(restore(s, &[SCREEN]), None, "the middle of the title bar (x 2090) is off screen");
        let s = saved(-500, 900, 1180, 820); // the middle (x 90) is on screen
        assert_eq!(restore(s, &[SCREEN]), Some(s));
    }

    #[test]
    fn a_saved_size_is_cut_down_to_its_screen() {
        let small = Rect { x: 0, y: 0, w: 1366, h: 768 };
        let s = restore(saved(0, 0, 2000, 2000), &[small]).expect("on the screen");
        assert_eq!((s.w, s.h), (1366 - SCREEN_MARGIN, 768 - SCREEN_MARGIN));
        assert_eq!((s.x, s.y), (0, 0), "the place itself is not moved");
        let s = restore(saved(0, 0, 800, 600), &[small]).expect("on the screen");
        assert_eq!((s.w, s.h), (800, 600), "a size that fits is not touched");
    }

    #[test]
    fn the_default_size_fits_a_small_screen() {
        let small = Rect { x: 0, y: 0, w: 1366, h: 768 };
        assert_eq!(fit(1180, 820, small), (1180, 768 - SCREEN_MARGIN));
        assert_eq!(fit(1180, 820, SCREEN), (1180, 820));
        assert_eq!(fit(100, 100, Rect { x: 0, y: 0, w: 10, h: 10 }), (0, 0), "a tiny screen does not underflow");
    }

    #[test]
    fn the_place_file_round_trips_and_an_old_one_without_maximized_loads() {
        let s = Saved { maximized: true, ..saved(-12, 34, 1000, 700) };
        let json = serde_json::to_string(&s).expect("serialize");
        assert_eq!(serde_json::from_str::<Saved>(&json).expect("parse"), s);
        let old: Saved = serde_json::from_str(r#"{"x":1,"y":2,"w":3,"h":4}"#).expect("no maximized key");
        assert_eq!(old, saved(1, 2, 3, 4));
        assert!(serde_json::from_str::<Saved>("not json").is_err());
    }

    #[test]
    fn the_place_file_sits_next_to_the_config_and_is_not_the_config() {
        let (state, config) = (state_path(), clarim_core::Config::path());
        assert_eq!(state.parent(), config.parent());
        assert_eq!(state.file_name().and_then(|n| n.to_str()), Some("window.json"));
    }

    #[test]
    fn only_web_links_leave_the_window() {
        assert!(is_web_url("https://vb-audio.com/Cable/"));
        assert!(is_web_url("http://example.com"));
        for bad in ["file:///C:/Windows/System32/calc.exe", "ms-settings:", "javascript:alert(1)", "C:\\Windows\\notepad.exe", ""] {
            assert!(!is_web_url(bad), "{bad}");
        }
    }

    #[test]
    fn the_window_icon_is_the_tray_glyph() {
        assert!(Icon::from_rgba(tray::glyph_rgba(false), tray::ICON_SIZE, tray::ICON_SIZE).is_ok());
    }
}
