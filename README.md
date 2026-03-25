<div align="center">

<br>

<img src="https://i.ibb.co/4gDjPqF9/wuwa.png" width="128" height="128" alt="Wuthering Waves Logo">

# Wuthering Waves Downloader

[![Rust nightly](https://img.shields.io/badge/Rust-1.87.0--nightly-orange?logo=rust)](https://www.rust-lang.org/) [![License](https://img.shields.io/badge/License-MIT-blue)](LICENSE)

High-performance, resilient downloader for Wuthering Waves with multi-CDN fallback, integrity verification, and a clean TUI experience.

[✨ Features](#-features) •
[📦 Requirements](#-requirements) •
[🛠️ Installation](#️-installation) •
[▶️ Usage](#️-usage) •
[🔍 Technical Details](#-technical-details) •
[⚙️ Configuration](#️-configuration) •
[📚 Documentation](https://deepwiki.com/yuhkix/wuwa-downloader/) •
[❓ FAQ](#-faq) •
[🧪 Development](#-development) •
[🤝 Contributing](#-contributing)

![Ferris](https://i.ibb.co/QVThVkd/Ferris.png)

</div>

## ✨ Features
- **Multi-CDN fallback**: Automatically tries multiple mirrors on failures
- **Interactive version selection**: Choose Live/Beta and OS/CN variants
- **Pipeline downloads**: Verification workers and download workers run concurrently
- **Integrity checks**: Per-file MD5 verification; corrupted or oversized files are deleted before download
- **Smart retries**: Up to 3 retry attempts per CDN with robust timeouts
- **Streaming downloads**: Chunked I/O with resume support when possible
- **Clear progress**: Verification bar, total download bar, and per-worker progress bars
- **Graceful interrupt**: CTRL-C to stop safely with a final summary including unprocessed files
- **Detailed logs**: Errors recorded with timestamps in `logs.log`

## 📦 Requirements
- **Rust nightly toolchain**: 1.87.0-nightly or newer
- **Windows**: Full console experience
- **Linux**: Fully supported

## 🛠️ Installation
```bash
rustup toolchain install nightly
rustup default nightly

git clone https://github.com/yuhkix/wuwa-downloader.git
cd wuwa-downloader

cargo build --release
```

## ▶️ Usage
### Running the Application
- **Windows**: `target\release\wuwa-downloader.exe`
- **Linux**: `./target/release/wuwa-downloader`

### Workflow
1. Select a version to download (Live/Beta and OS/CN)
2. Choose a download directory or press Enter for current directory
3. Enter the number of concurrent download workers or press Enter to use the default
4. Enter the number of concurrent verification workers or press Enter to use the default
5. Wait for the index file to be fetched and parsed
6. Monitor verification and download progress in the multi-bar UI
7. Review the final summary:
   - Successfully verified
   - Successfully downloaded
   - Failed
   - Unprocessed
   - Total files
8. Press Enter to exit only when there are no unprocessed files

## 🔍 Technical Details
### How It Works
- Remote config discovery via JSON
- Index parsing for resource listing
- Index `size` metadata is used instead of per-file HEAD preflight checks
- Verification workers validate local files before enqueueing downloads
- Download workers consume a shared queue with resume and CDN fallback support
- MD5 checksum validation and pre-delete handling for corrupted files

### Key Components
- `src/network/client.rs`: Config and download management
- `src/io/util.rs`: Resource parsing, prompts, and process control helpers
- `src/io/file.rs`: File operations and path handling
- `src/io/logging.rs`: Error logging system
- `src/download/progress.rs`: Multi-progress UI state
- `src/download/pipeline.rs`: Pipeline controller, verification workers, and download workers

## ⚙️ Configuration
- **Retry Policy**: 3 attempts per CDN
- **Worker Defaults**:
  - Verification workers: `8`
  - Download workers: `4`
- **Timeouts**: 30s for index/config fetches, extended timeout for transfers
- **Logging**: 
  - Errors: `logs.log`
- **Progress**:
  - Verification progress bar
  - Total download progress bar
  - Per-download-worker progress bars

## 📚 Documentation
For detailed guides, workflow overview, and deeper technical explanations, see the [official documentation](https://deepwiki.com/yuhkix/wuwa-downloader/).

## ❓ FAQ
- **Download location?** User-selected at runtime
- **Safe interruption?** Yes, via CTRL-C
- **What happens on interruption?** Completed files are kept; the summary shows failed and unprocessed counts separately
- **Why MD5?** Matches upstream checksums for integrity

## 🧪 Development
### Environment Setup
- **Required**: Rust nightly (1.87.0-nightly+)
- **Dependencies**: 
  - `reqwest`
  - `indicatif`
  - `flate2`
  - `colored`
  - `ctrlc`
  - `serde_json`

### Build Optimization
Release profile includes:
- Strip symbols
- Link-time optimization
- Maximum optimization level
- Single codegen unit

### Quick Start
```bash
cargo run --release
```

## 🤝 Contributing
Pull requests are welcome. Please ensure:
- Focused changes
- Clear documentation
- Brief motivation explanation

## 📜 License
Licensed under the **MIT License**. See [LICENSE](LICENSE).