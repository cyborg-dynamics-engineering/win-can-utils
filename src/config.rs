use crosscan::protocol::{VERSION, validate_name};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InterfaceConfig {
    pub version: u16,
    pub name: String,
    pub driver: String,
    pub device: String,
    pub bitrate: u32,
    pub timestamps: bool,
    pub up: bool,
}

impl InterfaceConfig {
    pub fn validate(&self) -> io::Result<()> {
        validate_name(&self.name)?;
        if self.version != VERSION
            || !matches!(self.driver.as_str(), "gsusb" | "pcan" | "slcan" | "virtual")
            || self.bitrate == 0
            || self.device.is_empty()
            || self.device.len() > 1024
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid interface config version, driver, device or bitrate",
            ));
        }
        Ok(())
    }
    pub fn load(path: &Path) -> io::Result<Self> {
        if std::fs::metadata(path)?.len() > 65_536 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Config is too large",
            ));
        }
        let config: Self = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        config.validate()?;
        Ok(config)
    }
    /// Write and sync beside the destination, then atomically replace it.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.validate()?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut temp = path.as_os_str().to_os_string();
        temp.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = PathBuf::from(temp);
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
            file.sync_all()?;
            drop(file);
            replace(&temp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // Indexers and virus scanners can briefly deny delete-sharing on Windows.
    // Retry only the transient sharing/access cases, without removing the old file.
    for attempt in 0..=10 {
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x1 | 0x8) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if attempt == 10 || !matches!(error.raw_os_error(), Some(5 | 32 | 33)) {
            return Err(error);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    unreachable!()
}

#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

pub fn default_path(name: &str) -> io::Result<PathBuf> {
    validate_name(name)?;
    let root = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "LOCALAPPDATA is unset; provide --config",
        )
    })?;
    Ok(PathBuf::from(root)
        .join("win-can-utils")
        .join("interfaces")
        .join(format!("{name}.json")))
}
