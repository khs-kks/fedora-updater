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

    /// Checks if a command is available, using cached results if available
    async fn is_command_available(&mut self, command: &str) -> bool {
        if let Some(&available) = self.cache.get(command) {
            return available;
        }

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
        if self.is_command_available(command).await {
            Command::new(command).args(args).output().await.ok()
        } else {
            None
        }
    }
}

/// Handles a command's output stream asynchronously
async fn handle_output_stream<R>(
    reader: BufReader<R>,
    is_stderr: bool,
    content: Option<Arc<Mutex<String>>>,
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
        // Print to appropriate stream with prefix
        if is_stderr {
            eprintln!("{} {}", "[stderr]".red(), line);
        } else {
            println!("{} {}", "[stdout]".blue(), line);
        }
    }
}

/// Executes a command and streams its output in real-time
async fn execute_command(
    command: &str,
    args: &[&str],
    sudo: bool,
) -> Result<(std::process::ExitStatus, String)> {
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

    // Spawn tasks to handle stdout and stderr
    let stdout_handle = tokio::spawn(handle_output_stream(
        stdout_reader,
        false,
        Some(stdout_content_clone),
    ));
    let stderr_handle = tokio::spawn(handle_output_stream(stderr_reader, true, None));

    // Wait for the command to complete
    let status = child.wait().await?;

    // Wait for output tasks to finish
    let _ = tokio::try_join!(stdout_handle, stderr_handle)?;

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
    if !cmd_cache.is_command_available("flatpak").await {
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
    if !cmd_cache.is_command_available("dnf5").await {
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
