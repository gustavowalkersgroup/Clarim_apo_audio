//! Output-equalizer writer: the service side of the Equalizer APO integration, plus the client helpers
//! the per-user agent uses to reach it.
//!
//! Program Files is admin-only, so the agent asks this LocalSystem service to write
//! `<Program Files>\EqualizerAPO\config\clarim-eq.txt` over the named pipe `\\.\pipe\ClarimEqApo`.
//! One request per connection: a JSON line of at most 8 KiB in, a JSON line out. The request carries
//! endpoint GUIDs and gains only. The file text always comes from `clarim_core::render_eqapo_file`
//! (which refuses anything that is not a braced GUID plus numbers, so no client text reaches a file
//! the Windows audio engine loads) and the folder always comes from the shell, never from the request.
//!
//! The pipe runs on overlapped I/O so every connect, read and write can time out and the service can
//! stop at once: a local client that connects and stays silent costs the server a few seconds, not a hang.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clarim_core::{is_stock_apo_config, render_eqapo_file, OutputEq, EQAPO_INCLUDE_LINE};

use crate::apo_service::ApoServiceStatus;
use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_BROKEN_PIPE, ERROR_BUFFER_OVERFLOW, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SEM_TIMEOUT,
    ERROR_TIMEOUT, HANDLE, HLOCAL, WAIT_EVENT, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_NONE,
    FILE_SHARE_READ, FILE_WRITE_DATA, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE,
};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

pub(crate) const PIPE_NAME: &str = r"\\.\pipe\ClarimEqApo";
/// Interactive users get exactly what a client needs: read and write data, read and write attributes,
/// read control, synchronize (0x120183). Not GENERIC_WRITE, which includes FILE_CREATE_PIPE_INSTANCE
/// (= FILE_APPEND_DATA): with it any user could add an instance of this pipe and answer the agent in the
/// service's place (tried: a medium-integrity user created one). SYSTEM and Administrators may do everything.
pub(crate) const PIPE_SDDL: &str = "D:(A;;0x120183;;;IU)(A;;GA;;;SY)(A;;GA;;;BA)";
/// What `open_client` asks for, spelled out for the same reason: GENERIC_WRITE would ask for FILE_APPEND_DATA too.
const CLIENT_ACCESS: u32 = FILE_READ_DATA.0 | FILE_WRITE_DATA.0 | FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE.0;

/// The one file Clarim owns in the Equalizer APO config folder, and the line that pulls it into `config.txt`.
const EQ_FILE: &str = "clarim-eq.txt";
const INCLUDE_LINE: &str = EQAPO_INCLUDE_LINE;
const BACKUP_FILE: &str = "config.txt.clarim-backup";
/// A `config.txt` bigger than this is not read: the folder is writable by every user (Equalizer APO's
/// installer grants `Users` full control), and the service reads and copies the file as LocalSystem.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// A request line, newline excluded, may not be longer than this.
const MAX_LINE: usize = 8 * 1024;
/// A reply line may be longer: the status lists every playback endpoint.
const MAX_REPLY: usize = 64 * 1024;
const PIPE_BUFFER: u32 = 8 * 1024;
/// The server gives a client this long to send its line, and to take the reply.
const SERVER_IO_TIMEOUT: Duration = Duration::from_secs(3);
const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(3);
/// Covers the service's file writes (a slow antivirus scan included).
const CLIENT_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------------------------

/// `<Program Files>\EqualizerAPO\config`, resolved by the shell. Fails when Equalizer APO is not installed.
fn config_dir() -> Result<PathBuf> {
    let root = super::known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramFiles)
        .context("Não foi possível localizar a pasta Arquivos de Programas.")?
        .join("EqualizerAPO");
    if !root.join("EqualizerAPO.dll").is_file() {
        bail!("Equalizer APO não está instalado");
    }
    Ok(root.join("config"))
}

/// What the pipe server applies in production: render the file and write it into the real config folder.
pub(crate) fn apply_installed(entries: &[(String, OutputEq)]) -> Result<()> {
    apply_to_dir(&config_dir()?, entries)
}

/// Renders the equalizer file, writes it into `dir` atomically and makes sure `dir\config.txt` includes it.
/// A bad entry fails before anything is touched.
fn apply_to_dir(dir: &Path, entries: &[(String, OutputEq)]) -> Result<()> {
    let text = render_eqapo_file(entries)?;
    if !dir.is_dir() {
        bail!("A pasta de configuração do Equalizer APO não foi encontrada.");
    }
    // The file first: an Include must never point at nothing.
    write_atomic(&dir.join(EQ_FILE), text.as_bytes())
        .context("Não foi possível gravar o arquivo do equalizador.")?;
    ensure_include(&dir.join("config.txt"))
}

fn is_include_line(line: &str) -> bool {
    line.trim_start_matches('\u{feff}').trim().eq_ignore_ascii_case(INCLUDE_LINE)
}

// The config folder is writable by every user, and the service touches it as LocalSystem. So no file in it
// is opened through a link: every open below asks for the link itself (`FILE_FLAG_OPEN_REPARSE_POINT`),
// and what is read must be a plain file of sane size. A planted symlink then counts as "already there"
// (`create_new`) or is refused, and never redirects a write to another path.

/// Opens `path` with `opts`, never following a link, and refuses (`InvalidData`) anything that is not a
/// plain file of at most `MAX_CONFIG_BYTES`.
fn open_plain(opts: &mut fs::OpenOptions, path: &Path) -> std::io::Result<fs::File> {
    let file = opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0).open(path)?;
    let meta = file.metadata()?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 || !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not a plain file of reasonable size"));
    }
    Ok(file)
}

fn read_plain(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_plain(fs::OpenOptions::new().read(true), path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A refused file (`open_plain`) gets its own PT-BR text; any other failure gets `what`.
fn config_error(e: std::io::Error, what: &'static str) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::InvalidData {
        anyhow!("O config.txt do Equalizer APO não é um arquivo comum (ou é grande demais); não foi alterado.")
    } else {
        anyhow::Error::new(e).context(what)
    }
}

/// Creates `path` holding `data` unless something is there already: never replaced, never through a link
/// (anything at that name, a planted link included, counts as "already there").
fn create_if_absent(path: &Path, data: &[u8]) -> std::io::Result<()> {
    match fs::OpenOptions::new().write(true).create_new(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0).open(path) {
        Ok(mut file) => file.write_all(data),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writes the first copy of the original `config.txt` (`bytes`, already read through `open_plain`) with
/// `create_if_absent`: the first original is never overwritten.
fn write_backup(backup: &Path, bytes: &[u8]) -> Result<()> {
    create_if_absent(backup, bytes).context("Não foi possível guardar uma cópia do config.txt original.")
}

/// Equalizer APO's own stock `config.txt` (`is_stock_apo_config`) lowers every attached device by 6 dB and
/// adds a bass boost, which would make Clarim's "Neutro" not neutral: it becomes just our Include line, and the
/// original goes to `config.txt.clarim-backup` first (never overwritten). Rewritten in place (not renamed over),
/// so the file keeps its permissions. `Ok(false)`: the file is not the stock one (any more), nothing was touched.
/// One handle does the check and the rewrite, and it denies other writers, so a customised file saved by the
/// user in between cannot be truncated; the backup holds exactly the bytes that were checked.
fn replace_stock_config(config: &Path) -> Result<bool> {
    const WHAT: &str = "Não foi possível alterar o config.txt do Equalizer APO.";
    let mut file = open_plain(fs::OpenOptions::new().read(true).write(true).share_mode(FILE_SHARE_READ.0), config)
        .map_err(|e| config_error(e, WHAT))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| config_error(e, WHAT))?;
    if !std::str::from_utf8(&bytes).is_ok_and(is_stock_apo_config) {
        return Ok(false);
    }
    write_backup(&config.with_file_name(BACKUP_FILE), &bytes)?;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.set_len(0))
        .and_then(|()| file.write_all(format!("{INCLUDE_LINE}\n").as_bytes()))
        .map_err(|e| config_error(e, WHAT))?;
    Ok(true)
}

/// Appends `Include: clarim-eq.txt` to `config` unless a line has it already (a stock APO `config.txt` is
/// replaced by that line instead, see `replace_stock_config`). The first time it changes an existing file,
/// the original is copied to `config.txt.clarim-backup` (never overwritten).
/// Other lines are never rewritten: the append is in place, so the file keeps its permissions.
fn ensure_include(config: &Path) -> Result<()> {
    let bytes = match read_plain(config) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(config_error(e, "Não foi possível ler o config.txt do Equalizer APO.")),
    };
    // Appending UTF-8 to a UTF-16 file (Notepad can save one) would corrupt it: refuse.
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("O config.txt do Equalizer APO não está em UTF-8; não foi alterado."))?;
    if is_stock_apo_config(text) {
        // before the "has the line" check: stock + our line is stock too
        match replace_stock_config(config) {
            Ok(true) => return Ok(()),
            Ok(false) => {} // saved by the user since it was read: handled below like the customised file it is
            // Stock + our line already works: a file that cannot be rewritten (read-only) only keeps its -6 dB,
            // which is a warning, not a failed EQ write.
            Err(e) if text.lines().any(is_include_line) => {
                log::warn!("could not neutralize Equalizer APO's stock config.txt {}: {e:#}", config.display());
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    if text.lines().any(is_include_line) {
        return Ok(());
    }
    if !bytes.is_empty() {
        write_backup(&config.with_file_name(BACKUP_FILE), &bytes)?;
    }
    let line = format!("{}{INCLUDE_LINE}\n", if bytes.is_empty() { "" } else { "\n" });
    // Opened again only now, for writing: a read-only config.txt that already has the line keeps working.
    open_plain(fs::OpenOptions::new().append(true).create(true), config)
        .and_then(|mut f| f.write_all(line.as_bytes()))
        .map_err(|e| config_error(e, "Não foi possível alterar o config.txt do Equalizer APO."))
}

/// `path.tmp` fully written and synced, then renamed over `path`. Equalizer APO reads the file when it
/// changes and may hold it for a moment, so a refused rename is retried briefly. The temp name is deleted
/// first (a leftover, or a planted link: `remove_file` removes the link, not its target) and created new.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(&tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    file.write_all(data).and_then(|()| file.sync_all()).with_context(|| format!("cannot write {}", tmp.display()))?;
    drop(file);
    let mut renamed = fs::rename(&tmp, path);
    for _ in 0..3 {
        if renamed.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        renamed = fs::rename(&tmp, path);
    }
    renamed.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        anyhow::Error::new(e).context(format!("cannot replace {}", path.display()))
    })
}

/// Called when Clarim attaches Equalizer APO to a device or finishes installing it, so the stock `config.txt`
/// (-6 dB, bass boost) stops applying before the user moves a slider. Best effort: a failure is a warning, never
/// a failed attach or install. Does nothing when APO is not installed or `config.txt` is not the stock one.
pub(crate) fn neutralize_stock_config() {
    let Ok(dir) = config_dir() else { return };
    match neutralize_in_dir(&dir) {
        Ok(true) => log::info!("replaced Equalizer APO's stock config.txt in {} (it lowers the volume by 6 dB); the original is in {BACKUP_FILE}", dir.display()),
        Ok(false) => {}
        Err(e) => log::warn!("could not neutralize Equalizer APO's stock config.txt in {}: {e:#}", dir.display()),
    }
}

/// True when `dir\config.txt` was the stock file and now holds only our Include line. A missing, customised or
/// non-UTF-8 `config.txt` is left alone. Our file is created (no EQ) when absent: an Include must never point at nothing.
fn neutralize_in_dir(dir: &Path) -> Result<bool> {
    let config = dir.join("config.txt");
    let bytes = match read_plain(&config) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(config_error(e, "Não foi possível ler o config.txt do Equalizer APO.")),
    };
    if !std::str::from_utf8(&bytes).is_ok_and(is_stock_apo_config) {
        return Ok(false);
    }
    // create_if_absent, not write_atomic: the pipe thread may be writing the real EQ right now, and an existing
    // file (the real EQ) must never be replaced by the empty one.
    create_if_absent(&dir.join(EQ_FILE), render_eqapo_file(&[])?.as_bytes()).context("Não foi possível gravar o arquivo do equalizador.")?;
    replace_stock_config(&config)
}

/// Uninstall cleanup, best effort: takes our Include line out of `config.txt` and deletes our file.
/// Does nothing when Equalizer APO is not installed.
pub(crate) fn remove() {
    let Ok(dir) = config_dir() else { return };
    match remove_from_dir(&dir) {
        Ok(()) => log::info!("removed the Clarim equalizer from {}", dir.display()),
        Err(e) => log::warn!("could not remove the Clarim equalizer from {}: {e:#}", dir.display()),
    }
}

fn remove_from_dir(dir: &Path) -> Result<()> {
    let config = dir.join("config.txt");
    let stripped = match read_plain(&config).and_then(|b| String::from_utf8(b).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))) {
        Ok(text) if text.lines().any(is_include_line) => {
            let kept: String = text.split_inclusive('\n').filter(|l| !is_include_line(l)).collect();
            // Opened again only now, for writing, and through the same checks (no link, no huge file).
            open_plain(fs::OpenOptions::new().write(true), &config)
                .and_then(|mut f| f.set_len(0).and_then(|()| f.write_all(kept.as_bytes())))
                .with_context(|| format!("cannot update {}", config.display()))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", config.display())),
    };
    let file = dir.join(EQ_FILE);
    let deleted = match fs::remove_file(&file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("cannot delete {}", file.display()))
        }
        _ => Ok(()),
    };
    stripped.and(deleted)
}

// ---------------------------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Entry {
    guid: String,
    #[serde(flatten)]
    eq: OutputEq,
}

#[derive(Serialize, Deserialize)]
struct Request {
    entries: Vec<Entry>,
}

/// One reply line: `{"ok":true}`, `{"ok":false,"error":"..."}` (PT-BR, ready for the dashboard), and for
/// the `status` command `{"ok":true,"status":{...}}`.
#[derive(Serialize, Deserialize)]
pub(crate) struct Reply {
    pub(crate) ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<ApoServiceStatus>,
}

impl Reply {
    pub(crate) fn ok() -> Reply {
        Reply { ok: true, error: None, status: None }
    }

    pub(crate) fn failed(error: &str) -> Reply {
        Reply { ok: false, error: Some(error.to_string()), status: None }
    }

    fn to_line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).unwrap_or_else(|_| br#"{"ok":false}"#.to_vec());
        line.push(b'\n');
        line
    }
}

fn parse_request(line: &[u8]) -> Result<Vec<(String, OutputEq)>> {
    // The serde message is not forwarded: it can quote the client's text.
    let request: Request = serde_json::from_slice(line).map_err(|_| anyhow!("Pedido inválido."))?;
    Ok(request.entries.into_iter().map(|e| (e.guid, e.eq)).collect())
}

/// Parses and applies one write-config request line (`{"entries":[...]}`). Logs one line, never the content.
pub(crate) fn process<F>(line: &[u8], apply: &F) -> Reply
where
    F: Fn(&[(String, OutputEq)]) -> Result<()> + ?Sized,
{
    let result = parse_request(line).and_then(|entries| apply(&entries).map(|()| entries.len()));
    match result {
        Ok(devices) => {
            log::info!("output EQ request applied ({devices} device(s))");
            Reply::ok()
        }
        Err(e) => {
            log::warn!("output EQ request refused: {e:#}");
            // The outermost message is the PT-BR one meant for the user; the chain went to the log.
            Reply::failed(&e.to_string())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Overlapped pipe I/O
// ---------------------------------------------------------------------------------------------

fn raw(h: &OwnedHandle) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The Win32 error code inside a windows-rs error (HRESULT_FROM_WIN32 form).
fn code_of(e: &windows::core::Error) -> u32 {
    let hr = e.code().0 as u32;
    if hr >> 16 == 0x8007 {
        hr & 0xFFFF
    } else {
        hr
    }
}

fn new_event() -> Result<OwnedHandle> {
    // SAFETY: plain create call: an unnamed manual-reset event with default security.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.context("cannot create an event")?;
    // SAFETY: CreateEventW succeeded, so the handle is new and only we own it.
    Ok(unsafe { OwnedHandle::from_raw_handle(event.0) })
}

/// Runs one overlapped call on `pipe` (`start` issues it with the OVERLAPPED it is given) and waits for
/// it: at most `timeout` (forever for `None`) and not past `stop`. A call that did not finish is
/// canceled and waited out before returning, so `start`'s buffers are never used by the kernel after
/// this returns. `Ok` = bytes transferred; `Err` = Win32 code, `ERROR_TIMEOUT` or `ERROR_OPERATION_ABORTED`
/// (stop) when it did not finish in time.
fn run_overlapped(
    pipe: HANDLE,
    event: HANDLE,
    stop: Option<HANDLE>,
    timeout: Option<Duration>,
    start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
) -> Result<u32, u32> {
    let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
    let ov = &raw mut ov; // the kernel writes through this while the call is pending
    // SAFETY: `event` is a valid event handle owned by the caller.
    let _ = unsafe { ResetEvent(event) };
    match start(ov) {
        Ok(()) => {}
        Err(e) if code_of(&e) == ERROR_IO_PENDING.0 => {}
        Err(e) => return Err(code_of(&e)),
    }

    let handles = [event, stop.unwrap_or(event)];
    let handles = &handles[..if stop.is_some() { 2 } else { 1 }];
    let wait_ms = timeout.map_or(INFINITE, |d| u32::try_from(d.as_millis()).unwrap_or(INFINITE - 1).min(INFINITE - 1));
    // SAFETY: every handle in the slice is valid for the whole call.
    let woke = unsafe { WaitForMultipleObjects(handles, false, wait_ms) };
    let finished = woke == WAIT_OBJECT_0;
    if !finished {
        // SAFETY: cancels only the call started above on `pipe`; `ov` is still alive.
        let _ = unsafe { CancelIoEx(pipe, Some(ov.cast_const())) };
    }
    let mut transferred = 0u32;
    // SAFETY: `ov` is the structure the call above used. Waiting (only after a cancel) ends promptly,
    // and returning is safe afterwards because the kernel is done with `ov` and the buffers.
    let result = unsafe { GetOverlappedResult(pipe, ov.cast_const(), &mut transferred, !finished) };
    match result {
        Ok(()) => Ok(transferred),
        Err(e) if !finished && code_of(&e) == ERROR_OPERATION_ABORTED.0 => {
            Err(if woke == WAIT_EVENT(WAIT_OBJECT_0.0 + 1) { ERROR_OPERATION_ABORTED.0 } else { ERROR_TIMEOUT.0 })
        }
        Err(e) => Err(code_of(&e)),
    }
}

/// Reads one `\n`-terminated line from `pipe` (newline not included). Data followed by EOF counts as a
/// line; EOF with nothing is `ERROR_BROKEN_PIPE`; more than `max_len` bytes is `ERROR_BUFFER_OVERFLOW`.
fn read_line(pipe: HANDLE, event: HANDLE, stop: Option<HANDLE>, timeout: Duration, max_len: usize) -> Result<Vec<u8>, u32> {
    let deadline = Instant::now() + timeout;
    let mut line = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ERROR_TIMEOUT.0);
        }
        let read = run_overlapped(pipe, event, stop, Some(left), |ov| {
            // SAFETY: `chunk` and `ov` stay alive until the call ends: run_overlapped finishes or cancels it first.
            unsafe { ReadFile(pipe, Some(&mut chunk[..]), None, Some(ov)) }
        });
        let got = match read {
            Ok(n) => n as usize,
            Err(code) if code == ERROR_BROKEN_PIPE.0 && !line.is_empty() => return Ok(line),
            Err(code) => return Err(code),
        };
        let end = chunk[..got].iter().position(|&b| b == b'\n');
        line.extend_from_slice(&chunk[..end.unwrap_or(got)]);
        if line.len() > max_len {
            return Err(ERROR_BUFFER_OVERFLOW.0);
        }
        if end.is_some() {
            return Ok(line);
        }
    }
}

fn write_all(pipe: HANDLE, event: HANDLE, stop: Option<HANDLE>, timeout: Duration, data: &[u8]) -> Result<(), u32> {
    let deadline = Instant::now() + timeout;
    let mut sent = 0;
    while sent < data.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ERROR_TIMEOUT.0);
        }
        let n = run_overlapped(pipe, event, stop, Some(left), |ov| {
            // SAFETY: `data` and `ov` stay alive until the call ends: run_overlapped finishes or cancels it first.
            unsafe { WriteFile(pipe, Some(&data[sent..]), None, Some(ov)) }
        })?;
        sent += n as usize;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Server (runs inside the service)
// ---------------------------------------------------------------------------------------------

/// A security descriptor built from an SDDL string (LocalAlloc memory, freed on drop).
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the pointer owns a LocalAlloc block that nothing mutates after creation; it is only read by
// CreateNamedPipeW and freed once, on drop.
unsafe impl Send for SecurityDescriptor {}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> Result<Self> {
        let sddl = wide(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is NUL-terminated and outlives the call; `descriptor` is a valid out pointer.
        // On success the descriptor is LocalAlloc memory that `Drop` frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .context("invalid pipe security descriptor")?;
        Ok(SecurityDescriptor(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful ConvertStringSecurityDescriptorToSecurityDescriptorW
        // and is freed exactly once.
        unsafe {
            LocalFree(HLOCAL(self.0 .0));
        }
    }
}

/// Makes pipe instances: the first one must be the first of its name (nobody squatted it), all carry the SDDL.
struct Listener {
    name: Vec<u16>,
    descriptor: SecurityDescriptor,
    first: bool,
}

impl Listener {
    fn instance(&mut self) -> Result<OwnedHandle> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor.0 .0,
            bInheritHandle: false.into(),
        };
        let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if self.first {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: `name` is NUL-terminated and `attributes` (with the descriptor it points to) lives
        // through the call; the call copies both.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(self.name.as_ptr()),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                Some(&attributes),
            )
        };
        if pipe.is_invalid() {
            return Err(windows::core::Error::from_win32()).context("cannot create the pipe");
        }
        self.first = false;
        // SAFETY: CreateNamedPipeW succeeded, so the handle is new and only we own it.
        Ok(unsafe { OwnedHandle::from_raw_handle(pipe.0) })
    }
}

/// The pipe server thread. Dropping it stops the thread (an in-flight request finishes first).
pub(crate) struct Server {
    stop: Arc<OwnedHandle>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Creates the pipe (so a failure is reported here and clients can connect as soon as this returns)
    /// and serves it on a thread. `apply` answers one request line; production passes `apo_proto::handle`
    /// bound to the service.
    pub(crate) fn start<F>(pipe_name: &str, sddl: &str, apply: F) -> Result<Server>
    where
        F: Fn(&[u8]) -> Reply + Send + 'static,
    {
        let mut listener =
            Listener { name: wide(pipe_name), descriptor: SecurityDescriptor::from_sddl(sddl)?, first: true };
        let first = listener.instance()?;
        let stop = Arc::new(new_event()?);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("eqapo-pipe".into())
            .spawn(move || serve(listener, first, &thread_stop, &apply))
            .context("cannot start the pipe thread")?;
        log::info!("output EQ pipe listening on {pipe_name}");
        Ok(Server { stop, thread: Some(thread) })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // SAFETY: `stop` is a valid event handle, owned by this struct.
            let _ = unsafe { SetEvent(raw(&self.stop)) };
            let _ = thread.join();
        }
    }
}

/// True when `stop` was signaled within `d`.
fn stopped_within(stop: HANDLE, d: Duration) -> bool {
    // SAFETY: `stop` is a valid event handle for the whole call.
    let woke = unsafe { WaitForSingleObject(stop, u32::try_from(d.as_millis()).unwrap_or(INFINITE - 1)) };
    woke == WAIT_OBJECT_0
}

// ponytail: requests are served one at a time; each is a small file write, and the next instance is
// already listening while one is served, so a second client queues instead of seeing no pipe.
fn serve<F>(mut listener: Listener, first: OwnedHandle, stop: &OwnedHandle, apply: &F)
where
    F: Fn(&[u8]) -> Reply,
{
    let event = match new_event() {
        Ok(event) => event,
        Err(e) => {
            log::error!("output EQ pipe stopped: {e:#}");
            return;
        }
    };
    let (event, stop) = (raw(&event), raw(stop));
    let mut pending = Some(first);
    loop {
        let conn = match pending.take() {
            Some(conn) => conn,
            None => match listener.instance() {
                Ok(conn) => conn,
                Err(e) => {
                    log::warn!("output EQ pipe: {e:#}");
                    if stopped_within(stop, Duration::from_secs(1)) {
                        return;
                    }
                    continue;
                }
            },
        };
        match connect(raw(&conn), event, stop) {
            Ok(()) => {}
            Err(code) if code == ERROR_OPERATION_ABORTED.0 => return,
            Err(code) => {
                log::warn!("output EQ pipe: connect failed (Win32 error {code})");
                if stopped_within(stop, Duration::from_millis(200)) {
                    return;
                }
                continue;
            }
        }
        pending = listener
            .instance()
            .map_err(|e| log::warn!("output EQ pipe: no next instance, will retry: {e:#}"))
            .ok();
        serve_connection(raw(&conn), event, stop, apply);
        // `conn` closes here; the client has its reply buffered (it was written before).
    }
}

/// Waits for a client on a fresh pipe instance, or for `stop`.
fn connect(pipe: HANDLE, event: HANDLE, stop: HANDLE) -> Result<(), u32> {
    // SAFETY: `ov` is kept alive by run_overlapped until the call ends or is canceled.
    match run_overlapped(pipe, event, Some(stop), None, |ov| unsafe { ConnectNamedPipe(pipe, Some(ov)) }) {
        Ok(_) => Ok(()),
        // The client got in between creating the instance and this call.
        Err(code) if code == ERROR_PIPE_CONNECTED.0 => Ok(()),
        Err(code) => Err(code),
    }
}

fn serve_connection<F>(pipe: HANDLE, event: HANDLE, stop: HANDLE, apply: &F)
where
    F: Fn(&[u8]) -> Reply,
{
    let reply = match read_line(pipe, event, Some(stop), SERVER_IO_TIMEOUT, MAX_LINE) {
        Ok(line) => apply(&line),
        Err(code) if code == ERROR_OPERATION_ABORTED.0 => return,
        Err(code) if code == ERROR_BROKEN_PIPE.0 => {
            log::debug!("output EQ pipe: a client left without a request");
            return;
        }
        Err(code) => {
            log::warn!("output EQ pipe: no request read (Win32 error {code})");
            Reply::failed(if code == ERROR_BUFFER_OVERFLOW.0 {
                "Pedido grande demais."
            } else {
                "O pedido não chegou a tempo."
            })
        }
    };
    if let Err(code) = write_all(pipe, event, Some(stop), SERVER_IO_TIMEOUT, &reply.to_line()) {
        log::warn!("output EQ pipe: reply not delivered (Win32 error {code})");
    }
}

// ---------------------------------------------------------------------------------------------
// Client (used by the agent)
// ---------------------------------------------------------------------------------------------

/// True when the service's pipe exists: an instance is listening, or all of them are busy right now.
pub fn eqapo_service_available() -> bool {
    pipe_exists(PIPE_NAME)
}

fn pipe_exists(name: &str) -> bool {
    let name = wide(name);
    // SAFETY: `name` is NUL-terminated and outlives the call, which only reads it.
    if unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1) }.as_bool() {
        return true;
    }
    // SAFETY: reads this thread's last-error value, set by the call above.
    unsafe { GetLastError() == ERROR_SEM_TIMEOUT }
}

/// Asks the Clarim service to write the output EQ for `entries` (`(endpoint GUID "{...}", EQ)`; the
/// complete list, not a change: the file is rewritten from it). Errors are PT-BR, ready for the dashboard.
pub fn eqapo_send(entries: &[(String, OutputEq)]) -> Result<()> {
    send_via(PIPE_NAME, entries)
}

#[cfg(test)]
pub(crate) fn eqapo_send_via_for_tests(pipe_name: &str, entries: &[(String, OutputEq)]) -> Result<()> {
    send_via(pipe_name, entries)
}

fn send_via(pipe_name: &str, entries: &[(String, OutputEq)]) -> Result<()> {
    let request = Request { entries: entries.iter().map(|(guid, eq)| Entry { guid: guid.clone(), eq: *eq }).collect() };
    expect_ok(exchange(pipe_name, &request)?)
}

/// `Ok` when the service said so, else its PT-BR error.
pub(crate) fn expect_ok(reply: Reply) -> Result<()> {
    if reply.ok {
        Ok(())
    } else {
        Err(anyhow!(reply.error.unwrap_or_else(|| "O serviço do Clarim recusou o pedido.".to_string())))
    }
}

/// One request line out, one reply line in. Fails (PT-BR) when the service cannot be reached, answers too
/// late or answers nonsense; a refusal by the service is an `Ok(Reply { ok: false, .. })`.
pub(crate) fn exchange(pipe_name: &str, request: &impl Serialize) -> Result<Reply> {
    let mut line = serde_json::to_vec(request).context("Pedido inválido.")?;
    if line.len() > MAX_LINE {
        bail!("Equalizador: pedido grande demais.");
    }
    line.push(b'\n');

    let pipe = open_client(pipe_name)?;
    let event = new_event().context("Falha na comunicação com o serviço do Clarim.")?;
    let (pipe, event) = (raw(&pipe), raw(&event));
    write_all(pipe, event, None, CLIENT_WRITE_TIMEOUT, &line).map_err(pipe_failure)?;
    let reply = read_line(pipe, event, None, CLIENT_REPLY_TIMEOUT, MAX_REPLY).map_err(pipe_failure)?;
    serde_json::from_slice(&reply).map_err(|_| anyhow!("Resposta inválida do serviço do Clarim."))
}

fn pipe_failure(code: u32) -> anyhow::Error {
    log::debug!("output EQ pipe client: Win32 error {code}");
    if code == ERROR_TIMEOUT.0 {
        anyhow!("O serviço do Clarim não respondeu a tempo.")
    } else {
        anyhow!("Falha na comunicação com o serviço do Clarim.")
    }
}

/// Opens the pipe for reading and writing, waiting up to `CLIENT_CONNECT_TIMEOUT` while it is busy.
/// The server may not impersonate this process (identification level only).
fn open_client(name: &str) -> Result<OwnedHandle> {
    let wname = wide(name);
    let deadline = Instant::now() + CLIENT_CONNECT_TIMEOUT;
    loop {
        // SAFETY: `wname` is NUL-terminated and outlives the call; no security attributes or template file.
        let opened = unsafe {
            CreateFileW(
                PCWSTR(wname.as_ptr()),
                CLIENT_ACCESS,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                HANDLE::default(),
            )
        };
        match opened {
            // SAFETY: CreateFileW succeeded, so the handle is new and only we own it.
            Ok(pipe) => return Ok(unsafe { OwnedHandle::from_raw_handle(pipe.0) }),
            Err(e) if code_of(&e) == ERROR_PIPE_BUSY.0 => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    bail!("O serviço do Clarim está ocupado.");
                }
                let slice = u32::try_from(left.as_millis().min(500)).unwrap_or(500);
                // SAFETY: `wname` as above. A refused wait is fine: the next CreateFileW reports why.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(wname.as_ptr()), slice) };
            }
            Err(e) if code_of(&e) == ERROR_FILE_NOT_FOUND.0 => bail!("O serviço do Clarim não está em execução."),
            Err(e) => {
                log::debug!("output EQ pipe client: cannot open the pipe: {e}");
                bail!("Não foi possível falar com o serviço do Clarim.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    const GUID: &str = "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}";
    const OTHER_GUID: &str = "{0A1B2C3D-0000-4000-8000-0123456789AB}";
    /// Everyone may use the test pipe: tests must not depend on being in an interactive session.
    const TEST_SDDL: &str = "D:(A;;GA;;;WD)";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("clarim-svc-eqapo-{}-{tag}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp dir");
            TempDir(dir)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.file(name)).unwrap_or_default()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn bass_boost(guid: &str) -> (String, OutputEq) {
        (guid.to_string(), OutputEq { bass_db: 5.0, mid_db: 0.0, treble_db: 0.0 })
    }

    fn unique_pipe(tag: &str) -> String {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        format!(r"\\.\pipe\ClarimEqApoTest-{}-{tag}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn fresh_dir_gets_the_file_and_a_new_config_without_backup() {
        let dir = TempDir::new("fresh");
        let entries = [bass_boost(GUID)];
        apply_to_dir(&dir.0, &entries).unwrap();
        assert_eq!(dir.read(EQ_FILE), render_eqapo_file(&entries).unwrap());
        assert_eq!(dir.read("config.txt"), "Include: clarim-eq.txt\n");
        assert!(!dir.file(BACKUP_FILE).exists(), "nothing to back up");
        assert!(!dir.file("clarim-eq.txt.tmp").exists(), "the temp file is renamed away");
    }

    #[test]
    fn existing_config_keeps_its_lines_is_backed_up_once_and_gets_one_include() {
        let dir = TempDir::new("existing");
        let original = "Preamp: -3 dB\r\nChannel: all"; // CRLF, no final newline
        fs::write(dir.file("config.txt"), original).unwrap();

        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        let after_first = dir.read("config.txt");
        assert_eq!(after_first, format!("{original}\nInclude: clarim-eq.txt\n"));
        assert_eq!(dir.read(BACKUP_FILE), original);

        // idempotent: another write changes the Clarim file, not config.txt
        apply_to_dir(&dir.0, &[bass_boost(OTHER_GUID)]).unwrap();
        assert_eq!(dir.read("config.txt"), after_first);
        assert!(dir.read(EQ_FILE).contains(OTHER_GUID));

        // the user removes the line and edits the file; the next write adds the line again,
        // and the backup still holds the very first original
        fs::write(dir.file("config.txt"), "Preamp: -9 dB\n").unwrap();
        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        assert_eq!(dir.read("config.txt"), "Preamp: -9 dB\n\nInclude: clarim-eq.txt\n");
        assert_eq!(dir.read(BACKUP_FILE), original, "the backup is made once");
    }

    /// The 157 bytes Equalizer APO 1.4.2's installer ships as `config.txt` (CRLF, no final newline).
    const STOCK: &str = "Preamp: -6 dB\r\nInclude: example.txt\r\nGraphicEQ: 25 0; 40 0; 63 0; 100 0; 160 0; 250 0; 400 0; 630 0; 1000 0; 1600 0; 2500 0; 4000 0; 6300 0; 10000 0; 16000 0";

    #[test]
    fn the_stock_apo_config_becomes_just_our_include_line_and_the_backup_keeps_the_stock_bytes() {
        let dir = TempDir::new("stock");
        fs::write(dir.file("config.txt"), STOCK).unwrap();
        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        assert_eq!(dir.read("config.txt"), "Include: clarim-eq.txt\n", "no -6 dB preamp, no example.txt, no GraphicEQ");
        assert_eq!(fs::read(dir.file(BACKUP_FILE)).unwrap(), STOCK.as_bytes());
        assert_eq!(dir.read(EQ_FILE), render_eqapo_file(&[bass_boost(GUID)]).unwrap());
        // idempotent, and the backup is still the stock file
        apply_to_dir(&dir.0, &[bass_boost(OTHER_GUID)]).unwrap();
        assert_eq!(dir.read("config.txt"), "Include: clarim-eq.txt\n");
        assert_eq!(fs::read(dir.file(BACKUP_FILE)).unwrap(), STOCK.as_bytes());

        // stock + our line already appended (an older Clarim): neutralized too; an existing backup is never touched
        let dir = TempDir::new("stock-appended");
        fs::write(dir.file("config.txt"), format!("{STOCK}\nInclude: clarim-eq.txt\n")).unwrap();
        fs::write(dir.file(BACKUP_FILE), STOCK).unwrap();
        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        assert_eq!(dir.read("config.txt"), "Include: clarim-eq.txt\n");
        assert_eq!(dir.read(BACKUP_FILE), STOCK);

        // a BOM and LF line endings are the stock file too
        let dir = TempDir::new("stock-bom");
        fs::write(dir.file("config.txt"), format!("\u{feff}{}\n", STOCK.replace("\r\n", "\n"))).unwrap();
        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        assert_eq!(dir.read("config.txt"), "Include: clarim-eq.txt\n");
    }

    #[test]
    fn a_read_only_stock_config_that_has_our_line_is_a_warning_not_a_failed_write() {
        let set_read_only = |dir: &TempDir, on: bool| {
            let mut perms = fs::metadata(dir.file("config.txt")).unwrap().permissions();
            perms.set_readonly(on);
            fs::set_permissions(dir.file("config.txt"), perms).unwrap();
        };
        // stock + our line (an older Clarim) and read-only: the EQ write still works, the file stays as it was
        let dir = TempDir::new("stock-appended-ro");
        let appended = format!("{STOCK}\nInclude: clarim-eq.txt\n");
        fs::write(dir.file("config.txt"), &appended).unwrap();
        set_read_only(&dir, true);
        let applied = apply_to_dir(&dir.0, &[bass_boost(GUID)]);
        set_read_only(&dir, false);
        applied.expect("the Include works already; only the -6 dB stays");
        assert_eq!(dir.read("config.txt"), appended);
        assert_eq!(dir.read(EQ_FILE), render_eqapo_file(&[bass_boost(GUID)]).unwrap());
        // stock without our line and read-only: the write is needed, so it fails
        let dir = TempDir::new("stock-ro");
        fs::write(dir.file("config.txt"), STOCK).unwrap();
        set_read_only(&dir, true);
        let applied = apply_to_dir(&dir.0, &[bass_boost(GUID)]);
        set_read_only(&dir, false);
        assert!(applied.is_err());
        assert_eq!(dir.read("config.txt"), STOCK);
    }

    #[test]
    fn the_stock_rewrite_checks_the_file_it_has_open_and_touches_nothing_else() {
        let dir = TempDir::new("recheck");
        let custom = STOCK.replace("-6 dB", "-4 dB");
        fs::write(dir.file("config.txt"), &custom).unwrap();
        assert!(!replace_stock_config(&dir.file("config.txt")).unwrap(), "not the stock file: left alone");
        assert_eq!(dir.read("config.txt"), custom);
        assert!(!dir.file(BACKUP_FILE).exists());
        fs::write(dir.file("config.txt"), STOCK).unwrap();
        assert!(replace_stock_config(&dir.file("config.txt")).unwrap());
        assert_eq!(dir.read("config.txt"), "Include: clarim-eq.txt\n", "rewritten from offset 0, no leftover bytes");
        assert_eq!(dir.read(BACKUP_FILE), STOCK);
    }

    #[test]
    fn a_customised_config_only_gets_the_include_line() {
        for (tag, custom) in [("value", STOCK.replace("-6 dB", "-5 dB")), ("line", format!("{STOCK}\r\nChannel: all"))] {
            let dir = TempDir::new(tag);
            fs::write(dir.file("config.txt"), &custom).unwrap();
            apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
            assert_eq!(dir.read("config.txt"), format!("{custom}\nInclude: clarim-eq.txt\n"), "{tag}: nothing but the append");
            assert_eq!(dir.read(BACKUP_FILE), custom);
        }
    }

    #[test]
    fn neutralizing_on_attach_touches_only_the_stock_file() {
        // stock, and our file does not exist yet: the Include must not point at nothing
        let dir = TempDir::new("neutralize");
        fs::write(dir.file("config.txt"), STOCK).unwrap();
        assert!(neutralize_in_dir(&dir.0).unwrap());
        assert_eq!(dir.read("config.txt"), "Include: clarim-eq.txt\n");
        assert_eq!(dir.read(EQ_FILE), render_eqapo_file(&[]).unwrap());
        assert_eq!(dir.read(BACKUP_FILE), STOCK);
        assert!(!neutralize_in_dir(&dir.0).unwrap(), "second time: nothing to do");

        // an EQ the agent already wrote is kept
        let dir = TempDir::new("neutralize-eq");
        fs::write(dir.file("config.txt"), STOCK).unwrap();
        fs::write(dir.file(EQ_FILE), "# agent's file\n").unwrap();
        assert!(neutralize_in_dir(&dir.0).unwrap());
        assert_eq!(dir.read(EQ_FILE), "# agent's file\n");

        // customised, missing and non-UTF-8 files are left exactly as they are
        let dir = TempDir::new("neutralize-custom");
        assert!(!neutralize_in_dir(&dir.0).unwrap(), "no config.txt: nothing created");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
        for custom in [STOCK.replace("-6 dB", "-5 dB").into_bytes(), format!("{STOCK}\nPreamp: -1 dB").into_bytes(), vec![0xFF, 0xFE, b'P', 0]] {
            fs::write(dir.file("config.txt"), &custom).unwrap();
            assert!(!neutralize_in_dir(&dir.0).unwrap());
            assert_eq!(fs::read(dir.file("config.txt")).unwrap(), custom);
        }
        assert!(!dir.file(EQ_FILE).exists() && !dir.file(BACKUP_FILE).exists());
        // a refused file (too big) is an error to log, still untouched
        fs::write(dir.file("config.txt"), vec![b'#'; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(neutralize_in_dir(&dir.0).is_err());
    }

    #[test]
    fn include_line_matching_ignores_case_spaces_and_bom() {
        assert!(is_include_line("Include: clarim-eq.txt"));
        assert!(is_include_line("  include: CLARIM-EQ.TXT \r\n"));
        assert!(is_include_line("\u{feff}Include: clarim-eq.txt"));
        assert!(!is_include_line("# Include: clarim-eq.txt"));
        assert!(!is_include_line("Include: other.txt"));
        let dir = TempDir::new("bom");
        fs::write(dir.file("config.txt"), "\u{feff}Include: clarim-eq.txt\n").unwrap();
        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        assert_eq!(dir.read("config.txt"), "\u{feff}Include: clarim-eq.txt\n", "no second Include");
    }

    #[test]
    fn bad_input_and_odd_folders_touch_nothing() {
        let dir = TempDir::new("bad");
        for bad in ["{not-a-guid}", "{5D16E8DA-37A3-4657-8D52-57FFE97F4EF9}\nVSTPlugin: x.dll", "all"] {
            let err = apply_to_dir(&dir.0, &[bass_boost(bad)]).unwrap_err();
            assert_eq!(err.to_string(), "Identificador de dispositivo inválido.");
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0, "the folder stays empty");

        let missing = dir.file("does-not-exist");
        let err = apply_to_dir(&missing, &[bass_boost(GUID)]).unwrap_err();
        assert!(err.to_string().contains("pasta de configuração"), "{err:#}");
        assert!(!missing.exists(), "a missing config folder is not created");

        // a config.txt that is not UTF-8 (UTF-16 from Notepad) is left alone
        let utf16 = [0xFF, 0xFE, b'I', 0, b'n', 0];
        fs::write(dir.file("config.txt"), utf16).unwrap();
        let err = apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err:#}");
        assert_eq!(fs::read(dir.file("config.txt")).unwrap(), utf16);
        assert!(!dir.file(BACKUP_FILE).exists());
    }

    #[test]
    fn a_huge_config_is_refused_untouched_and_not_copied() {
        let dir = TempDir::new("huge");
        let big = vec![b'#'; MAX_CONFIG_BYTES as usize + 1];
        fs::write(dir.file("config.txt"), &big).unwrap();
        let err = apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap_err();
        assert!(err.to_string().contains("não é um arquivo comum"), "{err:#}");
        assert_eq!(fs::read(dir.file("config.txt")).unwrap(), big, "not read into memory, not changed");
        assert!(!dir.file(BACKUP_FILE).exists(), "and not copied");
        // uninstall cleanup (best effort) leaves it alone too, but still deletes our own file
        assert!(remove_from_dir(&dir.0).is_err());
        assert_eq!(fs::read(dir.file("config.txt")).unwrap(), big);
        assert!(!dir.file(EQ_FILE).exists());
        // exactly at the cap is still a normal file
        let at_cap = TempDir::new("cap");
        fs::write(at_cap.file("config.txt"), vec![b'#'; MAX_CONFIG_BYTES as usize]).unwrap();
        apply_to_dir(&at_cap.0, &[bass_boost(GUID)]).unwrap();
        assert!(at_cap.read("config.txt").ends_with("Include: clarim-eq.txt\n"));
    }

    #[test]
    fn a_leftover_temp_file_is_replaced_and_an_existing_backup_name_is_never_written_to() {
        let dir = TempDir::new("stale");
        fs::write(dir.file("clarim-eq.txt.tmp"), "junk from an interrupted write").unwrap();
        fs::write(dir.file("config.txt"), "Preamp: -3 dB").unwrap();
        fs::write(dir.file(BACKUP_FILE), "whatever was already there").unwrap(); // a planted file or link looks the same
        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        assert!(!dir.file("clarim-eq.txt.tmp").exists());
        assert_eq!(dir.read(EQ_FILE), render_eqapo_file(&[bass_boost(GUID)]).unwrap());
        assert_eq!(dir.read(BACKUP_FILE), "whatever was already there", "create_new: never overwritten");
        assert_eq!(dir.read("config.txt"), "Preamp: -3 dB\nInclude: clarim-eq.txt\n");
    }

    #[test]
    fn links_in_the_config_folder_are_never_followed() {
        use std::os::windows::fs::symlink_file;
        let probe = TempDir::new("link-probe");
        if symlink_file(probe.file("nowhere"), probe.file("probe")).is_err() {
            // Creating a symlink needs a privilege (or Developer Mode): without it there is nothing to test.
            eprintln!("skipped: this account cannot create symlinks");
            return;
        }
        // a dangling link where the backup would go: the copy must not create its target
        let dir = TempDir::new("link-backup");
        let victim = dir.file("victim-backup.txt");
        fs::write(dir.file("config.txt"), "Preamp: -3 dB").unwrap();
        symlink_file(&victim, dir.file(BACKUP_FILE)).unwrap();
        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        assert!(!victim.exists(), "the backup was written through a link");
        // a dangling link where the temp file goes
        let dir = TempDir::new("link-tmp");
        let victim = dir.file("victim-tmp.txt");
        symlink_file(&victim, dir.file("clarim-eq.txt.tmp")).unwrap();
        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        assert!(!victim.exists(), "the equalizer file was written through a link");
        assert!(dir.read(EQ_FILE).contains("Clarim output equalizer"));
        // config.txt itself a link: refused, the target untouched (append and strip both)
        let dir = TempDir::new("link-config");
        let target = dir.file("target.txt");
        fs::write(&target, "Include: clarim-eq.txt\nsecret").unwrap();
        symlink_file(&target, dir.file("config.txt")).unwrap();
        let err = apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap_err();
        assert!(err.to_string().contains("não é um arquivo comum"), "{err:#}");
        assert!(remove_from_dir(&dir.0).is_err());
        assert_eq!(dir.read("target.txt"), "Include: clarim-eq.txt\nsecret");
    }

    #[test]
    fn remove_takes_out_only_our_include_and_our_file() {
        let dir = TempDir::new("remove");
        fs::write(dir.file("config.txt"), "Preamp: -3 dB\nChannel: all").unwrap();
        apply_to_dir(&dir.0, &[bass_boost(GUID)]).unwrap();
        remove_from_dir(&dir.0).unwrap();
        assert_eq!(dir.read("config.txt").lines().collect::<Vec<_>>(), ["Preamp: -3 dB", "Channel: all"]);
        assert!(!dir.file(EQ_FILE).exists());
        remove_from_dir(&dir.0).expect("removing twice is fine");
        remove_from_dir(&dir.file("gone")).expect("a missing folder is fine");
    }

    #[test]
    fn config_dir_is_the_apo_folder_or_says_it_is_not_installed() {
        // Read-only: whatever this machine has.
        match config_dir() {
            Ok(dir) => assert!(dir.ends_with(r"EqualizerAPO\config"), "{}", dir.display()),
            Err(e) => assert_eq!(e.to_string(), "Equalizer APO não está instalado"),
        }
    }

    #[test]
    fn requests_are_parsed_strictly_and_never_write_on_bad_input() {
        let dir = TempDir::new("process");
        let apply = |entries: &[(String, OutputEq)]| apply_to_dir(&dir.0, entries);
        let request = |gain: &str, guid: &str| {
            format!(r#"{{"entries":[{{"guid":"{guid}","bass_db":{gain},"mid_db":0,"treble_db":-2.5}}]}}"#)
        };

        let reply = process(request("5", GUID).as_bytes(), &apply);
        assert!(reply.ok && reply.error.is_none());
        let written = dir.read(EQ_FILE);
        assert!(written.contains(&format!("Device: {GUID}")) && written.contains("Gain 5.0 dB"), "{written}");
        assert!(written.contains("Gain -2.5 dB"), "{written}");

        let expected_wire = r#"{"ok":true}"#;
        assert_eq!(String::from_utf8(reply.to_line()).unwrap(), format!("{expected_wire}\n"));

        let injected = request("5", &format!("{GUID}\\nInclude: C:\\\\evil.txt"));
        for (bad, why) in [
            ("not json".to_string(), "Pedido inválido."),
            (r#"{"entries":"x"}"#.to_string(), "Pedido inválido."),
            (request("5", "{bad}"), "Identificador de dispositivo inválido."),
            (injected, "Identificador de dispositivo inválido."),
            (request("3e39", GUID), "Ganho do equalizador inválido."), // overflows f32 to infinity
        ] {
            let reply = process(bad.as_bytes(), &apply);
            assert!(!reply.ok, "{bad}");
            assert_eq!(reply.error.as_deref(), Some(why), "{bad}");
        }
        assert_eq!(dir.read(EQ_FILE), written, "refused requests leave the file as it was");
        assert!(!process(b"", &apply).ok);
    }

    #[test]
    fn production_sddl_is_a_valid_descriptor() {
        SecurityDescriptor::from_sddl(PIPE_SDDL).expect("the production SDDL parses");
        assert!(SecurityDescriptor::from_sddl("not sddl").is_err());
    }

    #[test]
    fn the_client_asks_for_no_more_than_the_production_sddl_grants() {
        // A mismatch between CLIENT_ACCESS and the interactive-users ACE is "access denied" for every user.
        let dir = TempDir::new("prod-sddl");
        let name = unique_pipe("prod-sddl");
        let dir_path = dir.0.clone();
        let server = Server::start(&name, PIPE_SDDL, move |line| process(line, &|entries: &[(String, OutputEq)]| apply_to_dir(&dir_path, entries))).unwrap();
        send_via(&name, &[bass_boost(GUID)]).expect("the production SDDL admits the production client");
        assert!(dir.read(EQ_FILE).contains(GUID));
        drop(server);
    }

    #[test]
    fn pipe_round_trip_between_client_and_server() {
        let dir = TempDir::new("pipe");
        let name = unique_pipe("round");
        assert!(!pipe_exists(&name));
        let dir_path = dir.0.clone();
        let server = Server::start(&name, TEST_SDDL, move |line| process(line, &|entries: &[(String, OutputEq)]| apply_to_dir(&dir_path, entries))).unwrap();
        assert!(pipe_exists(&name));

        let entries = [bass_boost(GUID)];
        send_via(&name, &entries).expect("a valid request is applied");
        assert_eq!(dir.read(EQ_FILE), render_eqapo_file(&entries).unwrap());
        assert_eq!(dir.read("config.txt"), "Include: clarim-eq.txt\n");

        // the server refuses what the renderer refuses, with the PT-BR text, and keeps serving
        let err = send_via(&name, &[bass_boost("{nope}")]).unwrap_err();
        assert_eq!(err.to_string(), "Identificador de dispositivo inválido.");
        send_via(&name, &[bass_boost(OTHER_GUID)]).expect("the server is still serving");
        assert!(dir.read(EQ_FILE).contains(OTHER_GUID));

        // an empty list is a valid "no EQ anywhere": header and `Device: all` only
        send_via(&name, &[]).unwrap();
        assert_eq!(dir.read(EQ_FILE), render_eqapo_file(&[]).unwrap());

        // the client refuses a request over the 8 KiB cap before connecting
        let many: Vec<_> = (0..200).map(|_| bass_boost(GUID)).collect();
        assert_eq!(send_via(&name, &many).unwrap_err().to_string(), "Equalizador: pedido grande demais.");

        drop(server);
        assert!(!pipe_exists(&name), "the pipe goes away with the server");
        let err = send_via(&name, &entries).unwrap_err();
        assert_eq!(err.to_string(), "O serviço do Clarim não está em execução.");
    }

    #[test]
    fn server_caps_the_request_size() {
        let name = unique_pipe("cap");
        let server = Server::start(&name, TEST_SDDL, |_| panic!("an oversize request must not be applied")).unwrap();
        let pipe = open_client(&name).unwrap();
        let event = new_event().unwrap();
        let (pipe, event) = (raw(&pipe), raw(&event));
        let line = vec![b'x'; MAX_LINE + 100]; // no newline, over the cap
        write_all(pipe, event, None, Duration::from_secs(3), &line).unwrap();
        let reply = read_line(pipe, event, None, Duration::from_secs(5), MAX_REPLY).unwrap();
        assert_eq!(String::from_utf8(reply).unwrap(), r#"{"ok":false,"error":"Pedido grande demais."}"#);
        drop(server);
    }

    #[test]
    fn a_silent_client_times_out_and_the_next_one_is_served() {
        let dir = TempDir::new("silent");
        let name = unique_pipe("silent");
        let dir_path = dir.0.clone();
        let server = Server::start(&name, TEST_SDDL, move |line| process(line, &|entries: &[(String, OutputEq)]| apply_to_dir(&dir_path, entries))).unwrap();

        let _silent = open_client(&name).unwrap(); // connects, never sends
        let started = Instant::now();
        send_via(&name, &[bass_boost(GUID)]).expect("served after the silent client times out");
        assert!(started.elapsed() >= Duration::from_secs(2), "the silent client held the server for its timeout");
        assert!(dir.read(EQ_FILE).contains(GUID));

        // and stopping the service is immediate even with a client hanging on
        let _silent_again = open_client(&name).unwrap();
        let stopping = Instant::now();
        drop(server);
        assert!(stopping.elapsed() < Duration::from_secs(2), "stop must not wait for a silent client");
    }
}
