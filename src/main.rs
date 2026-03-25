use colored::*;
use reqwest::Client;

#[cfg(not(target_os = "windows"))]
use std::process::Command;
use std::sync::atomic::Ordering;

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
    download::pipeline::run_pipeline,
    io::{
        console::print_results,
        file::get_dir,
        logging::setup_logging,
        util::{ask_concurrency, exit_with_error, parse_resources, setup_ctrlc},
    },
    network::client::{fetch_index, get_config},
};

#[tokio::main]
async fn main() {
    #[cfg(windows)]
    clear().unwrap();
    #[cfg(not(target_os = "windows"))]
    Command::new("clear").status().unwrap();

    #[cfg(windows)]
    {
        set_title("Wuthering Waves Downloader").unwrap();
        enable_ansi_support();
    }

    let log_file = setup_logging();
    let client = Client::new();

    let config = match get_config(&client).await {
        Ok(c) => c,
        Err(e) => exit_with_error(&log_file, &e),
    };

    let folder = match get_dir() {
        Ok(folder) => folder,
        Err(e) => exit_with_error(
            &log_file,
            &format!("Failed to read download directory: {}", e),
        ),
    };
    let options = match ask_concurrency() {
        Ok(options) => options,
        Err(e) => exit_with_error(&log_file, &format!("Failed to read concurrency: {}", e)),
    };

    #[cfg(windows)]
    clear().unwrap();
    #[cfg(not(target_os = "windows"))]
    Command::new("clear").status().unwrap();

    println!(
        "\n{} Download folder: {}",
        Status::info(),
        folder.display().to_string().cyan()
    );
    println!(
        "{} Download concurrency: {}",
        Status::info(),
        options.download_concurrency.to_string().cyan()
    );
    println!(
        "{} Verify concurrency: {}\n",
        Status::info(),
        options.verify_concurrency.to_string().cyan()
    );

    let data = match fetch_index(&client, &config, &log_file).await {
        Ok(data) => data,
        Err(e) => exit_with_error(&log_file, &e),
    };
    let resources = match parse_resources(&data) {
        Ok(resources) => resources,
        Err(err) => exit_with_error(&log_file, &err),
    };

    println!(
        "{} Found {} files to download\n",
        Status::info(),
        resources.len().to_string().cyan()
    );
    let should_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    setup_ctrlc(should_stop.clone());

    let result = run_pipeline(
        std::sync::Arc::new(client),
        std::sync::Arc::new(config),
        resources,
        folder.clone(),
        log_file.clone(),
        should_stop.clone(),
        options,
    )
    .await;

    #[cfg(windows)]
    clear().unwrap();

    print_results(&result, &folder);

    if should_stop.load(Ordering::SeqCst) {
        std::process::exit(130);
    }
}
