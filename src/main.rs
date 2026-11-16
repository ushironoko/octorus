use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, LeaveAlternateScreen},
};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::ffi::OsString;
use std::io;
use std::panic;
use std::path::Path;
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

    /// Start in the Repository Browser directly
    #[arg(long, default_value = "false")]
    browse: bool,

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

fn is_no_args(args: &Args) -> bool {
    args.pr.is_none()
        && !args.local
        && args.issue.is_none()
        && !args.git_ops
        && !args.browse
        && !args.ai_rally
}

/// Repo detection is allowed to degrade to a repo-less session for local,
/// Git Ops, Repository Browser, and no-entry-point sessions.
///
/// Consulted only when `--repo` is absent — an explicit repository is taken as
/// given and never detected, so this predicate ignores [`Args::repo`].
fn tolerates_missing_repo(args: &Args) -> bool {
    args.local || args.git_ops || args.browse || is_no_args(args)
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
#[allow(clippy::print_stdout, clippy::print_stderr)]
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
    octorus::ui::cleanup_mouse_capture();
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

fn no_color_from_env() -> bool {
    octorus::ui::theme::no_color_from_value(std::env::var("NO_COLOR").ok().as_deref())
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
        #[allow(clippy::print_stdout, clippy::print_stderr)]
        {
            println!();
        }
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
                #[allow(clippy::print_stdout, clippy::print_stderr)]
                {
                    println!("Rally sessions cleaned: {}", rally_dir.display());
                }
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

    let is_no_args = is_no_args(&args);

    let (repo, repo_available) = match args.repo.clone() {
        Some(r) => (r, true),
        None => {
            if tolerates_missing_repo(&args) {
                match github::detect_repo().await {
                    Ok(r) => (r, true),
                    Err(_) => ("local".to_string(), false),
                }
            } else {
                match github::detect_repo().await {
                    Ok(r) => (r, true),
                    Err(e) => {
                        #[allow(clippy::print_stdout, clippy::print_stderr)]
                        {
                            eprintln!("Error: {}", e);
                        }
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
        config.no_color = no_color_from_env();
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
    config.no_color = no_color_from_env();

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
                #[allow(clippy::print_stdout, clippy::print_stderr)]
                {
                    eprintln!("Error: {}", e);
                }
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
                #[allow(clippy::print_stdout, clippy::print_stderr)]
                {
                    eprintln!("Error: {}", e);
                }
                std::process::exit(1);
            }
        }
    }

    if args.local {
        run_with_local_diff(&repo, &config, &args, repo_available).await
    } else if let Some(pr) = args.pr.filter(|&n| n > 0) {
        run_with_pr(&repo, pr, &config, &args, repo_available).await
    } else {
        run_with_pr_list(&repo, config, &args, args.issue, repo_available).await
    }
}

async fn run_with_local_diff(
    repo: &str,
    config: &config::Config,
    args: &Args,
    repo_available: bool,
) -> Result<()> {
    let (retry_tx, mut retry_rx) = mpsc::channel::<RefreshRequest>(1);
    let repository_availability = if repo_available {
        app::RepositoryAvailability::Available
    } else {
        app::RepositoryAvailability::Unavailable
    };
    let (mut app, tx) = app::App::new_loading(repo, 0, config.clone(), repository_availability);
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
    if args.browse {
        app.open_repo_browse();
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
                    match local_startup_refresh_route(request) {
                        LocalStartupRefreshRoute::PullRequest { pr_number, cached } => {
                            let tx_retry = tx.clone();
                            let mode = match cached {
                                Some(cached) => loader::FetchMode::CheckUpdate(cached),
                                None => loader::FetchMode::Fresh,
                            };
                            loader::fetch_pr_data(repo.clone(), pr_number, mode, tx_retry).await;
                        }
                        LocalStartupRefreshRoute::WatchedLocal => {
                            refresh_pending.store(false, Ordering::Release);

                            loop {
                                let tx_retry = tx.clone();
                                loader::fetch_local_diff(repo.clone(), working_dir.clone(), tx_retry).await;

                                if !refresh_pending.swap(false, Ordering::AcqRel) {
                                    break;
                                }
                            }
                        }
                        LocalStartupRefreshRoute::LocalSentinel => {
                            // PR zero is the local-mode sentinel, never a GitHub PR number.
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
        #[allow(clippy::print_stdout, clippy::print_stderr)]
        {
            eprintln!("Error: {:#}", e);
        }
    }

    let exit_code = if result.is_ok() { 0 } else { 1 };
    std::process::exit(exit_code);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalStartupRefreshRoute {
    WatchedLocal,
    LocalSentinel,
    PullRequest {
        pr_number: u32,
        /// `Some` compares this cached identity. `None` is a full refetch.
        cached: Option<app::CachedPrCheck>,
    },
}

fn local_startup_refresh_route(request: RefreshRequest) -> LocalStartupRefreshRoute {
    match request {
        RefreshRequest::PrRefresh { pr_number } if pr_number > 0 => {
            LocalStartupRefreshRoute::PullRequest {
                pr_number,
                cached: None,
            }
        }
        RefreshRequest::PrCheckUpdate { pr_number, cached } if pr_number > 0 => {
            LocalStartupRefreshRoute::PullRequest {
                pr_number,
                cached: Some(cached),
            }
        }
        // PR zero is the local-mode sentinel, never a GitHub PR number.
        RefreshRequest::PrRefresh { .. } | RefreshRequest::PrCheckUpdate { .. } => {
            LocalStartupRefreshRoute::LocalSentinel
        }
        RefreshRequest::LocalRefresh => LocalStartupRefreshRoute::WatchedLocal,
    }
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
            let ignore =
                std::sync::Mutex::new(octorus::local_watch::GitIgnoreCache::new(&watch_dir));
            let callback = move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };

                let should_refresh = {
                    let mut ignore = ignore.lock().unwrap_or_else(|err| err.into_inner());
                    octorus::local_watch::should_refresh_local_change(
                        &event.paths,
                        &event.kind,
                        event.need_rescan(),
                        refresh_pending.load(Ordering::Acquire),
                        true,
                        &mut ignore,
                    )
                };

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

#[cfg(test)]
fn should_refresh_local_change(
    paths: &[std::path::PathBuf],
    kind: &notify::EventKind,
    ignore: &mut octorus::local_watch::GitIgnoreCache,
) -> bool {
    octorus::local_watch::should_refresh_local_change(paths, kind, false, false, true, ignore)
}

/// Run the app with a specific PR number (existing flow)
async fn run_with_pr(
    repo: &str,
    pr: u32,
    config: &config::Config,
    args: &Args,
    repo_available: bool,
) -> Result<()> {
    let (retry_tx, mut retry_rx) = mpsc::channel::<RefreshRequest>(1);
    let refresh_pending = Arc::new(AtomicBool::new(false));

    let repository_availability = if repo_available {
        app::RepositoryAvailability::Available
    } else {
        app::RepositoryAvailability::Unavailable
    };
    let (mut app, tx) = app::App::new_loading(repo, pr, config.clone(), repository_availability);

    app.set_retry_sender(retry_tx);
    start_update_check(&mut app);
    setup_working_dir(&mut app, args);

    if args.ai_rally {
        app.set_start_ai_rally_on_load(true);
    }
    if args.git_ops {
        app.open_git_ops();
    }
    if args.browse {
        app.open_repo_browse();
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
                        RefreshRequest::PrCheckUpdate { pr_number, cached } => {
                            let tx_retry = tx.clone();
                            loader::fetch_pr_data(
                                repo_clone.clone(),
                                pr_number,
                                loader::FetchMode::CheckUpdate(cached),
                                tx_retry,
                            )
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
        #[allow(clippy::print_stdout, clippy::print_stderr)]
        {
            eprintln!("Error: {:#}", e);
        }
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
    repo_available: bool,
) -> Result<()> {
    // Retry channel also handles PR list → Local mode transitions.
    let (retry_tx, mut retry_rx) = mpsc::channel::<RefreshRequest>(1);
    let refresh_pending = Arc::new(AtomicBool::new(false));

    let repository_availability = if repo_available {
        app::RepositoryAvailability::Available
    } else {
        app::RepositoryAvailability::Unavailable
    };
    let mut app = app::App::new_pr_list(repo, config, repository_availability);
    app.set_retry_sender(retry_tx);
    start_update_check(&mut app);
    setup_working_dir(&mut app, args);

    if args.ai_rally {
        app.set_pending_ai_rally(true);
    }
    if args.git_ops {
        app.open_git_ops();
    }
    if args.browse {
        app.open_repo_browse();
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
        let _ = pr_list_tx
            .send(result.map_err(|error| github::GhFailure::from_error(&error)))
            .await;
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
                        RefreshRequest::PrCheckUpdate { pr_number, cached } => {
                            let tx_retry = data_tx.clone();
                            loader::fetch_pr_data(
                                repo_for_retry.clone(),
                                pr_number,
                                loader::FetchMode::CheckUpdate(cached),
                                tx_retry,
                            )
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
        #[allow(clippy::print_stdout, clippy::print_stderr)]
        {
            eprintln!("Error: {:#}", e);
        }
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
                        app::RefreshRequest::PrCheckUpdate { pr_number, cached } => {
                            let tx_retry = data_tx.clone();
                            loader::fetch_pr_data(
                                repo_for_retry.clone(),
                                pr_number,
                                loader::FetchMode::CheckUpdate(cached),
                                tx_retry,
                            )
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
        #[allow(clippy::print_stdout, clippy::print_stderr)]
        {
            eprintln!("Error: {:#}", e);
        }
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
                    #[allow(clippy::print_stdout, clippy::print_stderr)]
                    {
                        eprintln!(
                            "Warning: Failed to get current directory: {}. AI Rally may not work correctly without --working-dir.",
                            e
                        );
                    }
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
    use notify::EventKind;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn test_should_refresh_local_change_ignores_access_events() {
        let paths = vec![PathBuf::from("src/main.rs")];
        let kind = EventKind::Access(AccessKind::Close(AccessMode::Write));

        assert!(!should_refresh_local_change(
            &paths,
            &kind,
            &mut octorus::local_watch::GitIgnoreCache::allow_all()
        ));
    }

    #[test]
    fn test_should_refresh_local_change_ignores_git_paths() {
        let paths = vec![PathBuf::from(".git/HEAD"), PathBuf::from(".git/index.lock")];
        let kind = EventKind::Create(CreateKind::File);

        assert!(!should_refresh_local_change(
            &paths,
            &kind,
            &mut octorus::local_watch::GitIgnoreCache::allow_all()
        ));
    }

    #[test]
    fn test_should_refresh_local_change_refreshes_subdir_change() {
        let paths = vec![
            PathBuf::from(".git/HEAD"),
            PathBuf::from("src/subdir/changed.rs"),
        ];
        let kind = EventKind::Create(CreateKind::File);

        assert!(should_refresh_local_change(
            &paths,
            &kind,
            &mut octorus::local_watch::GitIgnoreCache::allow_all()
        ));
    }

    #[test]
    fn test_is_git_file_identifies_git_path() {
        assert!(octorus::local_watch::is_git_file(std::path::Path::new(
            ".git/refs/heads/main"
        )));
        assert!(!octorus::local_watch::is_git_file(std::path::Path::new(
            "src/main.rs"
        )));
    }

    #[test]
    fn test_should_refresh_local_change_ignores_octorus_paths() {
        let paths = vec![
            PathBuf::from(".octorus/config.toml"),
            PathBuf::from(".octorus/prompts/reviewer.md"),
        ];
        let kind = EventKind::Create(CreateKind::File);

        assert!(!should_refresh_local_change(
            &paths,
            &kind,
            &mut octorus::local_watch::GitIgnoreCache::allow_all()
        ));
    }

    #[test]
    fn test_is_octorus_config_file() {
        assert!(octorus::local_watch::is_octorus_config_file(
            std::path::Path::new(".octorus/config.toml")
        ));
        assert!(octorus::local_watch::is_octorus_config_file(
            std::path::Path::new(".octorus/prompts/reviewer.md")
        ));
        assert!(!octorus::local_watch::is_octorus_config_file(
            std::path::Path::new("src/main.rs")
        ));
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
    fn test_no_args_tolerates_missing_repo() {
        let args = Args::parse_from(["or"]);

        assert!(tolerates_missing_repo(&args));
    }

    /// Every entry-point flag must exclude itself from `is_no_args`.
    ///
    /// `is_no_args` has a second consumer besides `tolerates_missing_repo`: the
    /// Cockpit dispatch. Testing it only through `tolerates_missing_repo` cannot
    /// see a broken conjunct, because `--local`, `--git-ops` and `--browse` each
    /// have their own disjunct there that keeps the answer `true` either way —
    /// so the flag would silently open Cockpit instead of its own screen while
    /// every existing test stayed green.
    ///
    /// The end-to-end tests cannot close this either: `or --browse` writes the
    /// alternate-screen escape and then fails on a non-tty before drawing a
    /// single frame, so stdout never names the screen that opened.
    #[test]
    fn test_every_entry_point_flag_excludes_itself_from_is_no_args() {
        assert!(is_no_args(&Args::parse_from(["or"])), "the baseline");

        for argv in [
            vec!["or", "--pr", "1"],
            vec!["or", "--local"],
            vec!["or", "--issue", "1"],
            vec!["or", "--git-ops"],
            vec!["or", "--browse"],
            vec!["or", "--ai-rally"],
        ] {
            let flag = argv[1];
            assert!(
                !is_no_args(&Args::parse_from(argv.clone())),
                "`or {flag}` is an entry point, so it is not a no-args session; \
                 treating it as one dispatches Cockpit instead of its own screen"
            );
        }
    }

    #[test]
    fn test_browse_tolerates_missing_repo() {
        let args = Args::parse_from(["or", "--browse"]);

        assert!(tolerates_missing_repo(&args));
    }

    #[test]
    fn test_local_tolerates_missing_repo() {
        let args = Args::parse_from(["or", "--local"]);

        assert!(tolerates_missing_repo(&args));
    }

    #[test]
    fn test_pr_without_explicit_repo_does_not_tolerate_missing_repo() {
        let args = Args::parse_from(["or", "--pr", "1"]);

        assert!(!tolerates_missing_repo(&args));
    }

    #[test]
    fn test_git_ops_tolerates_missing_repo() {
        let args = Args::parse_from(["or", "--git-ops"]);

        assert!(tolerates_missing_repo(&args));
    }

    #[test]
    fn test_local_startup_refresh_route_distinguishes_every_dispatch_target() {
        let cached = octorus::app::CachedPrCheck {
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            head_sha: "abc".to_string(),
            base_ref: "main".to_string(),
            base_sha: "def".to_string(),
            base_tip_sha: Some("cafebabe".to_string()),
        };
        assert_eq!(
            local_startup_refresh_route(RefreshRequest::PrRefresh { pr_number: 42 }),
            LocalStartupRefreshRoute::PullRequest {
                pr_number: 42,
                cached: None,
            }
        );
        assert_eq!(
            local_startup_refresh_route(RefreshRequest::PrCheckUpdate {
                pr_number: 42,
                cached: cached.clone(),
            }),
            LocalStartupRefreshRoute::PullRequest {
                pr_number: 42,
                cached: Some(cached.clone()),
            }
        );
        assert_eq!(
            local_startup_refresh_route(RefreshRequest::PrRefresh { pr_number: 0 }),
            LocalStartupRefreshRoute::LocalSentinel
        );
        assert_eq!(
            local_startup_refresh_route(RefreshRequest::PrCheckUpdate {
                pr_number: 0,
                cached,
            }),
            LocalStartupRefreshRoute::LocalSentinel
        );
        assert_eq!(
            local_startup_refresh_route(RefreshRequest::LocalRefresh),
            LocalStartupRefreshRoute::WatchedLocal
        );
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
              --browse                     Start in the Repository Browser directly
              --auto-focus                 Auto-focus changed file when local diff updates (for local mode)
              --working-dir <WORKING_DIR>  Working directory for AI agents (default: current directory)
              --accept-local-overrides     Accept local .octorus/ overrides for AI settings in headless mode. Without this flag, headless AI Rally will refuse to run if the local config overrides security-sensitive AI keys or local prompt files are detected in .octorus/prompts/
              --output <OUTPUT>            Write JSON result to a file (in addition to stdout). Useful when running as a background task where stdout may not be captured
          -h, --help                       Print help
          -V, --version                    Print version
        "#);
    }

    fn git(root: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("-c")
            .arg("user.email=test@example.com")
            .arg("-c")
            .arg("user.name=Test")
            .args(args)
            .status()
            .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
        assert!(status.success(), "git {args:?} failed with {status}");
    }

    fn refresh(cache: &mut octorus::local_watch::GitIgnoreCache, path: &std::path::Path) -> bool {
        should_refresh_local_change(
            &[path.to_path_buf()],
            &EventKind::Create(CreateKind::File),
            cache,
        )
    }

    #[test]
    fn watcher_without_git_still_ignores_target_and_node_modules() {
        let dir = tempdir().unwrap();
        // A broken .git stops discovery of a parent repository, so this exercises
        // the fallback that only skips target/ and node_modules/.
        std::fs::write(dir.path().join(".git"), "gitdir: /nonexistent\n").unwrap();
        let mut cache = octorus::local_watch::GitIgnoreCache::new(dir.path());

        assert!(cache.is_ignored(PathBuf::from("target/debug/octorus").as_path()));
        assert!(cache.is_ignored(PathBuf::from("node_modules/leftpad/index.js").as_path()));
        assert!(!cache.is_ignored(PathBuf::from("src/main.rs").as_path()));
        assert!(!cache.is_ignored(PathBuf::from("notes.txt").as_path()));
        assert!(refresh(&mut cache, PathBuf::from("src/main.rs").as_path()));
        assert!(!refresh(
            &mut cache,
            PathBuf::from("target/debug/octorus").as_path()
        ));
    }

    #[test]
    fn watcher_ignores_gitignored_paths_but_not_tracked_files_or_gitignore() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join("ignoreddir")).unwrap();
        std::fs::write(
            root.join(".gitignore"),
            "target/\nnode_modules/\n*.log\nignoreddir/\n",
        )
        .unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("keep.txt"), "keep\n").unwrap();
        std::fs::write(root.join("sub/.gitignore"), "secret.txt\n").unwrap();
        std::fs::write(root.join("sub/keep.rs"), "fn keep() {}\n").unwrap();
        std::fs::write(root.join("sub/secret.txt"), "secret\n").unwrap();
        std::fs::write(root.join("tracked.log"), "tracked\n").unwrap();
        std::fs::write(root.join("noise.log"), "noise\n").unwrap();
        std::fs::write(root.join("notes.txt"), "notes\n").unwrap();
        std::fs::write(root.join("target/debug/foo"), "bin\n").unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), "module\n").unwrap();
        std::fs::write(root.join("ignoreddir/a.txt"), "ignored\n").unwrap();
        std::fs::write(root.join("ignoreddir/tracked.txt"), "forced\n").unwrap();

        git(&root, &["init"]);
        git(&root, &["add", "-A"]);
        git(&root, &["add", "-f", "tracked.log"]);
        git(&root, &["add", "-f", "ignoreddir/tracked.txt"]);
        // Leave the force-added directory file out of the first index so the
        // cache records ignoreddir/ as a blanket prefix, then prove a later
        // index update lets that tracked file refresh.
        git(&root, &["reset", "--", "ignoreddir/tracked.txt"]);
        git(&root, &["commit", "-m", "init"]);
        // Untracked, and not ignored until .gitignore grows a rule for it.
        std::fs::write(root.join("scratch.tmp"), "scratch\n").unwrap();

        let mut cache = octorus::local_watch::GitIgnoreCache::new(&root);
        let kind = EventKind::Create(CreateKind::File);

        assert!(!refresh(&mut cache, &root.join("target/debug/foo")));
        assert!(!refresh(
            &mut cache,
            &root.join("node_modules/pkg/index.js")
        ));
        assert!(!refresh(&mut cache, &root.join("noise.log")));
        assert!(!refresh(&mut cache, &root.join("sub/secret.txt")));
        assert!(!refresh(&mut cache, &root.join("ignoreddir/a.txt")));
        assert!(!refresh(&mut cache, PathBuf::from(".git/HEAD").as_path()));
        assert!(!refresh(
            &mut cache,
            PathBuf::from(".octorus/config.toml").as_path()
        ));

        assert!(refresh(&mut cache, &root.join("src/main.rs")));
        assert!(refresh(&mut cache, PathBuf::from("src/main.rs").as_path()));
        assert!(refresh(&mut cache, &root.join("keep.txt")));
        assert!(refresh(&mut cache, &root.join("sub/keep.rs")));
        assert!(refresh(&mut cache, &root.join("notes.txt")));
        assert!(refresh(&mut cache, &root.join("src/new_untracked.rs")));
        assert!(refresh(&mut cache, &root.join("tracked.log")));
        assert!(refresh(&mut cache, &root.join(".gitignore")));
        assert!(refresh(&mut cache, &root.join("scratch.tmp")));

        assert!(should_refresh_local_change(
            &[root.join(".git/HEAD"), root.join("src/main.rs")],
            &kind,
            &mut cache
        ));
        assert!(!should_refresh_local_change(
            &[root.join("target/debug/foo"), root.join("noise.log")],
            &kind,
            &mut cache
        ));

        // A tracked file under a previously blanket-ignored directory still
        // refreshes once the index change is observed. The index event itself
        // does not reload the diff.
        assert!(!refresh(&mut cache, &root.join("ignoreddir/tracked.txt")));
        git(&root, &["add", "-f", "ignoreddir/tracked.txt"]);
        git(&root, &["commit", "-m", "track forced file"]);
        assert!(!refresh(&mut cache, &root.join(".git/index")));
        assert!(refresh(&mut cache, &root.join("ignoreddir/tracked.txt")));
        assert!(!refresh(&mut cache, &root.join("ignoreddir/a.txt")));

        std::fs::write(
            root.join(".gitignore"),
            "target/\nnode_modules/\n*.log\nignoreddir/\nscratch.tmp\n",
        )
        .unwrap();
        assert!(refresh(&mut cache, &root.join(".gitignore")));
        assert!(!refresh(&mut cache, &root.join("scratch.tmp")));
        assert!(refresh(&mut cache, &root.join("src/main.rs")));
        assert!(refresh(&mut cache, &root.join("tracked.log")));
        assert!(refresh(&mut cache, &root.join("notes.txt")));
    }

    fn decide(
        cache: &mut octorus::local_watch::GitIgnoreCache,
        paths: &[std::path::PathBuf],
        rescan: bool,
        pending: bool,
    ) -> bool {
        octorus::local_watch::should_refresh_local_change(
            paths,
            &EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            rescan,
            pending,
            true,
            cache,
        )
    }

    /// Path of `target` relative to the process cwd, without resolving symlinks.
    fn relative_to_cwd(target: &std::path::Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let cwd_comps: Vec<_> = cwd.components().collect();
        let target_comps: Vec<_> = target.components().collect();
        let mut shared = 0;
        while shared < cwd_comps.len()
            && shared < target_comps.len()
            && cwd_comps[shared] == target_comps[shared]
        {
            shared += 1;
        }
        let mut rel = PathBuf::new();
        for _ in shared..cwd_comps.len() {
            rel.push("..");
        }
        for component in &target_comps[shared..] {
            rel.push(component);
        }
        if rel.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            rel
        }
    }

    #[test]
    fn symlinked_and_relative_working_dir_refreshes_tracked_file_under_target() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("target")).unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(repo.join("target/kept.rs"), "fn kept() {}\n").unwrap();
        git(&repo, &["init"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["add", "-f", "target/kept.rs"]);
        git(&repo, &["commit", "-m", "track file under target"]);

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        let mut via_link = octorus::local_watch::GitIgnoreCache::new(&link);
        assert!(
            refresh(&mut via_link, &link.join("target/kept.rs")),
            "event path under the symlink must refresh the tracked file"
        );
        assert!(refresh(
            &mut via_link,
            &repo.canonicalize().unwrap().join("target/kept.rs")
        ));
        assert!(refresh(
            &mut via_link,
            std::path::Path::new("./target/kept.rs")
        ));
        assert!(
            refresh(&mut via_link, std::path::Path::new("/unrelated/target/foo")),
            "a path outside the work tree refreshes instead of the target/ fallback"
        );

        let rel = relative_to_cwd(&repo);
        assert!(rel.is_relative());
        let mut via_rel = octorus::local_watch::GitIgnoreCache::new(&rel);
        assert!(refresh(&mut via_rel, &repo.join("target/kept.rs")));
        assert!(refresh(&mut via_rel, &rel.join("target/kept.rs")));
        assert!(refresh(
            &mut via_rel,
            std::path::Path::new("./target/kept.rs")
        ));
        assert!(!refresh(&mut via_rel, &repo.join("target/other.rs")));
        assert!(!refresh(
            &mut via_rel,
            &repo.canonicalize().unwrap().join("target/other.rs")
        ));

        // `<base>/other/lnk/../repo` where `lnk` points at the repo. Collapsing
        // `..` before canonicalize would look for `other/repo`, git would fail,
        // and the tracked file under target/ would stop refreshing.
        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&repo, other.join("lnk")).unwrap();
        let dotdot = other.join("lnk").join("..").join("repo");
        let mut via_dotdot = octorus::local_watch::GitIgnoreCache::new(&dotdot);
        assert!(
            refresh(&mut via_dotdot, &repo.join("target/kept.rs")),
            "git must see the work tree through a symlink followed by .."
        );
        assert!(refresh(&mut via_dotdot, &dotdot.join("target/kept.rs")));
        assert!(refresh(
            &mut via_dotdot,
            &repo.canonicalize().unwrap().join("target/kept.rs")
        ));
        assert!(!refresh(&mut via_dotdot, &repo.join("target/other.rs")));
        assert!(!refresh(&mut via_dotdot, &dotdot.join("target/missing.rs")));

        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&repo, &alias).unwrap();
        assert!(
            !refresh(&mut via_dotdot, &alias.join("target/missing.rs")),
            "a deleted file under another spelling of the repo stays ignored"
        );
    }

    #[test]
    fn exclude_and_excludes_file_unignore_without_refreshing_the_rule_edit() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("secret.txt"), "secret\n").unwrap();
        std::fs::write(root.join("other.txt"), "other\n").unwrap();
        git(&root, &["init"]);
        git(&root, &["add", "other.txt"]);
        git(&root, &["commit", "-m", "init"]);
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), "secret.txt\n").unwrap();

        let mut cache = octorus::local_watch::GitIgnoreCache::new(&root);
        assert!(!refresh(&mut cache, &root.join("secret.txt")));

        std::fs::write(root.join(".git/info/exclude"), "").unwrap();
        assert!(
            !refresh(&mut cache, &root.join(".git/info/exclude")),
            "editing exclude reloads rules and does not itself refresh"
        );
        assert!(refresh(&mut cache, &root.join("secret.txt")));

        let excludes = dir.path().join("excludes");
        std::fs::write(&excludes, "other.txt\n").unwrap();
        git(
            &root,
            &["config", "core.excludesFile", &excludes.to_string_lossy()],
        );
        assert!(!refresh(&mut cache, &root.join(".git/config")));
        // other.txt is tracked, so an excludes rule does not hide it.
        assert!(refresh(&mut cache, &root.join("other.txt")));

        std::fs::write(root.join("hidden.txt"), "hidden\n").unwrap();
        std::fs::write(&excludes, "hidden.txt\n").unwrap();
        assert!(!refresh(&mut cache, &root.join(".git/config")));
        assert!(!refresh(&mut cache, &root.join("hidden.txt")));
        std::fs::write(&excludes, "").unwrap();
        assert!(!refresh(&mut cache, &root.join(".git/config")));
        assert!(refresh(&mut cache, &root.join("hidden.txt")));
    }

    #[test]
    fn event_burst_does_not_drop_the_next_refresh() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        git(&root, &["init"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-m", "init"]);

        let mut cache = octorus::local_watch::GitIgnoreCache::new(&root);
        assert!(refresh(&mut cache, &root.join("src/main.rs")));

        let mut burst = vec![root.join("src/main.rs")];
        for i in 0..3_000 {
            burst.push(root.join(format!("target/obj{i}.o")));
        }
        let started = std::time::Instant::now();
        assert!(decide(&mut cache, &burst, false, false));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(800),
            "the first refreshing path must not wait on the rest of the burst"
        );

        assert!(
            !decide(&mut cache, &burst, false, true),
            "a refresh already pending skips classification"
        );
        assert!(
            refresh(&mut cache, &root.join("src/main.rs")),
            "the next real change still refreshes"
        );
        assert!(
            decide(&mut cache, &[], true, false),
            "a rescan with no paths refreshes"
        );
        assert!(!decide(&mut cache, &[], false, false));
    }
}
