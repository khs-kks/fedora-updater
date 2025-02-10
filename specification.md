# Fedora Updater Specification

## Overview

The Fedora Updater is a command-line utility designed to update a Fedora system through two main mechanisms:

### 1. Flatpak Updates
- The program must detect if Flatpak is installed and, if so, perform a system update for Flatpak packages without requiring elevated privileges.

### 2. DNF5 Updates
- The program must verify if DNF5 is available and, if so, perform system updates that require sudo privileges.
- It must allow for two update modes:
  - **Immediate Update**: Instantly apply updates
  - **Offline Update**: Prepare updates that can be applied later (requiring a subsequent reboot command)

## Additional Requirements

### System Information
The tool should gather and display key system information such as:
- Distribution details
- Kernel version
- Version information for both Flatpak and DNF5

### Interactive Mode
- When enabled, the tool must prompt the user to choose between immediate and offline update modes for DNF5 updates

### Error Handling
- The program should handle failures gracefully
- Issue warnings if one update mechanism fails while the other succeeds
- Exit with an appropriate status if critical errors occur

### User-Friendly Output
- Console messages must clearly communicate:
  - Progress
  - Actions being taken
  - Any resulting errors or next steps

## Technical Implementation

### Commands to Execute

#### DNF5 Updates
1. Ensure DNF5 is installed by verifying the presence of the 'dnf5' executable
2. Check for available updates:
   ```bash
   sudo dnf5 --refresh check-upgrade
   ```
   - An exit code of 100 indicates that updates are available
3. For updating, execute one of the following depending on the update mode:
   - Offline Update:
     ```bash
     sudo dnf5 upgrade --offline -y
     ```
   - Immediate Update:
     ```bash
     sudo dnf5 upgrade -y
     ```
4. For immediate updates, post-update, verify if a reboot is required:
   ```bash
   sudo dnf5 needs-restarting
   ```
   - An exit code of 1 indicates a reboot is necessary

#### Flatpak Updates
1. Ensure Flatpak is installed by confirming the presence of the 'flatpak' executable
2. Update Flatpak packages:
   ```bash
   flatpak update -y
   ```

#### System Information
Gather system details by executing:
```bash
cat /etc/os-release       # for distribution information
uname -r                  # for kernel version
flatpak --version         # for Flatpak version
dnf5 --version           # for DNF5 version
```

### CLI Arguments

#### Command Structure
- The tool is invoked as "fedora-updater" followed by a subcommand
- The primary subcommand is "update", which initiates the update process for both Flatpak and DNF5

#### Optional Flags
- `--interactive` (`-i`): 
  - Enables interactive mode
  - In interactive mode, after checking for updates, prompts user to choose update type
  - If user inputs "now", performs immediate update
  - Otherwise, uses offline update by default

#### Positional Arguments
- The "update" command does not require any additional positional arguments

## Additional Considerations

### Error Handling & Logging
- The updater must report errors clearly
- Display warnings if one update mechanism fails while the other succeeds
- Exit with non-zero status for critical failures
- On-screen messages should provide sufficient detail for troubleshooting

### Test Coverage
Comprehensive tests should cover:
- Normal operations
- Edge cases (e.g., absence of Flatpak or DNF5, no available updates)
- Invalid-input scenarios

### Performance & Concurrency
- Most operations are I/O-bound
- The updater must efficiently stream real-time command output from both stdout and stderr
- Concurrency mechanisms should ensure output is captured without deadlocks

### Security
- Handle sudo privileges securely
- Only run privileged commands when explicitly permitted
- Validate that sensitive information is not exposed in error messages or logs

### User Experience
- CLI should clearly indicate each step of the process
- Particularly important in interactive mode where user input determines update mode
- Use color-coded output for clear and distinguishable status messages
