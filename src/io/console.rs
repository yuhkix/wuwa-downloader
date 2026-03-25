use crate::{config::status::Status, download::pipeline::PipelineResult};
use colored::Colorize;
use std::{io, path::Path};

pub fn print_results(result: &PipelineResult, folder: &Path) {
    let success = result.verified_ok + result.downloaded_ok;
    let unprocessed = result
        .total
        .saturating_sub(success.saturating_add(result.failed));

    let title = if success == result.total && result.failed == 0 && unprocessed == 0 {
        " DOWNLOAD COMPLETE ".on_blue().white().bold()
    } else {
        " PARTIAL DOWNLOAD ".on_blue().white().bold()
    };

    println!("\n{}\n", title);
    println!(
        "{} Successfully verified: {}",
        Status::success(),
        result.verified_ok.to_string().green()
    );
    println!(
        "{} Successfully downloaded: {}",
        Status::success(),
        result.downloaded_ok.to_string().green()
    );
    println!(
        "{} Failed: {}",
        Status::error(),
        result.failed.to_string().red()
    );
    println!(
        "{} Unprocessed: {}",
        Status::warning(),
        unprocessed.to_string().yellow()
    );
    println!(
        "{} Total files: {}",
        Status::info(),
        result.total.to_string().cyan()
    );
    println!(
        "{} Files saved to: {}",
        Status::info(),
        folder.display().to_string().cyan()
    );

    if unprocessed == 0 {
        println!("\n{} Press Enter to exit...", Status::warning());
        let _ = io::stdin().read_line(&mut String::new());
    }
}
