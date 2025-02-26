#![forbid(unsafe_code)]
#![deny(warnings)]

use anyhow::{Context, Result};
use clap::Parser;
use colored::*;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Pre-allocated buffer capacity for command output strings
const DEFAULT_OUTPUT_CAPACITY: usize = 4096;

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

/// Struct to manage output streams and handle line-by-line output
#[derive(Debug)]
struct CommandRunner {
    cmd_cache: CommandCache,
    // Pre-allocated buffer for command output, reused across commands
    output_buffer: String,
}

impl CommandRunner {
    /// Creates a new CommandRunner with pre-allocated resources
    fn new() -> Self {
        Self {
            cmd_cache: CommandCache::new(),
            output_buffer: String::with_capacity(DEFAULT_OUTPUT_CAPACITY),
        }
    }

    /// Preloads common commands into the cache
    async fn preload_common_commands(&mut self) {
        self.cmd_cache.preload_common_commands().await;
    }

    /// Executes a command and streams its output in real-time
    async fn execute_command(
        &mut self,
        command: &str,
        args: &[&str],
        sudo: bool,
    ) -> Result<(std::process::ExitStatus, String)> {
        // Clear the buffer before reusing
        self.output_buffer.clear();

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
        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        // Create a channel for output handling
        let (tx, rx) = mpsc::channel(100); // Buffer size of 100 messages
        let output_handler_task = tokio::spawn(output_handler(rx));

        // Process stdout and stderr in parallel
        let output = Arc::new(Mutex::new(String::with_capacity(DEFAULT_OUTPUT_CAPACITY)));
        let output_clone = Arc::clone(&output);

        // Create separate clones of the sender for each task
        let tx_stdout = tx.clone();
        let tx_stderr = tx.clone();

        let stdout_task = tokio::spawn(async move {
            while let Ok(Some(line)) = stdout_reader.next_line().await {
                // Store the line for later analysis
                if let Ok(mut guard) = output_clone.lock() {
                    guard.push_str(&line);
                    guard.push('\n');
                }

                let _ = tx_stdout.send(OutputMessage::Stdout(line)).await;
            }
        });

        let stderr_task = tokio::spawn(async move {
            while let Ok(Some(line)) = stderr_reader.next_line().await {
                let _ = tx_stderr.send(OutputMessage::Stderr(line)).await;
            }
        });

        // Wait for the command to complete
        let status = child.wait().await?;

        // Wait for output handling to complete
        drop(tx); // Close the original sender
        let _ = tokio::try_join!(stdout_task, stderr_task)?;
        output_handler_task.await?;

        // Extract the result
        let result = output.lock().expect("Failed to lock output").clone();

        // Copy to our reusable buffer for the next caller
        self.output_buffer = result.clone();

        Ok((status, result))
    }

    /// Displays system information
    async fn show_system_info(&mut self) -> Result<()> {
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
        if let Some(output) = self
            .cmd_cache
            .execute_if_available("flatpak", &["--version"])
            .await
        {
            println!(
                "Flatpak: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }

        // DNF5 version
        if let Some(output) = self
            .cmd_cache
            .execute_if_available("dnf5", &["--version"])
            .await
        {
            print!("DNF5: {}", String::from_utf8_lossy(&output.stdout));
        }

        Ok(())
    }

    /// Handles Flatpak updates
    async fn update_flatpak(&mut self) -> Result<bool> {
        // Try to use cached result first to avoid async overhead
        let flatpak_available = match self.cmd_cache.get_cached_availability("flatpak") {
            Some(available) => available,
            None => self.cmd_cache.is_command_available("flatpak").await,
        };

        if !flatpak_available {
            println!(
                "{}",
                "Flatpak is not installed. Skipping Flatpak updates.".yellow()
            );
            return Ok(false);
        }

        println!("{}", "Updating Flatpak packages...".green());

        let (status, output) = self
            .execute_command("flatpak", &["update", "-y"], false)
            .await?;

        if !status.success() {
            return Err(anyhow::anyhow!("Flatpak update failed"));
        }

        // Check if there were any updates
        Ok(!output.contains("Nothing to do"))
    }

    /// Handles DNF5 updates
    async fn update_dnf5(&mut self, interactive: bool) -> Result<bool> {
        // Try to use cached result first to avoid async overhead
        let dnf5_available = match self.cmd_cache.get_cached_availability("dnf5") {
            Some(available) => available,
            None => self.cmd_cache.is_command_available("dnf5").await,
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
        let (status, _) = self
            .execute_command("dnf5", &["--refresh", "check-upgrade"], true)
            .await?;

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
                let (status, _) = self
                    .execute_command("dnf5", &["upgrade", "-y"], true)
                    .await?;
                if !status.success() {
                    return Err(anyhow::anyhow!("DNF5 update failed"));
                }

                // Check if reboot is needed
                match self
                    .execute_command("dnf5", &["needs-restarting"], true)
                    .await
                {
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
                let (status, _) = self
                    .execute_command("dnf5", &["upgrade", "--offline", "-y"], true)
                    .await?;
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cmd_runner = CommandRunner::new();

    println!("{}", "Fedora Updater".green().bold());
    println!("─────────────────────────────\n");

    // Preload command availability checks to reduce async overhead later
    cmd_runner.preload_common_commands().await;

    cmd_runner.show_system_info().await?;
    println!("\n{}", "Starting update process...".green());

    let flatpak_result = cmd_runner.update_flatpak().await;
    let dnf5_result = cmd_runner.update_dnf5(cli.interactive).await;

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
