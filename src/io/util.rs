use serde_json::Value;
use std::{
    io,
    io::Write,
    sync::atomic::{AtomicUsize, Ordering},
    sync::Arc,
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

pub fn ask_concurrency() -> DownloadOptions {
    let defaults = DownloadOptions::default();

    print!(
        "{} Enter concurrent downloads [default {}]: ",
        Status::question(),
        defaults.download_concurrency
    );
    io::stdout().flush().unwrap();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_ok() {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return defaults;
        }

        if let Ok(parsed) = trimmed.parse::<usize>()
            && parsed > 0
        {
            return DownloadOptions {
                download_concurrency: parsed,
                ..defaults
            };
        }
    }

    println!(
        "{} Invalid value, using default concurrency {}",
        Status::warning(),
        defaults.download_concurrency
    );

    defaults
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
