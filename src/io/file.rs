use md5::{Digest, Md5};
use std::{
    fs,
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::config::status::Status;
use crate::io::util::read_line;

#[derive(Debug)]
pub enum VerificationError {
    Interrupted,
    Io(io::Error),
}

const CHECKSUM_CANCELLATION_ERROR: &str = "Checksum calculation cancelled";

fn calculate_md5_sync(path: &Path) -> io::Result<String> {
    calculate_md5_sync_interruptible(path, None)
}

fn calculate_md5_sync_interruptible(
    path: &Path,
    should_stop: Option<Arc<AtomicBool>>,
) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let mut reader = BufReader::with_capacity(262_144, file);
    let mut hasher = Md5::new();
    let mut buffer = [0_u8; 262_144];

    loop {
        if let Some(should_stop) = &should_stop
            && should_stop.load(Ordering::SeqCst)
        {
            return Err(io::Error::other(CHECKSUM_CANCELLATION_ERROR));
        }

        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

pub async fn calculate_md5(path: &Path) -> Result<String, String> {
    let path_buf = path.to_path_buf();
    tokio::task::spawn_blocking(move || calculate_md5_sync(&path_buf))
        .await
        .map_err(|e| format!("Failed to join MD5 task: {}", e))?
        .map_err(|e| format!("Failed to calculate MD5: {}", e))
}

pub async fn calculate_md5_interruptible(
    path: &Path,
    should_stop: Arc<AtomicBool>,
) -> Result<String, VerificationError> {
    let path_buf = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        calculate_md5_sync_interruptible(&path_buf, Some(should_stop))
    })
    .await
    .map_err(|e| {
        VerificationError::Io(io::Error::other(format!("Failed to join MD5 task: {}", e)))
    })?
    .map_err(|e| match e.kind() {
        io::ErrorKind::Other if e.to_string() == CHECKSUM_CANCELLATION_ERROR => {
            VerificationError::Interrupted
        }
        _ => VerificationError::Io(io::Error::new(
            e.kind(),
            format!("Failed to calculate MD5: {}", e),
        )),
    })
}

pub async fn check_existing_file(
    path: &Path,
    expected_md5: Option<&str>,
    expected_size: Option<u64>,
) -> bool {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(_) => return true,
    };

    if let Some(size) = expected_size
        && metadata.len() != size
    {
        if metadata.len() > size {
            let _ = tokio::fs::remove_file(path).await;
        }
        return true;
    }

    if let Some(md5) = expected_md5 {
        match calculate_md5(path).await {
            Ok(actual_md5) if actual_md5 == md5 => {}
            _ => {
                let _ = tokio::fs::remove_file(path).await;
                return true;
            }
        }
    }

    false
}

pub async fn check_existing_file_interruptible(
    path: &Path,
    expected_md5: Option<&str>,
    expected_size: Option<u64>,
    should_stop: Arc<AtomicBool>,
) -> Result<bool, VerificationError> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(VerificationError::Io(err)),
    };

    if let Some(size) = expected_size
        && metadata.len() != size
    {
        if metadata.len() > size {
            tokio::fs::remove_file(path)
                .await
                .map_err(VerificationError::Io)?;
        }
        return Ok(true);
    }

    if let Some(md5) = expected_md5 {
        match calculate_md5_interruptible(path, should_stop).await {
            Ok(actual_md5) if actual_md5 == md5 => {}
            Ok(_) => {
                tokio::fs::remove_file(path)
                    .await
                    .map_err(VerificationError::Io)?;
                return Ok(true);
            }
            Err(err) => return Err(err),
        }
    }

    Ok(false)
}

pub async fn file_size(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0)
}

pub fn get_filename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

pub fn get_dir() -> Result<PathBuf, io::Error> {
    loop {
        print!(
            "{} Please specify the directory where the game should be downloaded (press Enter to use the current directory): ",
            Status::question()
        );
        io::stdout().flush()?;

        let input = read_line()?;
        let path = input.trim();

        let path = if path.is_empty() {
            std::env::current_dir()?
        } else {
            PathBuf::from(shellexpand::tilde(path).into_owned())
        };

        if path.is_dir() {
            return Ok(path);
        }

        print!(
            "{} Directory does not exist. Create? (y/n): ",
            Status::warning()
        );
        io::stdout().flush()?;

        let input = read_line()?;

        if input.trim().eq_ignore_ascii_case("y") {
            fs::create_dir_all(&path)?;
            return Ok(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{VerificationError, check_existing_file_interruptible};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("wuwa-downloader-{name}-{nanos}"))
    }

    #[tokio::test]
    async fn check_existing_file_interruptible_returns_true_for_missing_file() {
        let path = unique_path("missing");

        let result = check_existing_file_interruptible(
            &path,
            None,
            Some(4),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();

        assert!(result);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn check_existing_file_interruptible_returns_true_and_keeps_undersized_file() {
        let path = unique_path("size-mismatch");
        fs::write(&path, b"abc").unwrap();

        let result = check_existing_file_interruptible(
            &path,
            None,
            Some(4),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();

        assert!(result);
        assert!(path.exists());
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn check_existing_file_interruptible_returns_true_and_deletes_oversized_file() {
        let path = unique_path("oversized");
        fs::write(&path, b"abcd").unwrap();

        let result = check_existing_file_interruptible(
            &path,
            None,
            Some(3),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();

        assert!(result);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn check_existing_file_interruptible_returns_true_and_deletes_for_checksum_mismatch() {
        let path = unique_path("checksum-mismatch");
        fs::write(&path, b"abc").unwrap();

        let result = check_existing_file_interruptible(
            &path,
            Some("deadbeef"),
            Some(3),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();

        assert!(result);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn check_existing_file_interruptible_returns_false_for_valid_file() {
        let path = unique_path("valid");
        fs::write(&path, b"abc").unwrap();

        let result = check_existing_file_interruptible(
            &path,
            Some("900150983cd24fb0d6963f7d28e17f72"),
            Some(3),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();

        assert!(!result);
        assert!(path.exists());
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn check_existing_file_interruptible_propagates_interruptions() {
        let path = unique_path("interrupted");
        fs::write(&path, b"abc").unwrap();

        let result = check_existing_file_interruptible(
            &path,
            Some("900150983cd24fb0d6963f7d28e17f72"),
            Some(3),
            Arc::new(AtomicBool::new(true)),
        )
        .await;

        assert!(matches!(result, Err(VerificationError::Interrupted)));
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn check_existing_file_interruptible_propagates_io_failures() {
        let path = unique_path("io-failure");
        fs::create_dir(&path).unwrap();

        let result = check_existing_file_interruptible(
            &path,
            Some("900150983cd24fb0d6963f7d28e17f72"),
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await;

        assert!(matches!(result, Err(VerificationError::Io(_))));
        let _ = fs::remove_dir(path);
    }
}
