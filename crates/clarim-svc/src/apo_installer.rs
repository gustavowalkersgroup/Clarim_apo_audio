//! Installing Equalizer APO for the user: download the pinned official installer, check its SHA-256, run it
//! silently, make sure it left a working install behind. Everything runs inside the LocalSystem service,
//! and only ever touches admin-only places (`<Program Files>\Clarim\cache`, `<Program Files>\EqualizerAPO`).
//!
//! The installer under `/S` still starts `DeviceSelector.exe /i` (an interactive Qt dialog: in session 0 it
//! would sit there for ever, invisible, and the installer waits for it), so while the installer runs, any
//! `DeviceSelector.exe` from the install folder is killed. The installer's own tail (`UpdateChecker.exe -i`,
//! which schedules an update check at every logon, a window nobody asked for) is undone afterwards with the
//! switch the installer itself uses when the "check for updates" box is cleared (`-u`).

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, FOLDERID_System};

use crate::user_error::user_err;
use crate::{apo_reg, sha256};

/// The official x64 installer of Equalizer APO 1.4.2 on SourceForge (the project's own download page).
pub(crate) const APO_URL: &str = "https://sourceforge.net/projects/equalizerapo/files/1.4.2/EqualizerAPO-x64-1.4.2.exe/download";
/// Its SHA-256. First taken from the `checksum64` (type sha256) of the Chocolatey community package `equalizerapo`
/// 1.4.2 (whose install script points at the same URL; the same hash is in a public ANY.RUN analysis), then checked
/// against a real download of this URL on 2026-11-25: it matches. SourceForge itself publishes no SHA-256. A wrong
/// value here would only make the install refuse ("hash diferente"), never run a file.
pub(crate) const APO_SHA256: &str = "7403be7427bbe1936a40dded082829b6e217fc4f5990fee5cba501f0ae055afa";
pub(crate) const INSTALLER_FILE: &str = "EqualizerAPO-x64-1.4.2.exe";

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(320); // curl's own --max-time is 300 s
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// An installer smaller than this is an error page, larger is not the installer either.
const SIZE_RANGE: std::ops::RangeInclusive<u64> = 1_000_000..=100_000_000;

/// `<Program Files>\EqualizerAPO`, from the shell (never an environment variable).
pub(crate) fn apo_dir() -> Option<PathBuf> {
    crate::known_folder(&FOLDERID_ProgramFiles).map(|p| p.join("EqualizerAPO"))
}

pub(crate) fn apo_installed() -> bool {
    apo_dir().is_some_and(|d| d.join("EqualizerAPO.dll").is_file())
}

/// Admin-only (inherits Program Files): a user cannot swap the file between the check and the run.
pub(crate) fn cache_dir() -> PathBuf {
    crate::install_dir().join("cache")
}

/// What `curl.exe` is asked: ignore any .curlrc (`-q`, which must come first), follow redirects, fail on HTTP errors, quiet but with the error text, five
/// minutes at most, and only `https` for the first request.
pub(crate) fn curl_args(url: &str, out: &Path) -> Vec<String> {
    ["-q", "-L", "--fail", "--silent", "--show-error", "--max-time", "300", "--proto", "=https", "--output"]
        .iter()
        .map(|s| (*s).to_string())
        .chain([out.display().to_string(), url.to_string()])
        .collect()
}

/// How a child process ended.
#[derive(Debug)]
pub(crate) enum Wait {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
}

/// Waits for `child`, calling `tick` every 250 ms; kills it on timeout or cancel.
pub(crate) fn wait_child(child: &mut Child, timeout: Duration, cancel: &AtomicBool, mut tick: impl FnMut()) -> std::io::Result<Wait> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Wait::Exited(status));
        }
        let end = if cancel.load(Ordering::Relaxed) {
            Some(Wait::Cancelled)
        } else if Instant::now() >= deadline {
            Some(Wait::TimedOut)
        } else {
            None
        };
        if let Some(end) = end {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(end);
        }
        tick();
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub(crate) fn system_exe(name: &str) -> Result<PathBuf> {
    // An absolute path from the shell: an elevated process must not search the cwd or PATH.
    Ok(crate::known_folder(&FOLDERID_System).context("cannot locate the System32 folder")?.join(name))
}

pub(crate) fn download(cancel: &AtomicBool) -> Result<PathBuf> {
    let dest = cache_dir().join(INSTALLER_FILE);
    download_to(APO_URL, &dest, "o Equalizer APO", cancel)?;
    Ok(dest)
}

/// Downloads `url` to `dest` (its folder is created) with the system `curl.exe`; a leftover `dest` is replaced and a
/// failed download leaves nothing. `what` completes the PT-BR error ("Não foi possível baixar {what}."). The caller checks the file.
pub(crate) fn download_to(url: &str, dest: &Path, what: &str, cancel: &AtomicBool) -> Result<()> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let _ = fs::remove_file(dest); // a leftover from an interrupted run
    let mut child = Command::new(system_exe("curl.exe")?)
        .args(curl_args(url, dest))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("cannot run curl.exe")?;
    let outcome = wait_child(&mut child, DOWNLOAD_TIMEOUT, cancel, || {}).context("waiting for curl.exe")?;
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
    }
    match outcome {
        Wait::Exited(status) if status.success() => Ok(()),
        other => {
            let _ = fs::remove_file(dest);
            log::warn!("download of {what} from {url} failed ({other:?}): {}", stderr.trim());
            user_bail!("Não foi possível baixar {what}. Verifique a conexão com a internet e tente de novo.")
        }
    }
}

/// Does the installer on disk have the pinned hash? A file that does not is deleted.
pub(crate) fn verify(path: &Path) -> Result<()> {
    let result = verify_against(path, APO_SHA256);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn verify_against(path: &Path, expected: &str) -> Result<()> {
    let mut file = fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if !SIZE_RANGE.contains(&size) {
        user_bail!("O arquivo baixado não é o instalador do Equalizer APO (tamanho inesperado). Nada foi instalado.");
    }
    let actual = sha256::sha256_hex(&mut file).with_context(|| format!("cannot read {}", path.display()))?;
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        log::warn!("installer hash {actual} differs from the pinned {expected}");
        user_bail!("O arquivo baixado não confere com o esperado (hash diferente). Nada foi instalado.");
    }
    Ok(())
}

/// Runs the verified installer silently and waits for it (five minutes at most). Its exit code is only
/// logged: the check that counts is `check_installed`.
pub(crate) fn run_installer(path: &Path, cancel: &AtomicBool) -> Result<()> {
    let selector = apo_dir().map(|d| d.join("DeviceSelector.exe"));
    let mut child = Command::new(path)
        .arg("/S")
        .current_dir(cache_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("cannot run the Equalizer APO installer")?;
    let outcome = wait_child(&mut child, INSTALL_TIMEOUT, cancel, || {
        if let Some(selector) = &selector {
            crate::kill_processes_running(selector);
        }
    })
    .context("waiting for the Equalizer APO installer")?;
    match outcome {
        Wait::Exited(status) => {
            log::info!("Equalizer APO installer finished ({status})");
            Ok(())
        }
        Wait::TimedOut | Wait::Cancelled => {
            for exe in ["DeviceSelector.exe", "UpdateChecker.exe", "Editor.exe"] {
                if let Some(dir) = apo_dir() {
                    crate::kill_processes_running(&dir.join(exe));
                }
            }
            Err(user_err("A instalação do Equalizer APO demorou demais e foi interrompida."))
        }
    }
}

/// Files, registry key and both classes, judged: all three or it is not a working Equalizer APO.
fn judge_install(dll: bool, key: bool, registered: bool) -> Result<()> {
    if !dll {
        user_bail!("A instalação do Equalizer APO não terminou (os arquivos não foram criados na pasta padrão).");
    }
    if !key || !registered {
        user_bail!("O Equalizer APO foi instalado, mas não ficou registrado no Windows. Reinicie o computador e tente de novo.");
    }
    Ok(())
}

/// The installer left a usable Equalizer APO: its files, its registry key and its two classes.
pub(crate) fn check_installed() -> Result<()> {
    judge_install(apo_installed(), apo_reg::apo_key_present(), apo_reg::apo_registered())
}

/// What "installed" means for the dashboard and for attaching a device. The DLL alone is not enough: after
/// an interrupted install or a failed `regsvr32` it sits on disk with no classes behind it, and attaching a
/// device to classes Windows cannot create breaks that device's effects. In that state the install runs
/// again (the NSIS installer can run over an existing install).
pub(crate) fn apo_ready() -> bool {
    check_installed().is_ok()
}

/// Turns off the automatic update check the installer schedules (best effort; failures are only logged).
pub(crate) fn disable_update_checker() {
    let Some(exe) = apo_dir().map(|d| d.join("UpdateChecker.exe")).filter(|p| p.is_file()) else { return };
    let spawned = Command::new(&exe)
        .arg("-u")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    match spawned {
        Ok(mut child) => match wait_child(&mut child, Duration::from_secs(30), &AtomicBool::new(false), || {}) {
            Ok(Wait::Exited(_)) => {}
            other => log::warn!("UpdateChecker -u did not finish cleanly: {other:?}"),
        },
        Err(e) => log::warn!("cannot run UpdateChecker -u: {e}"),
    }
}

/// Deletes the downloaded installer (and the cache folder when it is empty).
pub(crate) fn cleanup(path: &Path) {
    if let Err(e) = fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("cannot delete {}: {e}", path.display());
        }
    }
    let _ = fs::remove_dir(cache_dir()); // only succeeds when empty
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clarim-apo-install-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_pinned_installer_is_a_https_sourceforge_url_and_a_sha256() {
        assert!(APO_URL.starts_with("https://sourceforge.net/projects/equalizerapo/files/1.4.2/"));
        assert!(APO_URL.contains(INSTALLER_FILE) && INSTALLER_FILE.contains("x64"));
        assert!(APO_URL.contains(crate::EQUALIZER_APO_VERSION) && INSTALLER_FILE.contains(crate::EQUALIZER_APO_VERSION));
        assert_eq!(APO_SHA256.len(), 64);
        assert!(APO_SHA256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "lower-case hex");
    }

    #[test]
    fn curl_is_asked_for_a_quiet_failing_https_download_of_at_most_five_minutes() {
        let args = curl_args("https://example.org/x.exe", Path::new(r"C:\Program Files\Clarim\cache\x.exe"));
        assert_eq!(
            args,
            ["-q", "-L", "--fail", "--silent", "--show-error", "--max-time", "300", "--proto", "=https", "--output", r"C:\Program Files\Clarim\cache\x.exe", "https://example.org/x.exe"]
        );
    }

    #[test]
    fn a_file_with_the_wrong_hash_or_size_is_refused_and_removed() {
        let dir = temp("verify");
        let file = dir.join("fake.exe");
        // right size class, wrong content
        fs::write(&file, vec![7u8; 1_500_000]).unwrap();
        let err = verify_against(&file, APO_SHA256).unwrap_err().to_string();
        assert!(err.contains("hash diferente") && err.contains("Nada foi instalado"), "{err}");
        // the expected hash of exactly that content passes (case-insensitively)
        let actual = sha256::sha256_hex(&mut fs::File::open(&file).unwrap()).unwrap();
        verify_against(&file, &actual.to_uppercase()).expect("the file matches its own hash");
        // too small: an HTML error page, say
        fs::write(&file, b"<html>nope</html>").unwrap();
        assert!(verify_against(&file, APO_SHA256).unwrap_err().to_string().contains("tamanho inesperado"));
        // `verify` itself deletes what it refuses
        assert!(verify(&file).is_err());
        assert!(!file.exists(), "a refused installer must not stay in the cache");
        assert!(verify(&dir.join("missing.exe")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    fn system32(name: &str) -> Option<PathBuf> {
        let exe = Path::new(&std::env::var_os("SystemRoot")?).join("System32").join(name);
        exe.is_file().then_some(exe)
    }

    #[test]
    fn waiting_reports_exit_timeout_and_cancel_and_ticks_while_it_waits() {
        let (Some(cmd), Some(ping)) = (system32("cmd.exe"), system32("PING.EXE")) else { return };
        let never = AtomicBool::new(false);
        let mut quick = Command::new(&cmd).args(["/c", "exit", "3"]).stdout(Stdio::null()).spawn().unwrap();
        match wait_child(&mut quick, Duration::from_secs(10), &never, || {}).unwrap() {
            Wait::Exited(s) => assert_eq!(s.code(), Some(3)),
            other => panic!("{other:?}"),
        }
        let slow = || Command::new(&ping).args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).spawn().unwrap();
        let mut ticks = 0;
        let mut child = slow();
        assert!(matches!(wait_child(&mut child, Duration::from_millis(600), &never, || ticks += 1).unwrap(), Wait::TimedOut));
        assert!(ticks >= 1, "the tick (the DeviceSelector killer) runs while waiting");
        assert!(child.try_wait().unwrap().is_some(), "a timed-out child is killed");
        let cancel = AtomicBool::new(true);
        let mut child = slow();
        assert!(matches!(wait_child(&mut child, Duration::from_secs(10), &cancel, || {}).unwrap(), Wait::Cancelled));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn installed_needs_the_files_the_key_and_both_classes() {
        judge_install(true, true, true).expect("complete install");
        for (key, registered) in [(false, true), (true, false), (false, false)] {
            let err = judge_install(true, key, registered).unwrap_err().to_string();
            assert!(err.contains("não ficou registrado"), "DLL only is a half install: {err}");
        }
        assert!(judge_install(false, true, true).unwrap_err().to_string().contains("não terminou"));
        // read-only on the real machine: ready is never looser than the DLL check
        assert!(!apo_ready() || apo_installed());
    }

    #[test]
    fn the_cache_is_inside_the_clarim_program_files_folder() {
        assert!(cache_dir().ends_with(r"Clarim\cache"));
        assert!(cache_dir().is_absolute());
        let dir = apo_dir().expect("the shell knows Program Files");
        assert!(dir.ends_with("EqualizerAPO"));
    }
}
