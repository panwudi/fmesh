use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Result;

pub fn default_path() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_STATE_HOME").filter(|root| !root.is_empty()) {
        return Ok(PathBuf::from(root).join("agent-inbox/messages.md"));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is unset; provide --log")?;
    Ok(PathBuf::from(home).join(".local/state/agent-inbox/messages.md"))
}

pub fn append(path: &Path, from: &str, to: &str, text: &str) -> Result<String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.lock()?;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M %Z").to_string();
    let entry = format!("\n## From: {from}, To: {to}, Time: {stamp}\n\n{text}\n");
    file.write_all(entry.as_bytes())?;
    file.sync_data()?;
    Ok(stamp)
}
