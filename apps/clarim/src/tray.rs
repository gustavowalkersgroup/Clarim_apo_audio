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
//!
//! Other threads can also ask for a tooltip change (`set_tip`) and a discreet balloon (`notify`); both
//! are handed over through statics and applied by the loop right after the next message they wake it with.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIIF_NOSOUND, NIIF_RESPECT_QUIET_TIME,
    NIIF_WARNING, NIM_MODIFY, NOTIFYICONDATAW, QUERY_USER_NOTIFICATION_STATE, QUNS_ACCEPTS_NOTIFICATIONS,
};
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

/// Which icon Windows draws in a balloon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Balloon {
    Info,
    Warning,
}

/// A balloon waiting for the tray thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingBalloon {
    title: String,
    text: String,
    kind: Balloon,
    /// The shell refused it once already (it is kept for exactly one more try).
    failed: bool,
}

/// The notification-area id of Clarim's one icon. tray-icon numbers its icons from a process-global counter
/// that starts at 1, and `run_loop` builds the only `TrayIcon` through `TrayIconBuilder::default()`, which (unlike
/// `TrayIconBuilder::new()`) does not spend a number on a string id first. The `#[ignore]` tray test checks it.
const TRAY_UID: u32 = 1;
/// Balloons kept for the tray thread; a newer one pushes the oldest out.
const BALLOON_QUEUE_CAP: usize = 4;
const DEFAULT_TIP: &str = "Clarim";

static BALLOONS: Mutex<Vec<PendingBalloon>> = Mutex::new(Vec::new());
/// The tooltip `set_tip` asked for and the loop has not applied yet.
static TIP: Mutex<Option<String>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Drops the oldest entries past [`BALLOON_QUEUE_CAP`].
fn trim_balloons(queue: &mut Vec<PendingBalloon>) {
    let excess = queue.len().saturating_sub(BALLOON_QUEUE_CAP);
    queue.drain(..excess);
}

/// Windows lets a balloon through only while the user is at the desk and not busy: not in a full-screen
/// app or game, a presentation, quiet time or a locked session.
fn accepts_balloons(state: windows::core::Result<QUERY_USER_NOTIFICATION_STATE>) -> bool {
    matches!(state, Ok(QUNS_ACCEPTS_NOTIFICATIONS))
}

/// Shows a balloon from the tray icon, with no sound and respecting quiet time. Callable from any thread.
/// Returns false, and does nothing, when there is no tray yet or Windows says the user is busy
/// (full screen, presentation...): the caller tries again later. True means it is queued for the
/// tray thread (the shell may still refuse it, which is logged there).
pub fn notify(title: &str, text: &str, kind: Balloon) -> bool {
    if TRAY_THREAD.load(Ordering::SeqCst) == 0 {
        return false;
    }
    // SAFETY: a plain query with no arguments, valid on any thread.
    if !accepts_balloons(unsafe { SHQueryUserNotificationState() }) {
        return false;
    }
    let mut queue = lock(&BALLOONS);
    queue.push(PendingBalloon { title: title.to_owned(), text: text.to_owned(), kind, failed: false });
    trim_balloons(&mut queue);
    drop(queue);
    wake(WM_APP);
    true
}

/// Changes the icon's tooltip (applied only when it differs from what shows). Callable from any thread,
/// also before `run` starts (the initial tooltip).
pub fn set_tip(text: &str) {
    *lock(&TIP) = Some(text.to_owned());
    wake(WM_APP);
}

/// `s` as UTF-16 in a NUL-terminated field of `N` units: cut after the last whole char that fits in `N - 1`
/// units, so a surrogate pair is never split.
fn utf16_field<const N: usize>(s: &str) -> [u16; N] {
    let mut field = [0u16; N];
    let mut used = 0;
    for c in s.chars() {
        let len = c.len_utf16();
        if used + len > N - 1 {
            break;
        }
        c.encode_utf16(&mut field[used..]);
        used += len;
    }
    field
}

/// The `NIM_MODIFY` payload that raises one balloon from Clarim's icon on `hwnd`.
fn balloon_data(b: &PendingBalloon, hwnd: HWND) -> NOTIFYICONDATAW {
    let icon = match b.kind {
        Balloon::Info => NIIF_INFO,
        Balloon::Warning => NIIF_WARNING,
    };
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_INFO,
        szInfoTitle: utf16_field(&b.title),
        szInfo: utf16_field(&b.text),
        dwInfoFlags: icon | NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME,
        ..Default::default()
    }
}

/// Hands every queued balloon to `send` (the shell). One the shell refuses (explorer restarting, say) is logged
/// once and kept for one more try at the next drain, which is also when tray-icon has re-added the icon after
/// `TaskbarCreated`; a second refusal drops it.
fn drain_balloons(queue: &Mutex<Vec<PendingBalloon>>, hwnd: HWND, mut send: impl FnMut(&NOTIFYICONDATAW) -> bool) {
    let batch = std::mem::take(&mut *lock(queue));
    let mut retry = Vec::new();
    for mut balloon in batch {
        if send(&balloon_data(&balloon, hwnd)) {
            continue;
        }
        if balloon.failed {
            log::debug!("tray balloon \"{}\" refused twice, dropped", balloon.title);
        } else {
            log::warn!("the shell refused the tray balloon \"{}\"; trying once more", balloon.title);
            balloon.failed = true;
            retry.push(balloon);
        }
    }
    if !retry.is_empty() {
        let mut queue = lock(queue);
        retry.append(&mut queue); // the retries go before anything queued meanwhile
        *queue = retry;
        trim_balloons(&mut queue);
    }
}

fn toggle_text(paused: bool) -> &'static str {
    if paused {
        "Ativar microfone"
    } else {
        "Pausar microfone"
    }
}

pub(crate) const ICON_SIZE: u32 = 32;
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

/// RGBA pixels of the tray glyph, anti-aliased with 4x4 supersampling (the dashboard window uses it as its icon too).
pub(crate) fn glyph_rgba(paused: bool) -> Vec<u8> {
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
    let mut shown_tip = lock(&TIP).take().unwrap_or_else(|| DEFAULT_TIP.to_owned());
    // `default()`, not `new()`: `new()` takes counter value 1 for its generated string id and the icon would
    // get `uID` 2 (see `TRAY_UID`).
    let tray = TrayIconBuilder::default()
        .with_id("clarim")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false) // left click opens the dashboard, the menu is on the right click
        .with_tooltip(&shown_tip)
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

        // Tooltip and balloons asked for by other threads (`set_tip`, `notify`).
        let wanted_tip = lock(&TIP).take();
        if let Some(tip) = wanted_tip.filter(|t| *t != shown_tip) {
            if let Err(e) = tray.set_tooltip(Some(&tip)) {
                log::warn!("cannot update the tray tooltip: {e}");
            }
            shown_tip = tip;
        }
        // SAFETY: plain Win32 call with a fully initialised NOTIFYICONDATAW that outlives it.
        let send = |nid: &NOTIFYICONDATAW| unsafe { Shell_NotifyIconW(NIM_MODIFY, nid) }.as_bool();
        drain_balloons(&BALLOONS, HWND(tray.window_handle() as _), send);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{BOOL, E_FAIL};
    use windows::Win32::UI::Shell::{
        NIF_TIP, QUNS_BUSY, QUNS_NOT_PRESENT, QUNS_PRESENTATION_MODE, QUNS_QUIET_TIME, QUNS_RUNNING_D3D_FULL_SCREEN,
    };
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

    fn balloon(title: &str, text: &str, kind: Balloon) -> PendingBalloon {
        PendingBalloon { title: title.into(), text: text.into(), kind, failed: false }
    }

    /// The NUL-terminated text of a UTF-16 field.
    fn field_text(field: &[u16]) -> String {
        let len = field.iter().position(|&u| u == 0).expect("field is NUL-terminated");
        String::from_utf16(&field[..len]).expect("field holds valid UTF-16")
    }

    fn hwnd() -> HWND {
        HWND(0x1234 as *mut std::ffi::c_void)
    }

    fn titles(queue: &Mutex<Vec<PendingBalloon>>) -> Vec<String> {
        lock(queue).iter().map(|b| b.title.clone()).collect()
    }

    #[test]
    fn utf16_field_fits_text_and_ends_with_a_nul() {
        let field: [u16; 64] = utf16_field("Som vazando na chamada");
        assert_eq!(field_text(&field), "Som vazando na chamada");
        assert!(field[22..].iter().all(|&u| u == 0), "everything after the text is NUL");
        assert_eq!(field_text(&utf16_field::<64>("Ruído passando: não é eco")), "Ruído passando: não é eco");
        assert_eq!(utf16_field::<8>(""), [0u16; 8]);
    }

    #[test]
    fn utf16_field_keeps_at_most_n_minus_one_units() {
        let exact = "a".repeat(63);
        let field: [u16; 64] = utf16_field(&exact);
        assert_eq!((field_text(&field), field[63]), (exact, 0), "63 units fit with the NUL");
        let field: [u16; 64] = utf16_field(&"a".repeat(64));
        assert_eq!((field_text(&field).len(), field[63]), (63, 0), "the 64th unit is cut");
        let field: [u16; 256] = utf16_field(&"b".repeat(300));
        assert_eq!((field_text(&field).len(), field[255]), (255, 0), "szInfo keeps 255");
    }

    #[test]
    fn utf16_field_never_splits_a_surrogate_pair() {
        let emoji = "\u{1F600}"; // 2 UTF-16 units
        let fits = format!("{}{emoji}", "a".repeat(61));
        let field: [u16; 64] = utf16_field(&fits);
        assert_eq!(field_text(&field), fits, "61 + 2 = 63 units fit");
        let too_long = format!("{}{emoji}", "a".repeat(62));
        let field: [u16; 64] = utf16_field(&too_long);
        assert_eq!(field_text(&field), "a".repeat(62), "the whole emoji goes, not half of it");
        assert_eq!(field[62..], [0, 0], "no lone surrogate is left behind");
    }

    #[test]
    fn balloon_data_asks_for_a_silent_balloon_on_the_clarim_icon() {
        let warn = balloon_data(&balloon("Som vazando na chamada", "Abaixe o volume.", Balloon::Warning), hwnd());
        assert_eq!(warn.cbSize as usize, size_of::<NOTIFYICONDATAW>());
        assert_eq!((warn.hWnd, warn.uID, warn.uFlags), (hwnd(), 1, NIF_INFO));
        assert_eq!(warn.dwInfoFlags, NIIF_WARNING | NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME);
        assert_eq!(field_text(&warn.szInfoTitle), "Som vazando na chamada");
        assert_eq!(field_text(&warn.szInfo), "Abaixe o volume.");
        let info = balloon_data(&balloon("Microfone estourando", "Baixei 3 dB.", Balloon::Info), hwnd());
        assert_eq!(info.dwInfoFlags, NIIF_INFO | NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME);
    }

    #[test]
    fn balloon_data_truncates_a_long_title_and_text() {
        let nid = balloon_data(&balloon(&"t".repeat(100), &"x".repeat(400), Balloon::Info), hwnd());
        assert_eq!((field_text(&nid.szInfoTitle).len(), field_text(&nid.szInfo).len()), (63, 255));
    }

    #[test]
    fn only_the_accepting_state_lets_a_balloon_through() {
        assert!(accepts_balloons(Ok(QUNS_ACCEPTS_NOTIFICATIONS)));
        for (name, state) in [
            ("busy / full screen", QUNS_BUSY),
            ("locked / away", QUNS_NOT_PRESENT),
            ("presentation", QUNS_PRESENTATION_MODE),
            ("D3D full screen", QUNS_RUNNING_D3D_FULL_SCREEN),
            ("quiet time", QUNS_QUIET_TIME),
        ] {
            assert!(!accepts_balloons(Ok(state)), "{name}");
        }
        assert!(!accepts_balloons(Err(E_FAIL.into())), "a failed query is not a yes");
    }

    #[test]
    fn notify_says_no_while_no_tray_runs() {
        if TRAY_THREAD.load(Ordering::SeqCst) != 0 {
            return; // the #[ignore] tray test is running in this process
        }
        assert!(!notify("t", "x", Balloon::Info));
        assert!(lock(&BALLOONS).is_empty(), "nothing was queued");
    }

    #[test]
    fn the_queue_keeps_the_newest_four() {
        let mut queue: Vec<_> = (0..6).map(|i| balloon(&i.to_string(), "", Balloon::Info)).collect();
        trim_balloons(&mut queue);
        let left: Vec<_> = queue.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(left, ["2", "3", "4", "5"]);
        trim_balloons(&mut queue);
        assert_eq!(queue.len(), 4, "already within the cap");
    }

    #[test]
    fn drain_sends_every_balloon_in_order_and_empties_the_queue() {
        let queue = Mutex::new(vec![balloon("a", "", Balloon::Info), balloon("b", "", Balloon::Warning)]);
        let mut sent = Vec::new();
        drain_balloons(&queue, hwnd(), |nid| {
            sent.push(field_text(&nid.szInfoTitle));
            true
        });
        assert_eq!(sent, ["a", "b"]);
        assert!(titles(&queue).is_empty());
        drain_balloons(&queue, hwnd(), |_| panic!("nothing left to send"));
    }

    #[test]
    fn a_refused_balloon_gets_exactly_one_more_try() {
        let queue = Mutex::new(vec![balloon("a", "", Balloon::Warning)]);
        let mut tries = 0;
        drain_balloons(&queue, hwnd(), |_| {
            tries += 1;
            false
        });
        assert_eq!((tries, titles(&queue)), (1, vec!["a".to_string()]), "kept after the first refusal");
        assert!(lock(&queue)[0].failed);
        drain_balloons(&queue, hwnd(), |_| {
            tries += 1;
            false
        });
        assert_eq!((tries, titles(&queue)), (2, vec![]), "dropped after the second");
    }

    #[test]
    fn a_balloon_the_retry_gets_through_is_not_sent_again() {
        let queue = Mutex::new(vec![balloon("a", "", Balloon::Warning)]);
        let mut answers = [false, true].into_iter();
        let mut tries = 0;
        for _ in 0..3 {
            drain_balloons(&queue, hwnd(), |_| {
                tries += 1;
                answers.next().expect("no third try")
            });
        }
        assert_eq!(tries, 2);
        assert!(titles(&queue).is_empty());
    }

    #[test]
    fn a_retry_goes_before_newer_balloons_and_is_the_first_to_go_past_the_cap() {
        let queue = Mutex::new(vec![balloon("old", "", Balloon::Info)]);
        drain_balloons(&queue, hwnd(), |_| {
            lock(&queue).push(balloon("new", "", Balloon::Info)); // queued while the shell was busy refusing
            false
        });
        assert_eq!(titles(&queue), ["old", "new"], "the retry keeps its place at the front");

        let queue = Mutex::new(vec![balloon("old", "", Balloon::Info)]);
        drain_balloons(&queue, hwnd(), |_| {
            for i in 0..4 {
                lock(&queue).push(balloon(&format!("new{i}"), "", Balloon::Info));
            }
            false
        });
        assert_eq!(titles(&queue), ["new0", "new1", "new2", "new3"], "the queue stays at 4, oldest out");
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
    /// pause/resume updates, a tooltip change, a check that the icon is (window, `TRAY_UID`) for the shell,
    /// two quick simulated left clicks (must give one `Open`), and an exit request.
    /// Run it with `cargo test -p clarim tray_loop -- --ignored`.
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
            // The icon must be addressable as (window, TRAY_UID): that is how the balloons find it.
            let mut nid = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: TRAY_UID,
                uFlags: NIF_TIP,
                ..Default::default()
            };
            nid.szTip = utf16_field(DEFAULT_TIP);
            // SAFETY: plain Win32 call with a fully initialised NOTIFYICONDATAW that outlives it.
            let uid_found = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.as_bool();
            set_tip("Clarim · em chamada"); // the loop applies it through tray-icon
            for _ in 0..2 {
                // 6002 is tray-icon's private WM_USER_TRAYICON; the mouse message travels in lParam.
                // SAFETY: plain message post to a window of the tray thread, no pointers.
                unsafe { PostMessageW(hwnd, 6002, WPARAM(0), LPARAM(WM_LBUTTONUP as isize)) }.expect("post click");
            }
            std::thread::sleep(Duration::from_millis(700));
            request_exit();
            uid_found
        });
        let mut events = Vec::new();
        let started = Instant::now();
        run(|e| {
            events.push(e);
            TrayAction::None
        })
        .expect("tray run");
        let uid_found = helper.join().unwrap();
        assert!(uid_found, "NIM_MODIFY on (window_handle, {TRAY_UID}) found no icon: tray-icon's numbering changed");
        assert_eq!(events, [TrayEvent::Open], "a double click must open the dashboard once");
        assert!(started.elapsed() >= Duration::from_millis(1400), "returned before the exit request");
    }
}
