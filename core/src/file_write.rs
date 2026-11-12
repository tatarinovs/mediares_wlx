//! Checked, buffered writes with exclusive ownership of temporary and newly created files.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct Created {
    path: PathBuf,
    file: Option<File>,
    keep: bool,
}

impl Created {
    fn new(path: PathBuf) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            keep: false,
        })
    }

    fn write(&mut self, encode: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> io::Result<()> {
        let file = self.file.as_mut().expect("file is open until commit");
        buffered(file, encode)?;
        file.sync_all()?;
        self.file = None; // Close before rename or cleanup, including on Windows.
        Ok(())
    }
}

impl Drop for Created {
    fn drop(&mut self) {
        self.file = None;
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn buffered(
    writer: &mut dyn Write,
    encode: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let mut output = BufWriter::new(writer);
    // Some format writers finalize in Drop and discard its error. Remember even a transient
    // error there, rather than mistaking a subsequent successful flush for a complete file.
    let mut checked = Checked {
        writer: &mut output,
        failed: false,
    };
    encode(&mut checked)?;
    checked.flush()?;
    if checked.failed {
        Err(io::Error::other("image writer ignored an I/O error"))
    } else {
        Ok(())
    }
}

struct Checked<'a> {
    writer: &'a mut dyn Write,
    failed: bool,
}
impl Write for Checked<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.writer.write(bytes);
        self.failed |= result.is_err() || matches!(result, Ok(0)) && !bytes.is_empty();
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        let result = self.writer.flush();
        self.failed |= result.is_err();
        result
    }
}

/// Replaces `path` only after encoding, flushing and syncing a unique sibling file succeeded.
pub fn replace(
    path: &Path,
    encode: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or(Path::new(""));
    let mut temp = loop {
        let name = format!(
            ".mediares-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        match Created::new(parent.join(name)) {
            Ok(file) => break file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    temp.write(encode)?;
    std::fs::rename(&temp.path, path)
}

/// Creates a new file without overwriting an existing one; removes only our own failed output.
pub fn write_new(
    path: &Path,
    encode: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let mut output = Created::new(path.to_path_buf())?;
    output.write(encode)?;
    output.keep = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_final_buffer_flush_error_is_reported() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(buffered(&mut Full, |out| out.write_all(b"fits inside the buffer")).is_err());
        assert!(buffered(&mut Full, |out| {
            out.write_all(b"data")?;
            let _ = out.flush();
            Ok(())
        })
        .is_err());
    }

    #[test]
    fn failed_replacements_keep_the_target_and_other_temporary_files() {
        let dir = std::env::temp_dir().join(format!("mediares-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("photo.jpg");
        let other = dir.join("photo.jpg.mediares-tmp");
        std::fs::write(&target, b"original").unwrap();
        std::fs::write(&other, b"another operation").unwrap();
        assert!(replace(&target, |out| {
            out.write_all(b"partial")?;
            Err(io::Error::other("failed encoder"))
        })
        .is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert_eq!(std::fs::read(&other).unwrap(), b"another operation");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        assert!(write_new(&target, |out| out.write_all(b"wrong")).is_err());
        replace(&target, |out| out.write_all(b"complete")).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"complete");
        std::fs::remove_file(target).unwrap();
        std::fs::remove_file(other).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
