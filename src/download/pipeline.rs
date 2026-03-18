use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use indicatif::ProgressBar;
use reqwest::Client;
use tokio::sync::mpsc;

use crate::config::cfg::{Config, DownloadOptions, ResourceItem};
use crate::download::progress::{DownloadProgress, ProgressDisplay};
use crate::io::file::{
    calculate_md5_interruptible, check_existing_file_interruptible, file_size,
};
use crate::io::logging::{SharedLogFile, log_error};
use crate::network::client::download_file;

const MAX_PIPELINE_RETRIES: usize = 2;

pub struct DownloadTask {
    pub item: ResourceItem,
    pub expected_size: u64,
    pub attempt: usize,
}

pub struct PostVerifyTask {
    pub item: ResourceItem,
    pub expected_size: u64,
    pub attempt: usize,
    pub slot_index: usize,
}

pub struct PipelineStats {
    pub verified_ok: AtomicUsize,
    pub downloaded_ok: AtomicUsize,
    pub failed: AtomicUsize,
    pub total: AtomicUsize,
}

pub struct PipelineResult {
    pub verified_ok: usize,
    pub downloaded_ok: usize,
    pub failed: usize,
    pub total: usize,
}

struct ShutdownDebugState {
    verify_waiting: AtomicUsize,
    verify_active: AtomicUsize,
    download_waiting: AtomicUsize,
    download_active: AtomicUsize,
    post_verify_waiting: AtomicUsize,
    post_verify_active: AtomicUsize,
}

impl ShutdownDebugState {
    fn new() -> Self {
        Self {
            verify_waiting: AtomicUsize::new(0),
            verify_active: AtomicUsize::new(0),
            download_waiting: AtomicUsize::new(0),
            download_active: AtomicUsize::new(0),
            post_verify_waiting: AtomicUsize::new(0),
            post_verify_active: AtomicUsize::new(0),
        }
    }
}

impl PipelineStats {
    pub fn new(total: usize) -> Self {
        Self {
            verified_ok: AtomicUsize::new(0),
            downloaded_ok: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            total: AtomicUsize::new(total),
        }
    }

    pub fn snapshot(&self) -> PipelineResult {
        PipelineResult {
            verified_ok: self.verified_ok.load(Ordering::SeqCst),
            downloaded_ok: self.downloaded_ok.load(Ordering::SeqCst),
            failed: self.failed.load(Ordering::SeqCst),
            total: self.total.load(Ordering::SeqCst),
        }
    }
}

fn finalized_count(stats: &PipelineStats) -> usize {
    stats.verified_ok.load(Ordering::SeqCst)
        + stats.downloaded_ok.load(Ordering::SeqCst)
        + stats.failed.load(Ordering::SeqCst)
}

fn count_completed_bytes(progress: &DownloadProgress, total_bar: &ProgressBar, amount: u64) {
    if amount == 0 {
        return;
    }

    progress.downloaded_bytes.fetch_add(amount, Ordering::SeqCst);
    total_bar.set_position(progress.downloaded());
}

fn rollback_completed_bytes(progress: &DownloadProgress, total_bar: &ProgressBar, amount: u64) {
    if amount == 0 {
        return;
    }

    let mut current = progress.downloaded_bytes.load(Ordering::SeqCst);
    loop {
        let next = current.saturating_sub(amount);
        match progress
            .downloaded_bytes
            .compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
    total_bar.set_position(progress.downloaded());
}

async fn remove_file_if_exists(path: &Path) {
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn verification_worker(
    _worker_id: usize,
    folder: PathBuf,
    log_file: SharedLogFile,
    should_stop: Arc<AtomicBool>,
    verify_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<ResourceItem>>>,
    download_tx: mpsc::Sender<DownloadTask>,
    verify_bar: ProgressBar,
    total_bar: ProgressBar,
    progress: DownloadProgress,
    stats: Arc<PipelineStats>,
    debug: Arc<ShutdownDebugState>,
) {
    loop {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }

        debug.verify_waiting.fetch_add(1, Ordering::SeqCst);
        let item = {
            let mut rx = verify_rx.lock().await;
            rx.recv().await
        };
        debug.verify_waiting.fetch_sub(1, Ordering::SeqCst);

        let Some(item) = item else { break };

        if should_stop.load(Ordering::SeqCst) {
            break;
        }

        debug.verify_active.fetch_add(1, Ordering::SeqCst);
        let expected_size = item.size;
        let local_path = folder.join(item.dest.replace('\\', "/"));
        let is_valid = check_existing_file_interruptible(
            &local_path,
            item.md5.as_deref(),
            expected_size,
            should_stop.clone(),
        )
        .await;

        if is_valid {
            if let Some(expected_size) = expected_size {
                count_completed_bytes(&progress, &total_bar, expected_size);
            }
            stats.verified_ok.fetch_add(1, Ordering::SeqCst);
        } else if let Err(e) = download_tx
            .send(DownloadTask {
                expected_size: expected_size.unwrap_or(0),
                item,
                attempt: 0,
            })
            .await
        {
            log_error(
                &log_file,
                &format!("Failed to send to download queue: {}", e),
            );
        }

        verify_bar.inc(1);
        debug.verify_active.fetch_sub(1, Ordering::SeqCst);
    }

    if should_stop.load(Ordering::SeqCst) {
        verify_bar.set_message("stopped");
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_worker(
    worker_id: usize,
    client: Arc<Client>,
    config: Arc<Config>,
    folder: PathBuf,
    log_file: SharedLogFile,
    should_stop: Arc<AtomicBool>,
    download_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<DownloadTask>>>,
    post_verify_tx: mpsc::Sender<PostVerifyTask>,
    progress: DownloadProgress,
    display: Arc<ProgressDisplay>,
    stats: Arc<PipelineStats>,
    debug: Arc<ShutdownDebugState>,
) {
    loop {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }

        debug.download_waiting.fetch_add(1, Ordering::SeqCst);
        let task = {
            let mut rx = download_rx.lock().await;
            rx.recv().await
        };
        debug.download_waiting.fetch_sub(1, Ordering::SeqCst);

        let Some(task) = task else { break };

        if should_stop.load(Ordering::SeqCst) {
            break;
        }

        debug.download_active.fetch_add(1, Ordering::SeqCst);
        let slot_index = display.slot_pool.acquire_slot().await;
        let task_bar = display.slot_pool.bar(slot_index);
        task_bar.set_prefix(format!("DL {:02}", slot_index + 1));

        let filename = task
            .item
            .dest
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(task.item.dest.as_str())
            .to_string();

        if task.attempt > 0 {
            task_bar.set_message(format!(
                "retrying {} (attempt {}/{})",
                filename,
                task.attempt,
                MAX_PIPELINE_RETRIES
            ));
        } else {
            task_bar.set_message(format!("downloading {}", filename));
        }

        task_bar.set_length(task.expected_size);
        task_bar.set_position(0);

        let ok = download_file(
            &client,
            &config,
            &task.item.dest,
            &folder,
            task.item.md5.as_deref(),
            Some(task.expected_size),
            &log_file,
            &should_stop,
            &progress,
            &display.total_bar,
            &task_bar,
        )
        .await;

        if ok {
            let enqueue_result = post_verify_tx
                .send(PostVerifyTask {
                    item: task.item,
                    expected_size: task.expected_size,
                    attempt: task.attempt,
                    slot_index,
                })
                .await;
            if let Err(e) = enqueue_result {
                stats.failed.fetch_add(1, Ordering::SeqCst);
                log_error(
                    &log_file,
                    &format!(
                        "Download worker {} failed to enqueue post-verify: {}",
                        worker_id + 1,
                        e
                    ),
                );
                task_bar.set_position(0);
                task_bar.set_length(0);
                task_bar.set_message("idle");
                display.slot_pool.release_slot(slot_index).await;
            }
            debug.download_active.fetch_sub(1, Ordering::SeqCst);
        } else {
            if !should_stop.load(Ordering::SeqCst) {
                stats.failed.fetch_add(1, Ordering::SeqCst);
                log_error(
                    &log_file,
                    &format!("Download worker {} failed: {}", worker_id + 1, task.item.dest),
                );
            }
            task_bar.set_position(0);
            task_bar.set_length(0);
            if should_stop.load(Ordering::SeqCst) {
                task_bar.set_message("stopped");
            } else {
                task_bar.set_message("idle");
            }
            display.slot_pool.release_slot(slot_index).await;
            debug.download_active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn post_verify_worker(
    worker_id: usize,
    folder: PathBuf,
    log_file: SharedLogFile,
    should_stop: Arc<AtomicBool>,
    post_verify_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<PostVerifyTask>>>,
    download_tx: mpsc::Sender<DownloadTask>,
    progress: DownloadProgress,
    display: Arc<ProgressDisplay>,
    stats: Arc<PipelineStats>,
    debug: Arc<ShutdownDebugState>,
) {
    loop {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }

        debug.post_verify_waiting.fetch_add(1, Ordering::SeqCst);
        let task = {
            let mut rx = post_verify_rx.lock().await;
            rx.recv().await
        };
        debug.post_verify_waiting.fetch_sub(1, Ordering::SeqCst);

        let Some(task) = task else { break };

        debug.post_verify_active.fetch_add(1, Ordering::SeqCst);
        let task_bar = display.slot_pool.bar(task.slot_index);
        let filename = task
            .item
            .dest
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(task.item.dest.as_str())
            .to_string();
        let path = folder.join(task.item.dest.replace('\\', "/"));

        task_bar.set_prefix(format!("DL {:02}", task.slot_index + 1));
        task_bar.set_message(format!("verifying {}", filename));
        task_bar.set_length(task.expected_size);
        task_bar.set_position(task.expected_size);

        if should_stop.load(Ordering::SeqCst) {
            task_bar.set_position(0);
            task_bar.set_length(0);
            task_bar.set_message("stopped");
            display.slot_pool.release_slot(task.slot_index).await;
            debug.post_verify_active.fetch_sub(1, Ordering::SeqCst);
            break;
        }

        let verified = if let Some(expected_md5) = task.item.md5.as_deref() {
            match calculate_md5_interruptible(&path, should_stop.clone()).await {
                Ok(actual_md5) => actual_md5 == expected_md5,
                Err(err) => {
                    log_error(
                        &log_file,
                        &format!(
                            "Post-verify worker {} checksum calculation failed for {}: {}",
                            worker_id + 1,
                            task.item.dest,
                            err
                        ),
                    );
                    false
                }
            }
        } else {
            true
        };

        if verified {
            stats.downloaded_ok.fetch_add(1, Ordering::SeqCst);
            task_bar.set_position(0);
            task_bar.set_length(0);
            task_bar.set_message("idle");
            display.slot_pool.release_slot(task.slot_index).await;
            debug.post_verify_active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }

        let bytes_to_rollback = file_size(&path).await.min(task.expected_size);
        rollback_completed_bytes(&progress, &display.total_bar, bytes_to_rollback);
        remove_file_if_exists(&path).await;

        if !should_stop.load(Ordering::SeqCst) && task.attempt < MAX_PIPELINE_RETRIES {
            let next_attempt = task.attempt + 1;
            task_bar.set_message(format!(
                "retrying {} (attempt {}/{})",
                filename,
                next_attempt,
                MAX_PIPELINE_RETRIES
            ));
            if let Err(e) = download_tx
                .send(DownloadTask {
                    item: task.item,
                    expected_size: task.expected_size,
                    attempt: next_attempt,
                })
                .await
            {
                stats.failed.fetch_add(1, Ordering::SeqCst);
                log_error(
                    &log_file,
                    &format!(
                        "Post-verify worker {} failed to requeue {}: {}",
                        worker_id + 1,
                        filename,
                        e
                    ),
                );
            }
        } else if !should_stop.load(Ordering::SeqCst) {
            stats.failed.fetch_add(1, Ordering::SeqCst);
            log_error(
                &log_file,
                &format!(
                    "Post-verify worker {} exhausted retries for {}",
                    worker_id + 1,
                    filename
                ),
            );
        }

        task_bar.set_position(0);
        task_bar.set_length(0);
        if should_stop.load(Ordering::SeqCst) {
            task_bar.set_message("stopped");
        } else {
            task_bar.set_message("idle");
        }
        display.slot_pool.release_slot(task.slot_index).await;
        debug.post_verify_active.fetch_sub(1, Ordering::SeqCst);
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
    let verify_concurrency = options.verify_concurrency.max(1);
    let download_concurrency = options.download_concurrency.max(1);
    let post_verify_concurrency = verify_concurrency;

    let (verify_tx, verify_rx) = mpsc::channel::<ResourceItem>(verify_concurrency * 2);
    let (download_tx, download_rx) = mpsc::channel::<DownloadTask>(download_concurrency * 2);
    let (post_verify_tx, post_verify_rx) =
        mpsc::channel::<PostVerifyTask>(download_concurrency * 2);

    let total_download_size: u64 = resources.iter().filter_map(|item| item.size).sum();
    let display = Arc::new(ProgressDisplay::new(
        download_concurrency,
        total_download_size,
        total,
    ));
    let progress = DownloadProgress {
        total_bytes: Arc::new(AtomicU64::new(total_download_size)),
        downloaded_bytes: Arc::new(AtomicU64::new(0)),
        start_time: Instant::now(),
    };
    let stats = Arc::new(PipelineStats::new(total));
    let debug = Arc::new(ShutdownDebugState::new());

    let verify_rx = Arc::new(tokio::sync::Mutex::new(verify_rx));
    let mut verify_handles = Vec::with_capacity(verify_concurrency);
    for worker_id in 0..verify_concurrency {
        verify_handles.push(tokio::spawn(verification_worker(
            worker_id,
            folder.clone(),
            log_file.clone(),
            should_stop.clone(),
            verify_rx.clone(),
            download_tx.clone(),
            display.verify_bar.clone(),
            display.total_bar.clone(),
            progress.clone(),
            stats.clone(),
            debug.clone(),
        )));
    }

    let download_rx = Arc::new(tokio::sync::Mutex::new(download_rx));
    let mut download_handles = Vec::with_capacity(download_concurrency);
    for worker_id in 0..download_concurrency {
        download_handles.push(tokio::spawn(download_worker(
            worker_id,
            client.clone(),
            config.clone(),
            folder.clone(),
            log_file.clone(),
            should_stop.clone(),
            download_rx.clone(),
            post_verify_tx.clone(),
            progress.clone(),
            display.clone(),
            stats.clone(),
            debug.clone(),
        )));
    }

    let post_verify_rx = Arc::new(tokio::sync::Mutex::new(post_verify_rx));
    let mut post_verify_handles = Vec::with_capacity(post_verify_concurrency);
    for worker_id in 0..post_verify_concurrency {
        post_verify_handles.push(tokio::spawn(post_verify_worker(
            worker_id,
            folder.clone(),
            log_file.clone(),
            should_stop.clone(),
            post_verify_rx.clone(),
            download_tx.clone(),
            progress.clone(),
            display.clone(),
            stats.clone(),
            debug.clone(),
        )));
    }

    for item in resources {
        if should_stop.load(Ordering::SeqCst) {
            break;
        }

        if verify_tx.send(item).await.is_err() {
            break;
        }
    }
    drop(verify_tx);

    for handle in verify_handles {
        let _ = handle.await;
    }
    drop(download_tx);

    while !should_stop.load(Ordering::SeqCst) && finalized_count(&stats) < total {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let shutdown_monitor = if should_stop.load(Ordering::SeqCst) {
        let debug = debug.clone();
        let stats = stats.clone();
        let display = display.clone();
        Some(tokio::spawn(async move {
            loop {
                let finalized = finalized_count(&stats);
                let line = format!(
                    "shutdown: finalized={}/{} verify(w={},a={}) download(w={},a={}) postverify(w={},a={})",
                    finalized,
                    stats.total.load(Ordering::SeqCst),
                    debug.verify_waiting.load(Ordering::SeqCst),
                    debug.verify_active.load(Ordering::SeqCst),
                    debug.download_waiting.load(Ordering::SeqCst),
                    debug.download_active.load(Ordering::SeqCst),
                    debug.post_verify_waiting.load(Ordering::SeqCst),
                    debug.post_verify_active.load(Ordering::SeqCst),
                );
                display.status_bar.set_message(line);
                display.status_bar.tick();

                if finalized >= stats.total.load(Ordering::SeqCst)
                    || (debug.verify_waiting.load(Ordering::SeqCst) == 0
                        && debug.verify_active.load(Ordering::SeqCst) == 0
                        && debug.download_waiting.load(Ordering::SeqCst) == 0
                        && debug.download_active.load(Ordering::SeqCst) == 0
                        && debug.post_verify_waiting.load(Ordering::SeqCst) == 0
                        && debug.post_verify_active.load(Ordering::SeqCst) == 0)
                {
                    break;
                }

                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }))
    } else {
        None
    };

    {
        let mut rx = download_rx.lock().await;
        rx.close();
    }
    {
        let mut rx = post_verify_rx.lock().await;
        rx.close();
    }
    drop(post_verify_tx);

    for handle in download_handles {
        let _ = handle.await;
    }

    for handle in post_verify_handles {
        let _ = handle.await;
    }

    if let Some(handle) = shutdown_monitor {
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
        display.verify_bar.finish_with_message("verification stopped");
        display.total_bar.finish_with_message("download stopped");
    } else {
        display.status_bar.finish_with_message("completed");
        display.verify_bar.finish_with_message("verification complete");
        display.total_bar.finish_with_message("download complete");
    }

    stats.snapshot()
}
