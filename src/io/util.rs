use serde_json::Value;
use std::{
    io,
    io::Write,
    sync::Arc,
    sync::atomic::AtomicBool,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    config::{
        cfg::{DownloadOptions, ResourceItem},
        status::Status,
    },
    io::logging::{SharedLogFile, log_error},
};

pub fn parse_resources(data: &Value) -> Result<Vec<ResourceItem>, String> {
    let resources = data
        .get("resource")
        .and_then(Value::as_array)
        .ok_or_else(|| "No resources found in index file".to_string())?;

    let mut parsed = Vec::with_capacity(resources.len());
    for item in resources {
        if let Some(dest) = item.get("dest").and_then(Value::as_str) {
            parsed.push(ResourceItem {
                dest: dest.to_string(),
                md5: item
                    .get("md5")
                    .and_then(Value::as_str)
                    .map(|md5| md5.to_string()),
                size: item.get("size").and_then(Value::as_u64),
            });
        }
    }

    Ok(parsed)
}

pub fn ask_concurrency() -> Result<DownloadOptions, io::Error> {
    let defaults = DownloadOptions::default();
    let download_concurrency =
        prompt_concurrency("concurrent downloads", defaults.download_concurrency)?;
    let verify_concurrency =
        prompt_concurrency("concurrent verifications", defaults.verify_concurrency)?;

    Ok(DownloadOptions {
        download_concurrency,
        verify_concurrency,
    })
}

fn worker_count_limit(default_value: usize) -> usize {
    let computed = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get().saturating_mul(4))
        .unwrap_or(32)
        .min(64);
    computed.max(default_value)
}

fn clamp_worker_count(value: usize, default_value: usize) -> usize {
    value.min(worker_count_limit(default_value))
}

fn prompt_concurrency(label: &str, default_value: usize) -> Result<usize, io::Error> {
    print!(
        "{} Enter {} [default {}]: ",
        Status::question(),
        label,
        default_value
    );
    io::stdout().flush().unwrap();

    let input = read_line()?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(default_value);
    }

    if let Ok(parsed) = trimmed.parse::<usize>()
        && parsed > 0
    {
        let limit = worker_count_limit(default_value);
        if parsed > limit {
            println!(
                "{} Value too large, clamping {} to {}",
                Status::warning(),
                label,
                limit
            );
            return Ok(clamp_worker_count(parsed, default_value));
        }

        return Ok(parsed);
    }

    println!(
        "{} Invalid value, using default {} for {}",
        Status::warning(),
        default_value,
        label
    );
    Ok(default_value)
}

pub fn read_line() -> Result<String, io::Error> {
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input)
}

pub fn read_line_interruptible(should_stop: &AtomicBool) -> Result<String, io::Error> {
    if should_stop.load(Ordering::SeqCst) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Input interrupted",
        ));
    }

    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
        Ok(_) => {
            if should_stop.load(Ordering::SeqCst) {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Input interrupted",
                ))
            } else {
                Ok(input)
            }
        }
        Err(err)
            if err.kind() == io::ErrorKind::Interrupted || should_stop.load(Ordering::SeqCst) =>
        {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Input interrupted",
            ))
        }
        Err(err) => Err(err),
    }
}

pub fn get_version(data: &Value, category: &str, version: &str) -> Result<String, String> {
    data[category][version]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Missing {} URL", version))
}

pub fn exit_with_error(log_file: &SharedLogFile, error: &str) -> ! {
    log_error(log_file, error);

    #[cfg(windows)]
    clear().unwrap();

    println!("{} {}", Status::error(), error);
    println!("\n{} Press Enter to exit...", Status::warning());
    let _ = io::stdin().read_line(&mut String::new());
    std::process::exit(1);
}

pub fn setup_ctrlc(should_stop: Arc<std::sync::atomic::AtomicBool>) {
    let interrupt_count = Arc::new(AtomicUsize::new(0));

    ctrlc::set_handler(move || {
        let count = interrupt_count.fetch_add(1, Ordering::SeqCst) + 1;
        should_stop.store(true, Ordering::SeqCst);

        if count >= 2 {
            eprintln!("\n{} Force exiting after second Ctrl-C", Status::warning());
            std::process::exit(130);
        }
    })
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::{clamp_worker_count, worker_count_limit};

    #[test]
    fn clamp_worker_count_limits_large_values() {
        let default_value = 4;
        let limit = worker_count_limit(default_value);

        assert_eq!(clamp_worker_count(usize::MAX, default_value), limit);
    }

    #[test]
    fn worker_count_limit_never_drops_below_default() {
        assert!(worker_count_limit(8) >= 8);
    }
}
