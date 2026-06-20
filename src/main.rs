use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use org3d::{autogroup, db, extract, rename, scanner, server};

#[derive(Parser)]
#[command(name = "org3d", about = "Organise and browse 3D print files")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    #[arg(long, default_value = "org3d.db", global = true)]
    db: String,

    #[arg(long, default_value = "thumbs", global = true)]
    thumbs: String,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index all 3MF/STL files in a directory
    Scan { path: PathBuf },

    /// Start the web gallery
    Serve {
        path: PathBuf,
        #[arg(short, long, default_value_t = 3000)]
        port: u16,
    },

    /// Serve, scanning first only if the DB is empty (use --rescan to force)
    Run {
        path: PathBuf,
        #[arg(short, long, default_value_t = 3000)]
        port: u16,
        /// Re-scan even if the database already has indexed files
        #[arg(long)]
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "org3d=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    let thumb_dir = PathBuf::from(&cli.thumbs);

    match cli.cmd {
        Cmd::Scan { path } => {
            let conn = db::open(&cli.db)?;
            tracing::info!("scanning {}", path.display());
            let s = scanner::scan(&conn, &path, &thumb_dir)?;
            tracing::info!(
                "done: {} scanned, {} indexed, {} errors",
                s.scanned,
                s.inserted,
                s.errors
            );
        }

        Cmd::Serve { path, port } => {
            let cfg = server::AppConfig {
                db_path: cli.db,
                thumb_dir: cli.thumbs.clone(),
                files_root: path.to_string_lossy().to_string(),
                port,
                data_dir: cli.thumbs,
            };
            server::Application::build(cfg).await?.run().await?;
        }

        Cmd::Run { path, port, rescan } => {
            let conn = db::open(&cli.db)?;
            let existing = db::count(&conn).unwrap_or(0);
            if rescan || existing == 0 {
                tracing::info!("scanning {}", path.display());
                let s = scanner::scan(&conn, &path, &thumb_dir)?;
                tracing::info!(
                    "done: {} scanned, {} indexed, {} errors",
                    s.scanned,
                    s.inserted,
                    s.errors
                );
            } else {
                tracing::info!(
                    "{existing} models already indexed — skipping scan (use --rescan to force)"
                );
            }
            drop(conn);

            let cfg = server::AppConfig {
                db_path: cli.db,
                thumb_dir: cli.thumbs.clone(),
                files_root: path.to_string_lossy().to_string(),
                port,
                data_dir: cli.thumbs,
            };
            server::Application::build(cfg).await?.run().await?;
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
            let results = extract::extract_all(&path)?;
            let total: usize = results.iter().map(|r| r.files_extracted).sum();
            println!("Extracted {} ZIPs, {} files total.", results.len(), total);
            for r in &results {
                println!(
                    "  {} → {} ({} files)",
                    r.zip_path.display(),
                    r.dest_dir.display(),
                    r.files_extracted
                );
            }
            if scan {
                let conn = db::open(&cli.db)?;
                tracing::info!("re-scanning after extraction");
                let s = scanner::scan(&conn, &path, &thumb_dir)?;
                tracing::info!(
                    "done: {} scanned, {} indexed, {} errors",
                    s.scanned,
                    s.inserted,
                    s.errors
                );
            }
        }
    }

    Ok(())
}
