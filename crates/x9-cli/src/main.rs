//! x9 — small CLI over x9-client. Points at a real X9 or the mock via --base.

use anyhow::Result;
use clap::{Parser, Subcommand};
use x9_client::{models::ScanRequest, Client, DEFAULT_BASE};

#[derive(Parser)]
#[command(name = "x9", about = "Trimble X9 controller (smoke-test CLI)")]
struct Cli {
    /// scanner API base URL (default: the X9 over USB at 192.168.42.1:34567).
    #[arg(long, default_value = DEFAULT_BASE, env = "X9_BASE")]
    base: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// GET /status (battery, leveling, storage, imu, wifi, errors)
    Status,
    /// Leveling state (read from /status)
    Level,
    /// GET /scans
    Scans,
    /// GET /scans/{id}
    Scan { id: i64 },
    /// GET /scans/modes (available scan modes)
    ScanModes,
    /// GET /projects
    Projects,
    /// GET /tasks
    Tasks,
    /// GET /errors
    Errors,
    /// GET /diagnostics
    Diagnostics,
    /// GET /tzfs/{id} metadata
    Tzf { id: i64 },
    /// Download a scan's TZF point cloud: GET /tzfs/{id}/file
    DownloadTzf { id: i64, dest: String },
    /// Download a TZF preview JPEG: GET /tzfs/{id}/preview
    Preview { id: i64, dest: String },
    /// Bulk-download TZFs (+ optional previews) into a directory tree
    Pull {
        #[arg(long, default_value = "x9-pull")]
        out: String,
        /// Only scans whose file path contains this (e.g. a project name)
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        previews: bool,
        /// List what would be downloaded without downloading
        #[arg(long)]
        dry_run: bool,
    },
    /// POST /scans (ACTUATING — rotates head + fires laser). Pass BOTH
    /// --project-name and --project-uuid to target a project.
    LaunchScan {
        #[arg(long)]
        project_name: Option<String>,
        #[arg(long)]
        project_uuid: Option<String>,
        #[arg(long, default_value = "STD_L0")]
        scan_mode: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let c = Client::connect(&cli.base)?;
    match cli.cmd {
        Cmd::Status => pp(&c.status().await?)?,
        Cmd::Level => pp(&c.leveling().await?)?,
        Cmd::Scans => pp(&c.scans().await?)?,
        Cmd::Scan { id } => pp(&c.scan(id).await?)?,
        Cmd::ScanModes => pp(&c.scan_modes().await?)?,
        Cmd::Projects => pp(&c.projects().await?)?,
        Cmd::Tasks => pp(&c.tasks().await?)?,
        Cmd::Errors => pp(&c.errors().await?)?,
        Cmd::Diagnostics => pp(&c.diagnostics().await?)?,
        Cmd::Tzf { id } => pp(&c.tzf(id).await?)?,
        Cmd::DownloadTzf { id, dest } => {
            let n = c.download_tzf(id, std::path::Path::new(&dest)).await?;
            println!("downloaded {n} bytes -> {dest}");
        }
        Cmd::Preview { id, dest } => {
            let n = c.download_tzf_preview(id, std::path::Path::new(&dest)).await?;
            println!("downloaded {n} bytes -> {dest}");
        }
        Cmd::Pull { out, project, limit, previews, dry_run } => {
            let scans = c.scans().await?;
            let mut sel: Vec<_> = scans
                .into_iter()
                .filter_map(|s| {
                    let t = s.tzf?;
                    let file = t.file.clone()?;
                    let id = t.id?;
                    match &project {
                        Some(p) if !file.contains(p.as_str()) => None,
                        _ => Some((id, file, t.file_size.unwrap_or(0))),
                    }
                })
                .collect();
            sel.sort_by(|a, b| a.1.cmp(&b.1));
            if let Some(n) = limit { sel.truncate(n); }
            let total: u64 = sel.iter().map(|x| x.2).sum();
            println!("{} scans selected, {:.1} MB total", sel.len(), total as f64 / 1e6);
            for (id, file, size) in &sel {
                let dest = std::path::Path::new(&out).join(file);
                if dry_run {
                    println!("  [dry] tzf {id} -> {} ({:.1} MB)", dest.display(), *size as f64 / 1e6);
                    continue;
                }
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let n = c.download_tzf(*id, &dest).await?;
                println!("  pulled {} ({n} bytes)", dest.display());
                if previews {
                    let pdest = dest.with_extension("preview.jpg");
                    match c.download_tzf_preview(*id, &pdest).await {
                        Ok(pn) => println!("    preview {pn} bytes"),
                        Err(e) => println!("    preview failed: {e}"),
                    }
                }
            }
        }
        Cmd::LaunchScan { project_name, project_uuid, scan_mode } => {
            let req = ScanRequest {
                project_name,
                project_uuid,
                scan_mode: Some(scan_mode),
                ..Default::default()
            };
            pp(&c.launch_scan(&req).await?)?
        }
    }
    Ok(())
}

fn pp<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}
