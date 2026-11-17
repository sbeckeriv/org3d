use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::net::IpAddr;
use std::path::PathBuf;

use org3d::{autogroup, db, extract, rename, scanner, schedule, server};

/// Every option can also be set with the ORG3D_* environment variable shown in
/// `--help`, which is how the Docker image is configured.
#[derive(Parser)]
#[command(name = "org3d", about = "Organise and browse 3D print files", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// SQLite database file. Keep it on fast, always-on storage (SSD/cache).
    #[arg(long, env = "ORG3D_DB", default_value = "org3d.db", global = true)]
    db: String,

    /// Thumbnail directory.
    #[arg(long, env = "ORG3D_THUMBS", default_value = "thumbs", global = true)]
    thumbs: String,
}

#[derive(Args, Clone)]
struct ServeOpts {
    /// Library folder to browse.
    #[arg(env = "ORG3D_LIBRARY")]
    path: PathBuf,

    #[arg(short, long, env = "ORG3D_PORT", default_value_t = 3000)]
    port: u16,

    /// Address to listen on. Use 127.0.0.1 to keep it off the LAN.
    #[arg(long, env = "ORG3D_BIND", default_value = "0.0.0.0")]
    bind: IpAddr,

    /// Preview cache directory (converted STLs for the 3D viewer).
    #[arg(long, env = "ORG3D_CACHE", default_value = "cache")]
    cache: String,

    /// Preview cache size cap in MiB. 0 disables it.
    #[arg(long, env = "ORG3D_PREVIEW_CACHE_MB", default_value_t = 1024)]
    preview_cache_mb: u64,

    /// Daily rescan time, HH:MM in local time (honours TZ). Unset = manual only.
    #[arg(long, env = "ORG3D_RESCAN_AT", value_parser = schedule::parse_time)]
    rescan_at: Option<jiff::civil::Time>,

    /// Days a vanished file stays hidden-but-restorable before it is purged
    /// (losing its project assignment). 0 = never purge.
    #[arg(long, env = "ORG3D_PURGE_MISSING_DAYS", default_value_t = 30)]
    purge_missing_days: u32,

    /// Require a login, as `user:password` (password ≥ 8 chars).
    /// Strongly recommended when the port is reachable from your LAN.
    #[arg(
        long,
        env = "ORG3D_AUTH",
        hide_env_values = true,
        value_parser = org3d::auth::Credentials::parse
    )]
    auth: Option<org3d::auth::Credentials>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index all 3MF/STL files in a directory
    Scan {
        path: PathBuf,
        /// Allow marking a large share of the library missing in one scan
        /// (normally refused, since it usually means a disk is offline).
        #[arg(long)]
        allow_mass_missing: bool,
        #[arg(long, env = "ORG3D_PURGE_MISSING_DAYS", default_value_t = 30)]
        purge_missing_days: u32,
    },

    /// Start the web gallery
    Serve(ServeOpts),

    /// Serve; if the database is empty (or --rescan), scan in the background
    Run {
        #[command(flatten)]
        opts: ServeOpts,
        /// Re-scan on startup even if the database already has indexed files
        #[arg(long, env = "ORG3D_RESCAN_ON_START")]
        rescan: bool,
    },

    /// Preview or apply filename renames based on 3MF metadata
    Rename {
        path: PathBuf,
        /// Actually rename files (default is dry-run)
        #[arg(long)]
        apply: bool,
    },

    /// Extract ZIP archives and re-scan
    Extract {
        path: PathBuf,
        /// Re-index extracted files immediately
        #[arg(long)]
        scan: bool,
    },

    /// Auto-group models into projects based on shared folders
    Autogroup,

    /// Exit 0 if the server answers /health_check (for Docker HEALTHCHECK)
    Healthcheck {
        #[arg(short, long, env = "ORG3D_PORT", default_value_t = 3000)]
        port: u16,
    },
}

fn app_config(cli_db: &str, cli_thumbs: &str, o: &ServeOpts) -> server::AppConfig {
    server::AppConfig {
        db_path: cli_db.to_string(),
        thumb_dir: cli_thumbs.to_string(),
        cache_dir: o.cache.clone(),
        files_root: o.path.to_string_lossy().to_string(),
        bind: o.bind,
        port: o.port,
        data_dir: std::path::Path::new(cli_db)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        mode: server::Mode::Server,
        rescan_at: o.rescan_at,
        preview_cache_bytes: o.preview_cache_mb.saturating_mul(1024 * 1024),
        purge_missing_days: o.purge_missing_days,
        auth: o.auth.clone(),
    }
}

/// Minimal HTTP probe with std only, so the runtime image needs no curl.
fn healthcheck(port: u16) -> Result<()> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    s.write_all(b"GET /health_check HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut buf = [0u8; 32];
    let n = s.read(&mut buf)?;
    let line = String::from_utf8_lossy(&buf[..n]);
    anyhow::ensure!(line.contains(" 200 "), "unhealthy: {line}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "org3d=info".parse().expect("valid filter")),
        )
        // No colour escape codes in `docker logs` / Unraid's log viewer.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let cli = Cli::parse();
    let thumb_dir = PathBuf::from(&cli.thumbs);

    match cli.cmd {
        Cmd::Healthcheck { port } => healthcheck(port)?,

        Cmd::Scan {
            path,
            allow_mass_missing,
            purge_missing_days,
        } => {
            let conn = db::open(&cli.db)?;
            tracing::info!("scanning {}", path.display());
            let opts = scanner::ScanOptions {
                purge_missing_after_days: purge_missing_days,
                allow_mass_missing,
                ..Default::default()
            };
            let stats = scanner::scan_with(&conn, &path, &thumb_dir, &opts)?;
            for w in &stats.warnings {
                eprintln!("warning: {w}");
            }
            for f in &stats.failed {
                eprintln!("failed: {f}");
            }
        }

        Cmd::Serve(opts) => {
            server::Application::build(app_config(&cli.db, &cli.thumbs, &opts))
                .await?
                .run()
                .await?;
        }

        Cmd::Run { opts, rescan } => {
            let existing = {
                let conn = db::open(&cli.db)?;
                db::count_all(&conn).unwrap_or(0)
            };
            let app = server::Application::build(app_config(&cli.db, &cli.thumbs, &opts)).await?;
            // Scan in the background so the UI is reachable immediately,
            // instead of after a full first scan of a large library.
            if rescan || existing == 0 {
                app.start_scan();
            } else {
                tracing::info!(
                    "{existing} models already indexed — skipping startup scan (use --rescan to force)"
                );
            }
            app.run().await?;
        }

        Cmd::Rename { path, apply } => {
            let conn = db::open(&cli.db)?;
            let candidates = rename::plan(&conn, &path)?;
            if candidates.is_empty() {
                println!("Nothing to rename.");
                return Ok(());
            }
            for c in &candidates {
                println!("{} → {}", c.old_name, c.new_name);
            }
            if apply {
                let n = rename::apply(&conn, &candidates)?;
                println!("\nRenamed {n} files.");
            } else {
                println!(
                    "\n{} files would be renamed. Run with --apply to proceed.",
                    candidates.len()
                );
            }
        }

        Cmd::Autogroup => {
            let conn = db::open(&cli.db)?;
            let stats = autogroup::run(&conn)?;
            println!(
                "Created {} projects, assigned {} models.",
                stats.projects_created, stats.models_assigned
            );
            if stats.models_assigned == 0 {
                println!(
                    "Nothing to group — run 'scan' first, or all models are already assigned."
                );
            }
        }

        Cmd::Extract { path, scan } => {
            tracing::info!("extracting ZIPs in {}", path.display());
            let results = extract::extract_all(&path)
                .with_context(|| format!("extracting in {}", path.display()))?;
            let total: usize = results.iter().map(|r| r.files_extracted).sum();
            let done = results.iter().filter(|r| r.extracted()).count();
            println!("Extracted {done} ZIPs, {total} files total.");
            for r in &results {
                match &r.error {
                    Some(e) => println!("  {} FAILED: {e}", r.zip_path.display()),
                    None if r.skipped => {}
                    None => println!(
                        "  {} → {} ({} files)",
                        r.zip_path.display(),
                        r.dest_dir.display(),
                        r.files_extracted
                    ),
                }
            }
            if scan {
                let conn = db::open(&cli.db)?;
                tracing::info!("re-scanning after extraction");
                scanner::scan(&conn, &path, &thumb_dir)?;
            }
        }
    }

    Ok(())
}
