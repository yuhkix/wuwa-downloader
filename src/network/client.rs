use colored::Colorize;
use flate2::read::GzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use serde_json::{Value, from_str};
#[cfg(not(target_os = "windows"))]
use std::process::Command;
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    time::Duration,
    u64,
};

#[cfg(windows)]
use winconsole::console::clear;

use crate::config::cfg::Config;
use crate::config::status::Status;
use crate::download::progress::DownloadProgress;
use crate::io::file::{calculate_md5, check_existing_file, get_filename};
use crate::io::{logging::log_error, util::get_version};

const INDEX_URL: &str = "https://gist.githubusercontent.com/yuhkix/b8796681ac2cd3bab11b7e8cdc022254/raw/4435fd290c07f7f766a6d2ab09ed3096d83b02e3/wuwa.json";
const MAX_RETRIES: usize = 3;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10000);
const BUFFER_SIZE: usize = 262144;

fn handle_http_error(log_file: &fs::File, error_msg: &str) -> ! {
    log_error(log_file, error_msg);

    #[cfg(windows)]
    clear().unwrap();

    println!("{} {}", Status::error(), error_msg);
    println!("\n{} Press Enter to exit...", Status::warning());
    let _ = io::stdin().read_line(&mut String::new());
    std::process::exit(1);
}

fn decompress_if_gzipped(response: reqwest::blocking::Response) -> Result<String, String> {
    let content_encoding = response
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_encoding.contains("gzip") {
        let mut buffer = Vec::new();
        let mut resp = response;
        resp.copy_to(&mut buffer)
            .map_err(|e| format!("Error reading response bytes: {}", e))?;

        let mut gz = GzDecoder::new(&buffer[..]);
        let mut decompressed_text = String::new();
        gz.read_to_string(&mut decompressed_text)
            .map_err(|e| format!("Error decompressing: {}", e))?;
        Ok(decompressed_text)
    } else {
        response
            .text()
            .map_err(|e| format!("Error reading response: {}", e))
    }
}

pub fn fetch_index(client: &Client, config: &Config, log_file: &fs::File) -> Value {
    println!("{} Fetching index file...", Status::info());

    let response = match client
        .get(&config.index_url)
        .timeout(Duration::from_secs(30))
        .send()
    {
        Ok(resp) => resp,
        Err(e) => {
            let msg = format!("Error fetching index file: {}", e);
            handle_http_error(log_file, &msg);
        }
    };

    if !response.status().is_success() {
        let msg = format!("Error fetching index file: HTTP {}", response.status());
        log_error(log_file, &msg);

        #[cfg(windows)]
        clear().unwrap();

        println!("{} {}", Status::error(), msg);
        println!("\n{} Press Enter to exit...", Status::warning());
        let _ = io::stdin().read_line(&mut String::new());
        std::process::exit(1);
    }

    let text = match decompress_if_gzipped(response) {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("Error processing index file: {}", e);
            handle_http_error(log_file, &msg);
        }
    };

    println!("{} Index file downloaded successfully", Status::success());

    match from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("Error parsing index file JSON: {}", e);
            handle_http_error(log_file, &msg);
        }
    }
}

pub fn download_file(
    client: &Client,
    config: &Config,
    dest: &str,
    folder: &Path,
    expected_md5: Option<&str>,
    log_file: &fs::File,
    should_stop: &std::sync::atomic::AtomicBool,
    progress: &DownloadProgress,
) -> bool {
    if should_stop.load(std::sync::atomic::Ordering::SeqCst) {
        return false;
    }

    let dest = dest.replace('\\', "/");
    let path = folder.join(&dest);
    let filename = get_filename(&dest);

    let mut file_size = None;

    for base_url in &config.zip_bases {
        if should_stop.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }

        let url = format!("{}{}", base_url, dest);

        if let Ok(head_response) = client.head(&url).timeout(Duration::from_secs(10)).send() {
            if let Some(size) = head_response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
            {
                file_size = Some(size);
                progress
                    .total_bytes
                    .fetch_add(size, std::sync::atomic::Ordering::SeqCst);
                break;
            }
        }
    }

    if let (Some(md5), Some(size)) = (expected_md5, file_size) {
        if should_skip_download(&path, Some(md5), Some(size)) {
            println!(
                "{} File is valid: {}",
                Status::matched(),
                filename.bright_purple()
            );
            return true;
        }
    }

    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            log_error(log_file, &format!("Directory error for {}: {}", dest, e));
            println!("{} Directory error: {}", Status::error(), e);
            return false;
        }
    }

    for (i, base_url) in config.zip_bases.iter().enumerate() {
        let url = format!("{}{}", base_url, dest);

        let head_response = match client.head(&url).timeout(Duration::from_secs(10)).send() {
            Ok(resp) if resp.status().is_success() => resp,
            Ok(resp) => {
                log_error(
                    log_file,
                    &format!("CDN {} failed for {} (HTTP {})", i + 1, dest, resp.status()),
                );
                continue;
            }
            Err(e) => {
                log_error(
                    log_file,
                    &format!("CDN {} failed for {}: {}", i + 1, dest, e),
                );
                continue;
            }
        };

        let expected_size = file_size.or_else(|| {
            head_response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
        });

        if let (Some(md5), Some(size)) = (expected_md5, expected_size) {
            if check_existing_file(&path, Some(md5), Some(size)) {
                println!(
                    "{} File is valid: {}",
                    Status::matched(),
                    filename.bright_purple()
                );
                return true;
            }
        }

        println!("{} Downloading: {}", Status::progress(), filename.purple());

        let pb = ProgressBar::new(expected_size.unwrap_or(0));
        pb.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta}, {binary_bytes_per_sec})")
            .unwrap()
            .progress_chars("#>-"));

        let mut retries = MAX_RETRIES;
        let mut last_error = None;

        while retries > 0 {
            let result = download_single_file(&client, &url, &path, should_stop, progress, &pb);

            match result {
                Ok(_) => break,
                Err(e) => {
                    if should_stop.load(std::sync::atomic::Ordering::SeqCst) {
                        pb.finish_and_clear();
                        return false;
                    }

                    last_error = Some(e);
                    retries -= 1;
                    let _ = fs::remove_file(&path);

                    if retries > 0 {
                        println!(
                            "{} Retrying {}... ({} left)",
                            Status::warning(),
                            filename.yellow(),
                            retries
                        );
                    }
                }
            }
        }

        pb.finish_and_clear();

        if should_stop.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }

        if retries == 0 {
            log_error(
                log_file,
                &format!(
                    "Failed after retries for {}: {}",
                    dest,
                    last_error.unwrap_or_default()
                ),
            );
            println!("{} Failed: {}", Status::error(), filename.red());
            return false;
        }

        if let Some(expected) = expected_md5 {
            if should_stop.load(std::sync::atomic::Ordering::SeqCst) {
                return false;
            }

            let actual = calculate_md5(&path);
            if actual != expected {
                log_error(
                    log_file,
                    &format!(
                        "Checksum failed for {}: expected {}, got {}",
                        dest, expected, actual
                    ),
                );
                fs::remove_file(&path).unwrap();
                println!("{} Checksum failed: {}", Status::error(), filename.red());
                return false;
            }
        }

        println!("{} Downloaded: {}", Status::success(), filename.green());
        return true;
    }

    log_error(log_file, &format!("All CDNs failed for {}", dest));
    println!("{} All CDNs failed for {}", Status::error(), filename.red());
    false
}

fn download_single_file(
    client: &Client,
    url: &str,
    path: &Path,
    should_stop: &std::sync::atomic::AtomicBool,
    progress: &DownloadProgress,
    pb: &ProgressBar,
) -> Result<(), String> {
    let mut downloaded: u64 = 0;
    if path.exists() {
        downloaded = fs::metadata(path)
            .map_err(|e| format!("Metadata error: {}", e))?
            .len();
    }

    let request = client
        .get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .header("Connection", "keep-alive");

    let request = if downloaded > 0 {
        request.header("Range", format!("bytes={}-", downloaded))
    } else {
        request
    };

    let mut response = request
        .send()
        .map_err(|e| format!("Network error: {}", e))?;

    if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        return Err("Range not satisfiable. File may already be fully downloaded.".into());
    }

    if !response.status().is_success() && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
    {
        return Err(format!("HTTP error: {}", response.status()));
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("File error: {}", e))?;

    pb.set_position(downloaded);
    progress
        .downloaded_bytes
        .store(downloaded, std::sync::atomic::Ordering::SeqCst);

    let mut buffer = vec![0; BUFFER_SIZE];
    loop {
        if should_stop.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Download interrupted".into());
        }

        let bytes_read = response
            .read(&mut buffer)
            .map_err(|e| format!("Read error: {}", e))?;

        if bytes_read == 0 {
            break;
        }

        file.write_all(&buffer[..bytes_read])
            .map_err(|e| format!("Write error: {}", e))?;

        downloaded += bytes_read as u64;
        pb.set_position(downloaded);
        progress
            .downloaded_bytes
            .store(downloaded, std::sync::atomic::Ordering::SeqCst);
    }

    Ok(())
}

pub fn ask_download_mode(_client: &Client) -> Result<String, String> {
    println!("\n{} Download Mode Selection", Status::info());
    println!("{} 1. Latest game versions (from official sources)", Status::question());
    println!("{} 2. Custom version (provide resource URLs)", Status::question());

    loop {
        print!("\n{} Choose download mode (1 or 2): ", Status::question());
        io::stdout()
            .flush()
            .map_err(|e| format!("Failed to flush stdout: {}", e))?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {}", e))?;

        match input.trim() {
            "1" => {
                return Ok("latest".to_string());
            }
            "2" => {
                return Ok("custom".to_string());
            }
            _ => println!("{} Invalid choice, please enter 1 or 2", Status::error()),
        }
    }
}

pub fn get_custom_config(_client: &Client) -> Result<Config, String> {
    println!("\n{} Custom Version Configuration", Status::info());

    print!("{} Enter resource.json URL: ", Status::question());
    io::stdout()
        .flush()
        .map_err(|e| format!("Failed to flush stdout: {}", e))?;

    let mut index_url = String::new();
    io::stdin()
        .read_line(&mut index_url)
        .map_err(|e| format!("Failed to read input: {}", e))?;

    let index_url = index_url.trim();
    if index_url.is_empty() {
        return Err("Resource JSON URL cannot be empty".to_string());
    }

    let index_url = if index_url.starts_with("http://") || index_url.starts_with("https://") {
        index_url.to_string()
    } else {
        format!("https://{}", index_url)
    };

    print!("{} Enter resource base path URL (ending with /zip): ", Status::question());
    io::stdout()
        .flush()
        .map_err(|e| format!("Failed to flush stdout: {}", e))?;

    let mut base_url = String::new();
    io::stdin()
        .read_line(&mut base_url)
        .map_err(|e| format!("Failed to read input: {}", e))?;

    let base_url = base_url.trim().to_string();
    if base_url.is_empty() {
        return Err("Resource base path URL cannot be empty".to_string());
    }

    let base_url = if base_url.starts_with("http://") || base_url.starts_with("https://") {
        base_url
    } else {
        format!("https://{}", base_url)
    };

    let base_url = if base_url.ends_with('/') {
        base_url
    } else {
        format!("{}/", base_url)
    };

    println!("\n{} Configuration loaded successfully", Status::success());
    Ok(Config {
        index_url: index_url.to_string(),
        zip_bases: vec![base_url],
    })
}

pub fn get_config(client: &Client) -> Result<Config, String> {
    let mode = ask_download_mode(client)?;

    if mode == "custom" {
        return get_custom_config(client);
    }

    let selected_index_url = fetch_gist(client)?;

    #[cfg(windows)]
    clear().unwrap();

    println!("{} Fetching download configuration...", Status::info());

    let response = client
        .get(&selected_index_url)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| format!("Network error: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Server error: HTTP {}", response.status()));
    }

    let config_text = decompress_if_gzipped(response)?;
    let config: Value = from_str(&config_text).map_err(|e| format!("Invalid JSON: {}", e))?;

    let has_default = config.get("default").is_some();
    let has_predownload = config.get("predownload").is_some();

    let selected_config = match (has_default, has_predownload) {
        (true, false) => {
            println!("{} Using default.config", Status::info());
            "default"
        }
        (false, true) => {
            println!("{} Using predownload.config", Status::info());
            "predownload"
        }
        (true, true) => loop {
            print!(
                "{} Choose config to use (1=default, 2=predownload): ",
                Status::question()
            );
            io::stdout()
                .flush()
                .map_err(|e| format!("Failed to flush stdout: {}", e))?;

            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .map_err(|e| format!("Failed to read input: {}", e))?;

            match input.trim() {
                "1" => break "default",
                "2" => break "predownload",
                _ => println!("{} Invalid choice, please enter 1 or 2", Status::error()),
            }
        },
        (false, false) => {
            return Err(
                "Neither default.config nor predownload.config found in response".to_string(),
            );
        }
    };

    let config_data = config
        .get(selected_config)
        .ok_or_else(|| format!("Missing {} config in response", selected_config))?;

    let base_config = config_data
        .get("config")
        .ok_or_else(|| format!("Missing config in {} response", selected_config))?;

    let base_url = base_config
        .get("baseUrl")
        .and_then(Value::as_str)
        .ok_or("Missing or invalid baseUrl")?;

    let index_file = base_config
        .get("indexFile")
        .and_then(Value::as_str)
        .ok_or("Missing or invalid indexFile")?;

    let mut cdn_urls = Vec::new();
    if let Some(cdn_list) = config_data.get("cdnList").and_then(Value::as_array) {
        for cdn in cdn_list {
            if let Some(url) = cdn.get("url").and_then(Value::as_str) {
                cdn_urls.push(url.trim_end_matches('/').to_string());
            }
        }
    } else {
        println!(
            "{} CDN list not found. Please enter CDN URLs manually.",
            Status::warning()
        );
        print!("{} Enter CDN URLs (comma-separated): ", Status::question());
        io::stdout()
            .flush()
            .map_err(|e| format!("Failed to flush stdout: {}", e))?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {}", e))?;

        cdn_urls = input
            .trim()
            .split(',')
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    if cdn_urls.is_empty() {
        return Err("No valid CDN URLs found".to_string());
    }

    let full_index_url = format!("{}//{}", cdn_urls[0], index_file.trim_start_matches('/'));
    let zip_bases = cdn_urls
        .iter()
        .map(|cdn| format!("{}//{}", cdn, base_url.trim_start_matches('/')))
        .collect();

    Ok(Config {
        index_url: full_index_url,
        zip_bases,
    })
}

fn should_skip_download(path: &Path, md5: Option<&str>, size: Option<u64>) -> bool {
    if let (Some(md5), Some(size)) = (md5, size) {
        check_existing_file(path, Some(md5), Some(size))
    } else {
        false
    }
}

pub fn fetch_gist(client: &Client) -> Result<String, String> {
    let response = client
        .get(INDEX_URL)
        .timeout(Duration::from_secs(30))
        .send()
        .map_err(|e| format!("Network error: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Server error: HTTP {}", response.status()));
    }

    let gist_data_text = decompress_if_gzipped(response)?;
    let gist_data: Value = from_str(&gist_data_text).map_err(|e| format!("Invalid JSON: {}", e))?;

    #[cfg(not(target_os = "windows"))]
    Command::new("clear").status().unwrap();
    #[cfg(windows)]
    clear().unwrap();

    let entries = [
        ("live", "os", "Live - OS"),
        ("live", "cn", "Live - CN"),
        ("beta", "os", "Beta - OS"),
        ("beta", "cn", "Beta - CN"),
    ];

    println!("{} Available versions:", Status::info());

    for (i, (cat, ver, label)) in entries.iter().enumerate() {
        let index_url = get_version(&gist_data, cat, ver)?;

        let resp = match client.get(&index_url).send() {
            Ok(resp) => resp,
            Err(e) => {
                println!("{} Failed to fetch {}: {}", Status::warning(), index_url, e);
                continue;
            }
        };

        let version_json: Value = {
            let version_text = decompress_if_gzipped(resp)
                .unwrap_or_else(|_| "{}".to_string());
            from_str(&version_text).unwrap_or(Value::Null)
        };

        let version = version_json
            .get("default")
            .and_then(|d| d.get("config"))
            .and_then(|c| c.get("version"))
            .or_else(|| version_json.get("default").and_then(|d| d.get("version")))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        println!("{}. {} ({})", i + 1, label, version);
    }

    loop {
        print!("{} Select version: ", Status::question());
        io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();

        match input.trim() {
            "1" => return get_version(&gist_data, "live", "os"),
            "2" => return get_version(&gist_data, "live", "cn"),
            "3" => return get_version(&gist_data, "beta", "os"),
            "4" => return get_version(&gist_data, "beta", "cn"),
            _ => println!("{} Invalid selection", Status::error()),
        }
    }
}
