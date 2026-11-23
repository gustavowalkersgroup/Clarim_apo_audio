//! Tray icon on the calling (main) thread, driven by a plain Win32 message loop.
//!
//! `run` owns the thread until the user picks "Sair", `on_event` answers `TrayAction::Exit`, or
//! another thread calls `request_exit`. Other threads can also flip the paused look with
//! `set_mic_paused` (for example when the dashboard toggles the mic).
//!
//! tray-icon and muda raise their events synchronously from window procedures that run inside
//! `DispatchMessageW` on this thread, so draining their receivers after every message is already
//! prompt. (A `set_event_handler` would be wrong here: it is process-global, set-once, and it
//! diverts the events away from the receivers.)

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage, MSG,
    PM_NOREMOVE, WM_APP, WM_QUIT, WM_USER,
};

/// What the user asked for in the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// "Abrir painel", or a left click on the icon.
    Open,
    /// "Pausar microfone" / "Ativar microfone".
    ToggleMic,
    /// "Sair".
    Quit,
}

/// What the tray does after `on_event` handled an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    None,
    /// Show the mic as paused (grey icon, "Ativar microfone") or active (accent icon, "Pausar microfone").
    SetMicPaused(bool),
    /// Leave the message loop; `run` returns.
    Exit,
}

/// Thread id of the thread inside `run`, 0 when none is.
static TRAY_THREAD: AtomicU32 = AtomicU32::new(0);
static PAUSED: AtomicBool = AtomicBool::new(false);
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Left clicks closer together than this count as one (a double click must not open two windows).
const OPEN_DEBOUNCE: Duration = Duration::from_millis(700);

fn wake(message: u32) {
    let thread = TRAY_THREAD.load(Ordering::SeqCst);
    if thread != 0 {
        // SAFETY: posts a plain message with no pointers to a thread id; a stale id only makes it fail.
        // Failure is ignored on purpose: the loop re-reads the shared state after its next message anyway.
        let _ = unsafe { PostThreadMessageW(thread, message, WPARAM(0), LPARAM(0)) };
    }
}

/// Asks the tray thread to leave `run`. Callable from any thread, also before `run` starts.
pub fn request_exit() {
    EXIT_REQUESTED.store(true, Ordering::SeqCst);
    wake(WM_QUIT);
}

/// Shows the mic as paused or active. Callable from any thread, also before `run` starts (the
/// initial look). If the menu is open at that moment the update shows once it closes.
pub fn set_mic_paused(paused: bool) {
    PAUSED.store(paused, Ordering::SeqCst);
    wake(WM_APP);
}

fn toggle_text(paused: bool) -> &'static str {
    if paused {
        "Ativar microfone"
    } else {
        "Pausar microfone"
    }
}

const ICON_SIZE: u32 = 32;
const ACTIVE_RGB: [u8; 3] = [0x2F, 0x81, 0xF7]; // accent blue
const PAUSED_RGB: [u8; 3] = [0x8A, 0x8F, 0x98]; // grey

fn segment_distance(x: f32, y: f32, (ax, ay): (f32, f32), (bx, by): (f32, f32)) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    (x - (ax + t * dx)).hypot(y - (ay + t * dy))
}

/// Is the point (in 32x32 icon coordinates) on the mic glyph: capsule, cradle arc, stem, base,
/// plus a slash when paused so the state does not rest on colour alone.
fn on_glyph(x: f32, y: f32, paused: bool) -> bool {
    let capsule = segment_distance(x, y, (16.0, 10.5), (16.0, 15.5)) <= 5.0;
    let cradle = y >= 16.0 && (7.5..=9.5).contains(&(x - 16.0).hypot(y - 16.0));
    let stem = (15.0..=17.0).contains(&x) && (25.0..=28.0).contains(&y);
    let base = (11.0..=21.0).contains(&x) && (27.0..=29.0).contains(&y);
    let slash = paused && segment_distance(x, y, (6.0, 5.0), (26.0, 27.0)) <= 1.4;
    capsule || cradle || stem || base || slash
}

/// RGBA pixels of the tray glyph, anti-aliased with 4x4 supersampling.
fn glyph_rgba(paused: bool) -> Vec<u8> {
    const SS: u32 = 4;
    let [r, g, b] = if paused { PAUSED_RGB } else { ACTIVE_RGB };
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for py in 0..ICON_SIZE {
        for px in 0..ICON_SIZE {
            let covered = (0..SS * SS)
                .filter(|s| {
                    let x = px as f32 + ((s % SS) as f32 + 0.5) / SS as f32;
                    let y = py as f32 + ((s / SS) as f32 + 0.5) / SS as f32;
                    on_glyph(x, y, paused)
                })
                .count() as u32;
            rgba.extend_from_slice(&[r, g, b, (covered * 255 / (SS * SS)) as u8]);
        }
    }
    rgba
}

fn icon(paused: bool) -> Result<Icon> {
    Icon::from_rgba(glyph_rgba(paused), ICON_SIZE, ICON_SIZE).map_err(|e| anyhow!("tray icon: {e}"))
}

/// Creates the tray icon and pumps messages until exit. Must be called on the thread that should
/// own the icon (the main thread), at most once per process at a time.
pub fn run(mut on_event: impl FnMut(TrayEvent) -> TrayAction) -> Result<()> {
    // A thread has no message queue until it first calls into USER32; make one now so messages
    // other threads post right after `TRAY_THREAD` is set are kept instead of dropped.
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid MSG; PM_NOREMOVE with an empty range only creates the queue.
    let _ = unsafe { PeekMessageW(&mut msg, HWND::default(), WM_USER, WM_USER, PM_NOREMOVE) };
    // SAFETY: trivial query of the calling thread's id.
    TRAY_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let result = run_loop(&mut on_event);
    TRAY_THREAD.store(0, Ordering::SeqCst); // thread ids get reused; stop other threads posting to this one
    EXIT_REQUESTED.store(false, Ordering::SeqCst); // a later `run` must not exit at once
    result
}

fn run_loop(on_event: &mut impl FnMut(TrayEvent) -> TrayAction) -> Result<()> {
    let mut shown_paused = PAUSED.load(Ordering::SeqCst);
    let open = MenuItem::new("Abrir painel", true, None);
    let toggle = MenuItem::new(toggle_text(shown_paused), true, None);
    let quit = MenuItem::new("Sair", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &toggle, &PredefinedMenuItem::separator(), &quit])
        .map_err(|e| anyhow!("tray menu: {e}"))?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false) // left click opens the dashboard, the menu is on the right click
        .with_tooltip("Clarim")
        .with_icon(icon(shown_paused)?)
        .build()
        .map_err(|e| anyhow!("tray icon: {e}"))?;

    let mut last_open: Option<Instant> = None;
    let mut exit = false;
    while !exit && !EXIT_REQUESTED.load(Ordering::SeqCst) {
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid MSG; a null window handle receives every message of this thread.
        let got = unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) };
        if got.0 == -1 {
            // A broken message queue is a failure, not a quit: the agent then runs on without a tray
            // (and the dashboard-died watch still ends it with a failure code).
            return Err(anyhow!("GetMessageW failed: {}", windows::core::Error::from_win32()));
        }
        if got.0 == 0 {
            break; // WM_QUIT (request_exit)
        }
        // SAFETY: `msg` was just filled in by GetMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let mut events = Vec::new();
        while let Ok(e) = MenuEvent::receiver().try_recv() {
            if e.id == *open.id() {
                events.push(TrayEvent::Open);
            } else if e.id == *toggle.id() {
                events.push(TrayEvent::ToggleMic);
            } else if e.id == *quit.id() {
                events.push(TrayEvent::Quit);
            }
        }
        while let Ok(e) = TrayIconEvent::receiver().try_recv() {
            let left_click = matches!(
                e,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            );
            if left_click && last_open.is_none_or(|t| t.elapsed() >= OPEN_DEBOUNCE) {
                last_open = Some(Instant::now());
                events.push(TrayEvent::Open);
            }
        }
        for event in events {
            match on_event(event) {
                TrayAction::None => {}
                TrayAction::SetMicPaused(paused) => PAUSED.store(paused, Ordering::SeqCst),
                TrayAction::Exit => exit = true,
            }
        }

        // Pick up pauses requested from the callback above or from another thread.
        let paused = PAUSED.load(Ordering::SeqCst);
        if paused != shown_paused {
            toggle.set_text(toggle_text(paused));
            if let Err(e) = tray.set_icon(Some(icon(paused)?)) {
                log::warn!("cannot update the tray icon: {e}");
            }
            shown_paused = paused;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{EnumThreadWindows, GetClassNameW, PostMessageW, WM_LBUTTONUP};

    fn alpha(rgba: &[u8], x: u32, y: u32) -> u8 {
        rgba[((y * ICON_SIZE + x) * 4 + 3) as usize]
    }

    #[test]
    fn glyph_is_a_mic_in_the_state_colour() {
        let active = glyph_rgba(false);
        assert_eq!(active.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        assert_eq!(alpha(&active, 16, 12), 255, "capsule centre is solid");
        assert_eq!(alpha(&active, 1, 1), 0, "corner is transparent");
        assert_eq!(alpha(&active, 16, 22), 0, "gap between capsule and stem is empty");
        assert_eq!(&active[((12 * ICON_SIZE + 16) * 4) as usize..][..3], &ACTIVE_RGB);

        let paused = glyph_rgba(true);
        assert_eq!(&paused[((12 * ICON_SIZE + 16) * 4) as usize..][..3], &PAUSED_RGB);
        assert_ne!(active, paused, "paused adds the slash and changes colour");
    }

    #[test]
    fn glyph_is_edge_anti_aliased() {
        let rgba = glyph_rgba(false);
        assert!(rgba.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 255), "expected partially covered pixels");
    }

    #[test]
    fn glyph_converts_to_a_tray_icon() {
        assert!(icon(false).is_ok());
        assert!(icon(true).is_ok());
    }

    #[test]
    fn toggle_text_follows_the_state() {
        assert_eq!(toggle_text(false), "Pausar microfone");
        assert_eq!(toggle_text(true), "Ativar microfone");
    }

    /// Finds the hidden window tray-icon created on the tray thread (class "tray_icon_app").
    unsafe extern "system" fn pick_tray_window(hwnd: HWND, out: LPARAM) -> BOOL {
        let mut class = [0u16; 64];
        // SAFETY: `class` is a valid buffer; GetClassNameW returns the number of units it wrote.
        let len = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
        if String::from_utf16_lossy(&class[..len]) == "tray_icon_app" {
            // SAFETY: `out` is the `*mut Option<HWND>` the test passed to EnumThreadWindows.
            unsafe { *(out.0 as *mut Option<HWND>) = Some(hwnd) };
            return BOOL(0); // stop enumerating
        }
        BOOL(1)
    }

    /// Puts a real icon in the notification area for about 2 s, then drives it from another thread:
    /// pause/resume updates, two quick simulated left clicks (must give one `Open`), and an exit
    /// request. Run it with `cargo test -p clarim tray_loop -- --ignored`.
    #[test]
    #[ignore]
    fn tray_loop_end_to_end() {
        let helper = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            set_mic_paused(true);
            std::thread::sleep(Duration::from_millis(300));
            set_mic_paused(false);

            let mut tray_window: Option<HWND> = None;
            // SAFETY: the callback only writes the `Option<HWND>` it is handed, which outlives the call.
            let _ = unsafe {
                EnumThreadWindows(
                    TRAY_THREAD.load(Ordering::SeqCst),
                    Some(pick_tray_window),
                    LPARAM(&mut tray_window as *mut Option<HWND> as isize),
                )
            };
            let hwnd = tray_window.expect("tray window");
            for _ in 0..2 {
                // 6002 is tray-icon's private WM_USER_TRAYICON; the mouse message travels in lParam.
                // SAFETY: plain message post to a window of the tray thread, no pointers.
                unsafe { PostMessageW(hwnd, 6002, WPARAM(0), LPARAM(WM_LBUTTONUP as isize)) }.expect("post click");
            }
            std::thread::sleep(Duration::from_millis(700));
            request_exit();
        });
        let mut events = Vec::new();
        let started = Instant::now();
        run(|e| {
            events.push(e);
            TrayAction::None
        })
        .expect("tray run");
        helper.join().unwrap();
        assert_eq!(events, [TrayEvent::Open], "a double click must open the dashboard once");
        assert!(started.elapsed() >= Duration::from_millis(1400), "returned before the exit request");
    }
}
