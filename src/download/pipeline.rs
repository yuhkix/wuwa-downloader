use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use async_channel::{Receiver, Sender};
use indicatif::ProgressBar;
use reqwest::Client;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::cfg::{Config, DownloadOptions, ResourceItem};
use crate::download::progress::{DownloadProgress, ProgressDisplay};
use crate::io::file::{
    VerificationError, calculate_md5_interruptible, check_existing_file_interruptible, file_size,
};
use crate::io::logging::{SharedLogFile, log_error};
use crate::network::client::download_file;

const MAX_PIPELINE_RETRIES: usize = 2;
const DISPLAY_FILENAME_LIMIT: usize = 11;

pub struct DownloadTask {
    pub item: ResourceItem,
    pub expected_size: Option<u64>,
    pub attempt: usize,
}

pub struct PostVerifyTask {
    pub item: ResourceItem,
    pub expected_size: Option<u64>,
    pub attempt: usize,
}

pub struct PipelineResult {
    pub verified_ok: usize,
    pub downloaded_ok: usize,
    pub failed: usize,
    pub total: usize,
}

enum PipelineEvent {
    VerifiedValid { completed_bytes: Option<u64> },
    NeedDownload(DownloadTask),
    VerificationFailed { dest: String },
    VerificationAborted,
    DownloadSuccess(PostVerifyTask),
    DownloadFailed { dest: String },
    DownloadAborted,
    PostVerifySuccess,
    NeedRetry(DownloadTask),
    PostVerifyFailed { dest: String },
    PostVerifyIoFailed { dest: String },
    PostVerifyAborted,
}

async fn remove_file_if_exists(path: &Path) {
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

fn display_filename(dest: &str) -> String {
    let filename = dest.rsplit(['/', '\\']).next().unwrap_or(dest);
    let truncated: String = filename.chars().take(DISPLAY_FILENAME_LIMIT).collect();
    if filename.chars().count() > DISPLAY_FILENAME_LIMIT {
        format!("{}...", truncated)
    } else {
        filename.to_string()
    }
}

async fn verification_worker(
    rx: Receiver<ResourceItem>,
    event_tx: UnboundedSender<PipelineEvent>,
    folder: PathBuf,
    log_file: SharedLogFile,
    should_stop: Arc<AtomicBool>,
    verify_bar: ProgressBar,
) {
    while let Ok(item) = rx.recv().await {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }

        let expected_size = item.size;
        let local_path = folder.join(item.dest.replace('\\', "/"));
        let event = match check_existing_file_interruptible(
            &local_path,
            item.md5.as_deref(),
            expected_size,
            should_stop.clone(),
        )
        .await
        {
            Ok(false) => {
                verify_bar.inc(1);
                PipelineEvent::VerifiedValid {
                    completed_bytes: expected_size,
                }
            }
            Ok(true) => {
                verify_bar.inc(1);
                PipelineEvent::NeedDownload(DownloadTask {
                    item,
                    expected_size,
                    attempt: 0,
                })
            }
            Err(VerificationError::Interrupted) => PipelineEvent::VerificationAborted,
            Err(VerificationError::Io(err)) => {
                verify_bar.inc(1);
                log_error(
                    &log_file,
                    &format!("Verification failed for {}: {}", item.dest, err),
                );
                PipelineEvent::VerificationFailed { dest: item.dest }
            }
        };

        let _ = event_tx.send(event);
    }

    if should_stop.load(Ordering::SeqCst) {
        verify_bar.set_message("stopped");
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_worker(
    worker_id: usize,
    rx: Receiver<DownloadTask>,
    event_tx: UnboundedSender<PipelineEvent>,
    client: Arc<Client>,
    config: Arc<Config>,
    folder: PathBuf,
    log_file: SharedLogFile,
    should_stop: Arc<AtomicBool>,
    progress: DownloadProgress,
    display: Arc<ProgressDisplay>,
) {
    while let Ok(task) = rx.recv().await {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }

        let slot_index = display.slot_pool.acquire_slot().await;
        let task_bar = display.slot_pool.bar(slot_index);
        task_bar.set_prefix(format!("DL {:02}", slot_index + 1));

        let filename = display_filename(&task.item.dest);

        if task.attempt > 0 {
            task_bar.set_message(format!(
                "retrying {} (attempt {}/{})",
                filename, task.attempt, MAX_PIPELINE_RETRIES
            ));
        } else {
            task_bar.set_message(format!("downloading {}", filename));
        }

        task_bar.set_length(task.expected_size.unwrap_or(0));
        task_bar.set_position(0);

        let ok = download_file(
            &client,
            &config,
            &task.item.dest,
            &folder,
            task.expected_size,
            &log_file,
            &should_stop,
            &progress,
            &display.total_bar,
            &task_bar,
        )
        .await;

        task_bar.set_position(0);
        task_bar.set_length(0);

        if ok {
            task_bar.set_message("idle");
            display.slot_pool.release_slot(slot_index).await;
            let _ = event_tx.send(PipelineEvent::DownloadSuccess(PostVerifyTask {
                item: task.item,
                expected_size: task.expected_size,
                attempt: task.attempt,
            }));
            continue;
        }

        display.slot_pool.release_slot(slot_index).await;
        task_bar.set_message(if should_stop.load(Ordering::SeqCst) {
            "stopped"
        } else {
            "idle"
        });

        let event = if should_stop.load(Ordering::SeqCst) {
            PipelineEvent::DownloadAborted
        } else {
            log_error(
                &log_file,
                &format!(
                    "Download worker {} failed: {}",
                    worker_id + 1,
                    task.item.dest
                ),
            );
            PipelineEvent::DownloadFailed {
                dest: task.item.dest,
            }
        };
        let _ = event_tx.send(event);
    }
}

#[allow(clippy::too_many_arguments)]
async fn post_verify_worker(
    worker_id: usize,
    rx: Receiver<PostVerifyTask>,
    event_tx: UnboundedSender<PipelineEvent>,
    folder: PathBuf,
    log_file: SharedLogFile,
    should_stop: Arc<AtomicBool>,
    progress: DownloadProgress,
    display: Arc<ProgressDisplay>,
) {
    while let Ok(task) = rx.recv().await {
        let filename = display_filename(&task.item.dest);
        let path = folder.join(task.item.dest.replace('\\', "/"));

        if should_stop.load(Ordering::SeqCst) {
            let _ = event_tx.send(PipelineEvent::PostVerifyAborted);
            break;
        }

        let verification = if let Some(expected_md5) = task.item.md5.as_deref() {
            match calculate_md5_interruptible(&path, should_stop.clone()).await {
                Ok(actual_md5) => Ok(actual_md5 == expected_md5),
                Err(err) => Err(err),
            }
        } else if let Some(expected_size) = task.expected_size {
            match tokio::fs::metadata(&path).await {
                Ok(metadata) => Ok(metadata.len() == expected_size),
                Err(err) => Err(VerificationError::Io(err)),
            }
        } else {
            Ok(true)
        };

        match verification {
            Ok(true) => {
                let _ = event_tx.send(PipelineEvent::PostVerifySuccess);
                continue;
            }
            Err(VerificationError::Interrupted) => {
                let _ = event_tx.send(PipelineEvent::PostVerifyAborted);
                continue;
            }
            Err(VerificationError::Io(err)) => {
                log_error(
                    &log_file,
                    &format!(
                        "Post-verify worker {} failed for {}: {}",
                        worker_id + 1,
                        task.item.dest,
                        err
                    ),
                );
                let _ = event_tx.send(PipelineEvent::PostVerifyIoFailed { dest: filename });
                continue;
            }
            Ok(false) => {}
        }

        if should_stop.load(Ordering::SeqCst) {
            let _ = event_tx.send(PipelineEvent::PostVerifyAborted);
            continue;
        }

        if task.item.size.is_some() {
            let bytes_to_rollback = file_size(&path).await;
            progress
                .rollback_downloaded_bytes(&display.total_bar, bytes_to_rollback)
                .await;
        }
        remove_file_if_exists(&path).await;

        if task.attempt < MAX_PIPELINE_RETRIES {
            let _ = event_tx.send(PipelineEvent::NeedRetry(DownloadTask {
                item: task.item,
                expected_size: task.expected_size,
                attempt: task.attempt + 1,
            }));
        } else {
            log_error(
                &log_file,
                &format!(
                    "Post-verify worker {} exhausted retries for {}",
                    worker_id + 1,
                    filename
                ),
            );
            let _ = event_tx.send(PipelineEvent::PostVerifyFailed { dest: filename });
        }
    }
}

async fn enqueue_task<T>(tx: &Sender<T>, task: T) -> Result<(), T> {
    match tx.send(task).await {
        Ok(()) => Ok(()),
        Err(err) => Err(err.0),
    }
}

pub async fn run_pipeline(
    client: Arc<Client>,
    config: Arc<Config>,
    resources: Vec<ResourceItem>,
    folder: PathBuf,
    log_file: SharedLogFile,
    should_stop: Arc<AtomicBool>,
    options: DownloadOptions,
) -> PipelineResult {
    let total = resources.len();
    let total_download_size: u64 = resources.iter().filter_map(|item| item.size).sum();
    let verify_concurrency = options.verify_concurrency.max(1);
    let download_concurrency = options.download_concurrency.max(1);
    let post_verify_concurrency = verify_concurrency;

    let mut items_to_verify = Vec::new();
    let mut items_to_download = Vec::new();

    for item in resources {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }

        let local_path = folder.join(item.dest.replace('\\', "/"));
        let needs_verify = match tokio::fs::metadata(&local_path).await {
            Ok(meta) => {
                if let Some(expected_size) = item.size {
                    meta.len() == expected_size
                } else {
                    true
                }
            }
            Err(_) => false,
        };

        if needs_verify {
            items_to_verify.push(item);
        } else {
            items_to_download.push(item);
        }
    }

    let num_to_verify = items_to_verify.len();
    let display = Arc::new(ProgressDisplay::new(
        download_concurrency,
        total_download_size,
        num_to_verify,
    ));
    let progress = DownloadProgress {
        total_bytes: Arc::new(AtomicU64::new(total_download_size)),
        downloaded_bytes: Arc::new(AtomicU64::new(0)),
        total_bar_lock: Arc::new(tokio::sync::Mutex::new(())),
        start_time: Instant::now(),
    };

    let (event_tx, mut event_rx): (
        UnboundedSender<PipelineEvent>,
        UnboundedReceiver<PipelineEvent>,
    ) = mpsc::unbounded_channel();
    let (verify_tx, verify_rx) = async_channel::unbounded();
    let (download_tx, download_rx) = async_channel::unbounded();
    let (post_verify_tx, post_verify_rx) = async_channel::unbounded();

    let mut verify_handles = Vec::with_capacity(verify_concurrency);
    for _ in 0..verify_concurrency {
        verify_handles.push(tokio::spawn(verification_worker(
            verify_rx.clone(),
            event_tx.clone(),
            folder.clone(),
            log_file.clone(),
            should_stop.clone(),
            display.verify_bar.clone(),
        )));
    }
    drop(verify_rx);

    let mut download_handles = Vec::with_capacity(download_concurrency);
    for worker_id in 0..download_concurrency {
        download_handles.push(tokio::spawn(download_worker(
            worker_id,
            download_rx.clone(),
            event_tx.clone(),
            client.clone(),
            config.clone(),
            folder.clone(),
            log_file.clone(),
            should_stop.clone(),
            progress.clone(),
            display.clone(),
        )));
    }
    drop(download_rx);

    let mut post_verify_handles = Vec::with_capacity(post_verify_concurrency);
    for worker_id in 0..post_verify_concurrency {
        post_verify_handles.push(tokio::spawn(post_verify_worker(
            worker_id,
            post_verify_rx.clone(),
            event_tx.clone(),
            folder.clone(),
            log_file.clone(),
            should_stop.clone(),
            progress.clone(),
            display.clone(),
        )));
    }
    drop(post_verify_rx);

    for item in items_to_verify {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }
        if enqueue_task(&verify_tx, item).await.is_err() {
            break;
        }
    }
    drop(verify_tx);

    for item in items_to_download {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }
        let event = PipelineEvent::NeedDownload(DownloadTask {
            expected_size: item.size,
            item,
            attempt: 0,
        });
        if event_tx.send(event).is_err() {
            break;
        }
    }
    drop(event_tx);

    let mut result = PipelineResult {
        verified_ok: 0,
        downloaded_ok: 0,
        failed: 0,
        total,
    };
    let mut active_tasks = total;
    let mut shutting_down = should_stop.load(Ordering::SeqCst);

    loop {
        if !shutting_down && active_tasks == 0 {
            break;
        }

        if !shutting_down && should_stop.load(Ordering::SeqCst) {
            shutting_down = true;
            display
                .status_bar
                .set_message(format!("shutdown: left={}", active_tasks));
            download_tx.close();
            post_verify_tx.close();
        }

        if shutting_down {
            display
                .status_bar
                .set_message(format!("shutdown: left={}", active_tasks));
        } else {
            display
                .status_bar
                .set_message(format!("processing: {} files left", active_tasks));
        }

        tokio::select! {
            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };

                match event {
                    PipelineEvent::VerifiedValid { completed_bytes } => {
                        if let Some(bytes) = completed_bytes {
                            progress
                                .add_downloaded_bytes(&display.total_bar, bytes)
                                .await;
                        }
                        result.verified_ok += 1;
                        active_tasks = active_tasks.saturating_sub(1);
                    }
                    PipelineEvent::NeedDownload(task) => {
                        if shutting_down {
                            continue;
                        }

                        if enqueue_task(&download_tx, task).await.is_err() {
                            result.failed += 1;
                            active_tasks = active_tasks.saturating_sub(1);
                        }
                    }
                    PipelineEvent::VerificationFailed { dest } => {
                        let _ = dest;
                        result.failed += 1;
                        active_tasks = active_tasks.saturating_sub(1);
                    }
                    PipelineEvent::VerificationAborted => {
                    }
                    PipelineEvent::DownloadSuccess(task) => {
                        if shutting_down {
                            continue;
                        }

                        if enqueue_task(&post_verify_tx, task).await.is_err() {
                            result.failed += 1;
                            active_tasks = active_tasks.saturating_sub(1);
                        }
                    }
                    PipelineEvent::DownloadFailed { dest } => {
                        let _ = dest;
                        result.failed += 1;
                        active_tasks = active_tasks.saturating_sub(1);
                    }
                    PipelineEvent::DownloadAborted => {
                    }
                    PipelineEvent::PostVerifySuccess => {
                        result.downloaded_ok += 1;
                        active_tasks = active_tasks.saturating_sub(1);
                    }
                    PipelineEvent::NeedRetry(task) => {
                        if shutting_down {
                            continue;
                        }

                        if enqueue_task(&download_tx, task).await.is_err() {
                            result.failed += 1;
                            active_tasks = active_tasks.saturating_sub(1);
                        }
                    }
                    PipelineEvent::PostVerifyFailed { dest } => {
                        let _ = dest;
                        result.failed += 1;
                        active_tasks = active_tasks.saturating_sub(1);
                    }
                    PipelineEvent::PostVerifyIoFailed { dest } => {
                        let _ = dest;
                        result.failed += 1;
                        active_tasks = active_tasks.saturating_sub(1);
                    }
                    PipelineEvent::PostVerifyAborted => {
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                display.status_bar.tick();
            }
        }
    }

    drop(download_tx);
    drop(post_verify_tx);

    for handle in verify_handles {
        let _ = handle.await;
    }
    for handle in download_handles {
        let _ = handle.await;
    }
    for handle in post_verify_handles {
        let _ = handle.await;
    }

    let stopped = should_stop.load(Ordering::SeqCst);
    for slot in 0..display.slot_pool.len() {
        let slot_bar = display.slot_pool.bar(slot);
        if stopped {
            slot_bar.finish_with_message("stopped");
        } else {
            slot_bar.finish_with_message("idle");
        }
    }

    if stopped {
        display.status_bar.finish_with_message("stopped");
        display
            .verify_bar
            .finish_with_message("verification stopped");
        display.total_bar.finish_with_message("download stopped");
    } else {
        display.status_bar.finish_with_message("completed");
        display
            .verify_bar
            .finish_with_message("verification complete");
        display.total_bar.finish_with_message("download complete");
    }

    result
}
