use std::fs::File;
use std::io::Read;
use std::path::Path;

use subtle::ConstantTimeEq;

use crate::Result;

/// Intentionally has no Debug implementation; credentials never enter output.
#[derive(Clone)]
pub struct Token(Vec<u8>);

impl Token {
    pub(crate) fn from_text(text: &str) -> Result<Self> {
        if text.is_empty() || text.len() > 4096 || !text.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("token must be bounded printable ASCII without whitespace".into());
        }
        Ok(Self(text.as_bytes().to_vec()))
    }

    pub(crate) fn fresh() -> Result<Self> {
        let mut random = [0; 32];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        Self::from_text(
            &random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
        )
    }
    pub fn read(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|_| "cannot open token file")?;
        let metadata = file.metadata().map_err(|_| "cannot inspect token file")?;
        if !metadata.is_file() {
            return Err("token must be stored in a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o777 != 0o600 {
                return Err("token file permissions must be 0600".into());
            }
        }
        #[cfg(not(unix))]
        return Err("mode-600 token files require a Unix system".into());

        let mut bytes = Vec::new();
        file.take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read token file")?;
        if bytes.len() > 4096 {
            return Err("token file is too large".into());
        }
        let token = std::str::from_utf8(&bytes)
            .map_err(|_| "token file must contain ASCII text")?
            .trim();
        if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err("token must be nonempty printable ASCII without whitespace".into());
        }
        Ok(Self(token.as_bytes().to_vec()))
    }

    pub fn authorizes(&self, header: &[u8]) -> bool {
        let Some(value) = header.strip_prefix(b"Bearer ") else {
            return false;
        };
        bool::from(self.0.as_slice().ct_eq(value.trim_ascii()))
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}
