//! Logging for the long-running modes (agent, service): `%LOCALAPPDATA%\Clarim\clarim.log`, mirrored
//! to stderr when there is one. The file is cut back to empty once it passes 5 MB. `install` and `uninstall`
//! log to `%ProgramFiles%\Clarim\install.log` instead (1 MB), which an elevated run with no console can write.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const MAX_INSTALL_LOG_BYTES: u64 = 1024 * 1024;

pub fn log_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Clarim")
        .join("clarim.log")
}

/// `to_file` = agent/service: log to the file (and stderr), and route panics into the log (a GUI-subsystem
/// process has no console to show them on). Other commands log to stderr only.
pub fn init(to_file: bool) {
    init_with(to_file.then(|| (log_path(), MAX_LOG_BYTES)), false);
}

/// `install` / `uninstall`: the same, into `install.log` next to the installed exe. Without elevation that file
/// cannot be opened, and saying so would only come before the "Execute como administrador" hint that explains it.
pub fn init_install() {
    init_with(Some((clarim_svc::install_log_path(), MAX_INSTALL_LOG_BYTES)), true);
}

fn init_with(file: Option<(PathBuf, u64)>, quiet_when_denied: bool) {
    let mut builder = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    // An explicit RUST_LOG is the user's call; without one, keep the model libraries quiet.
    if std::env::var_os("RUST_LOG").is_none() {
        quiet_model_libraries(&mut builder);
    }
    if let Some((path, max)) = file {
        match Tee::open(path.clone(), max, true) {
            Ok(tee) => {
                builder.target(env_logger::Target::Pipe(Box::new(tee))).write_style(env_logger::WriteStyle::Never);
                std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
            }
            Err(e) if quiet_when_denied && e.kind() == io::ErrorKind::PermissionDenied => {}
            Err(e) => eprintln!("clarim: aviso: não foi possível gravar o log em {}: {e}", path.display()),
        }
    }
    let _ = builder.try_init();
}

/// libDF (`df`) logs "Possible clipping detected" at warn for every clipped frame, from the DSP thread
/// (a format + file write per frame); tract is chatty at info. Errors of `df` and warnings of tract stay.
fn quiet_model_libraries(builder: &mut env_logger::Builder) {
    builder.filter_module("df", log::LevelFilter::Error);
    for m in ["tract_core", "tract_linalg", "tract_onnx", "tract_hir", "tract_pulse"] {
        builder.filter_module(m, log::LevelFilter::Warn);
    }
}

/// Appends to the log file and echoes to stderr. Never fails because of stderr: with no console it
/// is an invalid handle, which std ignores.
struct Tee {
    file: File,
    path: PathBuf,
    size: u64,
    max: u64,
    echo: bool,
}

fn open_append(path: &Path, truncate: bool) -> io::Result<File> {
    if truncate {
        File::create(path)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

impl Tee {
    fn open(path: PathBuf, max: u64, echo: bool) -> io::Result<Tee> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let truncate = size > max;
        let file = open_append(&path, truncate)?;
        Ok(Tee { file, path, size: if truncate { 0 } else { size }, max, echo })
    }
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.echo {
            let _ = io::stderr().write_all(buf);
        }
        if self.size + buf.len() as u64 > self.max {
            // a failed reopen keeps writing to the old (oversized) file rather than losing the log
            if let Ok(file) = open_append(&self.path, true) {
                self.file = file;
                self.size = 0;
            }
        }
        self.file.write_all(buf)?;
        self.size += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clarim-log-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("clarim.log")
    }

    #[test]
    fn model_libraries_are_quiet_by_default_and_ours_are_not() {
        let mut builder = env_logger::Builder::new();
        builder.filter_level(log::LevelFilter::Info);
        quiet_model_libraries(&mut builder);
        let logger = builder.build();
        let wants = |target: &str, level: log::Level| {
            logger.matches(&log::Record::builder().target(target).level(level).build())
        };
        assert!(!wants("df::tract", log::Level::Warn), "per-frame clipping warnings are dropped");
        assert!(!wants("df::tract", log::Level::Info));
        assert!(wants("df::tract", log::Level::Error));
        assert!(!wants("tract_core::model", log::Level::Info));
        assert!(wants("tract_linalg::x86_64_fma", log::Level::Warn));
        assert!(wants("clarim_audio::pipeline", log::Level::Info));
        assert!(wants("clarim::agent", log::Level::Info));
    }

    #[test]
    fn appends_across_opens_and_cuts_back_when_over_the_limit() {
        let path = temp_log("rotate");
        let mut tee = Tee::open(path.clone(), 100, false).expect("open");
        tee.write_all(&[b'a'; 60]).expect("write");
        drop(tee);
        // reopened under the limit: appended to
        let mut tee = Tee::open(path.clone(), 100, false).expect("reopen");
        tee.write_all(&[b'b'; 30]).expect("write");
        assert_eq!(fs::metadata(&path).map(|m| m.len()).ok(), Some(90));
        // this write would pass the limit: the file restarts with just the new line
        tee.write_all(&[b'c'; 20]).expect("write");
        assert_eq!(fs::read(&path).ok(), Some(vec![b'c'; 20]));
        drop(tee);
        // an oversized file is emptied when opened
        fs::write(&path, [b'x'; 500]).expect("fill");
        drop(Tee::open(path.clone(), 100, false).expect("open oversized"));
        assert_eq!(fs::metadata(&path).map(|m| m.len()).ok(), Some(0));
        let _ = fs::remove_dir_all(path.parent().unwrap_or(&path));
    }
}
