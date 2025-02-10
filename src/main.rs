use anyhow::{Context, Result};
use clap::Parser;
use colored::*;
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::process::Stdio;

/// Fedora system updater that handles both Flatpak and DNF5 updates
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// Enable interactive mode for choosing update type
    #[arg(short, long)]
    interactive: bool,
}

/// Checks if a command is available in the system
fn is_command_available(command: &str) -> bool {
    Command::new("which")
        .arg(command)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
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

    // Spawn a thread to handle stdout
    let stdout_handle = std::thread::spawn(move || {
        stdout_reader.lines().for_each(|line| {
            if let Ok(line) = line {
                println!("{}", line);
            }
        });
    });

    // Spawn a thread to handle stderr
    let stderr_handle = std::thread::spawn(move || {
        stderr_reader.lines().for_each(|line| {
            if let Ok(line) = line {
                eprintln!("{}", line);
            }
        });
    });

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

/// Handles Flatpak updates
fn update_flatpak() -> Result<bool> {
    if !is_command_available("flatpak") {
        println!(
            "{}",
            "Flatpak is not installed. Skipping Flatpak updates.".yellow()
        );
        return Ok(false);
    }

    println!("{}", "Updating Flatpak packages...".green());

    // Run flatpak update with -y flag to automatically accept updates
    let status = execute_command("flatpak", &["update", "-y"], false);

    // Exit code 77 means no updates were available
    match status {
        Ok(_) => Ok(true),
        Err(e) => {
            if e.to_string().contains("exit code: 77") {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// Handles DNF5 updates
fn update_dnf5(interactive: bool) -> Result<bool> {
    if !is_command_available("dnf5") {
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
    let stdout_handle = std::thread::spawn(move || {
        stdout_reader.lines().for_each(|line| {
            if let Ok(line) = line {
                println!("{}", line);
            }
        });
    });

    // Handle stderr
    let stderr_handle = std::thread::spawn(move || {
        stderr_reader.lines().for_each(|line| {
            if let Ok(line) = line {
                eprintln!("{}", line);
            }
        });
    });

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

/// Displays system information
fn show_system_info() -> Result<()> {
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
    if is_command_available("flatpak") {
        if let Ok(output) = Command::new("flatpak").arg("--version").output() {
            println!(
                "Flatpak: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
    }

    // DNF5 version - show complete output
    if is_command_available("dnf5") {
        if let Ok(output) = Command::new("dnf5").arg("--version").output() {
            // Print all lines of DNF5 version info
            let version_info = String::from_utf8_lossy(&output.stdout);
            print!("DNF5: {}", version_info);
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    println!("{}", "Fedora Updater".green().bold());
    println!("─────────────────────────────\n");

    show_system_info()?;
    println!("\n{}", "Starting update process...".green());

    let flatpak_result = update_flatpak();
    let dnf5_result = update_dnf5(cli.interactive);

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
