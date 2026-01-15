use colored::Colorize;
use reqwest::blocking::Client;
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{self, Write},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

#[cfg(not(target_os = "windows"))]
use std::process::Command;

#[cfg(windows)]
use winconsole::console::{clear, set_title};

use crate::{
    config::{cfg::Config, status::Status},
    download::progress::DownloadProgress,
    io::logging::log_error,
    network::client::download_file,
};

pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub fn bytes_to_human(bytes: u64) -> String {
    match bytes {
        b if b > 1_000_000_000 => format!("{:.2} GB", b as f64 / 1_000_000_000.0),
        b if b > 1_000_000 => format!("{:.2} MB", b as f64 / 1_000_000.0),
        b if b > 1_000 => format!("{:.2} KB", b as f64 / 1_000.0),
        b => format!("{} B", b),
    }
}

fn log_url(url: &str) {
    let sanitized_url = if let Some(index) = url.find("://") {
        let (scheme, rest) = url.split_at(index + 3);
        format!("{}{}", scheme, rest.replace("//", "/"))
    } else {
        url.replace("//", "/")
    };

    if let Ok(mut url_log) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("urls.txt")
    {
        let _ = writeln!(url_log, "{}", sanitized_url);
    }
}

pub fn calculate_total_size(resources: &[Value], client: &Client, config: &Config) -> u64 {
    use std::collections::HashMap;
    
    let mut total_size = 0;
    let mut failed_urls = 0;
    let mut url_cache: HashMap<String, u64> = HashMap::new();

    println!("{} Processing files...", Status::info());

    for (i, item) in resources.iter().enumerate() {
        if let Some(dest) = item.get("dest").and_then(Value::as_str) {
            let mut file_size = 0;
            let mut found_valid_url = false;

            for base_url in &config.zip_bases {
                let url = format!("{}/{}", base_url, dest);
                log_url(&url);
                
                if let Some(&cached_size) = url_cache.get(&url) {
                    file_size = cached_size;
                    found_valid_url = true;
                    break;
                }
                
                match client
                    .head(&url)
                    .timeout(Duration::from_secs(15))
                    .send()
                {
                    Ok(response) => {
                        if let Some(len) = response.headers().get("content-length") {
                            if let Ok(len_str) = len.to_str() {
                                if let Ok(len_num) = len_str.parse::<u64>() {
                                    file_size = len_num;
                                    url_cache.insert(url, len_num);
                                    found_valid_url = true;
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("{} Failed to HEAD {}: {}", Status::warning(), url, e);
                    }
                }
            }

            if found_valid_url {
                total_size += file_size;
            } else {
                failed_urls += 1;
                println!(
                    "{} Could not determine size for file: {}",
                    Status::error(),
                    dest
                );
            }
        }

        if i % 10 == 0 {
            println!(
                "{} Processed {}/{} files...",
                Status::info(),
                i + 1,
                resources.len()
            );
        }
    }

    if failed_urls > 0 {
        println!(
            "{} Warning: Could not determine size for {} files",
            Status::warning(),
            failed_urls
        );
    }

    println!(
        "{} Total download size: {}",
        Status::info(),
        bytes_to_human(total_size).cyan()
    );

    #[cfg(not(target_os = "windows"))]
    Command::new("clear").status().unwrap();
    #[cfg(windows)]
    clear().unwrap();

    total_size
}

pub fn get_version(data: &Value, category: &str, version: &str) -> Result<String, String> {
    data[category][version]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Missing {} URL", version))
}

pub fn exit_with_error(log_file: &File, error: &str) -> ! {
    log_error(log_file, error);

    #[cfg(windows)]
    clear().unwrap();

    println!("{} {}", Status::error(), error);
    println!("\n{} Press Enter to exit...", Status::warning());
    let _ = io::stdin().read_line(&mut String::new());
    std::process::exit(1);
}

pub fn track_progress(
    total_size: u64,
) -> (
    Arc<std::sync::atomic::AtomicBool>,
    Arc<std::sync::atomic::AtomicUsize>,
    DownloadProgress,
) {
    let should_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let success = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let progress = DownloadProgress {
        total_bytes: Arc::new(std::sync::atomic::AtomicU64::new(total_size)),
        downloaded_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        start_time: Instant::now(),
    };

    (should_stop, success, progress)
}

pub fn start_title_thread(
    should_stop: Arc<std::sync::atomic::AtomicBool>,
    success: Arc<std::sync::atomic::AtomicUsize>,
    progress: DownloadProgress,
    total_files: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !should_stop.load(std::sync::atomic::Ordering::SeqCst) {
            let elapsed = progress.start_time.elapsed();
            let elapsed_secs = elapsed.as_secs();
            let downloaded_bytes = progress
                .downloaded_bytes
                .load(std::sync::atomic::Ordering::SeqCst);
            let total_bytes = progress
                .total_bytes
                .load(std::sync::atomic::Ordering::SeqCst);
            let current_success = success.load(std::sync::atomic::Ordering::SeqCst);

            let speed = if elapsed_secs > 0 {
                downloaded_bytes / elapsed_secs
            } else {
                0
            };
            let (speed_value, speed_unit) = if speed > 1_000_000 {
                (speed / 1_000_000, "MB/s")
            } else {
                (speed / 1_000, "KB/s")
            };

            let remaining_files = total_files - current_success;
            let remaining_bytes = total_bytes.saturating_sub(downloaded_bytes);
            let eta_secs = if speed > 0 && remaining_files > 0 {
                remaining_bytes / speed
            } else {
                0
            };
            let eta_str = format_duration(Duration::from_secs(eta_secs));

            let progress_percent = if total_bytes > 0 {
                format!(" ({}%)", (downloaded_bytes * 100 / total_bytes))
            } else {
                String::new()
            };

            let title = format!(
                "Wuthering Waves Downloader - {}/{} files - Total Downloaded: {}{} - Speed: {}{} - Total ETA: {}",
                current_success,
                total_files,
                bytes_to_human(downloaded_bytes),
                progress_percent,
                speed_value,
                speed_unit,
                eta_str
            );

            #[cfg(windows)]
            set_title(&title).unwrap();

            thread::sleep(Duration::from_secs(1));
        }
    })
}

pub fn setup_ctrlc(should_stop: Arc<std::sync::atomic::AtomicBool>) {
    ctrlc::set_handler(move || {
        should_stop.store(true, std::sync::atomic::Ordering::SeqCst);

        #[cfg(windows)]
        clear().unwrap();

        println!("\n{} Download interrupted by user", Status::warning());
    })
    .unwrap();
}

pub fn download_resources(
    client: &Client,
    config: &Config,
    resources: &[Value],
    folder: &std::path::Path,
    log_file: &File,
    should_stop: &Arc<std::sync::atomic::AtomicBool>,
    progress: &DownloadProgress,
    success: &Arc<std::sync::atomic::AtomicUsize>,
) {
    for item in resources {
        if should_stop.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }

        if let Some(dest) = item.get("dest").and_then(Value::as_str) {
            let md5 = item.get("md5").and_then(Value::as_str);
            if download_file(
                client,
                config,
                dest,
                folder,
                md5,
                log_file,
                should_stop,
                progress,
            ) {
                success.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}
