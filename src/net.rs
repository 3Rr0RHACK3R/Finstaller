use reqwest::{header::{HeaderValue, RANGE}, Client, StatusCode};
use futures::StreamExt;
use tokio::time::Duration;
use anyhow::{Result, bail, Context};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::io::{AsyncWriteExt, AsyncSeekExt, BufWriter};
use std::sync::atomic::{AtomicU64, Ordering};
use indicatif::ProgressBar;
use std::path::{Path, PathBuf};
use std::io::SeekFrom;
const RETRY_BACKOFF_MIN_MS: u64 = 100;
const RETRY_BACKOFF_MAX_MS: u64 = 5000;
const CHUNK_REQUEST_TIMEOUT_SECS: u64 = 30;
const DISK_WRITE_BUFFER_SIZE: usize = 512 * 1024;
const BURST_BUFFER_SIZE: usize = 2 * 1024 * 1024;
const BURST_RETRY_BACKOFF_MIN_MS: u64 = 50;
#[derive(Debug, Clone)]
pub struct DownloadInfo {
    pub total_size: u64,
    pub supports_ranges: bool,
}
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    pub allow_resume: bool,
    pub max_concurrent_writes: usize,
    pub max_retries: u32,
    pub burst_mode: bool,
}
pub async fn get_file_info(client: &Client, url: &url::Url) -> Result<DownloadInfo> {
    let mut total_size: u64 = 0;
    let mut supports_ranges = false;
    match client.head(url.clone()).send().await {
        Ok(resp) => {
            if let Some(len) = resp.headers().get(reqwest::header::CONTENT_LENGTH) {
                if let Ok(s) = len.to_str() {
                    total_size = s.parse().unwrap_or(0);
                }
            }
            if let Some(accept_ranges) = resp.headers().get(reqwest::header::ACCEPT_RANGES) {
                if let Ok(s) = accept_ranges.to_str() {
                    supports_ranges = s == "bytes";
                }
            }
        }
        Err(_) => {
            if let Ok(resp) = client.get(url.clone()).header(RANGE, "bytes=0-0").send().await {
                if resp.status() == StatusCode::PARTIAL_CONTENT {
                    supports_ranges = true;
                }
                if let Some(cr) = resp.headers().get(reqwest::header::CONTENT_RANGE) {
                    if let Ok(s) = cr.to_str() {
                        if let Some(pos) = s.rfind('/') {
                            total_size = s[pos+1..].parse().unwrap_or(0);
                        }
                    }
                }
            }
        }
    }
    Ok(DownloadInfo {
        total_size,
        supports_ranges,
    })
}
async fn pre_allocate_file(path: &Path, size: u64) -> Result<()> {
    let file = tokio::fs::File::create(path).await
        .with_context(|| format!("Failed to create file: {:?}", path))?;
    file.set_len(size).await
        .with_context(|| format!("Failed to pre-allocate {} bytes", size))?;
    Ok(())
}
async fn download_range_burst(
    client: &Client,
    url: &url::Url,
    start: u64,
    end: u64,
    output_path: PathBuf,
    progress_counter: Arc<AtomicU64>,
    config: &DownloadConfig,
) -> Result<()> {
    download_range_impl(client, url, start, end, output_path, progress_counter, 
                       config, BURST_BUFFER_SIZE, BURST_RETRY_BACKOFF_MIN_MS).await
}
async fn download_range_direct(
    client: &Client,
    url: &url::Url,
    start: u64,
    end: u64,
    output_path: PathBuf,
    progress_counter: Arc<AtomicU64>,
    config: &DownloadConfig,
) -> Result<()> {
    download_range_impl(client, url, start, end, output_path, progress_counter, 
                       config, DISK_WRITE_BUFFER_SIZE, RETRY_BACKOFF_MIN_MS).await
}
async fn download_range_impl(
    client: &Client,
    url: &url::Url,
    start: u64,
    end: u64,
    output_path: PathBuf,
    progress_counter: Arc<AtomicU64>,
    config: &DownloadConfig,
    buffer_size: usize,
    initial_backoff_ms: u64,
) -> Result<()> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&output_path)
        .await
        .with_context(|| format!("Thread failed to open file: {:?}", output_path))?;
    let mut writer = BufWriter::with_capacity(buffer_size, file);
    writer.seek(SeekFrom::Start(start)).await
        .context("Failed to seek to chunk start position")?;
    let mut current_position = start;
    let mut backoff_ms = initial_backoff_ms;
    let mut consecutive_failures = 0u32;
    while current_position <= end {
        if consecutive_failures > config.max_retries {
            bail!("Max retries exceeded for chunk range {}-{}", start, end);
        }
        let range_header = format!("bytes={}-{}", current_position, end);
        let mut req = client.get(url.clone());
        if let Ok(hv) = HeaderValue::from_str(&range_header) {
            req = req.header(RANGE, hv);
        }
        let resp_result = tokio::time::timeout(
            Duration::from_secs(CHUNK_REQUEST_TIMEOUT_SECS),
            req.send()
        ).await;
        let resp = match resp_result {
            Ok(Ok(r)) => r,
            _ => {
                consecutive_failures += 1;
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RETRY_BACKOFF_MAX_MS);
                continue;
            }
        };
        if !resp.status().is_success() {
            consecutive_failures += 1;
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(RETRY_BACKOFF_MAX_MS);
            continue;
        }
        let mut stream = resp.bytes_stream();
        let mut bytes_in_connection = 0u64;
        while let Some(item) = stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(_) => break, 
            };
            if let Err(e) = writer.write_all(&chunk).await {
                bail!("Disk write failed: {}", e);
            }
            let len = chunk.len() as u64;
            current_position += len;
            bytes_in_connection += len;
            progress_counter.fetch_add(len, Ordering::Relaxed);
        }
        if let Err(e) = writer.flush().await {
            bail!("Disk flush failed: {}", e);
        }
        if bytes_in_connection > 0 {
            consecutive_failures = 0;
            backoff_ms = initial_backoff_ms;
        } else {
            consecutive_failures += 1;
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(RETRY_BACKOFF_MAX_MS);
        }
    }
    Ok(())
}
pub async fn download_multi_stream(
    client: &Client,
    url: &url::Url,
    total_size: u64,
    chunk_size: u64,
    output_path: &std::path::Path,
    progress_bar: ProgressBar,
    config: &DownloadConfig,
) -> Result<()> {
    pre_allocate_file(output_path, total_size).await?;
    let mut chunks = Vec::new();
    let mut start = 0u64;
    while start < total_size {
        let end = std::cmp::min(start + chunk_size - 1, total_size - 1);
        chunks.push((start, end));
        start = end + 1;
    }
    let progress_counter = Arc::new(AtomicU64::new(0));
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent_writes));
    let mut tasks = Vec::new();
    let pb_clone = progress_bar.clone();
    let counter_clone = progress_counter.clone();
    let total_clone = total_size;
    let ui_handle = tokio::spawn(async move {
        let mut last_pos = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let current = counter_clone.load(Ordering::Relaxed);
            if current > last_pos {
                pb_clone.set_position(current);
                last_pos = current;
            }
            if current >= total_clone {
                break;
            }
        }
    });
    for (start, end) in chunks {
        let client = client.clone();
        let url = url.clone();
        let out_path = output_path.to_path_buf();
        let counter = progress_counter.clone();
        let sem = semaphore.clone();
        let cfg = config.clone();
        let task = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            if cfg.burst_mode {
                download_range_burst(
                    &client,
                    &url,
                    start,
                    end,
                    out_path,
                    counter,
                    &cfg
                ).await
            } else {
                download_range_direct(
                    &client,
                    &url,
                    start,
                    end,
                    out_path,
                    counter,
                    &cfg
                ).await
            }
        });
        tasks.push(task);
    }
    let results = futures::future::join_all(tasks).await;
    ui_handle.await.ok();
    progress_bar.set_position(total_size);
    for (idx, r) in results.into_iter().enumerate() {
        match r {
            Ok(Ok(_)) => {}, 
            Ok(Err(e)) => bail!("Worker {} failed: {}", idx, e),
            Err(e) => bail!("Worker {} panicked: {}", idx, e),
        }
    }
    Ok(())
}
pub async fn download_single_stream(
    client: &Client,
    url: &url::Url,
    file_path: &std::path::Path,
    progress_bar: &ProgressBar,
    config: &DownloadConfig,
) -> Result<()> {
    let file_exists = file_path.exists();
    let resume_from = if config.allow_resume && file_exists {
        tokio::fs::metadata(file_path).await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let mut attempts = 0;
    loop {
        attempts += 1;
        if attempts > config.max_retries {
            bail!("Failed single-stream download after {} retries", config.max_retries);
        }
        let mut request = client.get(url.clone());
        if resume_from > 0 {
            if let Ok(hv) = HeaderValue::from_str(&format!("bytes={}-", resume_from)) {
                request = request.header(RANGE, hv);
            }
        }
        let resp = match request.send().await {
            Ok(r) => r,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_MIN_MS)).await;
                continue;
            }
        };
        let status = resp.status();
        if !status.is_success() && status != StatusCode::PARTIAL_CONTENT {
            tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_MIN_MS)).await;
            continue;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resume_from > 0)
            .open(file_path)
            .await
            .context("Failed to open file for single stream")?;
        let buffer_size = if config.burst_mode { BURST_BUFFER_SIZE } else { DISK_WRITE_BUFFER_SIZE };
        let mut writer = BufWriter::with_capacity(buffer_size, file);
        if resume_from > 0 {
            progress_bar.inc(resume_from);
        }
        let mut stream = resp.bytes_stream();
        let mut success = false;
        while let Some(item) = stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(_) => break,
            };
            if let Err(e) = writer.write_all(&chunk).await {
                bail!("Disk write error: {}", e);
            }
            progress_bar.inc(chunk.len() as u64);
            success = true;
        }
        writer.flush().await?;
        if success {
            return Ok(());
        }
    }
}