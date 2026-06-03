use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, LeaveAlternateScreen},
};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::ffi::OsString;
use std::io;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// Use modules from the library crate
use octorus::app::RefreshRequest;
use octorus::{app, cache, config, github, headless, loader, syntax};

// init/update/migrate are only used by the binary, not needed for benchmarks
mod init;
mod local_comments;
mod migrate;
mod update;

#[derive(Parser, Debug)]
#[command(name = "or")]
#[command(
    about = "TUI for GitHub PRs, issues, local diffs, and Git Ops. AI-powered automated review cycles."
)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Repository name (e.g., "owner/repo"). Auto-detected from current directory if omitted.
    #[arg(short, long)]
    repo: Option<String>,

    /// Pull request number. Shows PR list if flag only (no number).
    #[arg(short, long, conflicts_with = "local", num_args = 0..=1, default_missing_value = "0")]
    pr: Option<u32>,

    /// Start AI Rally mode directly
    #[arg(long, default_value = "false")]
    ai_rally: bool,

    /// Force AI Rally review-only (proposal iteration) mode. Use --review-only=true.
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true", requires = "ai_rally")]
    review_only: Option<bool>,

    /// Show local git diff against current HEAD (no GitHub PR fetch)
    #[arg(long, default_value = "false", conflicts_with = "pr")]
    local: bool,

    /// Issue number. Shows issue detail directly if provided, issue list if flag only.
    #[arg(short, long, conflicts_with_all = ["pr", "local"], num_args = 0..=1, default_missing_value = "0")]
    issue: Option<u32>,

    /// Start in Git Ops view directly
    #[arg(long, default_value = "false")]
    git_ops: bool,

    /// Auto-focus changed file when local diff updates (for local mode)
    #[arg(long, default_value = "false")]
    auto_focus: bool,

    /// Working directory for AI agents (default: current directory)
    #[arg(long)]
    working_dir: Option<String>,

    /// Accept local .octorus/ overrides for AI settings in headless mode.
    /// Without this flag, headless AI Rally will refuse to run if the local config
    /// overrides security-sensitive AI keys or local prompt files are detected in .octorus/prompts/.
    #[arg(long, default_value = "false")]
    accept_local_overrides: bool,

    /// Write JSON result to a file (in addition to stdout).
    /// Useful when running as a background task where stdout may not be captured.
    #[arg(long)]
    output: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize configuration files and prompt templates
    Init {
        /// Force overwrite existing files
        #[arg(long, default_value = "false")]
        force: bool,
        /// Create local .octorus/ config in project root
        #[arg(long, default_value = "false")]
        local: bool,
    },
    /// Remove AI Rally session data
    Clean,
    /// Show saved local comments for the current worktree
    LocalComments {
        /// Repository name (e.g., "owner/repo"). Auto-detected if omitted.
        #[arg(short, long)]
        repo: Option<String>,

        /// Working directory used to scope local comments (defaults to current directory)
        #[arg(long)]
        working_dir: Option<String>,

        /// Maximum number of newest comments to show
        #[arg(short, long, default_value_t = 20)]
        limit: usize,

        /// Print JSON instead of plain text
        #[arg(long, default_value = "false")]
        json: bool,

        /// Show all comments, including resolved ones
        #[arg(long, conflicts_with_all = ["resolved", "purge"])]
        all: bool,

        /// Show only resolved comments
        #[arg(long, conflicts_with_all = ["all", "purge"])]
        resolved: bool,

        /// Delete all local comments for this (repo, working_dir) pair
        #[arg(long, default_value = "false", conflicts_with_all = ["all", "resolved", "json", "limit"])]
        purge: bool,
    },
    /// Update saved local comments for the current worktree
    UpdateLocalComment {
        /// Repository name (e.g., "owner/repo"). Auto-detected if omitted.
        #[arg(short, long)]
        repo: Option<String>,

        /// Working directory used to scope local comments (defaults to current directory)
        #[arg(long)]
        working_dir: Option<String>,

        /// Mark the specified comments as resolved
        #[arg(long, conflicts_with = "reopen")]
        resolve: bool,

        /// Reopen the specified comments
        #[arg(long, conflicts_with = "resolve")]
        reopen: bool,

        /// One or more local comment IDs to update
        #[arg(required = true, num_args = 1.., value_name = "ID")]
        ids: Vec<u64>,
    },
    /// Update to the latest version from GitHub Releases
    Update,
    /// Migrate configuration files and prompts after an update
    Migrate {
        /// Show what would change without applying
        #[arg(long, default_value = "false")]
        dry_run: bool,
        /// Migrate project-local .octorus/ config
        #[arg(long, default_value = "false")]
        local: bool,
        /// Overwrite all files regardless of customization
        #[arg(long, default_value = "false")]
        force: bool,
    },
}

/// Print ASCII art logo with nebula gradient (#eaafc8 → #654ea3)
fn print_logo() {
    use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
    use std::io::IsTerminal;

    const LOGO_LINES: [&str; 6] = [
        r"  ██████╗   ██████╗ ████████╗  ██████╗  ██████╗  ██╗   ██╗ ███████╗",
        r" ██╔═══██╗ ██╔════╝ ╚══██╔══╝ ██╔═══██╗ ██╔══██╗ ██║   ██║ ██╔════╝",
        r" ██║   ██║ ██║         ██║    ██║   ██║ ██████╔╝ ██║   ██║ ███████╗",
        r" ██║   ██║ ██║         ██║    ██║   ██║ ██╔══██╗ ██║   ██║ ╚════██║",
        r" ╚██████╔╝ ╚██████╗    ██║    ╚██████╔╝ ██║  ██║ ╚██████╔╝ ███████║",
        r"  ╚═════╝   ╚═════╝    ╚═╝     ╚═════╝  ╚═╝  ╚═╝  ╚═════╝  ╚══════╝",
    ];

    let mut stdout = io::stdout();
    let use_color = stdout.is_terminal();

    if use_color {
        const START: (u8, u8, u8) = (234, 175, 200); // #eaafc8
        const END: (u8, u8, u8) = (101, 78, 163); // #654ea3
        let steps = (LOGO_LINES.len() - 1) as f32;

        for (i, line) in LOGO_LINES.iter().enumerate() {
            let t = i as f32 / steps;
            let r = (START.0 as f32 + (END.0 as f32 - START.0 as f32) * t) as u8;
            let g = (START.1 as f32 + (END.1 as f32 - START.1 as f32) * t) as u8;
            let b = (START.2 as f32 + (END.2 as f32 - START.2 as f32) * t) as u8;
            let _ = execute!(
                stdout,
                SetForegroundColor(Color::Rgb { r, g, b }),
                Print(line),
                ResetColor,
                Print("\n")
            );
        }
    } else {
        for line in &LOGO_LINES {
            println!("{line}");
        }
    }
    println!();
}

fn is_root_help(raw_args: &[OsString]) -> bool {
    raw_args.len() == 1
        && raw_args[0]
            .to_str()
            .is_some_and(|arg| arg == "-h" || arg == "--help")
}

/// Restore terminal to normal state
fn restore_terminal() {
    octorus::ui::cleanup_keyboard_enhancement();
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// Set up panic hook to restore terminal on panic
fn setup_panic_hook() {
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        restore_terminal();
        original_hook(panic_info);
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    // Set up panic hook before anything else
    setup_panic_hook();

    // OR_DEBUG=1 enables file logging (file, not stderr, to avoid corrupting the TUI)
    if std::env::var("OR_DEBUG").ok().as_deref() == Some("1") {
        let log_dir = cache::cache_dir();
        if std::fs::create_dir_all(&log_dir).is_ok() {
            if let Ok(log_file) = std::fs::File::options()
                .create(true)
                .append(true)
                .open(log_dir.join("debug.log"))
            {
                use tracing_subscriber::EnvFilter;
                tracing_subscriber::fmt()
                    .with_writer(std::sync::Mutex::new(log_file))
                    .with_env_filter(EnvFilter::new("octorus=debug,or=debug"))
                    .init();
                tracing::info!("Debug logging enabled");
            }
        }
    }

    let raw_args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if is_root_help(&raw_args) {
        use clap::CommandFactory;
        print_logo();
        Args::command().print_help()?;
        println!();
        return Ok(());
    }

    let args = Args::parse();

    // Handle subcommands
    if let Some(command) = args.command {
        return match command {
            Commands::Init { force, local } => init::run_init(force, local),
            Commands::Clean => {
                cache::cleanup_rally_sessions();
                let rally_dir = cache::cache_dir().join("rally");
                println!("Rally sessions cleaned: {}", rally_dir.display());
                Ok(())
            }
            Commands::LocalComments {
                repo,
                working_dir,
                limit,
                json,
                all,
                resolved,
                purge,
            } => {
                if purge {
                    local_comments::purge_local_comments_command(repo, working_dir).await
                } else {
                    local_comments::show_local_comments_command(
                        repo,
                        working_dir,
                        limit,
                        json,
                        all,
                        resolved,
                    )
                    .await
                }
            }
            Commands::UpdateLocalComment {
                repo,
                working_dir,
                resolve,
                reopen,
                ids,
            } => {
                local_comments::update_local_comments_command(
                    repo,
                    working_dir,
                    resolve,
                    reopen,
                    ids,
                )
                .await
            }
            Commands::Update => {
                update::run_update()?;
                Ok(())
            }
            Commands::Migrate {
                dry_run,
                local,
                force,
            } => migrate::run_migrate(dry_run, local, force),
        };
    }

    let is_no_args =
        args.pr.is_none() && !args.local && args.issue.is_none() && !args.git_ops && !args.ai_rally;

    let (repo, repo_available) = match args.repo.clone() {
        Some(r) => (r, true),
        None => {
            if args.local || is_no_args {
                match github::detect_repo().await {
                    Ok(r) => (r, true),
                    Err(_) => ("local".to_string(), false),
                }
            } else {
                match github::detect_repo().await {
                    Ok(r) => (r, true),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
    };

    if is_no_args {
        let mut config = if let Some(ref dir) = args.working_dir {
            config::Config::load_for_dir(Path::new(dir))?
        } else {
            config::Config::load()?
        };
        apply_cli_config_overrides(&mut config, &args);
        return run_with_cockpit(&repo, config, &args, repo_available).await;
    }

    // Pre-initialize syntax highlighting in background to avoid delay on first diff view
    std::thread::spawn(|| {
        let _ = syntax::syntax_set();
        let _ = syntax::theme_set();
    });

    let mut config = if let Some(ref dir) = args.working_dir {
        config::Config::load_for_dir(Path::new(dir))?
    } else {
        config::Config::load()?
    };
    apply_cli_config_overrides(&mut config, &args);

    // Headless mode: --ai-rally with --pr <number> or --local bypasses TUI entirely
    if args.ai_rally && matches!(args.pr, Some(pr) if pr > 0) {
        let pr = args.pr.unwrap();
        let working_dir = resolve_working_dir(&args);
        match headless::run_headless_rally(
            &repo,
            pr,
            &config,
            working_dir.as_deref(),
            args.accept_local_overrides,
            args.output.as_deref(),
        )
        .await
        {
            Ok(approved) => std::process::exit(if approved { 0 } else { 1 }),
            Err(e) => {
                headless::write_error_json(&e.to_string(), args.output.as_deref());
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
    }
    if args.local && args.ai_rally {
        let working_dir = resolve_working_dir(&args);
        match headless::run_headless_rally_local(
            &repo,
            &config,
            working_dir.as_deref(),
            args.accept_local_overrides,
            args.output.as_deref(),
        )
        .await
        {
            Ok(approved) => std::process::exit(if approved { 0 } else { 1 }),
            Err(e) => {
                headless::write_error_json(&e.to_string(), args.output.as_deref());
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
    }

    if args.local {
        run_with_local_diff(&repo, &config, &args).await
    } else if let Some(pr) = args.pr.filter(|&n| n > 0) {
        run_with_pr(&repo, pr, &config, &args).await
    } else {
        run_with_pr_list(&repo, config, &args, args.issue).await
    }
}

async fn run_with_local_diff(repo: &str, config: &config::Config, args: &Args) -> Result<()> {
    let (retry_tx, mut retry_rx) = mpsc::channel::<RefreshRequest>(1);
    let (mut app, tx) = app::App::new_loading(repo, 0, config.clone());
    let working_dir = args.working_dir.clone();
    let refresh_pending = Arc::new(AtomicBool::new(false));

    app.set_retry_sender(retry_tx.clone());
    start_update_check(&mut app);
    setup_local_watch(retry_tx, working_dir.clone(), refresh_pending.clone());
    app.set_local_mode(true);
    app.set_local_auto_focus(args.auto_focus);
    setup_working_dir(&mut app, args);

    if args.ai_rally {
        app.set_start_ai_rally_on_load(true);
    }
    if args.git_ops {
        app.open_git_ops();
    }

    let cancel_token = CancellationToken::new();
    let token_clone = cancel_token.clone();
    let repo = repo.to_string();

    loader::fetch_local_diff(repo.clone(), working_dir.clone(), tx.clone()).await;

    tokio::spawn(async move {
        tokio::select! {
            _ = token_clone.cancelled() => {}
            _ = async {
                while let Some(request) = retry_rx.recv().await {
                    match request {
                        RefreshRequest::LocalRefresh => {
                            refresh_pending.store(false, Ordering::Release);

                            loop {
                                let tx_retry = tx.clone();
                                loader::fetch_local_diff(repo.clone(), working_dir.clone(), tx_retry).await;

                                if !refresh_pending.swap(false, Ordering::AcqRel) {
                                    break;
                                }
                            }
                        }
                        RefreshRequest::PrRefresh { .. } => {
                            // In local mode, pr_number == 0 is a dummy value that would
                            // produce invalid API calls, so treat PrRefresh as LocalRefresh.
                            let tx_retry = tx.clone();
                            loader::fetch_local_diff(repo.clone(), working_dir.clone(), tx_retry).await;
                        }
                    }
                }
            } => {}
        }
    });

    let result = app.run().await;
    cancel_token.cancel();

    if let Err(ref e) = result {
        restore_terminal();
        eprintln!("Error: {:#}", e);
    }

    let exit_code = if result.is_ok() { 0 } else { 1 };
    std::process::exit(exit_code);
}

fn setup_local_watch(
    refresh_tx: mpsc::Sender<RefreshRequest>,
    working_dir: Option<String>,
    refresh_pending: Arc<AtomicBool>,
) {
    let watch_dir = working_dir.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });

    std::thread::spawn({
        let refresh_tx = refresh_tx.clone();
        move || {
            let callback = move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };

                let should_refresh = should_refresh_local_change(&event.paths, &event.kind);

                if should_refresh && !refresh_pending.swap(true, Ordering::AcqRel) {
                    let _ = refresh_tx.try_send(RefreshRequest::LocalRefresh);
                }
            };

            let Ok(mut watcher) = RecommendedWatcher::new(callback, Config::default()) else {
                return;
            };

            let _ = watcher.watch(Path::new(&watch_dir), RecursiveMode::Recursive);

            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    });
}

fn should_refresh_local_change(paths: &[PathBuf], kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_))
        && paths
            .iter()
            .any(|path| !is_git_file(path) && !is_octorus_config_file(path))
}

fn is_git_file(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".git")
}

fn is_octorus_config_file(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".octorus")
}

/// Run the app with a specific PR number (existing flow)
async fn run_with_pr(repo: &str, pr: u32, config: &config::Config, args: &Args) -> Result<()> {
    let (retry_tx, mut retry_rx) = mpsc::channel::<RefreshRequest>(1);
    let refresh_pending = Arc::new(AtomicBool::new(false));

    let (mut app, tx) = app::App::new_loading(repo, pr, config.clone());

    app.set_retry_sender(retry_tx);
    start_update_check(&mut app);
    setup_working_dir(&mut app, args);

    if args.ai_rally {
        app.set_start_ai_rally_on_load(true);
    }
    if args.git_ops {
        app.open_git_ops();
    }

    let cancel_token = CancellationToken::new();
    let token_clone = cancel_token.clone();

    let repo_clone = repo.to_string();
    let pr_number = pr;
    let working_dir = args.working_dir.clone();

    tokio::spawn(async move {
        tokio::select! {
            _ = token_clone.cancelled() => {}
            _ = async {
                loader::fetch_pr_data(repo_clone.clone(), pr_number, loader::FetchMode::Fresh, tx.clone()).await;

                while let Some(request) = retry_rx.recv().await {
                    match request {
                        RefreshRequest::PrRefresh { pr_number } => {
                            let tx_retry = tx.clone();
                            loader::fetch_pr_data(repo_clone.clone(), pr_number, loader::FetchMode::Fresh, tx_retry)
                                .await;
                        }
                        RefreshRequest::LocalRefresh => {
                            refresh_pending.store(false, Ordering::Release);
                            loop {
                                let tx_retry = tx.clone();
                                loader::fetch_local_diff(repo_clone.clone(), working_dir.clone(), tx_retry).await;
                                if !refresh_pending.swap(false, Ordering::AcqRel) {
                                    break;
                                }
                            }
                        }
                    }
                }
            } => {}
        }
    });

    let result = app.run().await;
    cancel_token.cancel();

    if let Err(ref e) = result {
        restore_terminal();
        eprintln!("Error: {:#}", e);
    }

    // Immediate exit to avoid hanging on spawn_blocking tasks (e.g. tree-sitter
    // parsing). Background tasks are already cancelled; the OS reclaims resources.
    let exit_code = if result.is_ok() { 0 } else { 1 };
    std::process::exit(exit_code);
}

/// Run the app with PR list (new flow)
async fn run_with_pr_list(
    repo: &str,
    config: config::Config,
    args: &Args,
    issue_arg: Option<u32>,
) -> Result<()> {
    // Retry channel also handles PR list → Local mode transitions.
    let (retry_tx, mut retry_rx) = mpsc::channel::<RefreshRequest>(1);
    let refresh_pending = Arc::new(AtomicBool::new(false));

    let mut app = app::App::new_pr_list(repo, config);
    app.set_retry_sender(retry_tx);
    start_update_check(&mut app);
    setup_working_dir(&mut app, args);

    if args.ai_rally {
        app.set_pending_ai_rally(true);
    }
    if args.git_ops {
        app.open_git_ops();
    }

    match issue_arg {
        Some(n) if n > 0 => {
            app.open_issue_list();
            app.select_issue(n);
        }
        Some(_) => {
            app.open_issue_list();
        }
        None => {}
    }

    let (pr_list_tx, rx) = mpsc::channel(2);
    app.set_pr_list_receiver(rx);

    let repo_clone = repo.to_string();
    let state_filter = app.prs.pr_list_state_filter;

    tokio::spawn(async move {
        let result = github::fetch_pr_list(&repo_clone, state_filter, 30).await;
        let _ = pr_list_tx.send(result.map_err(|e| e.to_string())).await;
    });

    // Data channel for local-mode transitions from the PR list screen.
    let (data_tx, data_rx) = mpsc::channel(2);
    app.set_data_receiver(0, data_rx);

    let cancel_token = CancellationToken::new();
    let token_clone = cancel_token.clone();
    let repo_for_retry = repo.to_string();
    let working_dir = args.working_dir.clone();

    tokio::spawn(async move {
        tokio::select! {
            _ = token_clone.cancelled() => {}
            _ = async {
                while let Some(request) = retry_rx.recv().await {
                    match request {
                        RefreshRequest::PrRefresh { pr_number } => {
                            let tx_retry = data_tx.clone();
                            loader::fetch_pr_data(repo_for_retry.clone(), pr_number, loader::FetchMode::Fresh, tx_retry)
                                .await;
                        }
                        RefreshRequest::LocalRefresh => {
                            refresh_pending.store(false, Ordering::Release);
                            loop {
                                let tx_retry = data_tx.clone();
                                loader::fetch_local_diff(repo_for_retry.clone(), working_dir.clone(), tx_retry).await;
                                if !refresh_pending.swap(false, Ordering::AcqRel) {
                                    break;
                                }
                            }
                        }
                    }
                }
            } => {}
        }
    });

    let result = app.run().await;
    cancel_token.cancel();

    if let Err(ref e) = result {
        restore_terminal();
        eprintln!("Error: {:#}", e);
    }

    // Same immediate-exit rationale as run_with_pr.
    let exit_code = if result.is_ok() { 0 } else { 1 };
    std::process::exit(exit_code);
}

/// Run the app with Cockpit dashboard (no-args startup)
async fn run_with_cockpit(
    repo: &str,
    config: config::Config,
    args: &Args,
    repo_available: bool,
) -> Result<()> {
    let (retry_tx, mut retry_rx) = mpsc::channel::<app::RefreshRequest>(1);
    let refresh_pending = Arc::new(AtomicBool::new(false));

    let mut app = app::App::new_cockpit(repo, config, repo_available);
    app.set_retry_sender(retry_tx);
    start_update_check(&mut app);
    setup_working_dir(&mut app, args);

    app.open_cockpit();

    let (data_tx, data_rx) = mpsc::channel(2);
    app.set_data_receiver(0, data_rx);

    let cancel_token = CancellationToken::new();
    let token_clone = cancel_token.clone();

    let repo_for_retry = repo.to_string();
    let working_dir = args.working_dir.clone();

    tokio::spawn(async move {
        tokio::select! {
            _ = token_clone.cancelled() => {}
            _ = async {
                while let Some(request) = retry_rx.recv().await {
                    match request {
                        app::RefreshRequest::PrRefresh { pr_number } => {
                            let tx_retry = data_tx.clone();
                            loader::fetch_pr_data(repo_for_retry.clone(), pr_number, loader::FetchMode::Fresh, tx_retry)
                                .await;
                        }
                        app::RefreshRequest::LocalRefresh => {
                            refresh_pending.store(false, Ordering::Release);
                            loop {
                                let tx_retry = data_tx.clone();
                                loader::fetch_local_diff(repo_for_retry.clone(), working_dir.clone(), tx_retry).await;
                                if !refresh_pending.swap(false, Ordering::AcqRel) {
                                    break;
                                }
                            }
                        }
                    }
                }
            } => {}
        }
    });

    let result = app.run().await;
    cancel_token.cancel();

    if let Err(ref e) = result {
        restore_terminal();
        eprintln!("Error: {:#}", e);
    }

    let exit_code = if result.is_ok() { 0 } else { 1 };
    std::process::exit(exit_code);
}

/// Spawn a background version check and set the receiver on the App.
fn start_update_check(app: &mut app::App) {
    let (tx, rx) = mpsc::channel(1);
    app.set_update_check_receiver(rx);

    tokio::task::spawn_blocking(move || {
        let result = update::check_for_update();
        let _ = tx.blocking_send(result);
    });
}

/// Resolve working directory for headless mode
fn resolve_working_dir(args: &Args) -> Option<String> {
    if let Some(dir) = args.working_dir.clone() {
        Some(dir)
    } else {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    }
}

fn apply_cli_config_overrides(config: &mut config::Config, args: &Args) {
    if let Some(review_only) = args.review_only {
        config.ai.review_only = review_only;
        // A CLI value is explicit user input, so it supersedes the same
        // project-local key for headless/TUI local-override warnings.
        config.local_overrides.remove("ai.review_only");
    }
}

/// Set up working directory for AI agents
fn setup_working_dir(app: &mut app::App, args: &Args) {
    if let Some(dir) = args.working_dir.clone() {
        app.set_working_dir(Some(dir));
    } else {
        // current_dir() can fail in edge cases (e.g., if the current directory
        // has been deleted, or on some restricted environments). When --ai-rally is
        // used without --working-dir, we need a valid directory for the AI agents.
        match std::env::current_dir() {
            Ok(cwd) => {
                app.set_working_dir(Some(cwd.to_string_lossy().to_string()));
            }
            Err(e) => {
                if args.ai_rally {
                    eprintln!(
                        "Warning: Failed to get current directory: {}. AI Rally may not work correctly without --working-dir.",
                        e
                    );
                }
                // Continue without setting working_dir; it's optional for non-AI-Rally usage
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, CreateKind};
    use std::path::PathBuf;

    #[test]
    fn test_should_refresh_local_change_ignores_access_events() {
        let paths = vec![PathBuf::from("src/main.rs")];
        let kind = EventKind::Access(AccessKind::Close(AccessMode::Write));

        assert!(!should_refresh_local_change(&paths, &kind));
    }

    #[test]
    fn test_should_refresh_local_change_ignores_git_paths() {
        let paths = vec![PathBuf::from(".git/HEAD"), PathBuf::from(".git/index.lock")];
        let kind = EventKind::Create(CreateKind::File);

        assert!(!should_refresh_local_change(&paths, &kind));
    }

    #[test]
    fn test_should_refresh_local_change_refreshes_subdir_change() {
        let paths = vec![
            PathBuf::from(".git/HEAD"),
            PathBuf::from("src/subdir/changed.rs"),
        ];
        let kind = EventKind::Create(CreateKind::File);

        assert!(should_refresh_local_change(&paths, &kind));
    }

    #[test]
    fn test_is_git_file_identifies_git_path() {
        assert!(is_git_file(std::path::Path::new(".git/refs/heads/main")));
        assert!(!is_git_file(std::path::Path::new("src/main.rs")));
    }

    #[test]
    fn test_should_refresh_local_change_ignores_octorus_paths() {
        let paths = vec![
            PathBuf::from(".octorus/config.toml"),
            PathBuf::from(".octorus/prompts/reviewer.md"),
        ];
        let kind = EventKind::Create(CreateKind::File);

        assert!(!should_refresh_local_change(&paths, &kind));
    }

    #[test]
    fn test_is_octorus_config_file() {
        assert!(is_octorus_config_file(std::path::Path::new(
            ".octorus/config.toml"
        )));
        assert!(is_octorus_config_file(std::path::Path::new(
            ".octorus/prompts/reviewer.md"
        )));
        assert!(!is_octorus_config_file(std::path::Path::new("src/main.rs")));
    }

    #[test]
    fn test_review_only_cli_flag_parses_bool_value() {
        let args = Args::parse_from(["or", "--ai-rally", "--review-only=true", "--pr", "123"]);

        assert_eq!(args.review_only, Some(true));
    }

    #[test]
    fn test_review_only_cli_flag_requires_ai_rally() {
        let result = Args::try_parse_from(["or", "--review-only=true", "--pr", "123"]);

        assert!(result.is_err());
    }

    #[test]
    fn test_review_only_cli_override_sets_config_and_clears_local_warning() {
        let args = Args::parse_from(["or", "--ai-rally", "--review-only=true", "--local"]);
        let mut config = config::Config::default();
        config.ai.review_only = false;
        config.local_overrides.insert("ai.review_only".to_string());

        apply_cli_config_overrides(&mut config, &args);

        assert!(config.ai.review_only);
        assert!(!config.local_overrides.contains("ai.review_only"));
    }

    #[test]
    fn test_root_help_snapshot_includes_review_only_flag() {
        use clap::CommandFactory;
        use insta::assert_snapshot;

        let help = Args::command().render_help().to_string();

        assert_snapshot!(help, @r#"
        TUI for GitHub PRs, issues, local diffs, and Git Ops. AI-powered automated review cycles.

        Usage: or [OPTIONS] [COMMAND]

        Commands:
          init                  Initialize configuration files and prompt templates
          clean                 Remove AI Rally session data
          local-comments        Show saved local comments for the current worktree
          update-local-comment  Update saved local comments for the current worktree
          update                Update to the latest version from GitHub Releases
          migrate               Migrate configuration files and prompts after an update
          help                  Print this message or the help of the given subcommand(s)

        Options:
          -r, --repo <REPO>                Repository name (e.g., "owner/repo"). Auto-detected from current directory if omitted
          -p, --pr [<PR>]                  Pull request number. Shows PR list if flag only (no number)
              --ai-rally                   Start AI Rally mode directly
              --review-only [<BOOL>]       Force AI Rally review-only (proposal iteration) mode. Use --review-only=true [possible values: true, false]
              --local                      Show local git diff against current HEAD (no GitHub PR fetch)
          -i, --issue [<ISSUE>]            Issue number. Shows issue detail directly if provided, issue list if flag only
              --git-ops                    Start in Git Ops view directly
              --auto-focus                 Auto-focus changed file when local diff updates (for local mode)
              --working-dir <WORKING_DIR>  Working directory for AI agents (default: current directory)
              --accept-local-overrides     Accept local .octorus/ overrides for AI settings in headless mode. Without this flag, headless AI Rally will refuse to run if the local config overrides security-sensitive AI keys or local prompt files are detected in .octorus/prompts/
              --output <OUTPUT>            Write JSON result to a file (in addition to stdout). Useful when running as a background task where stdout may not be captured
          -h, --help                       Print help
          -V, --version                    Print version
        "#);
    }
}
