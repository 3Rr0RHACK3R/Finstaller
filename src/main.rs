use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

mod net;
mod logic;
mod jito;

use clap::Parser;
use reqwest::{Client, redirect::Policy};
use std::path::PathBuf;
use std::time::Duration as StdDuration;
use indicatif::{ProgressBar, ProgressStyle, MultiProgress};
use anyhow::{Result, Context};

#[derive(Parser)]
#[command(author, version, about = "A blazingly fast concurrent downloader with resume capability and integrity checks")]
struct Config {
    link: String,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(short, long, default_value_t = 16)]
    threads: usize,
    #[arg(short = 't', long, default_value_t = 30.0)]
    timeout: f32,
    #[arg(short = 'r', long, default_value_t = 10)]
    retries: u32,
    #[arg(short = 'c', long, default_value_t = 4)]
    concurrent_writes: usize,
    #[arg(long, default_value_t = 10737418240)]
    max_size: u64,
    #[arg(long)]
    no_resume: bool,
    #[arg(long)]
    jito: bool,
    #[arg(long)]
    no_cache: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();
    
    logic::validate_thread_count(config.threads)?;
    logic::validate_timeout(config.timeout)?;
    logic::validate_concurrent_writes(config.concurrent_writes)?;
    logic::validate_retries(config.retries)?;
    logic::validate_url(&config.link)?;
    
    let parsed_url = url::Url::parse(&config.link)
        .context("Failed to parse URL")?;
    
    let timeout = StdDuration::from_secs_f32(config.timeout);
    let domain = parsed_url.host_str().unwrap_or("unknown").to_string();
    
    let optimization_config = if config.jito {
        println!("\nJITO (Just-In-Time Optimizer) enabled");
        let cached_profile = if !config.no_cache {
            jito::load_cached_profile(&domain).await
        } else {
            None
        };
        
        let jito_config = if let Some(profile) = cached_profile {
            println!("Using cached server profile (age: <12h)");
            profile.to_optimization_config()
        } else {
            println!("Probing server capabilities...");
            let temp_client = Client::builder()
                .timeout(timeout)
                .redirect(Policy::limited(10))
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
                .build()
                .context("Failed to build probe client")?;
            
            let probe_result = jito::probe_server(&temp_client, &parsed_url).await?;
            let selected_config = jito::select_strategy(&probe_result, config.threads);
            jito::print_probe_summary(&probe_result, &selected_config);
            
            if !config.no_cache {
                let profile = jito::probe_result_to_server_profile(
                    domain.clone(),
                    &probe_result,
                    &selected_config,
                );
                if let Err(e) = jito::save_profile_cache(&profile).await {
                    eprintln!("Failed to cache profile: {}", e);
                }
            }
            selected_config
        };
        Some(jito_config)
    } else {
        None
    };
    
    let (final_threads, pool_size, use_http2_optimizations) = if let Some(ref opt_config) = optimization_config {
        (
            opt_config.thread_count, 
            opt_config.connection_pool_size.max(config.threads * 2),
            opt_config.enable_pipelining
        )
    } else {
        (config.threads, config.threads * 2, true)
    };
    
    let client = Client::builder()
        .timeout(timeout)
        .redirect(Policy::limited(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .pool_max_idle_per_host(pool_size)
        .pool_idle_timeout(StdDuration::from_secs(90))
        .tcp_keepalive(StdDuration::from_secs(60))
        .http2_adaptive_window(use_http2_optimizations)
        .http2_keep_alive_interval(StdDuration::from_secs(10))
        .http2_keep_alive_timeout(StdDuration::from_secs(20))
        .build()
        .context("Failed to build HTTP client")?;
    
    let output_path = match config.output {
        Some(path) => path,
        None => {
            let filename = logic::extract_filename_from_url(&parsed_url);
            PathBuf::from(filename)
        }
    };
    
    logic::validate_path(&output_path)?;
    
    println!("Fetching file information...");
    let download_info = net::get_file_info(&client, &parsed_url).await
        .context("Failed to fetch file metadata")?;
    
    logic::validate_file_size(download_info.total_size, config.max_size)?;
    
    if download_info.total_size == 0 {
        println!("Warning: Could not determine file size (Content-Length missing)");
    } else {
        println!("File size: {}", logic::format_file_size(download_info.total_size));
    }
    
    let (final_concurrent_writes, chunk_size, adaptive_enabled, burst_enabled) = if let Some(ref opt_config) = optimization_config {
        (
            opt_config.concurrent_writes,
            opt_config.chunk_size,
            opt_config.adaptive_chunk_sizing,
            opt_config.burst_mode
        )
    } else {
        (
            config.concurrent_writes, 
            logic::calculate_optimal_chunk_size(download_info.total_size, final_threads),
            false,
            false
        )
    };
    
    if !download_info.supports_ranges && final_threads > 1 {
        println!("Warning: Server does not support range requests. Fallback to single-stream.");
    }
    
    let temp_path = output_path.with_extension("part");
    println!("Output: {}", output_path.display());
    println!("Threads: {}", final_threads);
    println!("Concurrent Writes: {}", final_concurrent_writes);
    println!("Connection Pool: {}", pool_size);
    
    if adaptive_enabled {
        println!("Adaptive Chunk Sizing: ENABLED");
    }
    if burst_enabled {
        println!("Burst Mode: ENABLED (optimal conditions detected)");
    }
    
    let multi_progress = MultiProgress::new();
    let main_progress_bar = if download_info.total_size > 0 {
        multi_progress.add(ProgressBar::new(download_info.total_size))
    } else {
        multi_progress.add(ProgressBar::new_spinner())
    };
    
    main_progress_bar.enable_steady_tick(StdDuration::from_millis(100));
    let progress_style = ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}) ({eta})"
    );
    
    if let Ok(style) = progress_style {
        main_progress_bar.set_style(style.progress_chars("█░▒"));
    }
    
    let download_config = net::DownloadConfig {
        allow_resume: !config.no_resume,
        max_concurrent_writes: final_concurrent_writes,
        max_retries: config.retries,
        burst_mode: burst_enabled,
    };
    
    let use_multi_stream = logic::should_use_multi_stream(
        download_info.total_size,
        final_threads
    ) && download_info.supports_ranges;
    
    if use_multi_stream {
        println!("Starting multi-threaded download (Direct I/O)...");
        net::download_multi_stream(
            &client,
            &parsed_url,
            download_info.total_size,
            chunk_size,
            &temp_path,
            main_progress_bar.clone(),
            &download_config,
        ).await?;
    } else {
        if download_info.total_size > 0 {
            println!("Starting single-stream download...");
        } else {
            println!("Starting download (unknown size)...");
        }
        net::download_single_stream(
            &client,
            &parsed_url,
            &temp_path,
            &main_progress_bar,
            &download_config,
        ).await?;
    }
    
    main_progress_bar.finish_with_message("Download complete");
    
    if download_info.total_size > 0 {
        logic::verify_file_size(&temp_path, download_info.total_size).await?;
    }
    
    logic::finalize_download(&temp_path, &output_path).await?;
    println!("\nDownload completed successfully!");
    println!("File saved to: {}", output_path.display());
    
    Ok(())
}