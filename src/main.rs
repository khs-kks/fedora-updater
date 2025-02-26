#![forbid(unsafe_code)]
#![deny(warnings)]

use anyhow::{Context, Result};
use clap::Parser;
use colored::*;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};

/// Pre-allocated buffer capacity for command output strings
const DEFAULT_OUTPUT_CAPACITY: usize = 4096;
const DEFAULT_CHANNEL_CAPACITY: usize = 100;
const DEFAULT_LINE_CAPACITY: usize = 256;
const STRING_POOL_SIZE: usize = 32;

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
    // Use a static array of known commands to avoid heap allocations
    // This acts as a simple string interning mechanism
    known_commands: [&'static str; 4],
    // Store availability as a fixed-size array matching known_commands
    availability: [Option<bool>; 4],
}

impl CommandCache {
    fn new() -> Self {
        Self {
            known_commands: ["flatpak", "dnf5", "cat", "uname"],
            availability: [None, None, None, None],
        }
    }

    /// Preloads availability of commonly used commands
    /// Call this at startup to avoid async overhead during actual operations
    async fn preload_common_commands(&mut self) {
        // Check commands concurrently with minimum allocations
        let mut handles = Vec::with_capacity(self.known_commands.len());

        for (idx, &cmd) in self.known_commands.iter().enumerate() {
            // Spawn a task for each command
            let handle = tokio::spawn(async move {
                let available = Command::new("which")
                    .arg(cmd) // Pass &str directly
                    .output()
                    .await
                    .map(|output| output.status.success())
                    .unwrap_or(false);
                (idx, available)
            });

            handles.push(handle);
        }

        // Await all tasks and collect results
        for handle in handles {
            if let Ok((idx, available)) = handle.await {
                // Store result in our fixed-size array - no heap allocation
                self.availability[idx] = Some(available);
            }
        }
    }

    /// Checks if a command is cached as available
    fn is_cached_available(&self, command: &str) -> Option<bool> {
        // Check our static array - this is very fast
        for (idx, &cmd) in self.known_commands.iter().enumerate() {
            if cmd == command {
                return self.availability[idx];
            }
        }

        // Command not in our known list
        None
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

        // Check if this is one of our known commands
        for (idx, &cmd) in self.known_commands.iter().enumerate() {
            if cmd == command {
                self.availability[idx] = Some(available);
                return available;
            }
        }

        // Command not in our known list - return false as we don't support it
        false
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

/// Represents a single line of output from a command with a tag to indicate source
#[derive(Debug, Clone, Copy)]
enum OutputSource {
    Stdout,
    Stderr,
}

/// A string buffer that can be reused to avoid allocations
#[derive(Debug)]
struct StringBuffer {
    buffer: String,
}

impl StringBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: String::with_capacity(capacity),
        }
    }

    fn clear(&mut self) {
        self.buffer.clear();
    }

    fn as_str(&self) -> &str {
        &self.buffer
    }
}

/// A pool of string buffers that can be reused
#[derive(Debug)]
struct StringBufferPool {
    buffers: Vec<StringBuffer>,
}

impl StringBufferPool {
    fn new(size: usize, buffer_capacity: usize) -> Self {
        let mut buffers = Vec::with_capacity(size);
        for _ in 0..size {
            buffers.push(StringBuffer::new(buffer_capacity));
        }
        Self { buffers }
    }

    fn get(&mut self) -> StringBuffer {
        self.buffers
            .pop()
            .unwrap_or_else(|| StringBuffer::new(DEFAULT_LINE_CAPACITY))
    }

    fn return_buffer(&mut self, mut buffer: StringBuffer) {
        buffer.clear();
        if self.buffers.len() < STRING_POOL_SIZE {
            self.buffers.push(buffer);
        }
    }
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
    ) -> Result<(std::process::ExitStatus, &str)> {
        // Clear the buffer before reusing
        self.output_buffer.clear();

        // Log the command that's about to be executed
        // Avoid string allocation by building command display directly
        print!("{} ", "Executing command:".cyan().bold());
        if sudo {
            print!("{} ", "sudo".cyan());
        }
        print!("{} ", command.cyan());

        // Print arguments directly to avoid join allocation
        for arg in args {
            print!("{} ", arg.cyan());
        }
        println!();

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
        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let stderr = child.stderr.take().context("Failed to capture stderr")?;

        // Create readers for stdout and stderr
        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        // Create a shared buffer pool for output lines
        let buffer_pool = Arc::new(Mutex::new(StringBufferPool::new(
            STRING_POOL_SIZE,
            DEFAULT_LINE_CAPACITY,
        )));

        // Create a channel for output handling
        let (tx, rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);
        let output_handler_task = tokio::spawn(output_handler(rx, buffer_pool.clone()));

        // Use a channel for accumulating output - now using StringBuffer instead of String
        let (line_tx, mut line_rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);

        // Create separate clones of the sender for each task
        let tx_stdout = tx.clone();
        let line_tx_clone = line_tx.clone();
        let stdout_pool = buffer_pool.clone();

        let stdout_task = tokio::spawn(async move {
            while let Ok(Some(line)) = stdout_reader.next_line().await {
                // Get a buffer from the pool for the output handler
                let mut pool_guard = stdout_pool.lock().await;
                let mut output_buffer = pool_guard.get();
                let mut accum_buffer = pool_guard.get();
                drop(pool_guard); // Release the lock before further operations

                // Fill both buffers with the same content
                output_buffer.buffer.push_str(&line);
                accum_buffer.buffer.push_str(&line);

                // Send buffers to their respective channels
                let _ = line_tx_clone.send(accum_buffer).await;
                let _ = tx_stdout.send((OutputSource::Stdout, output_buffer)).await;
            }
        });

        let tx_stderr = tx.clone();
        let stderr_pool = buffer_pool.clone();

        let stderr_task = tokio::spawn(async move {
            while let Ok(Some(line)) = stderr_reader.next_line().await {
                // Get a buffer from the pool and fill it
                let mut pool_guard = stderr_pool.lock().await;
                let mut buffer = pool_guard.get();
                drop(pool_guard); // Release the lock before further operations

                buffer.buffer.push_str(&line);

                // Send the buffer to the output handler
                let _ = tx_stderr.send((OutputSource::Stderr, buffer)).await;
            }
        });

        // Wait for the command to complete
        let status = child.wait().await?;

        // Close senders to signal completion
        drop(tx);
        drop(line_tx);

        // Wait for output handling to complete
        let _ = tokio::try_join!(stdout_task, stderr_task)
            .context("Failed to join stdout/stderr tasks")?;
        output_handler_task
            .await
            .context("Failed to join output handler task")?;

        // Collect output lines from channel into our pre-allocated buffer
        while let Some(buffer) = line_rx.recv().await {
            self.output_buffer.push_str(buffer.as_str());
            self.output_buffer.push('\n');

            // Return the buffer to the pool
            let mut pool = buffer_pool.lock().await;
            pool.return_buffer(buffer);
        }

        // Return a reference to our buffer to avoid cloning
        Ok((status, &self.output_buffer))
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
async fn output_handler(
    mut rx: mpsc::Receiver<(OutputSource, StringBuffer)>,
    buffer_pool: Arc<Mutex<StringBufferPool>>,
) {
    while let Some((source, buffer)) = rx.recv().await {
        match source {
            OutputSource::Stdout => println!("{} {}", "[stdout]".blue(), buffer.as_str()),
            OutputSource::Stderr => eprintln!("{} {}", "[stderr]".red(), buffer.as_str()),
        }

        // Return the buffer to the pool
        let mut pool = buffer_pool.lock().await;
        pool.return_buffer(buffer);
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
