use colored::*;
use reqwest::blocking::Client;
use serde_json::Value;

#[cfg(not(target_os = "windows"))]
use std::process::Command;

#[cfg(windows)]
use winconsole::console::{clear, set_title};

#[cfg(windows)]
fn enable_ansi_support() {
    use std::ffi::c_void;
    
    unsafe extern "system" {
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
    }
    
    unsafe {
        const STD_OUTPUT_HANDLE: u32 = 0xFFFFFFF5u32 as u32;
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
        
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        if !stdout.is_null() {
            let mut mode: u32 = 0;
            if GetConsoleMode(stdout, &mut mode) != 0 {
                mode |= ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                SetConsoleMode(stdout, mode);
            }
        }
    }
}

use wuwa_downloader::{
    config::status::Status,
    io::{
        console::print_results,
        file::get_dir,
        logging::setup_logging,
        util::{
            calculate_total_size, download_resources, exit_with_error, setup_ctrlc,
            start_title_thread, track_progress,
        },
    },
    network::client::{fetch_index, get_config},
};

fn main() {
    #[cfg(windows)]
    {
        set_title("Wuthering Waves Downloader").unwrap();
        enable_ansi_support();
    }

    let log_file = setup_logging();
    let client = Client::new();

    let config = match get_config(&client) {
        Ok(c) => c,
        Err(e) => exit_with_error(&log_file, &e),
    };

    let folder = get_dir();

    #[cfg(windows)]
    clear().unwrap();
    #[cfg(not(target_os = "windows"))]
    Command::new("clear").status().unwrap();

    println!(
        "\n{} Download folder: {}\n",
        Status::info(),
        folder.display().to_string().cyan()
    );

    let data = fetch_index(&client, &config, &log_file);
    let resources = match data.get("resource").and_then(Value::as_array) {
        Some(res) => res,
        None => exit_with_error(&log_file, "No resources found in index file"),
    };

    println!(
        "{} Found {} files to download\n",
        Status::info(),
        resources.len().to_string().cyan()
    );

    let total_size = calculate_total_size(resources, &client, &config);

    #[cfg(windows)]
    clear().unwrap();

    let (should_stop, success, progress) = track_progress(total_size);

    let title_thread = start_title_thread(
        should_stop.clone(),
        success.clone(),
        progress.clone(),
        resources.len(),
    );

    setup_ctrlc(should_stop.clone());

    download_resources(
        &client,
        &config,
        resources,
        &folder,
        &log_file,
        &should_stop,
        &progress,
        &success,
    );

    should_stop.store(true, std::sync::atomic::Ordering::SeqCst);
    title_thread.join().unwrap();

    #[cfg(windows)]
    clear().unwrap();

    print_results(
        success.load(std::sync::atomic::Ordering::SeqCst),
        resources.len(),
        &folder,
    );
}
