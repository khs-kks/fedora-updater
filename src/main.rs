#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use clap::Parser;
use colored::*;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncRead;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Fedora system updater that handles both Flatpak and DNF5 updates
#[derive(Parser, Debug)]
#[command(
    author = "khs-kks",
    version,
    about = "A command-line utility to update Fedora systems through Flatpak and DNF5",
    after_help = "Repository: https://github.com/khs-kks/fedora-updater\nBuild Date: Feb 2025",
    help_template = "{about}\n\nUsage: {name} [OPTIONS]\n\nOptions:\n{options}\n\nAuthor: {author}{after-help}"
)]
struct Cli {
    /// Enable interactive mode for choosing update type
    #[arg(short, long)]
    interactive: bool,
}

/// Struct to manage command availability caching
#[derive(Debug)]
struct CommandCache {
    cache: HashMap<String, bool>,
}

impl CommandCache {
    fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// Preloads availability of commonly used commands
    /// Call this at startup to avoid async overhead during actual operations
    async fn preload_common_commands(&mut self) {
        // List of commonly used commands in this application
        // Using 'static str to avoid temporary allocations
        static COMMON_COMMANDS: [&str; 4] = ["flatpak", "dnf5", "cat", "uname"];

        // Check commands concurrently with minimum allocations
        let mut handles = Vec::with_capacity(COMMON_COMMANDS.len());

        for &cmd in &COMMON_COMMANDS {
            // Spawn a task for each command
            let handle = tokio::spawn(async move {
                let available = Command::new("which")
                    .arg(cmd) // Pass &str directly
                    .output()
                    .await
                    .map(|output| output.status.success())
                    .unwrap_or(false);
                (cmd, available)
            });

            handles.push(handle);
        }

        // Await all tasks and collect results
        for handle in handles {
            if let Ok((cmd, available)) = handle.await {
                // We only allocate strings when we need to store them in the cache
                self.cache.insert(cmd.to_string(), available);
            }
        }
    }

    /// Checks if a command is cached as available
    fn is_cached_available(&self, command: &str) -> Option<bool> {
        self.cache.get(command).copied()
    }

    /// Gets the availability of a command, returning immediately if cached
    /// This is a convenience method to avoid needing .await when we already know the result
    /// Returns None if the result isn't cached yet
    fn get_cached_availability(&self, command: &str) -> Option<bool> {
        self.is_cached_available(command)
    }

    /// Checks if a command is available
    /// Returns immediately with cached result if available
    async fn is_command_available(&mut self, command: &str) -> bool {
        // Fast path: return cached result if available
        if let Some(available) = self.is_cached_available(command) {
            return available;
        }

        // Slow path: check command availability and cache the result
        let available = Command::new("which")
            .arg(command)
            .output()
            .await
            .map(|output| output.status.success())
            .unwrap_or(false);

        self.cache.insert(command.to_string(), available);
        available
    }

    /// Executes a command if it's available, returns None if command is not available
    async fn execute_if_available(
        &mut self,
        command: &str,
        args: &[&str],
    ) -> Option<std::process::Output> {
        // Fast path: if we already know the command is unavailable, return None immediately
        if let Some(false) = self.is_cached_available(command) {
            return None;
        }

        // Check if command is available (uses cache if possible)
        if !self.is_command_available(command).await {
            return None;
        }

        // Log the command that's about to be executed
        let cmd_str = format!("{} {}", command, args.join(" "));
        println!("{} {}", "Executing command:".cyan().bold(), cmd_str.cyan());

        Command::new(command).args(args).output().await.ok()
    }
}

/// Represents a single line of output from a command
#[derive(Debug)]
enum OutputMessage {
    Stdout(String),
    Stderr(String),
}

/// Handles printing output messages in a serialized manner
async fn output_handler(mut rx: mpsc::Receiver<OutputMessage>) {
    while let Some(message) = rx.recv().await {
        match message {
            OutputMessage::Stdout(line) => println!("{} {}", "[stdout]".blue(), line),
            OutputMessage::Stderr(line) => eprintln!("{} {}", "[stderr]".red(), line),
        }
    }
}

/// Handles a command's output stream asynchronously
async fn handle_output_stream<R>(
    reader: BufReader<R>,
    is_stderr: bool,
    content: Option<Arc<Mutex<String>>>,
    tx: mpsc::Sender<OutputMessage>,
) where
    R: AsyncRead + Unpin,
{
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        // Store the line if we have a content buffer
        if let Some(content) = &content {
            if let Ok(mut guard) = content.lock() {
                guard.push_str(&line);
                guard.push('\n');
            }
        }

        // Send the line to our output handler
        let message = if is_stderr {
            OutputMessage::Stderr(line)
        } else {
            OutputMessage::Stdout(line)
        };

        // Ignore send errors (happens when receiver is dropped)
        let _ = tx.send(message).await;
    }
}

/// Executes a command and streams its output in real-time
async fn execute_command(
    command: &str,
    args: &[&str],
    sudo: bool,
) -> Result<(std::process::ExitStatus, String)> {
    // Log the command that's about to be executed
    let cmd_str = if sudo {
        format!("sudo {} {}", command, args.join(" "))
    } else {
        format!("{} {}", command, args.join(" "))
    };
    println!("{} {}", "Executing command:".cyan().bold(), cmd_str.cyan());

    let mut cmd = if sudo {
        let mut c = Command::new("sudo");
        c.arg(command);
        c.args(args);
        c
    } else {
        let mut c = Command::new(command);
        c.args(args);
        c
    };

    // Configure command to pipe stdout and stderr
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to execute {} command", command))?;

    // Get handles to stdout and stderr
    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let stderr = child.stderr.take().expect("Failed to capture stderr");

    // Create readers for stdout and stderr
    let stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);

    // Create a string to store stdout for later analysis
    let stdout_content = Arc::new(Mutex::new(String::new()));
    let stdout_content_clone = Arc::clone(&stdout_content);

    // Create channel for output handling
    let (tx, rx) = mpsc::channel(100); // Buffer size of 100 messages
    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();

    // Spawn the output handler task
    let output_handler_task = tokio::spawn(output_handler(rx));

    // Spawn tasks to handle stdout and stderr
    let stdout_handle = tokio::spawn(handle_output_stream(
        stdout_reader,
        false,
        Some(stdout_content_clone),
        tx_stdout,
    ));
    let stderr_handle = tokio::spawn(handle_output_stream(stderr_reader, true, None, tx_stderr));

    // Drop the original sender so the output handler will exit when stdout/stderr tasks complete
    drop(tx);

    // Wait for the command to complete
    let status = child.wait().await?;

    // Wait for output tasks to finish
    let _ = tokio::try_join!(stdout_handle, stderr_handle)?;

    // Wait for output handler to finish
    output_handler_task.await?;

    // Get the captured output
    let output = stdout_content
        .lock()
        .expect("Failed to access stdout content")
        .clone();

    Ok((status, output))
}

/// Displays system information
async fn show_system_info(cmd_cache: &mut CommandCache) -> Result<()> {
    println!("{}", "System Information:".blue().bold());

    // Distribution info
    if let Ok(output) = Command::new("cat").arg("/etc/os-release").output().await {
        let info = String::from_utf8_lossy(&output.stdout);
        if let Some(line) = info.lines().find(|l| l.starts_with("PRETTY_NAME=")) {
            println!(
                "Distribution: {}",
                line.split('=')
                    .nth(1)
                    .unwrap_or("Unknown")
                    .trim_matches('"')
            );
        }
    }

    // Kernel version
    if let Ok(output) = Command::new("uname").arg("-r").output().await {
        println!("Kernel: {}", String::from_utf8_lossy(&output.stdout).trim());
    }

    // Flatpak version
    if let Some(output) = cmd_cache
        .execute_if_available("flatpak", &["--version"])
        .await
    {
        println!(
            "Flatpak: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }

    // DNF5 version
    if let Some(output) = cmd_cache.execute_if_available("dnf5", &["--version"]).await {
        print!("DNF5: {}", String::from_utf8_lossy(&output.stdout));
    }

    Ok(())
}

/// Handles Flatpak updates
async fn update_flatpak(cmd_cache: &mut CommandCache) -> Result<bool> {
    // Try to use cached result first to avoid async overhead
    let flatpak_available = match cmd_cache.get_cached_availability("flatpak") {
        Some(available) => available,
        None => cmd_cache.is_command_available("flatpak").await,
    };

    if !flatpak_available {
        println!(
            "{}",
            "Flatpak is not installed. Skipping Flatpak updates.".yellow()
        );
        return Ok(false);
    }

    println!("{}", "Updating Flatpak packages...".green());

    let (status, output) = execute_command("flatpak", &["update", "-y"], false).await?;

    if !status.success() {
        return Err(anyhow::anyhow!("Flatpak update failed"));
    }

    // Check if there were any updates
    Ok(!output.contains("Nothing to do"))
}

/// Handles DNF5 updates
async fn update_dnf5(cmd_cache: &mut CommandCache, interactive: bool) -> Result<bool> {
    // Try to use cached result first to avoid async overhead
    let dnf5_available = match cmd_cache.get_cached_availability("dnf5") {
        Some(available) => available,
        None => cmd_cache.is_command_available("dnf5").await,
    };

    if !dnf5_available {
        println!(
            "{}",
            "DNF5 is not installed. Please install it first.".red()
        );
        return Err(anyhow::anyhow!("DNF5 not found"));
    }

    println!("{}", "Checking for DNF5 updates...".green());

    // Check for updates - exit code 100 means updates are available
    let (status, _) = execute_command("dnf5", &["--refresh", "check-upgrade"], true).await?;

    let has_updates = status.code() == Some(100);
    if !has_updates {
        println!("{}", "No DNF5 updates available.".green());
        return Ok(false);
    }

    println!("{}", "DNF5 updates are available.".green());

    let update_mode = if interactive {
        println!("\nChoose update mode:");
        println!("1. Immediate update (type 'now')");
        println!("2. Offline update (press Enter)");

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        if input.trim().to_lowercase() == "now" {
            "immediate"
        } else {
            "offline"
        }
    } else {
        "immediate"
    };

    match update_mode {
        "immediate" => {
            println!("{}", "Performing immediate DNF5 update...".green());
            let (status, _) = execute_command("dnf5", &["upgrade", "-y"], true).await?;
            if !status.success() {
                return Err(anyhow::anyhow!("DNF5 update failed"));
            }

            // Check if reboot is needed
            match execute_command("dnf5", &["needs-restarting"], true).await {
                Ok(_) => {
                    // needs-restarting already printed its output
                    // No need to show additional message as the command itself is clear
                }
                Err(e) => {
                    println!(
                        "{}",
                        "Warning: Could not determine if restart is needed.".yellow()
                    );
                    eprintln!("Error checking restart status: {}", e);
                }
            }
        }
        "offline" => {
            println!("{}", "Preparing offline DNF5 update...".green());
            let (status, _) =
                execute_command("dnf5", &["upgrade", "--offline", "-y"], true).await?;
            if !status.success() {
                return Err(anyhow::anyhow!("DNF5 offline update preparation failed"));
            }
            println!(
                "{}",
                "Offline update prepared. Changes will be applied on next reboot.".yellow()
            );
        }
        _ => unreachable!(),
    }

    Ok(true)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cmd_cache = CommandCache::new();

    println!("{}", "Fedora Updater".green().bold());
    println!("─────────────────────────────\n");

    // Preload command availability checks to reduce async overhead later
    cmd_cache.preload_common_commands().await;

    show_system_info(&mut cmd_cache).await?;
    println!("\n{}", "Starting update process...".green());

    let flatpak_result = update_flatpak(&mut cmd_cache).await;
    let dnf5_result = update_dnf5(&mut cmd_cache, cli.interactive).await;

    match (flatpak_result, dnf5_result) {
        (Ok(flatpak_updated), Ok(dnf_updated)) => {
            if flatpak_updated || dnf_updated {
                println!(
                    "{}",
                    "\nUpdates were successfully installed!".green().bold()
                )
            } else {
                println!(
                    "{}",
                    "\nSystem is up to date. No updates needed.".green().bold()
                )
            }
        }
        (Err(_), Ok(_)) => println!(
            "{}",
            "\nWarning: Flatpak updates failed, but DNF5 updates succeeded.".yellow()
        ),
        (Ok(_), Err(_)) => println!(
            "{}",
            "\nWarning: DNF5 updates failed, but Flatpak updates succeeded.".yellow()
        ),
        (Err(_), Err(_)) => {
            println!("{}", "\nError: Both update mechanisms failed.".red().bold());
            return Err(anyhow::anyhow!("All update mechanisms failed"));
        }
    }

    Ok(())
}
