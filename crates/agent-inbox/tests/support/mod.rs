use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static BINARY: OnceLock<PathBuf> = OnceLock::new();
#[cfg(unix)]
extern "C" fn cleanup() {
    if let Some(path) = BINARY.get() {
        let _ = std::fs::remove_file(path);
    }
}

pub fn fake_codex() -> PathBuf {
    BINARY
        .get_or_init(|| {
            let path = std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .join(format!("fake-codex-{}", std::process::id()));
            let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let output = Command::new(compiler)
                .arg("--edition=2024")
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_codex.rs"))
                .arg("-o")
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            // Prepare a freshly compiled executable before concurrent one-second
            // RPC budgets, so first-start overhead belongs to fixture setup.
            let warmup = Command::new(&path)
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(warmup.status.success(), "fake Codex failed to start");
            #[cfg(unix)]
            unsafe {
                libc::atexit(cleanup);
            }
            path
        })
        .clone()
}
