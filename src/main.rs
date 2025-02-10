use anyhow::{Context, Result};
use clap::Parser;
use colored::*;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread;

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
#[derive(Debug, Default)]
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
    fn is_command_available(&mut self, command: &str) -> bool {
        *self.cache.entry(command.to_string()).or_insert_with(|| {
            Command::new("which")
                .arg(command)
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        })
    }

    /// Executes a command if it's available, returns None if command is not available
    fn execute_if_available(
        &mut self,
        command: &str,
        args: &[&str],
    ) -> Option<std::process::Output> {
        if self.is_command_available(command) {
            Command::new(command).args(args).output().ok()
        } else {
            None
        }
    }
}

/// Handles a command's output stream in a separate thread
fn handle_output_stream(
    reader: BufReader<impl std::io::Read + Send + 'static>,
    is_stderr: bool,
    content: Option<Arc<Mutex<String>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        reader.lines().for_each(|line| {
            if let Ok(line) = line {
                // Store the line if we have a content buffer
                if let Some(content) = &content {
                    if let Ok(mut guard) = content.lock() {
                        guard.push_str(&line);
                        guard.push('\n');
                    }
                }
                // Print to appropriate stream
                if is_stderr {
                    eprintln!("{}", line);
                } else {
                    println!("{}", line);
                }
            }
        });
    })
}

/// Executes a command and streams its output in real-time
fn execute_command(command: &str, args: &[&str], sudo: bool) -> Result<()> {
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

    // Spawn a thread to handle stdout
    let stdout_handle = handle_output_stream(stdout_reader, false, Some(stdout_content_clone));

    // Spawn a thread to handle stderr
    let stderr_handle = handle_output_stream(stderr_reader, true, None);

    // Wait for the command to complete
    let status = child.wait()?;

    // Wait for output threads to finish
    stdout_handle.join().expect("Failed to join stdout thread");
    stderr_handle.join().expect("Failed to join stderr thread");

    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{} command failed", command))
    }
}

/// Displays system information
fn show_system_info(cmd_cache: &mut CommandCache) -> Result<()> {
    println!("{}", "System Information:".blue().bold());

    // Distribution info
    if let Ok(output) = Command::new("cat").arg("/etc/os-release").output() {
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
    if let Ok(output) = Command::new("uname").arg("-r").output() {
        println!("Kernel: {}", String::from_utf8_lossy(&output.stdout).trim());
    }

    // Flatpak version
    if let Some(output) = cmd_cache.execute_if_available("flatpak", &["--version"]) {
        println!(
            "Flatpak: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }

    // DNF5 version
    if let Some(output) = cmd_cache.execute_if_available("dnf5", &["--version"]) {
        print!("DNF5: {}", String::from_utf8_lossy(&output.stdout));
    }

    Ok(())
}

/// Handles Flatpak updates
fn update_flatpak(cmd_cache: &mut CommandCache) -> Result<bool> {
    if !cmd_cache.is_command_available("flatpak") {
        println!(
            "{}",
            "Flatpak is not installed. Skipping Flatpak updates.".yellow()
        );
        return Ok(false);
    }

    println!("{}", "Updating Flatpak packages...".green());

    // Run flatpak update with -y flag and stream output
    let mut child = Command::new("flatpak")
        .args(["update", "-y"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "Failed to execute flatpak update")?;

    // Get handles to stdout and stderr
    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let stderr = child.stderr.take().expect("Failed to capture stderr");

    // Create readers for stdout and stderr
    let stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);

    // Create a string to store stdout for later analysis
    let stdout_content = Arc::new(Mutex::new(String::new()));
    let stdout_content_clone = Arc::clone(&stdout_content);

    // Spawn a thread to handle stdout
    let stdout_handle = handle_output_stream(stdout_reader, false, Some(stdout_content_clone));

    // Spawn a thread to handle stderr
    let stderr_handle = handle_output_stream(stderr_reader, true, None);

    // Wait for the command to complete
    let status = child.wait()?;

    // Wait for output threads to finish
    stdout_handle.join().expect("Failed to join stdout thread");
    stderr_handle.join().expect("Failed to join stderr thread");

    if !status.success() {
        return Err(anyhow::anyhow!("Flatpak update failed"));
    }

    // Check the captured output to determine if updates were performed
    let output_content = stdout_content
        .lock()
        .expect("Failed to access stdout content");
    Ok(!output_content.contains("Nothing to do"))
}

/// Handles DNF5 updates
fn update_dnf5(cmd_cache: &mut CommandCache, interactive: bool) -> Result<bool> {
    if !cmd_cache.is_command_available("dnf5") {
        println!(
            "{}",
            "DNF5 is not installed. Please install it first.".red()
        );
        return Err(anyhow::anyhow!("DNF5 not found"));
    }

    println!("{}", "Checking for DNF5 updates...".green());

    // Check for updates - exit code 100 means updates are available
    let mut check_result = Command::new("sudo")
        .args(["dnf5", "--refresh", "check-upgrade"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "Failed to execute dnf5 check-upgrade")?;

    // Get handles to stdout and stderr
    let stdout = check_result
        .stdout
        .take()
        .expect("Failed to capture stdout");
    let stderr = check_result
        .stderr
        .take()
        .expect("Failed to capture stderr");

    // Create readers for stdout and stderr
    let stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);

    // Handle stdout
    let stdout_handle = handle_output_stream(stdout_reader, false, None);

    // Handle stderr
    let stderr_handle = handle_output_stream(stderr_reader, true, None);

    // Wait for the command to complete
    let status = check_result.wait()?;

    // Wait for output threads to finish
    stdout_handle.join().expect("Failed to join stdout thread");
    stderr_handle.join().expect("Failed to join stderr thread");

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
            execute_command("dnf5", &["upgrade", "-y"], true)?;

            // Check if reboot is needed
            match execute_command("dnf5", &["needs-restarting"], true) {
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
            execute_command("dnf5", &["upgrade", "--offline", "-y"], true)?;
            println!(
                "{}",
                "Offline update prepared. Changes will be applied on next reboot.".yellow()
            );
        }
        _ => unreachable!(),
    }

    Ok(true)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cmd_cache = CommandCache::new();

    println!("{}", "Fedora Updater".green().bold());
    println!("─────────────────────────────\n");

    show_system_info(&mut cmd_cache)?;
    println!("\n{}", "Starting update process...".green());

    let flatpak_result = update_flatpak(&mut cmd_cache);
    let dnf5_result = update_dnf5(&mut cmd_cache, cli.interactive);

    match (flatpak_result, dnf5_result) {
        (Ok(flatpak_updated), Ok(dnf_updated)) => {
            if flatpak_updated || dnf_updated {
                println!("{}", "\nAll updates completed successfully!".green().bold())
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
