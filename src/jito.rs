use reqwest::{Client, StatusCode, header::{RANGE, CONNECTION}};
use anyhow::{Result, Context};
use std::time::{Duration, Instant};
use std::cmp::{min, max};
use serde::{Serialize, Deserialize};
use std::collections::VecDeque;

const PROBE_TIMEOUT: u64 = 6000;
const CHUNK_TINY: u64 = 524_288;
const CHUNK_SMALL: u64 = 1_048_576;
const CHUNK_MEDIUM: u64 = 4_194_304;
const CHUNK_LARGE: u64 = 16_777_216;
const CHUNK_MEGA: u64 = 33_554_432;
const ADAPTIVE_PROBE_ROUNDS: usize = 3;
const CONNECTION_POOL_SIZE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerCapability {
    RangeSupport,
    MultiRange,
    Http2,
    Http3,
    KeepAlive,
    Gzip,
    Brotli,
    EarlyHints,
    ServerTiming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThrottlePattern {
    None,
    HardLimit(usize),
    BandwidthCap(u64),
    HighJitter,
    Unstable,
    TimeBasedThrottle,
    AdaptiveThrottle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadStrategy {
    Aggressive,
    Stealth,
    Single,
    Adaptive,
    HybridStealth,
    ConnectionPooled,
    BBROptimized,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerProfile {
    pub domain: String,
    pub capabilities: Vec<ServerCapability>,
    pub throttle: ThrottlePattern,
    pub optimal_threads: usize,
    pub optimal_chunk: u64,
    pub avg_speed: u64,
    pub timestamp: u64,
    pub latency_avg: u64,
    pub packet_loss_rate: f64,
    pub connection_pool_benefit: f64,
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub caps: Vec<ServerCapability>,
    pub throttle: ThrottlePattern,
    pub chunk_size: u64,
    pub speed_est: u64,
    pub latency_ms: u64,
    pub packet_loss: f64,
    pub jitter_ms: u64,
    pub supports_pipelining: bool,
    pub optimal_concurrent_streams: usize,
}

#[derive(Debug, Clone)]
pub struct OptimizationConfig {
    pub strategy: DownloadStrategy,
    pub thread_count: usize,
    pub chunk_size: u64,
    pub concurrent_writes: usize,
    pub connection_pool_size: usize,
    pub adaptive_chunk_sizing: bool,
    pub enable_pipelining: bool,
    pub burst_mode: bool,
    pub prefetch_multiplier: f64,
}

impl ServerProfile {
    pub fn to_optimization_config(&self) -> OptimizationConfig {
        let base_strategy = match self.throttle {
            ThrottlePattern::None => DownloadStrategy::BBROptimized,
            ThrottlePattern::HardLimit(_) => DownloadStrategy::HybridStealth,
            ThrottlePattern::TimeBasedThrottle => DownloadStrategy::Adaptive,
            _ => DownloadStrategy::Adaptive,
        };
        
        let use_pool = self.connection_pool_benefit > 1.2;
        
        OptimizationConfig {
            strategy: if use_pool { DownloadStrategy::ConnectionPooled } else { base_strategy },
            thread_count: self.optimal_threads,
            chunk_size: self.optimal_chunk,
            concurrent_writes: min(self.optimal_threads, 16),
            connection_pool_size: if use_pool { CONNECTION_POOL_SIZE } else { 0 },
            adaptive_chunk_sizing: true,
            enable_pipelining: self.capabilities.contains(&ServerCapability::KeepAlive),
            burst_mode: self.throttle == ThrottlePattern::None,
            prefetch_multiplier: calculate_prefetch_ratio(self.latency_avg, self.avg_speed),
        }
    }
}

pub async fn probe_server(client: &Client, url: &url::Url) -> Result<ProbeResult> {
    let mut caps = Vec::new();
    let probe_start = Instant::now();
    
    let head = client.head(url.clone())
        .timeout(Duration::from_millis(PROBE_TIMEOUT))
        .header(CONNECTION, "keep-alive")
        .send().await.context("HEAD failed")?;
    
    let base_latency = probe_start.elapsed().as_millis() as u64;
    
    if head.version() == reqwest::Version::HTTP_2 {
        caps.push(ServerCapability::Http2);
    }
    
    if head.version() == reqwest::Version::HTTP_3 {
        caps.push(ServerCapability::Http3);
    }
    
    if let Some(v) = head.headers().get(reqwest::header::CONNECTION) {
        if v.to_str().unwrap_or("").to_lowercase().contains("keep-alive") {
            caps.push(ServerCapability::KeepAlive);
        }
    }
    
    if head.headers().get("103").is_some() || head.headers().get("early-hints").is_some() {
        caps.push(ServerCapability::EarlyHints);
    }
    
    if head.headers().get("server-timing").is_some() {
        caps.push(ServerCapability::ServerTiming);
    }
    
    let ranges = check_ranges(client, url).await;
    if ranges { 
        caps.push(ServerCapability::RangeSupport); 
    }
    
    let supports_pipelining = check_pipelining_support(client, url).await;
    
    if ranges && check_multirange(client, url).await {
        caps.push(ServerCapability::MultiRange);
    }
    
    let (max_threads, throttle, speed, latency, packet_loss, jitter) = if ranges {
        analyze_connection_quality_deep(client, url).await?
    } else {
        (1, ThrottlePattern::Unstable, 0, base_latency, 0.0, 0)
    };
    
    let optimal_streams = if caps.contains(&ServerCapability::Http2) {
        min(max_threads, 128)
    } else if caps.contains(&ServerCapability::Http3) {
        min(max_threads, 256)
    } else {
        max_threads
    };
    
    let chunk_size = calculate_chunk_size_advanced(speed, max_threads, latency, packet_loss);
    
    Ok(ProbeResult {
        caps,
        throttle,
        chunk_size,
        speed_est: speed,
        latency_ms: latency,
        packet_loss,
        jitter_ms: jitter,
        supports_pipelining,
        optimal_concurrent_streams: optimal_streams,
    })
}

async fn check_ranges(client: &Client, url: &url::Url) -> bool {
    client.get(url.clone())
        .header(RANGE, "bytes=0-0")
        .header(CONNECTION, "keep-alive")
        .timeout(Duration::from_millis(PROBE_TIMEOUT / 2))
        .send().await
        .map(|r| r.status() == StatusCode::PARTIAL_CONTENT)
        .unwrap_or(false)
}

async fn check_multirange(client: &Client, url: &url::Url) -> bool {
    client.get(url.clone())
        .header(RANGE, "bytes=0-1,2-3")
        .header(CONNECTION, "keep-alive")
        .timeout(Duration::from_millis(PROBE_TIMEOUT / 2))
        .send().await
        .map(|r| {
            r.headers().get(reqwest::header::CONTENT_TYPE)
                .and_then(|ct| ct.to_str().ok())
                .map(|s| s.contains("multipart/byteranges"))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

async fn check_pipelining_support(client: &Client, url: &url::Url) -> bool {
    let mut handles = Vec::new();
    
    for i in 0..3 {
        let c = client.clone();
        let u = url.clone();
        handles.push(tokio::spawn(async move {
            let start = Instant::now();
            let range = format!("bytes={}-{}", i * 1024, (i * 1024) + 1023);
            let result = c.get(u).header(RANGE, range).header(CONNECTION, "keep-alive").send().await;
            (result.is_ok(), start.elapsed())
        }));
    }
    
    let results = futures::future::join_all(handles).await;
    let successes: Vec<_> = results.into_iter().filter_map(|r| r.ok()).collect();
    
    if successes.len() < 3 { 
        return false; 
    }
    
    let total_time: Duration = successes.iter().map(|(_, d)| *d).sum();
    let avg_time = total_time / successes.len() as u32;
    avg_time < Duration::from_millis(500)
}

async fn analyze_connection_quality_deep(
    client: &Client, 
    url: &url::Url
) -> Result<(usize, ThrottlePattern, u64, u64, f64, u64)> {
    let target_sizes = [2, 4, 8, 16];
    let mut speeds = Vec::new();
    let mut latencies = Vec::new();
    let mut errors = 0;
    let mut base_speed = 0.0;
    let mut packet_losses = Vec::new();
    
    for round in 0..ADAPTIVE_PROBE_ROUNDS {
        for &count in &target_sizes {
            if round > 0 && count > 8 { continue; }
            
            let start = Instant::now();
            let mut handles = Vec::new();
            let probe_size = 20480u64 * (1 << round);
            
            for i in 0..count {
                let c = client.clone();
                let u = url.clone();
                handles.push(tokio::spawn(async move {
                    let range_start = i as u64 * probe_size;
                    let range_end = range_start + probe_size - 1;
                    let range = format!("bytes={}-{}", range_start, range_end);
                    let req_start = Instant::now();
                    let result = c.get(u).header(RANGE, range).header(CONNECTION, "keep-alive").send().await;
                    (result, req_start.elapsed())
                }));
            }
            
            let results = futures::future::join_all(handles).await;
            let mut success = 0;
            let mut total_latency = Duration::ZERO;
            
            for r in results {
                if let Ok((Ok(resp), latency)) = r {
                    if resp.status().is_success() { 
                        success += 1;
                        total_latency += latency;
                    }
                }
            }
            
            if success < count {
                errors += 1;
                packet_losses.push(1.0 - (success as f64 / count as f64));
                if errors > 2 { 
                    let avg_loss = packet_losses.iter().sum::<f64>() / packet_losses.len() as f64;
                    return Ok((max(4, count / 2), ThrottlePattern::HardLimit(success), 
                              base_speed as u64, 100, avg_loss, 50)); 
                }
            } else {
                packet_losses.push(0.0);
            }
            
            let dur = start.elapsed().as_secs_f64();
            if dur > 0.0 {
                let speed = (success as f64 * probe_size as f64) / dur;
                if count == 2 { base_speed = speed; }
                
                if count > 2 && base_speed > 0.0 {
                    let efficiency = speed / (count as f64);
                    let scaling = efficiency / base_speed;
                    
                    if scaling < 0.55 {
                        let avg_loss = packet_losses.iter().sum::<f64>() / packet_losses.len() as f64;
                        return Ok((max(4, count / 2), ThrottlePattern::BandwidthCap(speed as u64), 
                                  speed as u64, (total_latency.as_millis() / success.max(1) as u128) as u64,
                                  avg_loss, calculate_jitter(&latencies)));
                    }
                }
                
                speeds.push(speed);
                latencies.push((total_latency.as_millis() / success.max(1) as u128) as u64);
            }
        }
    }
    
    let avg_latency = if !latencies.is_empty() {
        latencies.iter().sum::<u64>() / latencies.len() as u64
    } else { 50 };
    
    let avg_loss = if !packet_losses.is_empty() {
        packet_losses.iter().sum::<f64>() / packet_losses.len() as f64
    } else { 0.0 };
    
    let jitter = calculate_jitter(&latencies);
    let final_speed = *speeds.last().unwrap_or(&0.0);
    
    let max_recommended = if avg_loss < 0.01 && jitter < 30 { 32 }
                         else if avg_loss < 0.05 { 16 }
                         else { 8 };
    
    Ok((min(max_recommended, 32), 
        if avg_loss < 0.01 { ThrottlePattern::None } else { ThrottlePattern::AdaptiveThrottle },
        final_speed as u64, avg_latency, avg_loss, jitter))
}

fn calculate_jitter(latencies: &[u64]) -> u64 {
    if latencies.len() < 2 { return 0; }
    let mut diffs = Vec::new();
    for i in 1..latencies.len() {
        diffs.push(latencies[i].abs_diff(latencies[i-1]));
    }
    diffs.iter().sum::<u64>() / diffs.len() as u64
}

fn calculate_chunk_size_advanced(speed: u64, threads: usize, latency: u64, packet_loss: f64) -> u64 {
    if speed == 0 { return CHUNK_SMALL; }
    
    let base_chunk = match latency {
        l if l < 20 => CHUNK_MEGA,
        l if l < 50 => CHUNK_LARGE,
        l if l < 100 => CHUNK_MEDIUM,
        l if l < 200 => CHUNK_SMALL,
        _ => CHUNK_TINY,
    };
    
    let loss_factor = match packet_loss {
        p if p < 0.01 => 1.5,
        p if p < 0.05 => 1.0,
        _ => 0.6,
    };
    
    let split = speed / max(1, threads) as u64;
    let latency_adjusted = (split * (latency + 50) / 100) as f64;
    let final_size = (latency_adjusted * loss_factor) as u64;
    
    final_size.clamp(base_chunk / 4, base_chunk * 2)
}

fn calculate_prefetch_ratio(latency: u64, speed: u64) -> f64 {
    let base_ratio = 2.0;
    let latency_factor = if latency < 30 { 1.0 } else { 1.0 + (latency as f64 / 100.0) };
    let speed_factor = if speed > 100_000_000 { 1.5 } else { 1.0 };
    (base_ratio * latency_factor * speed_factor).min(5.0)
}

pub fn select_strategy(probe: &ProbeResult, threads: usize) -> OptimizationConfig {
    if !probe.caps.contains(&ServerCapability::RangeSupport) {
        return OptimizationConfig {
            strategy: DownloadStrategy::Single,
            thread_count: 1,
            chunk_size: CHUNK_LARGE,
            concurrent_writes: 1,
            connection_pool_size: 0,
            adaptive_chunk_sizing: false,
            enable_pipelining: false,
            burst_mode: false,
            prefetch_multiplier: 1.0,
        };
    }
    
    let optimal = min(probe.optimal_concurrent_streams, threads);
    
    let strategy = match probe.throttle {
        ThrottlePattern::None if probe.packet_loss < 0.01 => DownloadStrategy::BBROptimized,
        ThrottlePattern::None => DownloadStrategy::Aggressive,
        ThrottlePattern::HardLimit(_) => DownloadStrategy::HybridStealth,
        ThrottlePattern::TimeBasedThrottle => DownloadStrategy::Stealth,
        _ => DownloadStrategy::Adaptive,
    };
    
    let use_connection_pool = probe.supports_pipelining && 
                              probe.caps.contains(&ServerCapability::KeepAlive) &&
                              probe.latency_ms < 100;
    
    let burst_mode = probe.throttle == ThrottlePattern::None && 
                     probe.packet_loss < 0.01 && 
                     probe.jitter_ms < 30;
    
    OptimizationConfig {
        strategy,
        thread_count: optimal,
        chunk_size: probe.chunk_size,
        concurrent_writes: min(optimal, 16),
        connection_pool_size: if use_connection_pool { CONNECTION_POOL_SIZE } else { 8 },
        adaptive_chunk_sizing: true,
        enable_pipelining: probe.supports_pipelining,
        burst_mode,
        prefetch_multiplier: calculate_prefetch_ratio(probe.latency_ms, probe.speed_est),
    }
}

pub async fn load_cached_profile(domain: &str) -> Option<ServerProfile> {
    let path = std::env::temp_dir().join(format!("jito_{}.json", clean_domain(domain)));
    
    if let Ok(data) = tokio::fs::read(&path).await {
        if let Ok(p) = serde_json::from_slice::<ServerProfile>(&data) {
            let age = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() - p.timestamp;
            
            if age < 43200 {
                return Some(p);
            }
        }
    }
    None
}

pub async fn save_profile_cache(profile: &ServerProfile) -> Result<()> {
    let path = std::env::temp_dir().join(format!("jito_{}.json", clean_domain(&profile.domain)));
    let data = serde_json::to_vec_pretty(profile)?;
    tokio::fs::write(path, data).await?;
    Ok(())
}

fn clean_domain(d: &str) -> String {
    d.chars().filter(|c| c.is_alphanumeric() || *c == '.' || *c == '-').collect()
}

pub fn probe_result_to_server_profile(domain: String, probe: &ProbeResult, config: &OptimizationConfig) -> ServerProfile {
    ServerProfile {
        domain,
        capabilities: probe.caps.clone(),
        throttle: probe.throttle,
        optimal_threads: config.thread_count,
        optimal_chunk: config.chunk_size,
        avg_speed: probe.speed_est,
        timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        latency_avg: probe.latency_ms,
        packet_loss_rate: probe.packet_loss,
        connection_pool_benefit: if config.connection_pool_size > 0 { 1.5 } else { 1.0 },
    }
}

pub fn print_probe_summary(res: &ProbeResult, conf: &OptimizationConfig) {
    println!(" Strategy: {:?} | Threads: {} | Chunk: {:.2}MB", 
             conf.strategy, conf.thread_count, conf.chunk_size as f64 / 1024.0 / 1024.0);
    println!(" Caps: {:?}", res.caps);
    println!(" Throttle: {:?} | Latency: {}ms | Loss: {:.2}% | Jitter: {}ms", 
             res.throttle, res.latency_ms, res.packet_loss * 100.0, res.jitter_ms);
    println!(" Pool: {} | Pipelining: {} | Burst: {} | Adaptive: {}",
             conf.connection_pool_size, conf.enable_pipelining, conf.burst_mode, conf.adaptive_chunk_sizing);
    println!(" Prefetch: {:.1}x | Speed Est: {:.2}MB/s", 
             conf.prefetch_multiplier, res.speed_est as f64 / 1024.0 / 1024.0);
}