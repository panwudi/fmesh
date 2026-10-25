//! Short-lived local service registration for FlytoMesh's presence channel.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{Result, state};

const HEARTBEAT: Duration = Duration::from_secs(25);

pub struct Registration {
    path: PathBuf,
    inode: u64,
    content: String,
    port: u16,
    last_write: Instant,
}

fn owned_regular(path: &Path) -> Result<Option<u64>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err("FlytoMesh service name is owned by another local principal".into());
    }
    Ok(Some(metadata.ino()))
}

fn write_record(path: &Path, content: &str, previous: Option<u64>) -> Result<u64> {
    if owned_regular(path)? != previous {
        return Err("FlytoMesh service registration changed unexpectedly".into());
    }
    let temp = path.with_file_name(format!(".agent-inbox-{}", state::id()?));
    let result = (|| -> Result<u64> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        file.sync_all()?;
        if owned_regular(path)? != previous {
            return Err("FlytoMesh service registration changed unexpectedly".into());
        }
        fs::rename(&temp, path)?;
        owned_regular(path)?.ok_or("FlytoMesh service registration vanished".into())
    })();
    let _ = fs::remove_file(&temp);
    result
}

impl Registration {
    pub fn create(directory: &Path, port: u16, ident: &str) -> Result<Self> {
        if port == 0
            || ident.is_empty()
            || ident.len() > 64
            || ident == "-"
            || !ident
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b' ')
        {
            return Err("invalid FlytoMesh service port or identity".into());
        }
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.file_type().is_dir() || metadata.mode() & 0o1000 == 0 {
            return Err("FlytoMesh service directory must be a sticky directory".into());
        }
        let path = directory.join("agent-inbox");
        let content = format!("port {port} ident {ident}\n");
        let previous = owned_regular(&path)?;
        if previous.is_some()
            && fs::metadata(&path)?.modified()?.elapsed()? < Duration::from_secs(90)
        {
            return Err("FlytoMesh service is already registered by a live local process".into());
        }
        let inode = write_record(&path, &content, previous)?;
        Ok(Self {
            path,
            inode,
            content,
            port,
            last_write: Instant::now(),
        })
    }

    pub fn heartbeat(&mut self) -> Result<()> {
        self.verify()?;
        if self.last_write.elapsed() < HEARTBEAT {
            return Ok(());
        }
        self.inode = write_record(&self.path, &self.content, Some(self.inode))?;
        self.last_write = Instant::now();
        Ok(())
    }

    pub fn verify(&self) -> Result<u16> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.ino() != self.inode
            || metadata.len() > 256
        {
            return Err("FlytoMesh service registration no longer matches this listener".into());
        }
        let mut line = String::new();
        file.read_to_string(&mut line)?;
        if line != self.content {
            return Err("FlytoMesh service registration no longer matches this listener".into());
        }
        Ok(self.port)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.verify().is_ok() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_is_private_to_its_owner_and_removed_on_drop() {
        let dir =
            std::env::temp_dir().join(format!("inbox-mesh-services-{}", state::id().unwrap()));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o1733)).unwrap();
        let path = dir.join("agent-inbox");
        let mut registration = Registration::create(&dir, 19001, "fingerprint").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "port 19001 ident fingerprint\n"
        );
        assert_eq!(registration.verify().unwrap(), 19001);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        registration.heartbeat().unwrap();
        drop(registration);
        assert!(!path.exists());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn another_owners_or_symlinked_service_file_is_not_replaced() {
        let dir =
            std::env::temp_dir().join(format!("inbox-mesh-services-{}", state::id().unwrap()));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o1733)).unwrap();
        let path = dir.join("agent-inbox");
        std::os::unix::fs::symlink("elsewhere", &path).unwrap();
        assert!(Registration::create(&dir, 8832, "fingerprint").is_err());
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(path).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn a_live_registration_cannot_be_replaced_and_tampering_stops_renewal() {
        let dir =
            std::env::temp_dir().join(format!("inbox-mesh-services-{}", state::id().unwrap()));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o1733)).unwrap();
        let path = dir.join("agent-inbox");
        let mut registration = Registration::create(&dir, 19001, "fingerprint").unwrap();
        assert!(Registration::create(&dir, 19002, "other").is_err());
        fs::write(&path, "port 19002 ident other\n").unwrap();
        assert!(registration.heartbeat().is_err());
        drop(registration);
        assert!(path.exists());
        fs::remove_file(path).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
