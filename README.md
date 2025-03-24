# Fedora Updater

A command-line utility to streamline system updates on Fedora through both Flatpak and DNF5.

## Features

- **Unified Updates**: Handles both Flatpak and DNF5 updates in a single command
- **Real-time Progress**: Displays update progress in real-time
- **Interactive Mode**: Optional interactive mode for choosing DNF5 update type
- **Smart Detection**: 
  - Automatically detects if Flatpak/DNF5 are installed
  - Identifies when updates are available
  - Shows clear status messages
- **Update Options**:
  - DNF5: Supports both immediate and offline updates
  - Flatpak: Automatic updates with clear progress indication
- **Build Information**: Displays build date dynamically set during compilation

## Requirements

- RPM based Linux distribution
- Rust (for building from source)
- DNF5 (for system updates)
- Flatpak (optional, for Flatpak package updates)

## Installation

### From Source

1. Clone the repository:
   ```bash
   git clone https://github.com/khs-kks/fedora-updater.git
   cd fedora-updater
   ```

2. Build and install:
   ```bash
   cargo build --release
   sudo cp target/release/fedora-updater /usr/local/bin/
   ```

## Usage

### Basic Usage

Simply run:
```bash
fedora-updater
```

This will:
1. Check and perform Flatpak updates (if Flatpak is installed)
2. Check and perform DNF5 updates
3. Show real-time progress
4. Indicate if a system restart is needed

### Interactive Mode

Run with the `-i` or `--interactive` flag:
```bash
fedora-updater -i
```

In interactive mode:
1. For DNF5 updates, you can choose between:
   - Immediate update (type 'now')
   - Offline update (press Enter)

### Update Types

#### DNF5 Updates
- **Immediate**: Updates are applied immediately
- **Offline**: Updates are prepared and applied on next reboot

#### Flatpak Updates
- Updates are always performed immediately
- No reboot required

## Output

The program provides clear, color-coded output:
- 🟢 Green: Success messages and normal operation
- 🟡 Yellow: Warnings and important notifications
- 🔴 Red: Error messages

## TODO

### Known Issues
- **Output Interleaving**: [Resolved] The previously observed race condition between stderr and stdout outputs during package updates has been fixed.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
