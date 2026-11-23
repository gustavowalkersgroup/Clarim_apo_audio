//! Real-time mic pipeline:
//! WASAPI capture (48 kHz mono f32) -> ring -> DSP thread (`MicChain`) -> rings ->
//! WASAPI render into the virtual cable (+ optional monitor device).
//!
//! Threads never poll: the capture thread wakes the DSP thread after every packet, the DSP thread wakes
//! the render thread(s) after every processed block, and a small tick thread per render stream wakes its
//! render thread at every audio-engine tick. The render thread writes a block the moment it exists and
//! steers the queue with `pacing::Pacer` (see pacing.rs).
//!
//! `MicEngine` (engine.rs) owns one `EngineHandle` at a time; devices are already resolved to ids here.

use crate::calib::{CalibShared, Calibrator};
use crate::chain::{DspParams, MicChain};
use crate::devices::Flow;
use crate::dsp::FRAME;
use crate::pacing::{self, Pacer};
use anyhow::{anyhow, bail, Context, Result};
use clarim_core::DenoiseMode;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;
use std::fmt::Display;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};
use wasapi::{Direction, SampleType, ShareMode, WaveFormat};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority,
    THREAD_PRIORITY_TIME_CRITICAL,
};

const RING_CAPACITY: usize = FRAME * 24; // ~240 ms of headroom per hop
/// Level meters hold a peak and fall by this factor per 10 ms frame (about a 200 ms time constant).
const METER_DECAY: f32 = 0.95;
/// Smoothing of the dsp_us / latency readouts (per frame).
const READOUT_SMOOTHING: f32 = 0.05;
/// The capture thread waits this long for the capture event, then checks the device anyway (so a
/// device that disappeared surfaces as an error within ~100 ms).
const CAPTURE_WAIT_MS: u32 = 100;
/// The DSP thread sleeps until the capture thread wakes it; this only bounds the sleep (stop requests).
const DSP_WAIT: Duration = Duration::from_millis(20);
/// A render thread with nothing to do still checks its device this often (device loss shows up here).
const RENDER_WAIT: Duration = Duration::from_millis(200);
/// More than this queued in a render ring (2 blocks; at the very first write, 1) is a stale backlog,
/// not jitter.
const STALE_RING: usize = FRAME * 2;
/// A dry spell shorter than this is arrival jitter (the cushion grows); a longer one is a stall such as
/// the DeepFilterNet model build.
const STALL: Duration = Duration::from_millis(150);
/// Consecutive silent-flagged capture frames before the "muted or blocked" note (2 s).
const SILENT_NOTE_FRAMES: usize = 96_000;

/// Lock-free levels shared between the engine threads and `EngineHandle`.
pub struct Meters {
    in_peak: AtomicU32,
    out_peak: AtomicU32,
    vad: AtomicU32,
    dsp_us: AtomicU32,
    /// ms (f32 bits): capture period / 2 + capture backlog + denoiser delay + DSP time.
    pipeline_ms: AtomicU32,
    /// ms (f32 bits): averaged frames queued between the DSP thread and the audio engine.
    render_queue_ms: AtomicU32,
    /// 0 = not known yet (model still building), else 1 + DenoiseMode as u32.
    denoise_active: AtomicU32,
    focus_open: AtomicBool,
    /// Capture period in ms (f32 bits), for the latency estimate.
    capture_period_ms: AtomicU32,
    /// The device has been delivering silence-flagged packets for a couple of seconds.
    mic_silent: AtomicBool,
    /// The optional monitor stream could not start or stopped (the mic itself keeps running).
    monitor_failed: AtomicBool,
    /// Cable stream only. An underrun = the audio engine ran out of queued audio; an overrun = audio was
    /// dropped (a stall, a full ring, a capture discontinuity); a slip = one sample removed or repeated to
    /// follow the cable's clock.
    pub underruns: AtomicU32,
    pub overruns: AtomicU32,
    pub slips: AtomicU32,
}

/// Snapshot of the stream health counters (see `Meters`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub underruns: u32,
    pub overruns: u32,
    pub slips: u32,
}

impl Meters {
    fn new() -> Self {
        Self {
            in_peak: AtomicU32::new(0),
            out_peak: AtomicU32::new(0),
            vad: AtomicU32::new(0),
            dsp_us: AtomicU32::new(0),
            pipeline_ms: AtomicU32::new(0),
            render_queue_ms: AtomicU32::new(0),
            denoise_active: AtomicU32::new(0),
            focus_open: AtomicBool::new(false),
            capture_period_ms: AtomicU32::new(0),
            mic_silent: AtomicBool::new(false),
            monitor_failed: AtomicBool::new(false),
            underruns: AtomicU32::new(0),
            overruns: AtomicU32::new(0),
            slips: AtomicU32::new(0),
        }
    }
    fn set_f32(slot: &AtomicU32, v: f32) {
        slot.store(v.to_bits(), Ordering::Relaxed);
    }
    fn get_f32(slot: &AtomicU32) -> f32 {
        f32::from_bits(slot.load(Ordering::Relaxed))
    }
    pub fn in_peak(&self) -> f32 {
        Self::get_f32(&self.in_peak)
    }
    pub fn out_peak(&self) -> f32 {
        Self::get_f32(&self.out_peak)
    }
    pub fn vad(&self) -> f32 {
        Self::get_f32(&self.vad)
    }
    pub fn dsp_us(&self) -> u32 {
        self.dsp_us.load(Ordering::Relaxed)
    }
    /// ESTIMATE of the mic-to-cable delay in ms: half a capture period, the capture backlog, the denoiser's
    /// own delay, the DSP time and the audio the render thread keeps queued. It does not include the
    /// audio engine's own mixing delay, the cable driver or the buffers of the app reading the cable.
    pub fn latency_ms(&self) -> f32 {
        Self::get_f32(&self.pipeline_ms) + Self::get_f32(&self.render_queue_ms)
    }
    pub fn focus_open(&self) -> bool {
        self.focus_open.load(Ordering::Relaxed)
    }
    /// True once the capture device has returned only silence-flagged packets for about 2 s (typically
    /// the Windows microphone privacy switch).
    pub fn mic_silent(&self) -> bool {
        self.mic_silent.load(Ordering::Relaxed)
    }
    /// True once the monitor render thread has given up (device missing, busy or unplugged later). It
    /// stays true for the life of this engine: only a restart (toggling the monitor) tries again.
    pub fn monitor_failed(&self) -> bool {
        self.monitor_failed.load(Ordering::Relaxed)
    }
    pub fn counters(&self) -> Counters {
        Counters {
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            slips: self.slips.load(Ordering::Relaxed),
        }
    }
    /// The denoiser that is really running; `None` until the DSP thread has built its chain.
    pub fn denoise_active(&self) -> Option<DenoiseMode> {
        match self.denoise_active.load(Ordering::Relaxed) {
            1 => Some(DenoiseMode::Off),
            2 => Some(DenoiseMode::Light),
            3 => Some(DenoiseMode::Strong),
            _ => None,
        }
    }
    fn set_denoise_active(&self, mode: DenoiseMode) {
        let code = match mode {
            DenoiseMode::Off => 1,
            DenoiseMode::Light => 2,
            DenoiseMode::Strong => 3,
        };
        self.denoise_active.store(code, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// None = default capture device
    pub input_id: Option<String>,
    /// Render endpoint to write the processed mic into (VB-Cable "CABLE Input")
    pub output_id: String,
    /// Self-monitor render endpoint (headphones). `None` = no monitor thread at all; the engine is
    /// restarted when monitoring is switched on or off.
    pub monitor_id: Option<String>,
    pub params: DspParams,
    pub calib: Arc<CalibShared>,
}

/// Live updates for the DSP thread (no stream restart, no glitch). `Copy` payloads only.
#[derive(Clone, Copy)]
pub enum EngineCmd {
    SetParams(DspParams),
}

pub struct EngineHandle {
    cmd_tx: Sender<EngineCmd>,
    running: Arc<AtomicBool>,
    pub meters: Arc<Meters>,
    error: Arc<Mutex<Option<String>>>,
    threads: Vec<JoinHandle<()>>,
}

impl EngineHandle {
    /// Queue a live update. False when the queue is full or the DSP thread is gone; the caller keeps
    /// its "applied" state unchanged so the next `apply` sends it again.
    pub fn send(&self, cmd: EngineCmd) -> bool {
        match self.cmd_tx.try_send(cmd) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
    pub fn error(&self) -> Option<String> {
        lock(&self.error).clone()
    }
    pub fn stop(mut self) {
        self.shutdown();
    }
    fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // sleeping threads wake up now instead of at their next timeout
        for t in &self.threads {
            t.thread().unpark();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A poisoned mutex only means another thread panicked; the stored value is still usable.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn boost_thread_priority() {
    // SAFETY: GetCurrentThread returns a pseudo-handle that is always valid for the calling
    // thread, and SetThreadPriority only changes scheduling; no memory is accessed.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

/// Real-time scheduling of the current thread: registered with MMCSS as "Pro Audio" (so the Windows
/// scheduler keeps audio threads ahead of ordinary work), else plain TIME_CRITICAL priority. The MMCSS
/// registration is reverted when the guard drops.
struct RtGuard(Option<HANDLE>);

impl RtGuard {
    fn enter() -> Self {
        let mut task_index = 0u32;
        // SAFETY: `w!` is a valid NUL-terminated UTF-16 string and `task_index` a valid out pointer for
        // the duration of the call; the returned handle is only passed back to Revert on this thread.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
            Ok(h) => Self(Some(h)),
            Err(e) => {
                log::debug!("MMCSS registration failed ({e}), using TIME_CRITICAL priority");
                boost_thread_priority();
                Self(None)
            }
        }
    }
}

impl Drop for RtGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // SAFETY: `h` came from AvSetMmThreadCharacteristicsW on this very thread and is reverted once.
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
        }
    }
}

/// Flush denormal floats to zero (MXCSR FTZ | DAZ) on this thread. Denormals make the biquads, limiter,
/// gate and focus expander 10-20x slower while the mic is silent.
fn flush_denormals() {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: stmxcsr/ldmxcsr only read/write the calling thread's MXCSR through a valid stack slot;
    // bits 15 (FTZ) and 6 (DAZ) only change how denormal results and operands are treated.
    unsafe {
        let mut csr = 0u32;
        std::arch::asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack, preserves_flags));
        csr |= 0x8040;
        std::arch::asm!("ldmxcsr [{}]", in(reg) &csr, options(nostack, readonly, preserves_flags));
    }
}

fn open_device(flow: Flow, id: Option<&str>) -> Result<wasapi::Device> {
    let dir = match flow {
        Flow::Capture => Direction::Capture,
        Flow::Render => Direction::Render,
    };
    match id {
        None => wasapi::get_default_device(&dir).map_err(|e| anyhow!("default device: {e}")),
        Some(want) => {
            let collection =
                wasapi::DeviceCollection::new(&dir).map_err(|e| anyhow!("collection: {e}"))?;
            let n = collection
                .get_nbr_devices()
                .map_err(|e| anyhow!("count: {e}"))?;
            for i in 0..n {
                if let Ok(dev) = collection.get_device_at_index(i) {
                    if dev.get_id().map(|d| d == want).unwrap_or(false) {
                        return Ok(dev);
                    }
                }
            }
            bail!("audio device not found (id {want})")
        }
    }
}

/// Closes a kernel event on drop. `wasapi::Handle` (the stream event) never closes itself, so every
/// opened stream would leak one event for the life of the agent. The Handle stays in use for waiting;
/// this only owns the close, which must come after everything that waits on it has stopped.
struct EventCloser(HANDLE);

impl EventCloser {
    fn of(event: &wasapi::Handle) -> Self {
        // wasapi 0.15 keeps the HANDLE private; read it back. Both guards fail the build, not the audio, if
        // a wasapi upgrade changes `Handle` into something else than a bare HANDLE or makes it close itself.
        const _: () = assert!(std::mem::size_of::<wasapi::Handle>() == std::mem::size_of::<HANDLE>());
        const _: () = assert!(!std::mem::needs_drop::<wasapi::Handle>());
        // SAFETY: `wasapi::Handle` is a one-field struct holding exactly the HANDLE (same size, no Drop,
        // both asserted above), and `event` is a live reference, so copying its bytes yields that HANDLE.
        Self(unsafe { std::mem::transmute_copy::<wasapi::Handle, HANDLE>(event) })
    }
}

impl Drop for EventCloser {
    fn drop(&mut self) {
        // SAFETY: the event was created by wasapi's `set_get_eventhandle`, nothing else closes it, and the
        // owners drop this after the waiting threads were joined and the stream stopped.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Field order matters: the client is released first, the event is closed last.
struct OpenedStream {
    client: wasapi::AudioClient,
    event: wasapi::Handle,
    _event_closer: EventCloser,
    channels: usize,
    /// Default device period in ms.
    period_ms: f32,
}

fn open_stream(flow: Flow, id: Option<&str>) -> Result<OpenedStream> {
    let dir = match flow {
        Flow::Capture => Direction::Capture,
        Flow::Render => Direction::Render,
    };
    // try mono first for capture; render is always stereo
    let channel_attempts: &[usize] = match flow {
        Flow::Capture => &[1, 2],
        Flow::Render => &[2],
    };
    let mut last_err = anyhow!("no channel layout accepted");
    for &ch in channel_attempts {
        let device = open_device(flow, id)?;
        let mut client = device
            .get_iaudioclient()
            .map_err(|e| anyhow!("audio client: {e}"))?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, ch, None);
        let (def_period, _min_period) = client
            .get_periods()
            .map_err(|e| anyhow!("periods: {e}"))?;
        match client.initialize_client(&format, def_period, &dir, &ShareMode::Shared, true) {
            Ok(()) => {
                let event = client
                    .set_get_eventhandle()
                    .map_err(|e| anyhow!("event handle: {e}"))?;
                return Ok(OpenedStream {
                    client,
                    _event_closer: EventCloser::of(&event),
                    event,
                    channels: ch,
                    period_ms: def_period as f32 / 10_000.0, // 100 ns units
                });
            }
            Err(e) => last_err = anyhow!("init ({ch} ch): {e}"),
        }
    }
    Err(last_err)
}

/// Record a fatal stream error: `user` is the PT-BR text for the dashboard, `ctx` the English log context.
fn report_error(
    error: &Arc<Mutex<Option<String>>>,
    running: &Arc<AtomicBool>,
    user: &str,
    ctx: &str,
    detail: impl Display,
) {
    let detail = detail.to_string();
    if crate::recovery::is_audio_service_loss(&detail) {
        // Windows Audio stopped or restarted under us: the agent's retry reconnects once it is back.
        log::warn!("engine: {ctx}: {detail} (Windows Audio went away; waiting for it)");
    } else {
        log::error!("engine: {ctx}: {detail}");
    }
    *lock(error) = Some(crate::recovery::failure_text(user, &detail));
    running.store(false, Ordering::Relaxed);
}

/// Runs a thread body. A panic must not leave a "running" engine that outputs silence: with `error` it
/// stops the engine with a dashboard error, without (the optional monitor) it is only logged.
fn guarded(
    error: Option<&Arc<Mutex<Option<String>>>>,
    running: &Arc<AtomicBool>,
    what: &str,
    body: impl FnOnce(),
) {
    if catch_unwind(AssertUnwindSafe(body)).is_ok() {
        return;
    }
    match error {
        Some(e) => report_error(e, running, "Erro interno no processamento de áudio", &format!("{what} panicked"), "see log"),
        None => log::error!("monitor: {what} panicked"),
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .with_context(|| format!("spawn thread {name}"))
}

/// Start capture, DSP and render threads. Stream/device failures after this returns are reported
/// through `EngineHandle::error` / `is_running` (the threads stop themselves).
pub fn start(cfg: EngineConfig) -> Result<EngineHandle> {
    let running = Arc::new(AtomicBool::new(true));
    let meters = Arc::new(Meters::new());
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<EngineCmd>(64);

    let raw = HeapRb::<f32>::new(RING_CAPACITY);
    let (raw_prod, raw_cons) = raw.split();
    let cable = HeapRb::<f32>::new(RING_CAPACITY);
    let (cable_prod, cable_cons) = cable.split();

    // Threads are pushed into the handle as they spawn, so an early `?` drops the handle,
    // which stops and joins whatever already runs.
    let mut handle = EngineHandle {
        cmd_tx,
        running: running.clone(),
        meters: meters.clone(),
        error: error.clone(),
        threads: Vec::new(),
    };

    // Spawn order follows the wake-up chain (capture wakes dsp wakes render), last link first.
    // ---- cable render thread ----
    let mut bells: Vec<Thread> = Vec::new();
    {
        let (running, error, meters) = (running.clone(), error.clone(), meters.clone());
        let output_id = cfg.output_id.clone();
        handle.threads.push(spawn("clarim-render-cable", move || {
            let (run, err) = (running.clone(), error.clone());
            guarded(Some(&err), &run, "render thread", move || {
                render_thread(Some(output_id), cable_cons, running, Some(error), meters, true);
            });
        })?);
        bells.extend(handle.threads.last().map(|t| t.thread().clone()));
    }

    // ---- monitor render thread (optional, errors are non-fatal) ----
    let mut monitor_prod = None;
    if let Some(mon_id) = cfg.monitor_id.clone() {
        let (prod, cons) = HeapRb::<f32>::new(RING_CAPACITY).split();
        monitor_prod = Some(prod);
        let (running, meters) = (running.clone(), meters.clone());
        handle.threads.push(spawn("clarim-render-monitor", move || {
            let run = running.clone();
            let flag = meters.clone();
            guarded(None, &run, "render thread", move || {
                render_thread(Some(mon_id), cons, running, None, meters, false);
            });
            // The render loop returns while the engine still runs only when its stream failed (or panicked):
            // say so, the mic keeps working and the dashboard shows a note.
            if run.load(Ordering::Relaxed) {
                flag.monitor_failed.store(true, Ordering::Relaxed);
            }
        })?);
        bells.extend(handle.threads.last().map(|t| t.thread().clone()));
    }

    // ---- dsp thread ----
    let dsp_bell = {
        let (running, error, meters) = (running.clone(), error.clone(), meters.clone());
        let cfg2 = cfg.clone();
        handle.threads.push(spawn("clarim-dsp", move || {
            let (run, err) = (running.clone(), error.clone());
            guarded(Some(&err), &run, "dsp thread", move || {
                dsp_thread(cfg2, raw_cons, cable_prod, monitor_prod, cmd_rx, &running, &meters, &bells);
            });
        })?);
        handle.threads.last().map(|t| t.thread().clone())
    };

    // ---- capture thread ----
    {
        let (running, error, meters) = (running.clone(), error.clone(), meters.clone());
        let input_id = cfg.input_id.clone();
        handle.threads.push(spawn("clarim-capture", move || {
            let (run, err) = (running.clone(), error.clone());
            guarded(Some(&err), &run, "capture thread", move || {
                capture_thread(input_id, raw_prod, dsp_bell, running, error, meters);
            });
        })?);
    }

    Ok(handle)
}

/// Consecutive silent frames reported by the device; true once they add up to `SILENT_NOTE_FRAMES`.
fn track_silence(run: &mut usize, frames: usize, silent: bool) -> bool {
    *run = if silent { run.saturating_add(frames) } else { 0 };
    *run >= SILENT_NOTE_FRAMES
}

fn capture_thread(
    input_id: Option<String>,
    mut raw_prod: impl Producer<Item = f32>,
    dsp: Option<Thread>,
    running: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    meters: Arc<Meters>,
) {
    if let Err(e) = wasapi::initialize_mta().ok() {
        report_error(&error, &running, "Falha ao iniciar o COM", "COM init", e);
        return;
    }
    let _rt = RtGuard::enter();

    let stream = match open_stream(Flow::Capture, input_id.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            report_error(&error, &running, "Não foi possível abrir o microfone", "microphone open failed", e);
            return;
        }
    };
    let ch = stream.channels;
    Meters::set_f32(&meters.capture_period_ms, stream.period_ms);
    let capture_client = match stream.client.get_audiocaptureclient() {
        Ok(c) => c,
        Err(e) => {
            report_error(&error, &running, "Falha no cliente de captura", "capture client", e);
            return;
        }
    };
    if let Err(e) = stream.client.start_stream() {
        report_error(&error, &running, "Falha ao iniciar a captura do microfone", "capture start", e);
        return;
    }
    log::info!("capture stream started ({ch} ch @ 48 kHz f32, period {:.1} ms)", stream.period_ms);

    // Pre-sized with headroom; they only grow if the device delivers an unusually large packet.
    let mut byte_buf = vec![0u8; FRAME * 16 * ch * 4];
    let mut mono = vec![0f32; FRAME * 16];
    let mut silent_run = 0usize;
    let mut ring_full = false;

    while running.load(Ordering::Relaxed) {
        // A timeout is not "nothing to do": the drain below is also what notices a vanished device.
        let _ = stream.event.wait_for_event(CAPTURE_WAIT_MS);
        loop {
            let frames = match capture_client.get_next_nbr_frames() {
                Ok(Some(f)) if f > 0 => f as usize,
                Ok(_) => break,
                Err(e) => {
                    report_error(&error, &running, "Falha ao ler o microfone", "capture read", e);
                    return;
                }
            };
            let bytes = frames * ch * 4;
            if bytes > byte_buf.len() {
                byte_buf.resize(bytes, 0);
                mono.resize(frames, 0.0);
            }
            let flags = match capture_client.read_from_device(&mut byte_buf[..bytes]) {
                Ok((_, flags)) => flags,
                Err(e) => {
                    report_error(&error, &running, "Falha ao ler o microfone", "capture read", e);
                    return;
                }
            };
            if flags.data_discontinuity {
                meters.overruns.fetch_add(1, Ordering::Relaxed);
            }
            let silent = flags.silent;
            meters.mic_silent.store(track_silence(&mut silent_run, frames, silent), Ordering::Relaxed);
            if silent {
                // the buffer content is undefined for a silent packet
                mono[..frames].fill(0.0);
            } else {
                // bytes -> f32 frames, downmix to mono; a non-finite sample would poison every IIR state
                for (out, frame) in mono[..frames].iter_mut().zip(byte_buf[..bytes].chunks_exact(ch * 4)) {
                    let mut acc = 0.0f32;
                    for s in frame.chunks_exact(4) {
                        let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                        if v.is_finite() {
                            acc += v;
                        }
                    }
                    *out = acc / ch as f32;
                }
            }

            // a full ring means the DSP thread stalled: the newest samples are dropped
            if pacing::rising(&mut ring_full, raw_prod.vacant_len() < frames) {
                meters.overruns.fetch_add(1, Ordering::Relaxed);
            }
            let _ = raw_prod.push_slice(&mono[..frames]);
            if let Some(d) = &dsp {
                d.unpark();
            }
        }
    }
    let _ = stream.client.stop_stream();
}

#[allow(clippy::too_many_arguments)]
fn dsp_thread(
    cfg: EngineConfig,
    mut raw_cons: impl Consumer<Item = f32>,
    mut cable_prod: impl Producer<Item = f32>,
    mut monitor_prod: Option<impl Producer<Item = f32>>,
    cmd_rx: Receiver<EngineCmd>,
    running: &AtomicBool,
    meters: &Meters,
    bells: &[Thread],
) {
    flush_denormals();
    // In strong mode this builds the DeepFilterNet3 model (~0.5 s). The capture ring overflows
    // meanwhile; the backlog rule below drops what is stale afterwards. Real-time scheduling starts only
    // after the build, so it does not hog a core at that priority.
    let mut chain = MicChain::new(&cfg.params);
    let _rt = RtGuard::enter();
    let mut active = chain.denoise_active();
    meters.set_denoise_active(active);
    let mut calibrator = Calibrator::new();
    let calib = &cfg.calib;
    // Calibration is stored relative to unity input gain, so it survives later gain changes.
    let mut input_gain_db = cfg.params.input_gain_db;

    let mut frame = [0f32; FRAME];
    let (mut in_hold, mut out_hold) = (0.0f32, 0.0f32);
    let (mut dsp_us_avg, mut pipeline_avg) = (0.0f32, 0.0f32);
    let mut cable_full = false;

    while running.load(Ordering::Relaxed) {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                EngineCmd::SetParams(p) => {
                    input_gain_db = p.input_gain_db;
                    chain.set_params(&p); // first switch to strong stalls this thread ~0.5 s
                }
            }
        }
        if let Some(frames) = calib.take_request() {
            calibrator.start(frames);
        }

        // Sleep until the capture thread has a packet for us.
        if raw_cons.occupied_len() < FRAME {
            std::thread::park_timeout(DSP_WAIT);
            continue;
        }
        // A backlog of more than 3 frames (device stall, DSP stall): drop it once, keep the newest frame.
        let skip = pacing::capture_skip(raw_cons.occupied_len(), RING_CAPACITY);
        if skip > 0 {
            raw_cons.skip(skip);
            meters.overruns.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let got = raw_cons.pop_slice(&mut frame);
        if got < FRAME {
            continue;
        }

        let in_peak = frame.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        in_hold = in_peak.max(in_hold * METER_DECAY);

        let t0 = Instant::now();
        let info = chain.process(&mut frame);
        let dsp_us = t0.elapsed().as_micros() as f32;

        let out_peak = frame.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        out_hold = out_peak.max(out_hold * METER_DECAY);

        if chain.denoise_active() != active {
            active = chain.denoise_active();
            meters.set_denoise_active(active);
        }
        if calibrator.active() {
            let rms_db = 10.0 * info.post_denoise_ms.max(1e-12).log10() - input_gain_db;
            match calibrator.feed(rms_db, info.vad) {
                Some(result) => {
                    log::info!("calibration finished: {result:?} dBFS");
                    calib.finish(result);
                }
                None => calib.set_progress(calibrator.progress()),
            }
        }

        if pacing::rising(&mut cable_full, cable_prod.vacant_len() < FRAME) {
            meters.overruns.fetch_add(1, Ordering::Relaxed);
        }
        let _ = cable_prod.push_slice(&frame);
        if let Some(m) = monitor_prod.as_mut() {
            let _ = m.push_slice(&frame);
        }
        for b in bells {
            b.unpark();
        }

        // Our own pipeline delay up to the render queue: half a capture period (a packet is delivered
        // at its end), what still waits in the capture ring, the denoiser's delay and the DSP time.
        let pipeline = Meters::get_f32(&meters.capture_period_ms) * 0.5
            + raw_cons.occupied_len() as f32 / 48.0
            + chain.latency_ms()
            + dsp_us * 0.001;
        dsp_us_avg += (dsp_us - dsp_us_avg) * READOUT_SMOOTHING;
        pipeline_avg += (pipeline - pipeline_avg) * READOUT_SMOOTHING;

        Meters::set_f32(&meters.in_peak, in_hold);
        Meters::set_f32(&meters.out_peak, out_hold);
        Meters::set_f32(&meters.vad, info.vad);
        meters.focus_open.store(info.focus_open, Ordering::Relaxed);
        meters.dsp_us.store(dsp_us_avg as u32, Ordering::Relaxed);
        Meters::set_f32(&meters.pipeline_ms, pipeline_avg);
    }
    calib.cancel();
}

/// Wakes a render thread at every audio-engine tick: waits on the stream's event handle and unparks the
/// render thread, which then parks on one condition shared with the DSP thread's "block ready" bell. It
/// also counts the ticks, so the render thread knows how many periods the engine asked for between two wakes.
struct TickWatcher {
    stop: Arc<AtomicBool>,
    ticks: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl TickWatcher {
    fn spawn(event: wasapi::Handle, render: Thread, running: Arc<AtomicBool>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let ticks = Arc::new(AtomicU32::new(0));
        let (flag, count) = (stop.clone(), ticks.clone());
        let thread = spawn("clarim-render-tick", move || {
            let _rt = RtGuard::enter();
            while running.load(Ordering::Relaxed) && !flag.load(Ordering::Relaxed) {
                if event.wait_for_event(100).is_ok() {
                    count.fetch_add(1, Ordering::Relaxed); // before the unpark: the woken thread must see it
                    render.unpark();
                }
            }
        })?;
        Ok(Self { stop, ticks, thread: Some(thread) })
    }

    /// Engine ticks since the last call. Call it BEFORE reading the endpoint's padding: a tick that lands
    /// in between is then missed by the count (a late detection at worst), never counted without its
    /// consumption being visible in the padding (which would be a false underrun).
    fn take_ticks(&self) -> u32 {
        self.ticks.swap(0, Ordering::Relaxed)
    }
}

impl Drop for TickWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn render_thread(
    device_id: Option<String>,
    mut cons: impl Consumer<Item = f32>,
    running: Arc<AtomicBool>,
    fatal_error: Option<Arc<Mutex<Option<String>>>>,
    meters: Arc<Meters>,
    is_cable: bool,
) {
    let fail = |user: &str, ctx: &str, detail: &dyn Display| {
        if let Some(err) = &fatal_error {
            report_error(err, &running, user, ctx, detail);
        } else {
            log::warn!("monitor: {ctx}: {detail}");
        }
    };

    if let Err(e) = wasapi::initialize_mta().ok() {
        fail("Falha ao iniciar o COM", "COM init", &e);
        return;
    }
    let _rt = RtGuard::enter();

    // Kept whole (not destructured): its fields drop in order at the end, client first, event closed last,
    // after `watcher` below has joined the thread that waits on it.
    let stream = match open_stream(Flow::Render, device_id.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            fail("Não foi possível abrir o microfone virtual", "output open failed", &e);
            return;
        }
    };
    let (ch, period_ms, client) = (stream.channels, stream.period_ms, &stream.client);
    let render_client = match client.get_audiorenderclient() {
        Ok(c) => c,
        Err(e) => {
            fail("Falha no cliente de saída", "render client", &e);
            return;
        }
    };
    let buffer_frames = match client.get_bufferframecount() {
        Ok(b) => b as usize,
        Err(e) => {
            fail("Falha no buffer de saída", "buffer size", &e);
            return;
        }
    };
    // Frames the audio engine takes per tick.
    let period_frames = ((period_ms * 48.0).round() as usize).clamp(FRAME / 4, buffer_frames.max(FRAME / 4));
    let mut pacer = Pacer::new(period_frames);

    // Pre-sized (`bytes` stays zero here, i.e. silence); both only ever need `buffer_frames` + a slip.
    let blockalign = ch * 4;
    let mut mono = vec![0f32; buffer_frames + 4];
    let mut bytes = vec![0u8; mono.len() * blockalign];
    // Start with the pacer's target of silence, not the whole buffer: that is the queue it steers around.
    let prefill = pacer.target_frames().min(buffer_frames);
    let _ = render_client.write_to_device(prefill, &bytes[..prefill * blockalign], None);
    if let Err(e) = client.start_stream() {
        fail("Falha ao iniciar a saída", "render start", &e);
        return;
    }
    let watcher = match TickWatcher::spawn(stream.event, std::thread::current(), running.clone()) {
        Ok(t) => t,
        Err(e) => {
            fail("Falha ao iniciar a saída", "render tick thread", &e);
            return;
        }
    };
    log::info!(
        "render stream started ({ch} ch @ 48 kHz f32, buffer {buffer_frames} frames, period {period_frames} frames, cable={is_cable})"
    );

    let mut last_t = Instant::now();
    // Frames queued (endpoint + ring) and in the endpoint alone, as left by the previous wake.
    let mut last_level = prefill as f32;
    let mut last_total = prefill;
    let mut last_sample = 0.0f32;
    let mut fade = true; // the first audio, and the first after a gap, fades in
    let mut primed = false; // some audio has been written
    let mut dry_since: Option<Instant> = None;

    while running.load(Ordering::Relaxed) {
        // Wakes: a block is ready (DSP thread), an engine tick (tick thread), or the timeout. All three
        // run the same code, so a vanished device is noticed by the padding query below within 200 ms.
        std::thread::park_timeout(RENDER_WAIT);
        if !running.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        let ticks = watcher.take_ticks(); // before the padding query, see `take_ticks`
        let padding = match client.get_current_padding() {
            Ok(p) => (p as usize).min(buffer_frames),
            Err(e) => {
                fail("Falha na saída de áudio", "render padding", &e);
                return;
            }
        };
        pacer.hold(last_level, now.duration_since(last_t).as_secs_f32() * 1000.0);
        last_t = now;

        // The engine's ticks since the last wake wanted more than we had queued: count it, restart cleanly.
        if primed && dry_since.is_none() && pacing::ran_dry(padding, last_total, period_frames, ticks) {
            dry_since = Some(now);
            fade = true;
            if is_cable {
                meters.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }

        // More than a block or two waiting means this thread (or the device) stalled, or the stream just
        // started after a slow open: playing it would keep the delay for good.
        let mut ring = cons.occupied_len();
        let stale = pacing::excess(ring, if primed { STALE_RING } else { FRAME }, FRAME);
        if stale > 0 {
            cons.skip(stale);
            ring -= stale;
            fade = true;
            if is_cable {
                meters.overruns.fetch_add(1, Ordering::Relaxed);
            }
        }

        let free = buffer_frames - padding;
        let want = ring.min(free);
        let mut written = 0;
        if want > 0 {
            let mut len = cons.pop_slice(&mut mono[..want]);
            let was_dry = dry_since.take();
            if was_dry.is_some_and(|t0| now.duration_since(t0) < STALL) {
                pacer.bump();
            }
            // Empty endpoint after a gap (or at the very start): rebuild the queue with silence, which
            // the engine was playing anyway.
            let reprime = padding == 0 && (!primed || was_dry.is_some());
            if fade {
                pacing::fade_in(&mut mono, len);
                fade = false;
            }
            if reprime {
                let lead = pacer.target_frames().saturating_sub(len).min(free - len);
                mono.copy_within(0..len, lead);
                mono[..lead].fill(0.0);
                len += lead;
                pacer.reset();
            } else {
                let room = if free > len { len + 1 } else { len }; // a repeated frame needs one spare slot
                let new_len = pacing::apply_slip(pacer.slip(), last_sample, &mut mono[..room], len);
                if new_len != len {
                    len = new_len;
                    if is_cable {
                        meters.slips.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            for (&v, out) in mono[..len].iter().zip(bytes[..len * blockalign].chunks_exact_mut(blockalign)) {
                let b = v.to_le_bytes();
                for c in out.chunks_exact_mut(4) {
                    c.copy_from_slice(&b);
                }
            }
            if let Err(e) = render_client.write_to_device(len, &bytes[..len * blockalign], None) {
                fail("Falha ao escrever na saída de áudio", "render write", &e);
                return;
            }
            last_sample = mono[len - 1];
            written = len;
            primed = true;
        }
        last_total = padding + written;
        last_level = (last_total + ring - want) as f32;
        if is_cable {
            Meters::set_f32(&meters.render_queue_ms, pacer.avg_frames() / 48.0);
        }
    }
    let _ = client.stop_stream();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::Noise;
    use crate::selftest::Voice;
    use clarim_core::EqPreset;
    use ringbuf::traits::Observer;

    /// All processing off, 0 dB gains: the DSP thread must pass audio through untouched.
    fn passthrough() -> DspParams {
        DspParams {
            denoise: DenoiseMode::Off,
            denoise_strength: 1.0,
            voice_focus: false,
            focus_threshold_db: -40.0,
            gate_on: false,
            gate_threshold_db: -50.0,
            hpf_on: false,
            auto_level: false,
            eq: EqPreset::Flat,
            tone_db: [0.0; 3],
            input_gain_db: 0.0,
            output_gain_db: 0.0,
        }
    }

    /// The DSP thread wired to in-memory rings: no audio devices involved.
    struct Rig {
        raw_prod: ringbuf::HeapProd<f32>,
        cable_cons: ringbuf::HeapCons<f32>,
        cmd_tx: Sender<EngineCmd>,
        running: Arc<AtomicBool>,
        meters: Arc<Meters>,
        calib: Arc<CalibShared>,
        thread: Option<JoinHandle<()>>,
    }

    impl Rig {
        fn new() -> Self {
            Self::with(passthrough())
        }

        fn with(params: DspParams) -> Self {
            let (raw_prod, raw_cons) = HeapRb::<f32>::new(RING_CAPACITY).split();
            let (cable_prod, cable_cons) = HeapRb::<f32>::new(RING_CAPACITY).split();
            let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(64);
            let running = Arc::new(AtomicBool::new(true));
            let meters = Arc::new(Meters::new());
            let calib = Arc::new(CalibShared::new());
            let cfg = EngineConfig {
                input_id: None,
                output_id: String::new(),
                monitor_id: None,
                params,
                calib: calib.clone(),
            };
            let thread = {
                let (running, meters) = (running.clone(), meters.clone());
                std::thread::spawn(move || {
                    dsp_thread(cfg, raw_cons, cable_prod, None::<ringbuf::HeapProd<f32>>, cmd_rx, &running, &meters, &[]);
                })
            };
            Self { raw_prod, cable_cons, cmd_tx, running, meters, calib, thread: Some(thread) }
        }

        /// What the capture thread does after a push.
        fn wake(&self) {
            if let Some(t) = &self.thread {
                t.thread().unpark();
            }
        }

        /// Push one frame in, wait for the processed frame to come out.
        fn roundtrip(&mut self, frame: &[f32; FRAME]) -> [f32; FRAME] {
            assert_eq!(self.raw_prod.push_slice(frame), FRAME);
            self.wake();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.cable_cons.occupied_len() < FRAME {
                assert!(Instant::now() < deadline, "DSP thread did not answer");
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut out = [0f32; FRAME];
            assert_eq!(self.cable_cons.pop_slice(&mut out), FRAME);
            out
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.running.store(false, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                t.thread().unpark();
                let _ = t.join();
            }
        }
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn dsp_thread_passes_audio_and_applies_live_params() {
        let mut rig = Rig::new();
        let mut noise = Noise::new(31);
        let mut frame = || -> [f32; FRAME] { std::array::from_fn(|_| noise.sample() * 0.3) };

        for _ in 0..5 {
            let input = frame();
            assert_eq!(rig.roundtrip(&input), input, "everything off must be bit-exact");
        }
        assert_eq!(rig.meters.denoise_active(), Some(DenoiseMode::Off));
        assert!(rig.meters.in_peak() > 0.1 && rig.meters.out_peak() > 0.1);

        // +6 dB output gain arrives live, without a restart
        let mut louder = passthrough();
        louder.output_gain_db = 6.0;
        assert!(rig.cmd_tx.try_send(EngineCmd::SetParams(louder)).is_ok());
        let (mut ratio, mut input, mut out) = (0.0, [0f32; FRAME], [0f32; FRAME]);
        for _ in 0..30 {
            input = frame();
            out = rig.roundtrip(&input);
            ratio = rms(&out) / rms(&input);
        }
        assert!((ratio - 1.995).abs() < 0.05, "expected about +6 dB, got ratio {ratio} ({} vs {})", rms(&out), rms(&input));
    }

    #[test]
    fn dsp_thread_reports_calibration_result_and_failure() {
        let mut rig = Rig::new();
        let (mut voice, mut noise) = (Voice::new(), Noise::new(32));
        let mut speech = || -> [f32; FRAME] { std::array::from_fn(|_| voice.sample() + noise.sample() * 0.01) };

        rig.calib.request(1.0);
        assert!(rig.calib.calibrating());
        for _ in 0..130 {
            rig.roundtrip(&speech());
        }
        assert!(!rig.calib.calibrating(), "1 s calibration must be over after 1.3 s of audio");
        let db = rig.calib.take_result().expect("speech-like input must calibrate");
        assert!((-30.0..-15.0).contains(&db), "voice level {db} dBFS");
        assert!(rig.calib.take_result().is_none(), "result is handed out once");

        rig.calib.request(1.0);
        for _ in 0..130 {
            let quiet = std::array::from_fn(|_| noise.sample() * 0.001);
            rig.roundtrip(&quiet);
        }
        assert!(rig.calib.take_failed(), "silence must not calibrate");
        assert!(rig.calib.take_result().is_none());
    }

    #[test]
    fn calibration_is_referenced_to_unity_input_gain() {
        let level_at = |gain_db: f32| -> f32 {
            let mut rig = Rig::with(DspParams { input_gain_db: gain_db, ..passthrough() });
            let (mut voice, mut noise) = (Voice::new(), Noise::new(32));
            rig.calib.request(1.0);
            for _ in 0..130 {
                let speech = std::array::from_fn(|_| voice.sample() + noise.sample() * 0.01);
                rig.roundtrip(&speech);
            }
            rig.calib.take_result().expect("speech-like input must calibrate")
        };
        let (flat, boosted) = (level_at(0.0), level_at(6.0));
        assert!((flat - boosted).abs() < 1.0, "same voice, +6 dB gain: {flat} vs {boosted} dBFS");
    }

    #[test]
    fn dsp_thread_drops_a_stale_capture_backlog_once_and_keeps_the_newest_frame() {
        let mut rig = Rig::new();
        let mut noise = Noise::new(33);
        let backlog: Vec<f32> = (0..FRAME * 10).map(|_| noise.sample() * 0.3).collect();
        // all at once, as after a stall
        assert_eq!(rig.raw_prod.push_slice(&backlog), backlog.len());
        rig.wake();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rig.cable_cons.occupied_len() < FRAME {
            assert!(Instant::now() < deadline, "DSP thread did not answer");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(rig.cable_cons.occupied_len(), FRAME, "only the newest frame survives");
        let mut out = [0f32; FRAME];
        rig.cable_cons.pop_slice(&mut out);
        assert_eq!(out[..], backlog[FRAME * 9..]);
        assert_eq!(rig.meters.counters().overruns, 1);
    }

    #[test]
    fn silence_note_needs_two_seconds_of_silent_packets() {
        let mut run = 0;
        for _ in 0..199 {
            assert!(!track_silence(&mut run, FRAME, true));
        }
        assert!(track_silence(&mut run, FRAME, true), "200 x 10 ms = 2 s");
        assert!(!track_silence(&mut run, FRAME, false), "one real packet clears it");
        assert!(!track_silence(&mut run, FRAME, true));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn denormals_are_flushed_on_the_dsp_thread_only() {
        let tiny = || std::hint::black_box(1e-40f32) * std::hint::black_box(1.0f32);
        assert!(tiny() > 0.0, "plain threads keep denormals");
        let flushed = std::thread::spawn(move || {
            flush_denormals();
            tiny()
        })
        .join()
        .expect("thread");
        assert_eq!(flushed, 0.0);
        assert!(tiny() > 0.0, "MXCSR is per thread");
    }

    /// Manual hardware check (read-only): opens the real microphone for one second and counts frames.
    /// Run with `cargo test -p clarim-audio -- --ignored capture_opens_real_mic --nocapture`.
    #[test]
    #[ignore = "opens the real microphone for 1 s (read-only, nothing is played or stored)"]
    fn capture_opens_real_mic() {
        crate::init_com().expect("COM");
        let inputs = crate::list_inputs(&[]).expect("list inputs");
        let mic = crate::pick_real_mic(&inputs).expect("no real microphone");
        let stream = open_stream(Flow::Capture, Some(&mic.id)).expect("open capture stream");
        let client = stream.client.get_audiocaptureclient().expect("capture client");
        stream.client.start_stream().expect("start");
        let (mut frames, mut peak) = (0usize, 0.0f32);
        let mut bytes = vec![0u8; 48_000 * stream.channels * 4];
        let end = Instant::now() + Duration::from_secs(1);
        while Instant::now() < end {
            let _ = stream.event.wait_for_event(200);
            while let Ok(Some(n)) = client.get_next_nbr_frames() {
                if n == 0 {
                    break;
                }
                let len = n as usize * stream.channels * 4;
                client.read_from_device(&mut bytes[..len]).expect("read");
                frames += n as usize;
                peak = bytes[..len].chunks_exact(4).fold(peak, |p, b| p.max(f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs()));
            }
        }
        let _ = stream.client.stop_stream();
        println!("'{}': {} ch, period {:.1} ms, {frames} frames in 1 s, peak {peak:.4}", mic.name, stream.channels, stream.period_ms);
        assert!((40_000..56_000).contains(&frames), "expected about 48000 frames, got {frames}");
    }
}
