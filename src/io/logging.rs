use std::{
    fs::{self, OpenOptions},
    io::Write,
    time::SystemTime,
};

pub fn setup_logging() -> fs::File {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open("logs.log")
        .expect("Failed to create/open log file")
}

pub fn log_error(mut log_file: &fs::File, message: &str) {
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let _ = writeln!(log_file, "[{}] ERROR: {}", timestamp, message);
}
