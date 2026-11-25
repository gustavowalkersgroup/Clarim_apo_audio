//! VB-Cable, the virtual microphone, for the guided install (`clarim setup`).
//!
//! Licence: VB-Audio's readme does not allow integrating the VB-CABLE package "in another software installation
//! procedure without Author agreement", and allows copying it only "AS IS without any modification". So Clarim never
//! bundles, repackages or silently installs it: `prepare` downloads the official, unmodified zip from VB-Audio's own
//! site, checks its SHA-256 and unpacks it as it is, and `run_setup` starts VB-Audio's OWN installer in a normal window,
//! which the user clicks through ("Install Driver"). Only the x64 installer is used, never the A+B / C+D variants.
//!
//! Unlike the Equalizer APO install this does not run in the service: the installer has a window, and a LocalSystem
//! service has no desktop to show it on. It runs in the elevated `setup` process, in the user's session, and uses the same
//! admin-only places (`<Program Files>\Clarim\cache`) and helpers (`curl.exe`, SHA-256, `wait_child`) as `apo_installer`.

use std::fs;
use std::ops::RangeInclusive;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::apo_installer::{cache_dir, download_to, system_exe, wait_child, Wait};
use crate::user_error::user_err;
use crate::sha256;

/// Where a person goes to get VB-Cable by hand (hash mismatch, no `tar.exe`, a failed install...).
pub const VBCABLE_PAGE: &str = "https://vb-audio.com/Cable/";

const URL: &str = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip";
/// SHA-256 of that zip (about 1.3 MB), checked against a real download on 2026-11-25. VB-Audio may replace the zip in
/// place with a newer pack; then the hash differs and nothing runs (`Package::Changed`).
const SHA256: &str = "bc44e8974191a5a2427d6c979e2dab08dbb09925f3f435dfa2f0ba3e45b6eb90";
const ZIP_FILE: &str = "VBCABLE_Driver_Pack46.zip";
/// The folder the zip is unpacked into, inside the cache.
const PACK_DIR: &str = "VBCABLE_Driver_Pack46";
/// VB-Audio's installer, at the root of the zip.
const SETUP_EXE: &str = "VBCABLE_Setup_x64.exe";
/// A zip outside this is not the pack (the real one is about 1.3 MB) and not a changed pack either: the download went wrong.
const SIZE_RANGE: RangeInclusive<u64> = 500_000..=20_000_000;
/// How long the installer window may stay open before it is closed (the user has to click through it).
const SETUP_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What `prepare` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Package {
    /// VB-Audio's installer, unpacked unmodified and ready to start.
    Ready(PathBuf),
    /// The zip at the pinned URL has the right size but no longer the pinned hash. It was deleted and nothing was unpacked or run.
    /// (A zip of the wrong size is an error instead: the download itself went wrong.)
    Changed,
}

/// Does `path` hold the zip with SHA-256 `expected`? `Ok(false)` is "a package of the right size with another hash": VB-Audio
/// replaced it. A file of the wrong size is not that, it is a download that did not bring the package (a block page, a proxy or
/// Wi-Fi login page, a cut transfer), so it is an error, like a failed download; so is a file that cannot be read.
fn is_the_pack(path: &Path, expected: &str) -> Result<bool> {
    let mut file = fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if !SIZE_RANGE.contains(&size) {
        log::warn!("the VB-Cable download is {size} bytes, not a package of the expected size");
        return Err(user_err(
            "O arquivo baixado do VB-Cable não é o pacote (tamanho inesperado). Pode ser uma página de bloqueio, de login do Wi-Fi \
             ou de proxy no lugar dele: verifique a conexão com a internet e tente de novo.",
        ));
    }
    let actual = sha256::sha256_hex(&mut file).with_context(|| format!("cannot read {}", path.display()))?;
    let same = actual.eq_ignore_ascii_case(expected);
    if !same {
        log::warn!("the VB-Cable package hash {actual} differs from the pinned {expected}");
    }
    Ok(same)
}

/// Is every entry of the zip listing a plain relative path that stays inside the target folder, and is VB-Audio's
/// installer at the root? `tar` refuses `..` and absolute paths by itself; this is Clarim's own check, made before anything is
/// unpacked, so that a surprise in the zip is an error and never a file outside the cache.
fn check_entries<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut has_setup = false;
    for name in names.into_iter().map(str::trim_end).filter(|n| !n.is_empty()) {
        let outside = name.contains('\0')
            || name.contains(':') // a drive letter, an alternate data stream
            || name.starts_with(['/', '\\'])
            || name.split(['/', '\\']).any(|part| part == "..");
        if outside {
            log::warn!("the VB-Cable package lists a path outside its folder: {name:?}");
            return Err(user_err("O pacote do VB-Cable baixado tem um conteúdo inesperado. Nada foi instalado."));
        }
        has_setup |= name == SETUP_EXE;
    }
    if !has_setup {
        log::warn!("the VB-Cable package has no {SETUP_EXE} at its root");
        return Err(user_err("O pacote do VB-Cable baixado não traz o instalador esperado. Nada foi instalado."));
    }
    Ok(())
}

/// Unpacks `zip` into a fresh `dest` with the `tar.exe` that ships with Windows 10 and later (bsdtar reads zip files),
/// started by its absolute System32 path, never through PATH.
fn extract(zip: &Path, dest: &Path) -> Result<()> {
    let tar = system_exe("tar.exe")?;
    let no_tar = |e: std::io::Error| {
        log::warn!("cannot run {}: {e}", tar.display());
        user_err("Não foi possível abrir o pacote do VB-Cable (o tar.exe do Windows não rodou).")
    };
    let run = |flag: &str, extra: &[&Path]| {
        let mut cmd = Command::new(&tar);
        cmd.arg(flag).arg(zip);
        for p in extra {
            cmd.arg("-C").arg(p);
        }
        cmd.stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW).output().map_err(no_tar)
    };
    let listing = run("-tf", &[])?;
    if !listing.status.success() {
        log::warn!("tar -tf failed: {}", String::from_utf8_lossy(&listing.stderr).trim());
        return Err(user_err("Não foi possível abrir o pacote do VB-Cable."));
    }
    check_entries(String::from_utf8_lossy(&listing.stdout).lines())?;

    let _ = fs::remove_dir_all(dest); // a leftover from an interrupted run: always start from the verified zip
    fs::create_dir_all(dest).with_context(|| format!("cannot create {}", dest.display()))?;
    let unpacked = run("-xf", &[dest])?;
    // symlink_metadata: a link named like the installer (a zip can carry one) is not the installer
    if !unpacked.status.success() || !fs::symlink_metadata(dest.join(SETUP_EXE)).is_ok_and(|m| m.is_file()) {
        log::warn!("tar -xf failed: {}", String::from_utf8_lossy(&unpacked.stderr).trim());
        let _ = fs::remove_dir_all(dest);
        return Err(user_err("Não foi possível abrir o pacote do VB-Cable."));
    }
    Ok(())
}

/// Downloads VB-Audio's official zip into `<Program Files>\Clarim\cache`, checks it against the pinned SHA-256 and unpacks
/// it, unmodified, into `...\cache\VBCABLE_Driver_Pack46\`. The zip is deleted once unpacked or refused. The unpacked folder stays
/// (about 1 MB) until `clarim uninstall` removes the whole cache. Errors are PT-BR.
pub fn prepare() -> Result<Package> {
    let zip = cache_dir().join(ZIP_FILE);
    download_to(URL, &zip, "o VB-Cable", &AtomicBool::new(false))?;
    let result = is_the_pack(&zip, SHA256).and_then(|same| {
        if !same {
            return Ok(Package::Changed);
        }
        let pack = cache_dir().join(PACK_DIR);
        extract(&zip, &pack)?;
        Ok(Package::Ready(pack.join(SETUP_EXE)))
    });
    let _ = fs::remove_file(&zip);
    result
}

/// Starts `setup` (VB-Audio's `VBCABLE_Setup_x64.exe` from `prepare`) in a normal, visible window, with no silent switch, and
/// waits until it is closed (ten minutes at most, then it is closed for the user). The caller is elevated, so it starts with no
/// second prompt. Its exit code means nothing: the caller looks for the "CABLE Input" device afterwards.
pub fn run_setup(setup: &Path) -> Result<()> {
    let mut child = Command::new(setup)
        .current_dir(setup.parent().unwrap_or(Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot start the VB-Cable installer")?;
    let deadline = Instant::now() + SETUP_TIMEOUT;
    match wait_child(&mut child, SETUP_TIMEOUT, &AtomicBool::new(false), || {}).context("waiting for the VB-Cable installer")? {
        Wait::Exited(status) => log::info!("the VB-Cable installer exited ({status})"),
        Wait::TimedOut | Wait::Cancelled => {
            return Err(user_err("O instalador do VB-Cable ficou aberto por mais de 10 minutos e foi fechado."));
        }
    }
    // In case the installer handed its window to a second process before exiting, wait for that one too.
    while !crate::pids_named(SETUP_EXE).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::FOLDERID_ProgramFiles;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clarim-vbcable-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_pinned_package_is_vb_audios_own_https_download_of_the_x64_installer() {
        assert_eq!(URL, "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip");
        assert!(URL.ends_with(ZIP_FILE) && ZIP_FILE.starts_with(PACK_DIR));
        assert_eq!(SETUP_EXE, "VBCABLE_Setup_x64.exe", "never the A+B / C+D variants, never the 32-bit setup");
        assert_eq!(SHA256.len(), 64);
        assert!(SHA256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "lower-case hex");
        assert!(VBCABLE_PAGE.starts_with("https://vb-audio.com/"));
    }

    #[test]
    fn listings_that_leave_the_folder_are_refused_and_the_setup_exe_must_be_there() {
        let ok = ["VBCABLE_Setup_x64.exe", "vbMmeCable64_win10.inf", "sub/dir/", "sub/dir/vbaudio_cable64_win10.sys", "a..b/ok.txt", "./x.cat", ""];
        check_entries(ok).expect("a plain relative listing");
        check_entries(["VBCABLE_Setup_x64.exe\r"]).expect("a CR from a CRLF listing is not part of the name");
        for bad in [
            "../evil.exe",
            "a/../../evil.exe",
            r"a\..\evil.exe",
            "..",
            "/etc/passwd",
            r"\Windows\System32\x.dll",
            r"\\server\share\x.exe",
            r"C:\Windows\x.exe",
            "C:x.exe",
            "VBCABLE_Setup_x64.exe:stream",
            "nul\0byte",
        ] {
            let err = check_entries(["VBCABLE_Setup_x64.exe", bad]).unwrap_err().to_string();
            assert!(err.contains("conteúdo inesperado") && err.contains("Nada foi instalado"), "{bad:?}: {err}");
        }
        for without in [vec![], vec!["readme.txt", "sub/VBCABLE_Setup_x64.exe"], vec!["VBCABLE_Setup.exe"]] {
            let err = check_entries(without).unwrap_err().to_string();
            assert!(err.contains("instalador esperado"), "{err}");
        }
    }

    #[test]
    fn a_wrong_size_is_a_failed_download_and_only_the_right_size_with_another_hash_is_a_changed_package() {
        let dir = temp("hash");
        let file = dir.join("fake.zip");
        // too small: a block page, a captive portal, a cut transfer. Not "the package changed".
        fs::write(&file, b"<html>nope</html>").unwrap();
        let err = is_the_pack(&file, SHA256).unwrap_err();
        assert!(err.to_string().contains("tamanho inesperado") && err.to_string().contains("conexão"), "{err:#}");
        fs::write(&file, vec![0u8; 21_000_000]).unwrap();
        assert!(is_the_pack(&file, SHA256).is_err(), "too big is a failed download as well");
        // right size class, other content: the pinned URL serves something else now
        fs::write(&file, vec![7u8; 1_300_000]).unwrap();
        assert!(!is_the_pack(&file, SHA256).unwrap(), "changed");
        // the pinned hash, here the hash of exactly that content, matches (case-insensitively)
        let own = sha256::sha256_hex(&mut fs::File::open(&file).unwrap()).unwrap();
        assert!(is_the_pack(&file, &own).unwrap(), "the pinned hash matches");
        assert!(is_the_pack(&file, &own.to_uppercase()).unwrap(), "case-insensitively");
        assert!(is_the_pack(&dir.join("missing.zip"), SHA256).is_err(), "unreadable is an error, not a verdict");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pack_folder_and_the_zip_live_in_the_admin_only_cache() {
        assert!(cache_dir().join(PACK_DIR).join(SETUP_EXE).ends_with(r"Clarim\cache\VBCABLE_Driver_Pack46\VBCABLE_Setup_x64.exe"));
        // under the shell's Program Files (admin-only to write), never a folder the caller's environment names
        let program_files = crate::known_folder(&FOLDERID_ProgramFiles).expect("the shell knows Program Files");
        assert!(cache_dir().starts_with(&program_files), "{}", cache_dir().display());
        assert!(cache_dir().join(ZIP_FILE).starts_with(program_files.join("Clarim")));
    }

    /// A zip of empty, stored files with exactly these names (no zip tool writes a `..` name, so it is made by hand).
    fn zip_with(names: &[&str]) -> Vec<u8> {
        let (mut files, mut central) = (Vec::new(), Vec::new());
        for name in names {
            let (offset, len) = (files.len() as u32, (name.len() as u16).to_le_bytes());
            files.extend([0x50, 0x4b, 0x03, 0x04, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // signature, version, flags, stored, time, date
            files.extend([0u8; 12]); // crc, sizes: nothing inside
            files.extend(len);
            files.extend([0, 0]); // extra field length
            files.extend(name.bytes());
            central.extend([0x50, 0x4b, 0x01, 0x02, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend([0u8; 12]);
            central.extend(len);
            central.extend([0u8; 12]); // extra, comment, disk, internal and external attributes
            central.extend(offset.to_le_bytes());
            central.extend(name.bytes());
        }
        let count = (names.len() as u16).to_le_bytes();
        let mut zip = files;
        let central_at = zip.len() as u32;
        zip.extend(&central);
        zip.extend([0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0]);
        zip.extend(count);
        zip.extend(count);
        zip.extend((central.len() as u32).to_le_bytes());
        zip.extend(central_at.to_le_bytes());
        zip.extend([0, 0]);
        zip
    }

    /// The real `tar.exe` on zips made in a temp folder: a listing that leaves the folder is refused by `extract` itself, before
    /// anything is unpacked; the same zip without the hostile name unpacks. Skipped when this Windows has no tar.exe.
    #[test]
    fn extract_refuses_a_hostile_listing_end_to_end() {
        let tar = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_default().join("System32").join("tar.exe");
        if !tar.is_file() {
            return;
        }
        let dir = temp("hostile");
        let (zip, dest) = (dir.join("pack.zip"), dir.join("out"));
        fs::write(&zip, zip_with(&[SETUP_EXE, "sub/x.inf"])).unwrap();
        extract(&zip, &dest).expect("the same builder, a plain listing: unpacks");
        assert!(dest.join(SETUP_EXE).is_file() && dest.join("sub").join("x.inf").is_file());
        let _ = fs::remove_dir_all(&dest);
        for bad in ["../evil.txt", "sub/../../evil.txt"] {
            fs::write(&zip, zip_with(&[SETUP_EXE, bad])).unwrap();
            let err = extract(&zip, &dest).unwrap_err().to_string();
            assert!(err.contains("conteúdo inesperado") && err.contains("Nada foi instalado"), "{bad}: {err}");
            assert!(!dest.exists(), "{bad}: nothing is unpacked, not even the folder is made");
            assert!(!dir.join("evil.txt").exists(), "{bad}: nothing outside the folder");
        }
        fs::write(&zip, zip_with(&["readme.txt"])).unwrap();
        assert!(extract(&zip, &dest).unwrap_err().to_string().contains("instalador esperado"));
        assert!(!dest.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Runs the real `tar.exe` on a tiny zip made in a temp folder (no network, no devices): the listing and the unpacking
    /// the setup relies on. Skipped when this Windows has no tar.exe.
    #[test]
    fn tar_lists_and_unpacks_a_zip_the_way_the_setup_expects() {
        let tar = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_default().join("System32").join("tar.exe");
        if !tar.is_file() {
            return;
        }
        let dir = temp("tar");
        let src = dir.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join(SETUP_EXE), b"not really an exe").unwrap();
        fs::write(src.join("sub").join("x.inf"), b"inf").unwrap();
        let zip = dir.join("pack.zip");
        // `-a` picks the zip format from the extension
        let made = Command::new(&tar)
            .arg("-a")
            .arg("-cf")
            .arg(&zip)
            .arg("-C")
            .arg(&src)
            .args([SETUP_EXE, "sub"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(made.status.success() && zip.is_file(), "{}", String::from_utf8_lossy(&made.stderr));
        let listing = Command::new(&tar).arg("-tf").arg(&zip).creation_flags(CREATE_NO_WINDOW).output().unwrap();
        let text = String::from_utf8_lossy(&listing.stdout).into_owned();
        check_entries(text.lines()).unwrap_or_else(|e| panic!("{e:#}: {text}"));
        let dest = dir.join("out");
        extract(&zip, &dest).expect("tar unpacks the zip");
        assert_eq!(fs::read(dest.join(SETUP_EXE)).unwrap(), b"not really an exe", "unpacked unmodified");
        assert_eq!(fs::read(dest.join("sub").join("x.inf")).unwrap(), b"inf");
        let _ = fs::remove_dir_all(&dir);
    }
}
