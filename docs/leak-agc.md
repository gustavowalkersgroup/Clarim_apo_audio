# Leak warnings, voice AGC and mic clip guard: design (v2)

Status: design contract for the implementation, 2026-11-23. v2 adds the fixes from two adversarial design reviews (DSP and systems).

User request (PT-BR), in English:

- During **calls only**, if sound still leaks into the virtual mic even with suppression, warn the user **discreetly**.
- Keep the voice sent to calls from **ever clipping or sitting too low**.

## 0. Definitions and known limits

- **Call running**: at least one process (not on the ignore list) has an *Active* capture session on the virtual mic's capture endpoint. That endpoint is "CABLE Output (VB-Audio Virtual Cable)", the capture partner of the render device Clarim writes into. A hangover of 8 s rides over brief Inactive flips.
- **Echo leak**: what the speakers play (any app, read via WASAPI loopback of the physical output devices) gets into the real mic and survives into Clarim's processed output.
- **Noise leak**: background noise that is not echo and still passes the suppressor (output level during non-speech frames).
- **Output**: the final post-chain frame, exactly what is pushed into the cable (`pipeline.rs` dsp loop, after `chain.process`).
- **Speech level metric** (used everywhere below): the **dB mean of 10 ms frame RMS over learned speech frames**. For the synthetic `selftest::Voice`, this reads about 3.3 dB below its power RMS.

Known limits (also documented in the README):

- **The call app may already cancel its own playback.** Discord (echo cancellation on by default) and Meet/Chrome cancel the echo of their own playback downstream on CABLE Output. A device loopback cannot tell that audio apart, so the echo warning matters most for *other* apps' audio: game voice lines, YouTube, podcasts. DFN3 Strong already removes most music and effects, so with music only, no warning is the correct result.
- **Very loud echo is not scored.** Echo within 10 dB of the user's own voice level is excluded from scoring (it is indistinguishable from the user talking). A leak that loud is obvious anyway.
- **Balloons are skipped while the user is busy.** Windows reports busy, full-screen or presentation mode, and the balloon waits until they alt-tab. The dashboard banner is always there.

## 1. Architecture

```
routing worker (1-2 s scan) --captures()--> backend housekeeping (1 s tick)
                                              |  CallTracker: in_call, apps
                                              |  refs wanted = in_call && leak_warnings && mic running
                                              |-> MicEngine::set_references(ids)  -> Loopbacks (1 thread / render device, non-blocking mgmt)
                                              |                                        | 10 ms block powers -> RefSlots (lock-free)
                                              |                                        v
mic capture -> DSP thread: MicChain (AGC inside) -> output frame -> EchoDetector x4 + NoiseFloor + clip counter
                                              |                                        | atomics (Meters)
                                              |<-- MicEngine::status() (MicStatus) ----+
                                              |  LeakMonitor: alert + balloon (rate limited)
                                              |  MicVolumeGuard: lower/raise real mic endpoint volume (persisted, bounded)
                                              |-> tray::notify (no-sound balloon, skipped when the user is busy/full screen)
                                              '-> overlay fields into MicStatus -> dashboard (10 Hz meters)
```

Rules that every part keeps:

- **DSP thread is real-time.** It does no allocation after setup, takes no lock, makes no syscall and does no logging. New state is preallocated fixed arrays. Data crosses threads only through atomics, the existing SPSC rings and the existing `Copy` command channel.
- **Lifetimes are independent.** Loopback streams live outside the mic pipeline: a mic restart (`MicEngine::shutdown` in `apply`) does **not** touch them. They are managed **without ever blocking on a thread join** while the `mic` mutex is held.
- **Decision policy lives outside the DSP thread.** The policy (thresholds over time, rate limits, Windows volume changes, balloons) is pure Rust in `apps/clarim/src/callwatch.rs`, unit-tested with a fake clock (`Instant` passed in).
- **Untrusted data at the trust boundary.** Loopback samples are external input: scrub non-finite values exactly like the capture thread does.

## 2. clarim-core (`crates/clarim-core/src/lib.rs`)

### 2.1 New `MicSettings` fields

Add each field to the struct and to the `Default` literal. The struct already has `#[serde(default)]`.

- `leak_warnings: bool`, default `true`. Doc: "During calls, warn (tray balloon without sound + dashboard) when sound still leaks into the virtual mic."
- `auto_mic_volume: bool`, default `true`. Doc: "During calls, with `auto_level` on, lower the real mic's Windows volume in 3 dB steps when the input clips on speech (at most 12 dB in total, only on mics with a hardware volume control). Give back what Clarim took when the voice stays too quiet, and on quit."
- `mic_volume_taken: Option<MicVolumeTaken>`, default `None`. Clarim's bookkeeping, never edited by the dashboard. `check_mic` keeps the stored value, exactly like `voice_level_db`.

  ```rust
  #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
  pub struct MicVolumeTaken { pub device_id: String, pub baseline_db: f32, pub taken_db: f32 }
  ```

Change the `auto_level` doc to: "Voice AGC: keeps the speech level near -23 dBFS (dB mean of 10 ms frame RMS; gain -12..+20 dB), learns only from the user's speech, does not raise the noise between words above -55 dBFS; the -1 dBFS limiter stays after it."

### 2.2 New types

```rust
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum LeakKind { Echo, Noise }

/// One app capture session on a capture endpoint (from the routing worker).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct CaptureSession { pub pid: u32, pub exe: String, pub device_id: String, pub active: bool }
```

### 2.3 New `MicStatus` fields

Every new field gets `#[serde(default)]`.

Engine fields, filled by `MicEngine::status()`:

- `agc_gain_db: f32`: current AGC gain in dB (0 when off).
- `input_voice_db: Option<f32>`: AGC input speech level, dBFS. `None` until confident.
- `voice_db: Option<f32>`: speech level at the cable (`input_voice_db + agc gain + output gain`), dBFS. `None` until confident.
- `echo_score: f32`: 0..1, the max over running references with an active reference (0 otherwise).
- `echo_db: Option<f32>`: leak level for the argmax detector, dBFS.
- `echo_lag_ms: f32`: best lag of the argmax detector.
- `noise_db: Option<f32>`: residual noise at the output during non-speech, dBFS.
- `clip_events: u32`: monotonic count of clipped raw input speech frames since the engine started. It resets on restart; consumers use `saturating_sub` and treat a decrease as a reset.
- `references: u32`: number of loopback reference streams running.

Backend overlay fields (the engine leaves them at their defaults; the backend fills them):

- `in_call: bool`
- `call_apps: Vec<String>`: exe names, deduped and sorted, without unknown.
- `leak: Option<LeakKind>`: the current alert, only while `in_call`.
- `mic_volume_note: Option<String>`: PT-BR note about the last Windows mic volume action. Shown for 60 s; the "saturating before the Windows volume" note stays for the run.
- `mic_too_quiet: bool`

Tests:

- `new_fields_default_when_missing_from_old_json` covers the new fields.
- A partial JSON keeps `leak_warnings` and `auto_mic_volume` true and `mic_volume_taken` None.

## 3. clarim-routing

- **Enumerate capture sessions once per scan, always.** The worker calls `sessions::enumerate_capture` on every scan, no longer gated by `call_to_chat`. On error, log at debug level and use an empty list.
- **Promotion keeps its gating.** Change `note_captures` to take the list: `note_captures(list: &[CaptureInfo], captures, now) -> HashSet<u32>`, so the endpoints are not enumerated twice. Call it only when `call_to_chat`, exactly as today. `list_sessions_once` passes its own list.
- **Publish the captures.** `Shared` gets `captures: Vec<CaptureSession>`, published every scan, converted from all `CaptureInfo`.
- **Stale data on failure.** If render enumeration fails and the scan returns early, publish `captures = vec![]`, so a call cannot stay alive while Windows Audio is down.
- **pid→exe cache.** Pruning keeps capture pids every scan.
- **Accessor.** New `pub fn captures(&self) -> Vec<CaptureSession>` on `RoutingEngine`. **It must not call `self.poll.touch()`.**
- **Tests:**
  - Build `RoutingEngine { tx, shared, poll, worker: None }` in the test module, call `captures()`, and assert that the poll's last-polled stamp is unchanged.
  - Existing tests still pass.
- **Scan period.** The scan runs every 2 s when routing is off or observe-only, so the call state can lag up to 3 s.

## 4. clarim-audio: DSP (pure, unit-tested)

### 4.1 `dsp/agc.rs`: `Agc`

```rust
pub struct Agc { .. }
impl Agc {
    pub fn new() -> Self;
    /// One FRAME in place. `vad`: RNNoise speech probability of this frame (pre-denoise).
    /// `noise_ms`: mean square after the denoiser, before Voice Focus/gate (FrameInfo.post_denoise_ms).
    /// `out_gain_db`: the output gain that follows the AGC (for the limiter guard). `enabled`: auto_level.
    pub fn process(&mut self, buf: &mut [f32], vad: f32, noise_ms: f32, out_gain_db: f32, enabled: bool);
    pub fn gain_db(&self) -> f32;
    /// Speech level at the AGC input (dBFS), None until confident.
    pub fn input_voice_db(&self) -> Option<f32>;
    /// Shift the learned levels by a known input change (input gain or Windows mic volume), in dB.
    pub fn shift_db(&mut self, db: f32);
    pub fn reset(&mut self);
}
```

Public constants: `AGC_TARGET_DB = -23.0`, `AGC_MAX_GAIN_DB = 20.0`, `AGC_MIN_GAIN_DB = -12.0`.

Private constants:

| Name | Value | Meaning |
|---|---|---|
| `RISE_DB_PER_FRAME` | 0.03 | 3 dB/s |
| `FALL_DB_PER_FRAME` | 0.10 | 10 dB/s |
| `FAST_FALL_DB_PER_FRAME` | 0.30 | limiter guard and disable ramp |
| `SPEECH_VAD` | `crate::calib::SPEECH_VAD` (0.6) | |
| `SPEECH_RUN` | 12 | adjacent speech frames |
| `LEVEL_LEAK` | 0.9975 | per learned frame |
| `CONFIDENT_FRAMES` | 40 | |
| `NOISE_VAD` | 0.2 | |
| `MAX_OUT_NOISE_DB` | -55.0 | |
| `LEARN_FLOOR_DB` | -60.0 | |
| `GUARD_DB` | -3.0 | |
| `OUTLIER_DB` | 20.0 | |

Per frame:

1. **Frame level.** `rms_db` = 10·log10(mean square) of the input frame, floored at -90. `peak` = max |s|.
2. **Speech run.** If `vad >= SPEECH_VAD`, then `run += 1`; else `run = 0`.
3. **Learn speech level** when all of these hold:
   - `run >= SPEECH_RUN`
   - `rms_db > LEARN_FLOOR_DB`
   - `learned < CONFIDENT_FRAMES || rms_db > level - OUTLIER_DB`

   Frames 20 dB or more below a confident estimate are leaked or distant voices, or focus-muted frames, not the user. Louder frames are always learned, so a rising voice is followed. The estimator is a weighted dB average: `num = num*LEVEL_LEAK + rms_db; den = den*LEVEL_LEAK + 1; level = num/den; learned += 1`.
4. **Noise-in estimate.** On frames with `vad < NOISE_VAD`, take `n_db` = 10·log10(noise_ms) floored at -90 (before Voice Focus and the gate). Initialise to the first such frame. Track the minimum: alpha 0.1 when lower, 0.01 when higher.
5. **Target.** Once `learned >= CONFIDENT_FRAMES`:
   - `target = clamp(AGC_TARGET_DB - level, MIN, MAX)`, then `target = min(target, max(MAX_OUT_NOISE_DB - noise_in, 0))`.
   - Raise by at most RISE per frame, only when `run >= SPEECH_RUN`.
   - Lower by at most FALL per frame at any time.
6. **Limiter guard**, only when enabled (also before confidence). If `peak * db_to_lin(gain + out_gain_db) > db_to_lin(GUARD_DB)`, then `gain = max(gain - FAST_FALL, MIN)`.
7. **When `!enabled`.** Ramp the gain toward 0 by FAST_FALL per frame. Once it is exactly 0, the block bypasses (bit-exact, no multiply).
8. **Apply.** Ramp the gain linearly across the frame from the previous frame's gain to the new one, sample by sample.

Learning (steps 3 and 4) runs even when `!enabled`.

`shift_db(db)`: `num += db*den; noise_in += db` (no-op before any learning).

Unit tests. Measure the output **in the test itself** with the speech level metric. Feed `vad = 1.0` for speech frames unless noted.

1. Voice scaled to a speech level of -37 dBFS, 1500 frames: the output speech level over the last 200 frames is on target ± 1, and the gain never exceeds 20.
2. Voice at a speech level of -12 dBFS, 500 frames: the output is on target ± 1 over the last 200 frames, and the gain is at least -12.
3. Noise only (vad 0) for 3000 frames leaves the gain at 0.
4. Noise cap: a post-denoise noise of -60 dBFS (vad 0 frames) interleaved with quiet speech limits the gain to 5 ± 0.5 dB.
5. Once confident, speech frames 25 dB below the estimate move neither the estimate nor the gain.
6. Disabled from the start is bit-exact, including on loud input.
7. Toggling off ramps +20 dB to 0 within 70 frames, with no per-sample gain step larger than the per-frame ramp divided by FRAME.
8. Output stays finite and `|s| <= 1` with the limiter after it, with a +20 dB output gain.
9. `shift_db(-3)` after confidence raises the gain target by 3 dB at once (the estimate moves; the gain then follows at the rise rate).

### 4.2 `dsp/leak.rs`: `EchoDetector` and `NoiseFloor`

```rust
pub const ECHO_LAGS: usize = 80; // 0..790 ms in 10 ms steps
pub struct EchoDetector { .. }   // fixed arrays only
impl EchoDetector {
    pub fn new() -> Self;
    pub fn reset(&mut self);
    /// One 10 ms step. `ref_ms`: mean square of the reference block for this frame. `out_ms`: mean square of
    /// Clarim's output frame. `voice_db`: the user's speech level at the output (FrameInfo.voice_db).
    /// A frame without a reference block is simply not pushed.
    pub fn push(&mut self, ref_ms: f32, out_ms: f32, voice_db: Option<f32>);
    /// The chain's denoiser delay (`MicChain::latency_ms`); sets the first lag searched (step 7). The DSP
    /// thread calls it before every push (`reset` forgets it; an unset detector uses the Strong value).
    pub fn set_denoise_delay_ms(&mut self, ms: f32);
    /// 0..1, see step 7.
    pub fn score(&self) -> f32;
    /// Leak level (dBFS) while the reference at the best lag is active; None until known.
    pub fn level_db(&self) -> Option<f32>;
    pub fn best_lag_ms(&self) -> f32;
    /// Frames since the last pushed reference block above -60 dB (saturating).
    pub fn ref_inactive_frames(&self) -> u32;
}
```

`EchoDetector` is the WebRTC ResidualEchoDetector, adapted to the **dB domain** and a short lag window:

1. **Levels.** `r = 10·log10(max(ref_ms,1e-10))` clamped to [-90, 0]. `c` likewise from `out_ms`, but clamped to **[-85, 0]**. Active reference = `r > -60`.
2. **Learn gate.** Learn this frame only when both hold:
   - at least one of the 80 history entries, or `r` itself, is active;
   - `c <= voice_db.unwrap_or(-30.0) - 10.0`. A louder frame is the user's own speech.

   A silent output is **not** gated out: it is learned as `c = -85`. That is what ends an echo (the user lowers the speakers or mutes the headset while the game keeps playing), so the score and level must keep following it down. Gating it out froze the old score for the rest of the call.

   When the gate fails, only push the history (step 3) and update `ref_inactive_frames`.
3. **History.** Push `r` with the reference running mean and sigma **as of this frame** into an 80-slot circular history (per-index mean/sigma, as WebRTC).
4. **Reference statistics.** Update them with every pushed `r`, silent ones (-90) included, but only while the learn gate is open. Use EMA with `ALPHA = 0.002` (about 5 s of learned frames). Initialise them on the first active frame.
5. **Capture statistics.** Update the capture mean and variance with EMA ALPHA. Initialise them on the first learned frame.
6. **Covariance per lag.** For each lag k: `cov[k] = (1-ALPHA)*cov[k] + ALPHA*(c - c_mean)*(r_hist[k] - r_mean_hist[k])`, then `ncc[k] = cov[k] / (max(c_sigma, 3.0) * max(r_sigma_hist[k], 6.0))`. The sigma floors keep a nearly flat output from normalising to full scale.
7. **Score.** Take `ncc[k]` at the best `k` in `first_lag..=77` where `ncc[k]` is a local maximum (`>= ncc[k-1]` and `>= ncc[k+1]`); 0 when there is none. Clamp to 0..1 and multiply by `min(1, learned/500)`.

   `first_lag = clamp(denoiser delay / 10 ms, 1, 3)`: 3 for Strong (30 ms), 1 for Light (10 ms) and Off. The reference is read 2 blocks behind the newest (section 5.1) and the render and capture buffers add their own delay, so the measured lag is roughly `render + capture + denoiser - 15 ms - (0..10 ms)`. With Off and Light that is often 0-3 frames, so a fixed bound of 3 left the warning blind exactly where the least echo is removed. A peak below `first_lag`, at the far edge, or a monotone slope is the user's own voice coming back (a monitor, Windows "Escutar este dispositivo", which lands near `denoiser - 20 ms` or earlier) or a slow trend, never a leak. Remember the best lag.
8. **Level.** `level_db` = EMA (alpha 0.01) of `c` over learned frames in which `r_hist[best_lag] > -60`. It stays `None` until 50 such frames.

`NoiseFloor`:

```rust
pub struct NoiseFloor { .. }
impl NoiseFloor {
    pub fn new() -> Self;
    /// Frames with vad < 0.2 count; level = 10*log10(out_ms) floored at -90.
    pub fn push(&mut self, out_ms: f32, vad: f32);
    /// Median of the last 10 one-second P20 values, None until 3 valid seconds.
    pub fn floor_db(&self) -> Option<f32>;
    pub fn reset(&mut self);
}
```

- Every 100 pushes, if at least 30 non-speech levels were collected in that second, compute their 20th percentile with `select_nth_unstable_by(f32::total_cmp)` on the fixed array (in place, no allocation).
- Push that P20 into a 10-entry ring.

Unit tests. The reference is a deterministic telegraph envelope: powers alternating between -20 and -50 dB, with dwell times drawn from 50-400 ms using `Noise`.

**EchoDetector:**

1. Positive case. The output is the same envelope delayed 250 ms, attenuated 30 dB, plus independent noise at -70, with `voice_db = Some(-23)`. After 15 s, `score > 0.6`, `best_lag_ms` is 250 ± 20, and `level_db` is about -50..-80.
2. An independent output envelope over the same level range for 30 s gives `score < 0.3`.
3. A silent output (out_ms = 0) gives `score < 0.05`. (A fresh detector then reports `level_db` = Some(-85), which `LeakMonitor` treats as clear.)
4. After the positive case, 80+ frames of a silent reference make the score bit-identical frame to frame for the rest of the silence. From fresh, it stays 0. `ref_inactive_frames` grows.
4b. After the positive case, 3000 frames of an **active** reference with a silent output (out_ms = 0) give `score < 0.3` and `level_db < -56`: an echo that goes away because the output went silent clears.
5. A loud user voice (frames above voice-10) is never learned: the score is unchanged by speech-only frames.
6. A delay of 600 ms is detected with `best_lag_ms` 600 ± 20. A delay of 1200 ms gives `score < 0.3`.
7. An output that equals the reference at 0 lag (a self-loop) gives `score < 0.3` (no interior local maximum at or beyond `first_lag`), with any denoiser delay. A 1- or 2-frame delay scores `> 0.5` with `set_denoise_delay_ms(0.0)` and `(10.0)`, and `< 0.3` with `(30.0)` and with an unset detector.
8. Everything stays finite at the extremes (0, 1e-30, 1.0).

**NoiseFloor:**

1. Constant -50 dB non-speech gives -50 ± 0.5.
2. Clicks (10% of frames at -10 dB) do not move the P20.
3. Speech-only input (vad 0.9) gives None.

### 4.3 `chain.rs` changes

- Replace `comp: Compressor` with `agc: Agc` at the same position: after tone, before output gain and the limiter. Call it as `self.agc.process(buf, vad, post_denoise_ms, p.output_gain_db, p.auto_level)`.
- In `set_params`, when `input_gain_db` changes, call `self.agc.shift_db(new - old)`.
- New `pub fn shift_input_db(&mut self, db: f32)`, which calls `agc.shift_db(db)`. It is used for Windows mic volume changes.
- `FrameInfo` gains:
  - `agc_gain_db: f32`
  - `input_voice_db: Option<f32>`
  - `voice_db: Option<f32>` = input + agc gain + `p.output_gain_db`
- Remove `Compressor` and its test from `dsp/basic.rs`.
- Add `pub mod agc; pub mod leak;` to `dsp/mod.rs`.
- Update the module doc. `off_with_everything_disabled_is_bit_exact` must keep passing.
- `selftest.rs`:
  - Replace the "+6 dB auto-level makeup" wording.
  - Print the AGC gain and the output speech level for the auto_level mode.
  - Its pass/fail logic is unchanged.

## 5. clarim-audio: references, pipeline, engine

### 5.1 `refs.rs`: lock-free reference slots and loopback threads

```rust
pub const MAX_REFS: usize = 4;
const REF_RING: usize = 64;                       // 640 ms of 10 ms powers
#[derive(Debug)] pub struct RefSlot { gen: AtomicU32, running: AtomicBool, written: AtomicU64, ring: [AtomicU32; REF_RING] }
#[derive(Debug)] pub struct RefSlots(pub [RefSlot; MAX_REFS]);  // Arc'd; created once in MicEngine::new
impl RefSlot { pub(crate) fn begin(&self); pub(crate) fn push(&self, ms: f32); pub(crate) fn end(&self); }
pub enum RefStep { Off, Reset, Block(f32), Missing }
pub struct RefReader { gen: u32, last_w: u64, stale: u32, was_running: bool }  // DSP-thread side
impl RefReader { pub fn new() -> Self; pub fn next(&mut self, slot: &RefSlot) -> RefStep; }
pub struct Loopbacks { .. }                        // owned by MicEngine
impl Loopbacks {
    pub fn new(slots: Arc<RefSlots>) -> Self;
    /// Make exactly these render endpoints run (max MAX_REFS). Idempotent. NEVER joins a running thread.
    pub fn set(&mut self, ids: &[String]);
    /// Signal every stream to stop; joins only finished threads; waits at most `wait` for the rest, then detaches.
    pub fn stop_all(&mut self, wait: Duration);
    pub fn running(&self) -> u32;
}
impl Drop for Loopbacks { /* stop_all(300 ms) */ }
```

**Protocol:**

- `begin`: `gen.fetch_add(1, Release)`, then `running.store(true, Release)`.
- `push`: store the value into `ring[w % REF_RING]` (Relaxed), then `written.fetch_add(1, Release)`.
- `end`: `running.store(false, Release)`.

**Consumer**: `RefReader::next`, once per mic frame. Load `running`, then `gen`, both Acquire.

| Condition | Result |
|---|---|
| Not running | `Off` |
| `gen` changed | `Reset`, then record the gen and `last_w = written` |
| Otherwise | `w = written.load(Acquire)`, then see the next three rows |
| `w < 2` | `Missing` |
| `w` equals `last_w` for more than 5 consecutive calls | `Missing` |
| Else | `Block(ring[(w-2) % REF_RING])`, updating `last_w`/`stale` |

Reading a fixed 2 blocks behind the newest keeps the delay constant (the 80-lag search absorbs the offset). A late packet repeats a block instead of inserting a fake silence, and a capture-backlog skip on the DSP side needs no correction.

**Management** (`Loopbacks::set`):

- Diff the wanted ids against the running set.
- For a removed id, set its stop flag and move the `JoinHandle` to `retiring`.
- A slot is reused only after its previous thread `is_finished()`.
- Every call joins only finished handles.
- A dead thread (finished on its own) is restarted at most every 10 s.
- More than MAX_REFS ids, and each device's open error, are logged once per distinct id set, not on every call.

**Producer** (one thread per device, `clarim-loopback`, normal priority, COM MTA via `wasapi::initialize_mta`):

1. **Open through the existing pipeline helpers.** Add a `Flow::Loopback` to pipeline.rs: `open_device` finds a **Render** device by id; `open_stream` calls `initialize_client(&fmt, 2_000_000 /*200 ms*/, &Direction::Capture, &ShareMode::Shared, true)`. The existing `OpenedStream`/`EventCloser` then closes the event handle (wasapi 0.15 leaks it otherwise).
2. **Format.** Try 48 kHz float 2 ch first. If that is refused, use `get_mixformat()` and `rate/100` frames per block.
3. **Start.** Call `start_stream()`. Call `slot.begin()` only after it succeeds. A drop guard calls `slot.end()` and `stop_stream()` on every exit, panics included.
4. **Loop** until stop: `wait_for_event(20 ms)` (Err on timeout is normal), then drain with `get_next_nbr_frames`/`read_from_device` sized from the packet.
   - A `silent` packet counts as zeros.
   - Scrub non-finite samples to 0.
   - The block value is the **mean of (L²+R²)/2** over the block (not the square of the mono sum), pushed every block.
5. **Zero-fill.** Only in an iteration whose drain returned 0 frames: if no block was pushed for more than 30 ms of wall clock, push zero blocks for the elapsed whole 10 ms periods, at most 10.
6. **Stop latency.** On stop, exit within about 30 ms.

Unit tests (no devices):

- The ring wraps.
- The reader reads newest-2.
- A late producer repeats the block, never silence.
- More than 5 stale calls give Missing.
- A gen change gives Reset.
- Not running gives Off.
- A producer/consumer cross-thread smoke test.
- `set` never blocks: a fake slow thread does not delay `set`.

Device tests (`#[ignore]`, with the command in the doc comment):

- `loopback_default_render`: 3 s on the default render device; print blocks per second, frames per packet, the longest gap and the level.
- `echo_live`, a read-only calibration aid:
  1. Real default mic through `MicChain` (Strong, auto_level on).
  2. `EchoDetector` against the default render loopback.
  3. Run for `CLARIM_ECHO_SECS` (default 60) seconds.
  4. Print `score`, `best_lag_ms`, `level_db`, `voice_db` and the AGC gain every second.

  No cable or engine is needed.

### 5.2 `pipeline.rs`

- `EngineConfig` gains `refs: Arc<RefSlots>`. Update every literal (engine.rs, the test `Rig`).
- `EngineCmd` gains `MicVolumeShift(f32)` (`Copy`). The DSP thread calls `chain.shift_input_db(db)`.
- `dsp_thread` preallocates `[RefReader; 4]`, `[EchoDetector; 4]` and a `NoiseFloor` before the RT guard.
- **Per frame:**
  - **Before `chain.process`** (raw frame), compute `clipped`:
    - at least 2 samples with |s| ≥ 0.985; **or**
    - a run of at least 8 bit-identical consecutive samples with |s| ≥ 0.25 (a flat top survives software scaling).
  - **After `chain.process`:**
    1. If `clipped && info.vad >= 0.5`, then `clip_frames += 1` (wrapping). Clipping on non-speech (desk thumps) does not count.
    2. `out_ms` = mean square of `frame`.
    3. For each slot, `match reader.next(slot)`:
       - `Off`: reset the detector once on a running→not-running edge.
       - `Reset`: `det.reset()`.
       - `Block(ms)`: `det.set_denoise_delay_ms(chain.latency_ms())`, then `det.push(ms, out_ms, info.voice_db)`.
       - `Missing`: skip the slot this frame.
    4. `echo_score` = max over running slots whose `ref_inactive_frames() <= 300` (3 s). `echo_db` and `echo_lag_ms` come from that argmax; when there is none, the score is 0 and the level is NaN.
    5. `noise.push(out_ms, info.vad)`.
- **Meters** gains (f32 bits, Relaxed, every frame):
  - `agc_gain_db`
  - `input_voice_db`, `voice_db`
  - `echo_score`, `echo_db`, `echo_lag_ms`
  - `noise_db`
  - `clip_frames: AtomicU32`

  NaN means None. Add getters for each.
- **Rig tests:**
  1. **Echo rises.** Set up a slot by hand (`begin`); after the first roundtrip, push exactly one power per roundtrip. Feed frames whose output envelope follows the reference with a delay of 25 frames. Assert that `meters.echo_score` rises above 0.5.
  2. **Silence clears.** Continue with 5 s of silent reference blocks and assert `echo_score == 0`.
  3. **Clipping counts.** Frames with peaks at 1.0 during speech-like input raise `clip_frames`.
  4. **Speech level.** With auto_level on and `selftest::Voice` through the real RNNoise, `voice_db` becomes Some within 400 frames. This verifies that SPEECH_VAD 0.6 with a 12-frame run is reachable; if not, report the measured VAD distribution in the test failure message.

### 5.3 `engine.rs`

- `MicEngine` owns `refs: Arc<RefSlots>` (created in `new`, passed in each `EngineConfig`) and `loopbacks: Loopbacks`.
- `pub fn set_references(&mut self, ids: &[String])` delegates to `loopbacks.set` when enabled.
- **`shutdown()` does not touch the loopbacks.** `loopbacks.stop_all(..)` is called only from:
  - `MicEngine::stop()`;
  - the `!s.enabled` branch of `apply`;
  - `Drop for Loopbacks`.
- `pub fn shift_mic_volume(&self, db: f32) -> bool`: sends `EngineCmd::MicVolumeShift(db)`.
- `status()` fills the new engine fields from the meters (NaN → None), plus `references = loopbacks.running()`.
- New getters (the resolved devices while running):
  - `input_device_id(&self) -> Option<String>`
  - `output_device_id(&self) -> Option<String>`
  - `monitor_device_id(&self) -> Option<String>`: the **resolved** monitor id when the monitor is on. `monitor_id` None means the default output's id.
- `pub fn virtual_mic_capture<'a>(inputs: &'a [DeviceInfo], output_name: &str) -> Option<&'a DeviceInfo>`: the capture endpoint that is virtual and has the same `device_group` as `output_name` (make `device_group`/`is_cable_partner` pub(crate) as needed). Unit-test it with the `dev()` helpers.
- `pub fn default_render_id(comms: bool) -> Option<String>`: `wasapi::get_default_device_for_role(&Direction::Render, &Role::Communications | Console)`.
- `crates/clarim-audio/src/lib.rs`:
  - `mod refs; mod endpoint;`
  - `pub use engine::{virtual_mic_capture, default_render_id};`
  - `pub use endpoint::*;`
  - re-export what the backend needs (e.g. `dsp::agc::{AGC_TARGET_DB, AGC_MAX_GAIN_DB}`).

### 5.4 `endpoint.rs`: Windows mic volume

Add `Win32_Media_Audio_Endpoints` to clarim-audio's windows features.

```rust
pub struct EndpointVolume { pub db: f32, pub min_db: f32, pub max_db: f32, pub hardware: bool }
pub fn endpoint_volume(device_id: &str) -> anyhow::Result<EndpointVolume>;    // hardware = QueryHardwareSupport() & ENDPOINT_HARDWARE_SUPPORT_VOLUME (0x1)
/// Set the endpoint master volume in dB (clamped to the range), tagged with Clarim's event-context GUID; returns the value read back.
pub fn set_endpoint_volume_db(device_id: &str, db: f32) -> anyhow::Result<f32>;
```

- Use `IMMDeviceEnumerator::GetDevice(id)`, then `Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)`, then `GetVolumeRange`, `GetMasterVolumeLevel`, `QueryHardwareSupport` and `SetMasterVolumeLevel(db, &CLARIM_EVENT_GUID)`.
- **COM.** A guard calls `CoInitializeEx(MTA)`; `S_OK` and `S_FALSE` must be balanced with `CoUninitialize` on drop. On `RPC_E_CHANGED_MODE`, do not uninitialise. The housekeeping thread is already MTA (backend.rs:357), so S_FALSE is the normal case. It is called from the backend housekeeping thread, never from the DSP thread.
- The device test is `#[ignore]` and read-only: print the default mic's volume, range and hardware flag.

## 6. apps/clarim

### 6.1 `callwatch.rs` (new, pure, unit-tested; `now: Instant` always passed in)

```rust
pub const NOT_CALL_EXES: &[&str] = &["obs64.exe", "obs32.exe"];
pub struct CallTracker { .. }   // HANGOVER = 8 s
impl CallTracker { pub fn update(&mut self, now: Instant, captures: &[CaptureSession], cable_capture_id: Option<&str>) -> CallState; }
pub struct CallState { pub in_call: bool, pub apps: Vec<String> }
```

`CallTracker`:

- Counts only active sessions whose `device_id` equals the cable capture id (case-insensitive) and whose exe is not in `NOT_CALL_EXES`.
- `unknown` exes count for `in_call` but are left out of `apps`.
- `apps` is deduped and sorted, and is kept during the hangover.
- With no cable id, there is never a call.

```rust
pub struct LeakMonitor { .. }
impl LeakMonitor { pub fn tick(&mut self, now: Instant, in_call: bool, enabled: bool, m: &LeakMeasures) -> LeakOut; pub fn delivered(&mut self, kind: LeakKind, now: Instant); }
pub struct LeakMeasures { pub echo_score: f32, pub echo_db: Option<f32>, pub echo_lag_ms: f32, pub noise_db: Option<f32>, pub references: u32 }
pub struct LeakOut { pub alert: Option<LeakKind>, pub balloon: Option<LeakKind> }
```

`LeakMonitor` ticks every 1 s:

| Kind | Raise | Clear |
|---|---|---|
| Echo | `references > 0 && echo_score >= 0.5 && echo_db >= Some(-50)` and \|lag - previous tick's lag\| <= 20 ms, for 4 consecutive ticks | `echo_score < 0.3` or `echo_db` None or `< -56`, for 10 consecutive ticks |
| Noise | `noise_db >= -48` for 8 consecutive ticks | `noise_db < -54` (or None) for 10 consecutive ticks |

- **Precedence.** `alert` = Echo if active, else Noise if active.
- **Balloon.**
  - When an alert is active and that kind was not delivered in this call, `balloon = Some(kind)` on every tick until `delivered(kind, now)`.
  - After a delivery, the same kind balloons again only after 10 min of continuous activity.
- **Reset.** `!in_call` or `!enabled` clears everything: the counters, the active flags and the per-call memory. The output is None/None.
- **Logging.** While in a call, the backend logs one line every 10 s: `score lag echo_db voice_db agc_gain noise refs`, for tuning from `clarim.log`.

```rust
pub struct MicVolumeGuard { .. }
impl MicVolumeGuard {
    pub fn new(taken_db: f32) -> Self;                       // from the persisted record
    pub fn tick(&mut self, now: Instant, active: bool, clip_delta: u32, input_voice_db: Option<f32>, agc_gain_db: f32) -> VolAction;
    pub fn applied(&mut self, delta_db: f32, now: Instant); // read-back delta, negative = lowered
    pub fn disable(&mut self, reason: GuardOff);            // NoHardware | Ineffective | Error  (for this run)
    pub fn disabled(&self) -> Option<GuardOff>;
    pub fn taken_db(&self) -> f32;
    pub fn too_quiet(&self) -> bool;
}
pub enum VolAction { None, Lower(f32), Raise(f32) }
```

`MicVolumeGuard`:

- **`active`** = in_call && auto_level && auto_mic_volume && mic running && the record's device (if any) is the current input.
- **Lower(3.0)** when all of these hold:
  - the clip frames over the last 2 ticks sum to 5 or more;
  - `taken < 12`;
  - at least 10 s have passed since the last action;
  - not disabled.
- **Ineffective check.** Keep a 10-tick ring of clip deltas. 10 active ticks after a Lower, if the clip frames in those 10 ticks are not at most half of the 10 ticks before the Lower, return `Raise(episode_db)` once to give back that episode, then `disable(Ineffective)`.
- **Raise(min(3, taken))**, giving back, when all of these hold:
  - `taken > 0`;
  - for 10 consecutive ticks: `agc_gain_db >= AGC_MAX_GAIN_DB - 0.5` and `input_voice_db + agc_gain_db < AGC_TARGET_DB - 6`;
  - no clip in the last 60 s;
  - at least 10 s since the last action.
- **`applied(delta)`.** `taken = clamp(taken - delta, 0, 12)`. If |delta| < 0.5, only the cooldown starts.
- **`too_quiet`**: the quiet condition for 10 ticks with `taken == 0`, or with the guard disabled (a disabled guard never gives anything back, so only the user can help). It is tracked only while `active`: with `auto_mic_volume` off, or a record that belongs to another mic, there is no verdict (the README says so).
- **When `!active`**, return None and reset the windows and counters; `taken` survives.

### 6.2 `backend.rs`

`tick_mic` runs every 1 s, after the existing work, still holding `mic`. Lock order: mic → cfg (read, or write for persistence) → leaf locks `devices`, `routing`, `call`. None of these steps joins a thread or blocks for long.

1. **Cable id.** `virtual_mic_capture(&devices.inputs, &status.output_name)`.
2. **Call state.** `captures` comes from `routing.captures()` (non-touching; empty when routing is None). Then `call = tracker.update(now, &captures, cable_id)`.
3. **References.** `want_refs` = in_call && leak_warnings && status.running.
   - Ids: the distinct effective devices of the 4 channels (None → `default_render_id(false)`), plus `default_render_id(false)` and `default_render_id(true)`.
   - Exclude the engine's output and monitor (`eng.output_device_id()`, `eng.monitor_device_id()`), virtual devices, and anything not in `devices.outputs`. Cap at 4.
   - If every candidate is excluded, log once per call that echo detection is off for this call.
   - Then `eng.set_references(&ids)` (empty when not wanted).
4. **Leak.** `leak = monitor.tick(..)`. If `leak.balloon` is set, call `tray::notify(title, text, Balloon::Warning)`; on `true`, `monitor.delivered(kind, now)`.

   | Kind | Title | Text |
   |---|---|---|
   | Echo | "Som vazando na chamada" | "O som das caixas está entrando no seu microfone. Abaixe o volume ou use fone." |
   | Noise | "Ruído passando na chamada" | "Tem ruído de fundo passando pelo filtro. Aproxime o microfone ou ligue o Foco na voz." |

5. **Mic volume** (only when `guard.tick` returns an action, the guard is not disabled, and the input id is known).
   1. **Read.** `v = endpoint_volume(id)`.
   2. **Lower.**
      - If `!v.hardware`, or `v.max_db - v.min_db < 3`, or `v.db - v.min_db < 0.5` (already at the floor, nothing left to lower: otherwise the guard would retry every 10 s, rewriting `config.json`, and never tell the user): `guard.disable(NoHardware)`, then note (stays for the run): "O microfone está saturando antes do volume do Windows: diminua o 'Aumento do microfone' (Propriedades do microfone > Níveis) ou afaste o microfone."
      - Otherwise, set `max(v.db - 3, v.min_db)`. If there is no record, create `MicVolumeTaken { device_id: id, baseline_db: v.db, taken_db: 0 }`.
   3. **Raise.** Set `min(v.db + n, record.baseline_db)`. If `v.db >= record.baseline_db - 0.5` (the user raised it), clear the record and skip.
   4. **Apply the read-back delta.** `delta = readback - v.db`, then `guard.applied(delta, now)`, then `record.taken_db = guard.taken_db()`. Clear the record when it reaches 0.
   5. **Compensate.**
      - `eng.shift_mic_volume(delta)`, so the AGC estimate follows.
      - If `cfg.mic.voice_level_db` is Some, add `delta` to it so the Voice Focus calibration stays valid.
      - Persist cfg (`mic_volume_taken` included) and `eng.apply`.
   6. **Note** (60 s): "Baixei o volume do microfone no Windows em 3 dB porque estava estourando." / "Devolvi 3 dB ao volume do microfone no Windows (a voz estava baixa)." / for an Ineffective give-back, the NoHardware text above.
   7. **Balloon** once per call for a Lower: `("Microfone estourando", "Baixei o volume do microfone no Windows em 3 dB.", Balloon::Info)`.
   8. **Errors.** Log at warn and `guard.disable(Error)` for the run.
6. **Overlay.** Store `in_call`, `call_apps`, `leak`, `mic_volume_note` and `mic_too_quiet` in a new leaf `Mutex<CallOverlay>`. `mic_status()` and `snapshot()` copy it into the MicStatus they return, including the cached `last_mic` path.
7. **Tooltip.** `tray::set_tip`:
   - "Clarim · som vazando na chamada" while an alert is active;
   - else "Clarim · em chamada" while in a call;
   - else "Clarim".

   Change only on transitions.
8. **Graceful quit.** `shutdown()` gives the volume back, best effort: if a record exists for a present device and the current volume is within 0.5 dB of `baseline - taken`, set `baseline_db`, clear the record and persist. If the user changed the volume meanwhile, just clear the record.
9. **`check_mic`.** Keep `mic_volume_taken` from `stored`, like `voice_level_db`. Nothing else is needed for the new bools.

### 6.3 `tray.rs`

- `pub enum Balloon { Info, Warning }`.
- `pub fn notify(title: &str, text: &str, kind: Balloon) -> bool`:
  1. Return false when no tray thread is registered.
  2. Call `SHQueryUserNotificationState()` on the caller thread. Unless it is `Ok(QUNS_ACCEPTS_NOTIFICATIONS)`, return false so the backend retries next tick.
  3. Otherwise push into a static queue (`Mutex<Vec<..>>`, cap 4, drop the oldest), call `wake(WM_APP)` and return true.
- `pub fn set_tip(text: &str)`: store and wake. The loop calls `tray.set_tooltip(Some(text))`.
- **In `run_loop`**, after the PAUSED block, drain the queue: `Shell_NotifyIconW(NIM_MODIFY, &nid)`.
  - `cbSize = size_of::<NOTIFYICONDATAW>()`
  - `hWnd = HWND(tray.window_handle() as _)`
  - **`uID = 1`.** tray-icon 0.26.1 numbers icons from a process-global counter starting at 1, and Clarim builds exactly one `TrayIcon`. There is no probe.
  - `uFlags = NIF_INFO`
  - `szInfoTitle` (≤ 63 UTF-16 units + NUL) and `szInfo` (≤ 255 + NUL), truncated safely
  - `dwInfoFlags = (NIIF_WARNING | NIIF_INFO per kind) | NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME`
- **On a FALSE return** (for example explorer restarting), log once at warn and keep the item for one more try at the next drain.
- Extend the existing `#[ignore]` tray end-to-end test, if one exists, to assert that `NIM_MODIFY` + `NIF_TIP` on (`window_handle()`, 1) returns TRUE.

## 7. clarim-web

### 7.1 `dashboard.html`

All text goes through `textContent`, never `innerHTML`.

Toggles:

- **`#m-level`**: relabel "Volume automático", with the hint "Mantém sua voz no mesmo volume na chamada: não estoura nem fica baixa."
- **`m-leak`**: new switch "Avisar se vazar som na chamada" → `leak_warnings`, in the "Voz" column.
- **`m-micvol`**: new switch "Ajustar o volume do microfone no Windows se estourar" → `auto_mic_volume`, in `<details class="adv">`. In `syncDeps`, disable it when auto_level is off.

Readouts:

- **AGC readout**: a `.mini` line under `#m-level`, "ganho +6 dB · voz −23 dBFS", using `fmtDb` and the true minus. Show "voz —" when `voice_db` is null. Hidden when auto_level is off.
- **Call pill**: in the mic card header, "Em chamada: Discord" (exe names without `.exe`, title-cased first letter, joined with ", "). Hidden when `!in_call`.

Banners:

- **`#leak`**: a `.banner.warn` with `role="status"`. Shown when `in_call && leak`.
  - echo: "Tem som das caixas vazando no seu microfone: o pessoal da chamada pode ouvir o jogo, vídeos ou outros sons daí. Abaixe o volume das caixas ou use fone."
  - noise: "Tem ruído de fundo passando pelo filtro na chamada. Aproxime o microfone ou ligue o Foco na voz." Append " Tente também o filtro Forte." only when `S.config.mic.denoise !== 'strong'`.
- **`#micvol`**: a `.banner.info` showing `mic_volume_note` when present.
- **`#quiet`**: a `.hint` line shown when `mic_too_quiet`: "Sua voz está chegando baixa mesmo com o volume automático no máximo. Suba o volume do microfone no Windows ou aproxime o microfone."

Other:

- **Debug line** in Avançado, a `.mini` shown only while `in_call`: "Vazamento: eco 0,12 · atraso 250 ms · ruído −62 dBFS · referências 1".
- **`applyMeters` key**: add the following.
  - `in_call`, `leak`, `mic_volume_note`, `mic_too_quiet`
  - `(call_apps||[]).join()`
  - rounded `agc_gain_db`, rounded `voice_db`
  - `Math.round(echo_score*100)`, `Math.round(noise_db)`, `references`

### 7.2 Tests and mocks

`lib.rs`:

- `dashboard_carries_the_required_notices` adds needles for the new banners and toggles.
- `dashboard_makes_no_external_requests` must still pass.

`tests/it/main.rs`:

- The mock compiles.
- A new test posts `leak_warnings`/`auto_mic_volume` false to `/api/mic` and asserts on `s.backend.mic`, as `volume_is_clamped_and_mic_settings_round_trip` does.
- A second test: a POST without the keys keeps both true.

`examples/mock.rs`:

- The full `MicStatus` literal gets the new fields.
- New flags `--in-call`, `--leak-echo`, `--leak-noise`, `--mic-vol-note`, `--too-quiet`, documented in the header comment.

## 8. Docs

README.md (PT-BR, same register as the rest):

- A section "Volume automático" (target, that it learns from your voice, and to turn off the call app's own AGC).
- A section "Avisos de vazamento na chamada": what is detected, only during calls, discreet balloon, not in full screen (waits until alt-tab), the known limits from section 0, how to turn it off.
- The Windows mic volume auto-adjust: up to 12 dB, only mics with a hardware volume, given back when the voice is low and on quit, how to turn it off.
- Troubleshooting entries.

SPEC.md:

- MicSettings block, chain description (AGC replaces the compressor), MicStatus fields.
- Notices, and the housekeeping hook.

## 9. Acceptance

The automated gate runs in this order, all green:

1. `cargo build --release`
2. `cargo clippy --release --all-targets -- -D warnings`
3. `cargo test --release --no-fail-fast` (the known flaky `guard_is_exclusive_and_visible_to_the_probe` may be skipped)

Run `cargo test` in release because `tract` debug assertions make DFN fall back in debug.

Other criteria:

- **No allocation, lock or join on the DSP thread.** No join, and no blocking COM wait, under the `mic` mutex. A reviewer checks both explicitly.
- **Back-compat.** An old config.json loads with the new defaults.
- **Calibration** before tuning thresholds: run `echo_live` with the user's permission (speech from a non-call app on the speakers, then silence) and adjust the 6.1 thresholds if needed.
- **Live test**, after install, with the user's permission:
  1. Open an app capturing CABLE Output. `in_call` turns true within 3 s and the references start.
  2. Play SPEECH from a non-call app on the speakers. The warning shows within about 20 s.
  3. Stop the app: everything stops within 10 s.
  4. AGC: `voice_db` settles near -23.
