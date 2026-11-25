//! Speaker references for the echo detector (design: docs/leak-agc.md section 5.1).
//!
//! One `clarim-loopback` thread per render device reads its WASAPI loopback (what the speakers play) and
//! publishes the power of every 10 ms into a lock-free `RefSlot`. The DSP thread reads the slots once per
//! mic frame through a `RefReader`: atomics only, no lock, no syscall.
//!
//! `Loopbacks` lives outside the mic pipeline, so a mic restart does not touch the streams. It never
//! blocks on a thread join: a thread that has to stop is told so and parked in `retiring`, and only
//! joined (instantly) once it has finished. Its slot stays taken until then, because the old thread's
//! last act (`RefSlot::end`) would otherwise switch off a new thread's stream.

use crate::devices::Flow;
use crate::dsp::FRAME;
use crate::pipeline::open_stream;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Most loopback streams at once (a mic frame costs one detector per stream).
pub const MAX_REFS: usize = 4;
/// 640 ms of 10 ms powers.
const REF_RING: usize = 64;
/// A reader repeats the last block for this many calls without news from the producer, then reports `Missing`.
const MAX_STALE_CALLS: u32 = 5;

/// One reference stream's mailbox. Single producer (its loopback thread), single consumer (the DSP thread).
#[derive(Debug)]
pub struct RefSlot {
    /// Bumped by every `begin`: a reader that sees a new value knows it is looking at a new stream.
    gen: AtomicU32,
    running: AtomicBool,
    /// Blocks published since the last `begin`.
    written: AtomicU64,
    /// Mean square of each 10 ms block (f32 bits), at `written % REF_RING`.
    ring: [AtomicU32; REF_RING],
}

impl RefSlot {
    fn new() -> Self {
        Self {
            gen: AtomicU32::new(0),
            running: AtomicBool::new(false),
            written: AtomicU64::new(0),
            ring: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    /// A stream starts publishing into this slot. Call it only once the stream really runs.
    pub(crate) fn begin(&self) {
        self.written.store(0, Ordering::Relaxed); // published by the Release below
        self.gen.fetch_add(1, Ordering::Release);
        self.running.store(true, Ordering::Release);
    }

    /// Publish one block's mean square. Producer thread only.
    pub(crate) fn push(&self, ms: f32) {
        let w = self.written.load(Ordering::Relaxed);
        self.ring[(w % REF_RING as u64) as usize].store(ms.to_bits(), Ordering::Relaxed);
        self.written.fetch_add(1, Ordering::Release);
    }

    /// The stream stopped publishing.
    pub(crate) fn end(&self) {
        self.running.store(false, Ordering::Release);
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

/// The four slots, shared (`Arc`) between the loopback threads and every mic pipeline.
#[derive(Debug)]
pub struct RefSlots(pub [RefSlot; MAX_REFS]);

impl RefSlots {
    pub fn new() -> Self {
        Self(std::array::from_fn(|_| RefSlot::new()))
    }
}

impl Default for RefSlots {
    fn default() -> Self {
        Self::new()
    }
}

/// What the DSP thread gets for one mic frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RefStep {
    /// No stream runs in this slot.
    Off,
    /// A new stream started (or this reader is new): forget what the detector knows.
    Reset,
    /// Mean square of the reference block for this frame.
    Block(f32),
    /// A stream runs but has no block to give (yet, or for too long).
    Missing,
}

/// DSP-thread side of one slot. Plain fields, no allocation.
#[derive(Debug, Default)]
pub struct RefReader {
    gen: u32,
    last_w: u64,
    stale: u32,
}

impl RefReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Once per mic frame. Reads the block two behind the newest, so the delay between reference and mic
    /// stays constant whatever the packet timing; a late packet repeats the previous block (for up to 5
    /// calls) instead of inserting a fake silence.
    pub fn next(&mut self, slot: &RefSlot) -> RefStep {
        if !slot.running.load(Ordering::Acquire) {
            return RefStep::Off;
        }
        let gen = slot.gen.load(Ordering::Acquire);
        if gen != self.gen {
            self.gen = gen;
            self.last_w = slot.written.load(Ordering::Acquire);
            self.stale = 0;
            return RefStep::Reset;
        }
        let w = slot.written.load(Ordering::Acquire);
        if w < 2 {
            return RefStep::Missing;
        }
        if w == self.last_w {
            self.stale = self.stale.saturating_add(1);
            if self.stale > MAX_STALE_CALLS {
                return RefStep::Missing;
            }
        } else {
            (self.last_w, self.stale) = (w, 0);
        }
        let ms = f32::from_bits(slot.ring[((w - 2) % REF_RING as u64) as usize].load(Ordering::Relaxed));
        RefStep::Block(ms)
    }
}

/// A producer thread reports why it ended on its own; `Ok` only after being told to stop.
type Handle = JoinHandle<Result<(), String>>;

struct Worker {
    stop: Arc<AtomicBool>,
    handle: Handle,
}

/// A wanted device. `worker` is `None` once its thread ended on its own (device unplugged, open refused):
/// the slot stays reserved and the stream is tried again `RESTART_AFTER` after the last start.
struct Active {
    id: String,
    slot: usize,
    worker: Option<Worker>,
    started: Instant,
}

/// A thread told to stop that has not finished yet. It still owns its slot.
struct Retiring {
    slot: usize,
    worker: Worker,
}

/// A dead stream is restarted at most this often.
const RESTART_AFTER: Duration = Duration::from_secs(10);

/// The loopback streams: exactly the wanted render endpoints run, at most `MAX_REFS`.
pub struct Loopbacks {
    slots: Arc<RefSlots>,
    active: Vec<Active>,
    retiring: Vec<Retiring>,
    /// The id set of the last `set`, and the ids whose failure was already logged for it.
    seen: Vec<String>,
    logged: HashSet<String>,
}

impl Loopbacks {
    pub fn new(slots: Arc<RefSlots>) -> Self {
        Self { slots, active: Vec::new(), retiring: Vec::new(), seen: Vec::new(), logged: HashSet::new() }
    }

    /// Make exactly these render endpoints run (at most `MAX_REFS`, the first ones win). Idempotent, and it
    /// NEVER joins a running thread: streams that are no longer wanted are told to stop and reaped on a
    /// later call, once they have finished.
    pub fn set(&mut self, ids: &[String]) {
        self.reap();
        let mut want: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids {
            if !want.contains(id) {
                want.push(id.clone());
            }
        }
        if want != self.seen {
            if want.len() > MAX_REFS {
                log::warn!("loopback: {} render devices asked for, only the first {MAX_REFS} are watched", want.len());
            }
            self.seen.clone_from(&want);
            self.logged.clear();
        }
        want.truncate(MAX_REFS);

        let mut i = 0;
        while i < self.active.len() {
            if want.contains(&self.active[i].id) {
                i += 1;
            } else {
                let gone = self.active.swap_remove(i);
                self.retire(gone);
            }
        }
        for id in &want {
            match self.active.iter().position(|a| &a.id == id) {
                Some(i) => {
                    let (dead, slot) = (self.active[i].worker.is_none(), self.active[i].slot);
                    if dead && self.active[i].started.elapsed() >= RESTART_AFTER {
                        self.start(id, slot, Some(i));
                    }
                }
                None => {
                    if let Some(slot) = self.free_slot() {
                        self.start(id, slot, None);
                    } // else: every slot still belongs to a stopping thread; the next call retries
                }
            }
        }
    }

    /// Signal every stream to stop; join only the threads that have finished; wait at most `wait` for the
    /// rest, then leave them to finish alone (their slots stay reserved until they do).
    pub fn stop_all(&mut self, wait: Duration) {
        for a in std::mem::take(&mut self.active) {
            self.retire(a);
        }
        self.seen.clear();
        self.logged.clear();
        let deadline = Instant::now() + wait;
        loop {
            self.reap();
            if self.retiring.is_empty() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Streams that are publishing right now.
    pub fn running(&self) -> u32 {
        let live = |a: &&Active| a.worker.is_some() && self.slots.0[a.slot].is_running();
        self.active.iter().filter(live).count() as u32
    }

    fn free_slot(&self) -> Option<usize> {
        (0..MAX_REFS).find(|&s| self.active.iter().all(|a| a.slot != s) && self.retiring.iter().all(|r| r.slot != s))
    }

    /// Tell the stream's thread to stop and park it until it has finished.
    fn retire(&mut self, a: Active) {
        if let Some(worker) = a.worker {
            worker.stop.store(true, Ordering::Relaxed);
            self.retiring.push(Retiring { slot: a.slot, worker });
        }
    }

    /// Start (or, with `at`, restart) the stream of `id` in `slot`. A failure to even spawn the thread is
    /// treated like a stream that died at once.
    fn start(&mut self, id: &str, slot: usize, at: Option<usize>) {
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (slots, id, stop) = (self.slots.clone(), id.to_string(), stop.clone());
            std::thread::Builder::new().name("clarim-loopback".into()).spawn(move || produce(&id, &slots.0[slot], &stop))
        };
        let worker = match worker {
            Ok(handle) => Some(Worker { stop, handle }),
            Err(e) => {
                if self.logged.insert(id.to_string()) {
                    log::warn!("loopback '{id}': cannot start the thread: {e}");
                }
                None
            }
        };
        let entry = Active { id: id.to_string(), slot, worker, started: Instant::now() };
        match at {
            Some(i) => self.active[i] = entry,
            None => self.active.push(entry),
        }
    }

    /// Join the threads that have finished (that returns at once) and note why a stream died by itself.
    fn reap(&mut self) {
        let mut i = 0;
        while i < self.retiring.len() {
            if self.retiring[i].worker.handle.is_finished() {
                let _ = self.retiring.swap_remove(i).worker.handle.join();
            } else {
                i += 1;
            }
        }
        for a in &mut self.active {
            if a.worker.as_ref().is_some_and(|w| w.handle.is_finished()) {
                let Some(w) = a.worker.take() else { continue };
                let why = match w.handle.join() {
                    Ok(Ok(())) => "stopped".to_string(),
                    Ok(Err(e)) => e,
                    Err(_) => "the thread panicked".to_string(),
                };
                if self.logged.insert(a.id.clone()) {
                    log::warn!("loopback '{}': {why} (trying again every {RESTART_AFTER:?})", a.id);
                }
            }
        }
    }
}

impl Drop for Loopbacks {
    fn drop(&mut self) {
        self.stop_all(Duration::from_millis(300));
    }
}

/// Ends the stream's publishing on every exit from the producer, panics included.
struct Publishing<'a> {
    client: &'a wasapi::AudioClient,
    slot: &'a RefSlot,
}

impl<'a> Publishing<'a> {
    /// Start the stream, and only then announce it.
    fn start(client: &'a wasapi::AudioClient, slot: &'a RefSlot) -> Result<Self, String> {
        client.start_stream().map_err(|e| format!("start: {e}"))?;
        slot.begin();
        Ok(Self { client, slot })
    }
}

impl Drop for Publishing<'_> {
    fn drop(&mut self) {
        self.slot.end();
        let _ = self.client.stop_stream();
    }
}

/// Cuts a stream of frame powers into blocks of `size` frames (10 ms) and averages each.
struct Blocker {
    size: usize,
    sum: f32,
    n: usize,
}

impl Blocker {
    fn new(size: usize) -> Self {
        Self { size: size.max(1), sum: 0.0, n: 0 }
    }

    fn add(&mut self, power: f32) -> Option<f32> {
        self.sum += power;
        self.n += 1;
        (self.n == self.size).then(|| {
            let mean = self.sum / self.size as f32;
            self.clear();
            mean
        })
    }

    fn clear(&mut self) {
        (self.sum, self.n) = (0.0, 0);
    }
}

/// Mean of the squares of one interleaved f32 frame's channels: (L² + R²) / 2 for stereo. Non-finite
/// samples count as 0 (the loopback is external data: nothing may poison the detectors).
fn frame_power(frame: &[u8]) -> f32 {
    let mut sum = 0.0f32;
    for s in frame.chunks_exact(4) {
        let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        if v.is_finite() {
            sum += v * v;
        }
    }
    sum / (frame.len() / 4).max(1) as f32
}

/// Wait this long for loopback data before looking at the stop flag and the clock.
const WAIT_MS: u32 = 20;
/// No block for longer than this: the device plays nothing (a loopback delivers no packets then).
const SILENCE_AFTER: Duration = Duration::from_millis(30);
const BLOCK: Duration = Duration::from_millis(10);
/// At most this many silent blocks are made up per iteration.
const MAX_FILL: u32 = 10;

/// The producer thread body: loopback of render endpoint `id` into `slot` until `stop`.
fn produce(id: &str, slot: &RefSlot, stop: &AtomicBool) -> Result<(), String> {
    wasapi::initialize_mta().ok().map_err(|e| format!("COM init: {e}"))?;
    let stream = open_stream(Flow::Loopback, Some(id)).map_err(|e| format!("{e:#}"))?;
    let ch = stream.channels;
    let capture = stream.client.get_audiocaptureclient().map_err(|e| format!("capture client: {e}"))?;
    let buffer_frames = stream.client.get_bufferframecount().map_err(|e| format!("buffer size: {e}"))? as usize;
    let _publishing = Publishing::start(&stream.client, slot)?;
    log::info!("loopback reference started: '{id}' ({ch} ch @ {} Hz)", stream.rate);

    // Pre-sized to the whole endpoint buffer; it only grows if the device hands over more than that.
    let mut bytes = vec![0u8; buffer_frames.max(FRAME) * ch * 4];
    let mut blocker = Blocker::new(stream.rate as usize / 100);
    let mut last_block = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // A timeout is normal (nothing is playing): the drain below is also what notices a vanished device.
        let _ = stream.event.wait_for_event(WAIT_MS);
        let mut drained = 0usize;
        let mut pushed = false;
        loop {
            let frames = match capture.get_next_nbr_frames() {
                Ok(Some(f)) if f > 0 => f as usize,
                Ok(_) => break,
                Err(e) => return Err(format!("read: {e}")),
            };
            let len = frames * ch * 4;
            if len > bytes.len() {
                bytes.resize(len, 0);
            }
            let (_, flags) = capture.read_from_device(&mut bytes[..len]).map_err(|e| format!("read: {e}"))?;
            drained += frames;
            for frame in bytes[..len].chunks_exact(ch * 4) {
                // a silent packet's content is undefined: it counts as zeros
                let power = if flags.silent { 0.0 } else { frame_power(frame) };
                if let Some(ms) = blocker.add(power) {
                    slot.push(ms);
                    pushed = true;
                }
            }
        }
        if pushed {
            last_block = Instant::now();
        } else if drained == 0 {
            // Nothing arrived at all. The detectors need to hear the silence, not a stalled stream.
            let gap = last_block.elapsed();
            if gap > SILENCE_AFTER {
                let whole = (gap.as_millis() / BLOCK.as_millis()) as u32;
                for _ in 0..whole.min(MAX_FILL) {
                    slot.push(0.0);
                }
                blocker.clear();
                // after a long gap the clock restarts instead of catching up in bursts
                last_block = if whole > MAX_FILL { Instant::now() } else { last_block + BLOCK * whole };
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Publish `n` blocks valued 0, 1, 2, ... into a slot that has begun.
    fn fill(slot: &RefSlot, from: u32, to: u32) {
        for v in from..to {
            slot.push(v as f32);
        }
    }

    fn running_slot() -> RefSlot {
        let slot = RefSlot::new();
        slot.begin();
        slot
    }

    #[test]
    fn a_slot_that_never_began_is_off() {
        let (slot, mut reader) = (RefSlot::new(), RefReader::new());
        assert_eq!(reader.next(&slot), RefStep::Off);
        slot.push(1.0);
        assert_eq!(reader.next(&slot), RefStep::Off, "blocks do not count before begin()");
    }

    #[test]
    fn a_new_stream_resets_the_reader_once_and_a_stopped_one_is_off() {
        let (slot, mut reader) = (running_slot(), RefReader::new());
        assert_eq!(reader.next(&slot), RefStep::Reset, "first sight of a running stream");
        assert_eq!(reader.next(&slot), RefStep::Missing, "no block yet");
        fill(&slot, 0, 5);
        assert!(matches!(reader.next(&slot), RefStep::Block(_)));

        slot.end();
        assert_eq!(reader.next(&slot), RefStep::Off);
        slot.begin(); // the same slot, a new stream
        assert_eq!(reader.next(&slot), RefStep::Reset);
        assert_eq!(reader.next(&slot), RefStep::Missing, "the new stream starts empty: nothing of the old one leaks");
    }

    #[test]
    fn the_reader_gives_the_block_two_behind_the_newest() {
        let (slot, mut reader) = (running_slot(), RefReader::new());
        assert_eq!(reader.next(&slot), RefStep::Reset);
        fill(&slot, 0, 1);
        assert_eq!(reader.next(&slot), RefStep::Missing, "one block is not enough: newest-2 does not exist");
        fill(&slot, 1, 2);
        assert_eq!(reader.next(&slot), RefStep::Block(0.0));
        for v in 2..40u32 {
            fill(&slot, v, v + 1);
            assert_eq!(reader.next(&slot), RefStep::Block((v - 1) as f32), "newest {v}");
        }
    }

    #[test]
    fn the_ring_wraps_without_mixing_up_blocks() {
        let (slot, mut reader) = (running_slot(), RefReader::new());
        reader.next(&slot);
        for v in 0..(REF_RING as u32 * 3 + 7) {
            slot.push(v as f32);
            if v >= 1 {
                assert_eq!(reader.next(&slot), RefStep::Block((v - 1) as f32), "after {} blocks", v + 1);
            }
        }
    }

    #[test]
    fn a_late_producer_repeats_the_block_and_never_inserts_silence() {
        let (slot, mut reader) = (running_slot(), RefReader::new());
        reader.next(&slot);
        fill(&slot, 10, 14); // blocks 10..13: newest-2 is 12
        assert_eq!(reader.next(&slot), RefStep::Block(12.0));
        for call in 1..=MAX_STALE_CALLS {
            assert_eq!(reader.next(&slot), RefStep::Block(12.0), "stale call {call} repeats the block");
        }
        fill(&slot, 14, 15);
        assert_eq!(reader.next(&slot), RefStep::Block(13.0), "news: back on the newest-2 block, no stale count left");
        assert_eq!(reader.next(&slot), RefStep::Block(13.0));
    }

    #[test]
    fn more_than_five_stale_calls_give_missing_until_news_arrives() {
        let (slot, mut reader) = (running_slot(), RefReader::new());
        reader.next(&slot);
        fill(&slot, 0, 10);
        assert!(matches!(reader.next(&slot), RefStep::Block(_)));
        for _ in 0..MAX_STALE_CALLS {
            assert!(matches!(reader.next(&slot), RefStep::Block(_)));
        }
        for call in 0..20 {
            assert_eq!(reader.next(&slot), RefStep::Missing, "stale call {} past the limit", call + 6);
        }
        fill(&slot, 10, 11);
        assert_eq!(reader.next(&slot), RefStep::Block(9.0));
    }

    #[test]
    fn producer_and_consumer_on_two_threads_never_see_torn_or_future_blocks() {
        let slot = Arc::new(RefSlot::new());
        let done = Arc::new(AtomicBool::new(false));
        let producer = {
            let (slot, done) = (slot.clone(), done.clone());
            std::thread::spawn(move || {
                slot.begin();
                let mut v = 0u32;
                while !done.load(Ordering::Relaxed) {
                    slot.push(v as f32);
                    v += 1;
                    if v.is_multiple_of(16) {
                        std::thread::yield_now();
                    }
                }
                slot.end();
                v
            })
        };
        let mut reader = RefReader::new();
        let (mut last, mut blocks, mut resets) = (-1.0f32, 0u32, 0u32);
        let deadline = Instant::now() + Duration::from_secs(10);
        while blocks < 2_000 {
            match reader.next(&slot) {
                RefStep::Reset => resets += 1,
                RefStep::Block(v) => {
                    assert!(v.fract() == 0.0 && v >= 0.0, "torn block {v}");
                    assert!(v >= last, "time went backwards: {last} then {v}");
                    (last, blocks) = (v, blocks + 1);
                }
                RefStep::Missing | RefStep::Off => std::thread::yield_now(),
            }
            assert!(Instant::now() < deadline, "only {blocks} blocks seen, {resets} resets");
        }
        done.store(true, Ordering::Relaxed);
        let pushed = producer.join().expect("producer");
        assert!(last < pushed as f32, "read block {last} but only {pushed} were published");
        assert_eq!(reader.next(&slot), RefStep::Off, "a finished stream reads as off");
        assert_eq!(resets, 1, "one stream, one reset");
    }

    fn fake_worker(run_for: Duration) -> Worker {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let end = Instant::now() + run_for;
            while Instant::now() < end && !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(2));
            }
            std::thread::sleep(run_for); // slow to wind down, whatever the flag says
            Ok(())
        });
        Worker { stop, handle }
    }

    #[test]
    fn set_never_waits_for_a_thread_and_keeps_its_slot_until_it_has_finished() {
        let mut lb = Loopbacks::new(Arc::new(RefSlots::new()));
        lb.active.push(Active {
            id: "gone".into(),
            slot: 2,
            worker: Some(fake_worker(Duration::from_millis(400))),
            started: Instant::now(),
        });

        let t0 = Instant::now();
        lb.set(&[]);
        assert!(t0.elapsed() < Duration::from_millis(150), "set() waited {:?} for a slow thread", t0.elapsed());
        assert!(lb.active.is_empty() && lb.retiring.len() == 1, "the stream is stopping, not gone");
        assert_eq!(lb.free_slot(), Some(0), "slots 0 and 1 are free");
        lb.retiring.push(Retiring { slot: 0, worker: fake_worker(Duration::from_millis(400)) });
        lb.retiring.push(Retiring { slot: 1, worker: fake_worker(Duration::from_millis(400)) });
        lb.retiring.push(Retiring { slot: 3, worker: fake_worker(Duration::from_millis(400)) });
        assert_eq!(lb.free_slot(), None, "a slot with a stopping thread is never handed out");

        let t0 = Instant::now();
        lb.stop_all(Duration::from_millis(50));
        assert!(t0.elapsed() < Duration::from_millis(250), "stop_all waited {:?} past its limit", t0.elapsed());
        assert!(!lb.retiring.is_empty(), "stragglers are detached, not joined");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !lb.retiring.is_empty() {
            assert!(Instant::now() < deadline, "threads never reaped");
            std::thread::sleep(Duration::from_millis(20));
            lb.set(&[]);
        }
        assert_eq!(lb.free_slot(), Some(0));
    }

    #[test]
    fn a_dead_stream_waits_before_it_is_restarted_and_is_not_counted_as_running() {
        let slots = Arc::new(RefSlots::new());
        let mut lb = Loopbacks::new(slots.clone());
        let finished = std::thread::spawn(|| Err::<(), _>("device unplugged".to_string()));
        while !finished.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        lb.active.push(Active {
            id: "dev".into(),
            slot: 1,
            worker: Some(Worker { stop: Arc::new(AtomicBool::new(false)), handle: finished }),
            started: Instant::now(),
        });
        slots.0[1].begin();
        assert_eq!(lb.running(), 1);

        lb.reap();
        assert!(lb.active[0].worker.is_none(), "the dead thread was joined");
        assert!(lb.logged.contains("dev"), "its failure was noted once");
        slots.0[1].end();
        assert_eq!(lb.running(), 0);

        // asking for the same device again right away does not respawn it
        lb.set(&["dev".to_string()]);
        assert!(lb.active.len() == 1 && lb.active[0].worker.is_none(), "restart must wait {RESTART_AFTER:?}");
    }

    fn db(ms: f32) -> f32 {
        10.0 * ms.max(1e-9).log10()
    }

    /// Manual hardware check (read-only, nothing is played or stored): 3 s of loopback on the default render
    /// device, first the raw WASAPI stream (frames per packet, longest gap between packets, level), then
    /// the real producer thread (blocks per second in the slot, which must stay near 100 even when nothing
    /// plays). Play some audio on the default output meanwhile.
    /// Run with `cargo test --release -p clarim-audio --lib -- --ignored loopback_default_render --nocapture`.
    #[test]
    #[ignore = "opens a loopback on the default render device for 6 s (read-only)"]
    fn loopback_default_render() {
        crate::init_com().expect("COM");
        let secs = 3.0f32;

        let stream = open_stream(Flow::Loopback, None).expect("open loopback stream");
        let capture = stream.client.get_audiocaptureclient().expect("capture client");
        stream.client.start_stream().expect("start");
        let (ch, rate) = (stream.channels, stream.rate as usize);
        let mut bytes = vec![0u8; rate * ch * 4];
        let (mut packets, mut frames, mut silent, mut power) = (0u32, 0usize, 0u32, 0.0f32);
        let (mut last, mut longest) = (Instant::now(), Duration::ZERO);
        let end = Instant::now() + Duration::from_secs_f32(secs);
        while Instant::now() < end {
            let _ = stream.event.wait_for_event(20);
            while let Ok(Some(n)) = capture.get_next_nbr_frames() {
                if n == 0 {
                    break;
                }
                let len = n as usize * ch * 4;
                let (_, flags) = capture.read_from_device(&mut bytes[..len]).expect("read");
                let now = Instant::now();
                (longest, last) = (longest.max(now - last), now);
                packets += 1;
                frames += n as usize;
                if flags.silent {
                    silent += 1;
                } else {
                    power += bytes[..len].chunks_exact(ch * 4).map(frame_power).sum::<f32>();
                }
            }
        }
        let _ = stream.client.stop_stream();
        println!(
            "raw: {ch} ch @ {rate} Hz, {packets} packets ({silent} silent) = {:.1} blocks/s, {:.0} frames/packet, longest gap {:?}, level {:.1} dBFS",
            frames as f32 / (rate / 100) as f32 / secs,
            frames as f32 / packets.max(1) as f32,
            longest,
            db(power / frames.max(1) as f32),
        );
        drop(capture);
        drop(stream);

        let id = crate::engine::default_render_id(false).expect("default render id");
        let slots = Arc::new(RefSlots::new());
        let mut loopbacks = Loopbacks::new(slots.clone());
        loopbacks.set(&[id]);
        let slot = &slots.0[0];
        let (mut seen, mut gap_from, mut longest) = (0u64, Instant::now(), Duration::ZERO);
        let end = Instant::now() + Duration::from_secs_f32(secs);
        while Instant::now() < end {
            let w = slot.written.load(Ordering::Acquire);
            if w != seen {
                let now = Instant::now();
                (longest, gap_from, seen) = (longest.max(now - gap_from), now, w);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        println!(
            "producer: {} streams running, {seen} blocks = {:.1} blocks/s, longest gap between updates {:?}, newest block {:.1} dBFS",
            loopbacks.running(),
            seen as f32 / secs,
            longest,
            db(f32::from_bits(slot.ring[(seen.saturating_sub(1) % REF_RING as u64) as usize].load(Ordering::Relaxed))),
        );
        loopbacks.stop_all(Duration::from_millis(500));
        assert!(seen > 0, "the producer published nothing");
    }

    /// Calibration aid for the echo detector (read-only: no cable, no engine, nothing is played or stored).
    /// The real default microphone goes through `MicChain` (Strong, auto level on) while an `EchoDetector`
    /// compares its output with the loopback of the default output; once a second it prints the score, best
    /// lag, leak level, speech level and AGC gain. Play speech from a non-call app on the speakers, then stay
    /// silent, and watch the numbers. `CLARIM_ECHO_SECS` sets the length (default 60).
    /// Run with `$env:CLARIM_ECHO_SECS=60; cargo test --release -p clarim-audio --lib -- --ignored echo_live --nocapture`.
    #[test]
    #[ignore = "opens the real microphone and a loopback for CLARIM_ECHO_SECS seconds (read-only)"]
    fn echo_live() {
        use crate::chain::{DspParams, MicChain};
        use crate::dsp::leak::EchoDetector;
        use clarim_core::{DenoiseMode, MicSettings};

        crate::init_com().expect("COM");
        let secs: u64 = std::env::var("CLARIM_ECHO_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
        let inputs = crate::list_inputs(&[]).expect("list inputs");
        let mic = crate::pick_real_mic(&inputs).expect("no real microphone");
        let render = crate::engine::default_render_id(false).expect("no default output device");
        let settings = MicSettings { denoise: DenoiseMode::Strong, auto_level: true, ..MicSettings::default() };
        let mut chain = MicChain::new(&DspParams::from_settings(&settings));

        let slots = Arc::new(RefSlots::new());
        let mut loopbacks = Loopbacks::new(slots.clone());
        loopbacks.set(&[render]);
        let (mut reader, mut det) = (RefReader::new(), EchoDetector::new());

        let stream = open_stream(Flow::Capture, Some(&mic.id)).expect("open the microphone");
        let capture = stream.client.get_audiocaptureclient().expect("capture client");
        stream.client.start_stream().expect("start");
        println!("microphone '{}', {secs} s: play speech on the speakers, then stay silent", mic.name);

        let ch = stream.channels;
        let (mut bytes, mut pending) = (vec![0u8; 48_000 * ch * 4], Vec::<f32>::new());
        let (mut frames, mut ref_db) = (0u32, f32::NAN);
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            let _ = stream.event.wait_for_event(100);
            while let Ok(Some(n)) = capture.get_next_nbr_frames() {
                if n == 0 {
                    break;
                }
                let len = n as usize * ch * 4;
                let (_, flags) = capture.read_from_device(&mut bytes[..len]).expect("read");
                pending.extend(bytes[..len].chunks_exact(ch * 4).map(|f| {
                    let v: f32 = f.chunks_exact(4).map(|s| f32::from_le_bytes([s[0], s[1], s[2], s[3]])).filter(|v| v.is_finite()).sum();
                    if flags.silent { 0.0 } else { v / ch as f32 }
                }));
                while pending.len() >= FRAME {
                    let mut frame = [0f32; FRAME];
                    frame.copy_from_slice(&pending[..FRAME]);
                    pending.drain(..FRAME);
                    let info = chain.process(&mut frame);
                    let out_ms = frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32;
                    match reader.next(&slots.0[0]) {
                        RefStep::Reset => det.reset(),
                        RefStep::Block(ms) => {
                            ref_db = db(ms);
                            det.set_denoise_delay_ms(chain.latency_ms());
                            det.push(ms, out_ms, info.voice_db);
                        }
                        RefStep::Off | RefStep::Missing => {}
                    }
                    frames += 1;
                    if frames.is_multiple_of(100) {
                        println!(
                            "{:3} s  score {:.2}  lag {:3.0} ms  leak {:>7}  voice {:>7}  agc {:+5.1} dB  ref {:6.1} dB  idle {} frames",
                            frames / 100,
                            det.score(),
                            det.best_lag_ms(),
                            det.level_db().map_or("-".into(), |v| format!("{v:.1}")),
                            info.voice_db.map_or("-".into(), |v| format!("{v:.1}")),
                            info.agc_gain_db,
                            ref_db,
                            det.ref_inactive_frames(),
                        );
                    }
                }
            }
        }
        let _ = stream.client.stop_stream();
        loopbacks.stop_all(Duration::from_millis(500));
    }

    #[test]
    fn blocks_are_the_mean_power_of_ten_milliseconds() {
        let mut b = Blocker::new(480);
        let stereo = |l: f32, r: f32| [l.to_le_bytes(), r.to_le_bytes()].concat();
        assert!((frame_power(&stereo(0.6, 0.0)) - 0.18).abs() < 1e-6, "(L^2 + R^2) / 2, not the square of the sum");
        assert!((frame_power(&stereo(0.5, -0.5)) - 0.25).abs() < 1e-6, "opposite phases do not cancel");
        assert_eq!(frame_power(&stereo(f32::NAN, f32::INFINITY)), 0.0, "non-finite samples count as zero");
        let out: Vec<f32> = (0..960).filter_map(|_| b.add(0.25)).collect();
        assert_eq!(out, vec![0.25, 0.25], "one block per 480 frames");
    }
}
