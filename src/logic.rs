use std::path::PathBuf;
use anyhow::{Result, bail, Context};
pub fn validate_url(url_string: &str) -> Result<()> {
    let parsed_url = url::Url::parse(url_string)
        .context("Invalid URL format")?;
    let scheme = parsed_url.scheme();
    if scheme != "https" && scheme != "http" {
        bail!("Only HTTP and HTTPS protocols are allowed");
    }
    if let Some(host) = parsed_url.host_str() {
        if host == "localhost" || host.starts_with("127.") || host == "::1" {
            bail!("Localhost/Loopback addresses are restricted");
        }
        if host.starts_with("192.168.") || host.starts_with("10.") || host.starts_with("169.254.") {
            bail!("Private network addresses are restricted");
        }
        if host.starts_with("172.") {
            if let Some(second_octet) = host.split('.').nth(1) {
                if let Ok(num) = second_octet.parse::<u8>() {
                    if num >= 16 && num <= 31 {
                        bail!("Private network addresses are restricted");
                    }
                }
            }
        }
    }
    Ok(())
}
pub fn validate_path(path: &PathBuf) -> Result<()> {
    let path_string = path.to_string_lossy();
    if path_string.contains("..") {
        bail!("Path traversal detected");
    }
    if cfg!(windows) {
        if path_string.contains('\\') && path_string.contains('/') {
        }
        let dangerous_chars = ['<', '>', ':', '"', '|', '?', '*', '\0'];
        if path_string.chars().any(|c| dangerous_chars.contains(&c) && c != ':' && c != '\\') {
             bail!("Path contains invalid characters");
        }
    }
    Ok(())
}
pub fn validate_thread_count(threads: usize) -> Result<()> {
    if threads == 0 || threads > 128 {
        bail!("Thread count must be between 1 and 128");
    }
    Ok(())
}
pub fn validate_file_size(size: u64, max_size: u64) -> Result<()> {
    if size > max_size {
        bail!("File size {} exceeds maximum allowed {}", size, max_size);
    }
    Ok(())
}
pub fn validate_timeout(timeout: f32) -> Result<()> {
    if timeout <= 0.0 || timeout > 7200.0 {
        bail!("Timeout must be between 0 and 7200 seconds");
    }
    Ok(())
}
pub fn validate_concurrent_writes(concurrent_writes: usize) -> Result<()> {
    if concurrent_writes == 0 || concurrent_writes > 64 {
        bail!("Concurrent writes must be between 1 and 64");
    }
    Ok(())
}
pub fn validate_retries(retries: u32) -> Result<()> {
    if retries > 100 {
        bail!("Retries cannot exceed 100");
    }
    Ok(())
}
pub fn extract_filename_from_url(url: &url::Url) -> String {
    let filename = url.path_segments()
        .and_then(|segments| segments.last())
        .filter(|name| !name.is_empty())
        .unwrap_or("download");
    let decoded = urlencoding::decode(filename).unwrap_or(std::borrow::Cow::Borrowed(filename));
    let sanitized: String = decoded.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if sanitized.is_empty() { "download.bin".to_string() } else { sanitized }
}
pub async fn verify_file_size(file_path: &std::path::Path, expected_size: u64) -> Result<()> {
    if expected_size == 0 { return Ok(()); }
    let metadata = tokio::fs::metadata(file_path).await
        .context("Failed to read file metadata for verification")?;
    if metadata.len() != expected_size {
        bail!("Integrity Check Failed: Expected {} bytes, found {} bytes", expected_size, metadata.len());
    }
    Ok(())
}
pub fn should_use_multi_stream(total_size: u64, threads: usize) -> bool {
    total_size > 5_242_880 && threads > 1
}
pub fn calculate_optimal_chunk_size(total_size: u64, threads: usize) -> u64 {
    let target_chunks_per_thread = 4;
    let ideal_count = threads * target_chunks_per_thread;
    let size = total_size / ideal_count as u64;
    size.clamp(1_048_576, 67_108_864)
}
pub fn format_file_size(bytes: u64) -> String {
    const UNIT: f64 = 1024.0;
    if bytes < 1024 { return format!("{} B", bytes); }
    let exp = (bytes as f64).ln() / UNIT.ln();
    let pre = "KMGTPE".chars().nth(exp as usize - 1).unwrap_or('?');
    format!("{:.2} {}B", (bytes as f64) / UNIT.powi(exp as i32), pre)
}
pub async fn finalize_download(
    temp_path: &std::path::Path,
    final_path: &std::path::Path,
) -> Result<()> {
    if final_path.exists() {
        tokio::fs::remove_file(final_path).await
            .context("Failed to remove existing output file")?;
    }
    tokio::fs::rename(temp_path, final_path).await
        .context("Failed to rename partial file to final name")?;
    Ok(())
}