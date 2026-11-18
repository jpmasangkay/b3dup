# ⚡ b3dup
[![Ask DeepWiki](https://devin.ai/assets/askdeepwiki.png)](https://deepwiki.com/jpmasangkay/b3dup)

### Fast, Multithreaded Duplicate File Finder & Disk Space Analyzer — Pure Rust

**Walk. Filter. Hash. Pure Rust.**

A fast, multithreaded CLI & full-screen TUI duplicate file finder and disk space analyzer built in pure Rust. Features a 3-stage BLAKE3 elimination funnel (size grouping via `walkdir` without following symlinks, parallel 4 KB prefix hashing via `rayon` that finalizes `<= 4 KB` files early, and streaming 64 KB full-file BLAKE3 hashing), lock-free single-threaded grouping, live `indicatif` progress indicators, a color-coded terminal report with `humansize`, pretty-printed `--json` export, safe `-i` / `--dry-run` cleanup, a deterministic 2,000-file `bench-gen` workload generator, and an interactive `ratatui` TUI with hex/ASCII file previews.

[![Rust](https://img.shields.io/badge/Rust-1.88+-DEA584?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Edition](https://img.shields.io/badge/Edition-2021-blue?style=for-the-badge)](https://www.rust-lang.org)
[![blake3](https://img.shields.io/badge/blake3-1.8.7-00ADD8?style=for-the-badge)](https://github.com/BLAKE3-team/BLAKE3)
[![rayon](https://img.shields.io/badge/rayon-1.12.0-8A2BE2?style=for-the-badge)](https://github.com/rayon-rs/rayon)
[![ratatui](https://img.shields.io/badge/ratatui-0.30.2-00ADD8?style=for-the-badge)](https://ratatui.rs)
[![crossterm](https://img.shields.io/badge/crossterm-0.29.0-333333?style=for-the-badge)](https://github.com/crossterm-rs/crossterm)
[![clap](https://img.shields.io/badge/clap-4.6.8-orange?style=for-the-badge)](https://github.com/clap-rs/clap)

🖥️ **Repository:** [github.com/jpmasangkay/b3dup](https://github.com/jpmasangkay/b3dup)

---

## ✨ Features

### ⚡ 3-Stage BLAKE3 Deduplication Pipeline

- **Stage 1 — Filesystem walk & size grouping** — traverses directories with `walkdir` (`follow_links(false)`), considers regular files only, skips zero-byte files and anything below `--min-size`, shows a live spinner with file counts, and drops unique file sizes immediately.
- **Stage 2 — Parallel 4 KB prefix hash** — uses `rayon` (`into_par_iter`) to BLAKE3-hash only the first 4,096 bytes of each candidate, groups by `(size, prefix_hash)` single-threaded (no `DashMap` or shared mutexes), and treats files `<= 4 KB` as final so they skip Stage 3 entirely.
- **Stage 3 — Streaming 64 KB full-file hash** — streams remaining candidates through a fixed 64 KB buffer so large files are never loaded completely into memory, groups by full BLAKE3 digest, sorts duplicate groups by wasted space `(copies - 1) * size` descending, and tracks per-stage funnel counts and timings.
- **Resilient error handling** — never panics on unreadable files, permission errors, or files vanishing mid-scan; skips them cleanly and reports the skipped count.

### 🖥️ Full-Screen Interactive TUI (`tui` / `--tui`)

- **Live dashboard & 3-stage funnel** — displays total scanned files, duplicate groups/files, skipped files, total reclaimable space, marked deletion totals, `LIVE` / `DRY RUN` mode badge, and proportional 3-stage funnel bars.
- **Split-pane duplicate browser** — scrollable list of duplicate groups ranked by wasted space on the left, paired with full BLAKE3 hash details, interactive `[ ]` / `[X]` file-copy checklist, and a 64-byte hex + ASCII file preview on the right.
- **Single-copy safety guard & modals** — prevents marking every copy in a group (at least one `[KEEP]` copy must remain), supports one-key auto-marking (`a` per group, `A` across all groups), and opens confirmation or dry-run preview modals before modifying disk.

### 🛠️ CLI Reporting, Safe Cleanup & Benchmark Generator

- **Color-coded terminal report** — summarizes files scanned, total elapsed ms, per-stage funnel, duplicate totals, human-readable wasted space (`humansize`), and top `--top N` groups.
- **Interactive CLI deletion (`-i, --interactive`) & `--dry-run`** — presents a `dialoguer::MultiSelect` per group with nothing pre-selected, blocks deleting all copies in any group, asks for `Confirm` before removal, and combines cleanly with `--dry-run`.
- **Pretty JSON export (`--json <OUT_FILE>`)** — writes `scan_path`, `files_scanned`, `duplicate_groups`, `duplicate_files`, `wasted_bytes`, `elapsed_ms`, and `groups: [{hash, size, files}]`.
- **Deterministic benchmark generator (`bench-gen <TARGET_DIR>`)** — uses an internal seeded `Xorshift64` PRNG (no external `rand` dependency) to generate 2,000 files across 80 nested folders (3–4 levels deep): 1,000 unique files (1 KB–512 KB), 400 same-size non-duplicates (200 differing in the first 4 KB + 200 sharing an identical 4 KB prefix to exercise Stage 3), and 600 exact duplicates (120 groups × 5 copies). Refuses to run if the target exists and is non-empty.

### 🛡️ Code Quality & Release Tuning

- **Strict Rust invariants** — uses owned types (`PathBuf`, `Vec`, `HashMap`), `anyhow::Result`, zero explicit lifetime annotations, zero `unsafe`, and zero `unwrap()`/`expect()` outside `#[cfg(test)]`.
- **Panic-safe TUI lifecycle** — uses `ratatui::try_init()`, `ratatui::try_restore()`, and an RAII `Drop` guard so raw mode and the alternate screen always restore cleanly.
- **LTO release profile** — configured with `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, and `strip = true`.

---

## 🛠️ Tech Stack & Source Layout

| Component / Crate | Purpose |
|-------------------|---------|
| **[src/main.rs](src/main.rs)** | `clap` CLI subcommands (`scan`, `tui`, `bench-gen`), 3-stage BLAKE3 pipeline, `bench-gen` xorshift generator, terminal/JSON reporting, `dialoguer` cleanup, and unit tests |
| **[src/tui.rs](src/tui.rs)** | Full-screen `ratatui` TUI state machine, responsive split-pane dashboard, 64-byte hex/ASCII preview, deletion safety guards, modals, and `TestBackend` tests |
| **[Cargo.toml](Cargo.toml)** | Dependencies and `fat` LTO release profile |
| **Rust 2021 Edition (`1.88+`)** | Memory-safe systems programming with owned data and `anyhow` error handling |
| **blake3 `1.8.7`** | High-speed cryptographic hashing for 4 KB partial and 64 KB streaming full hashes |
| **rayon `1.12.0`** | Data-parallel hashing across CPU cores with single-threaded grouping |
| **walkdir `2.5.0`** | Recursive filesystem traversal with symlinks disabled |
| **ratatui `0.30.2` & crossterm `0.29.0`** | Interactive split-pane TUI, scrollbars, overlays, and keyboard event handling |
| **clap `4.6.8` (`derive`)** | Declarative CLI argument and subcommand parsing |
| **indicatif `0.17.11` & console `0.15.11`** | Live per-stage spinners/progress bars and ANSI-styled terminal output |
| **dialoguer `0.11.0`** | Interactive CLI `MultiSelect` and `Confirm` prompts |
| **humansize `2.1.3`** | Human-readable IEC (`KiB` / `MiB` / `GiB`) byte formatting |
| **serde `1.0.229` & serde_json `1.0.154`** | Pretty-printed JSON report serialization |
| **anyhow `1.0.104`** | Contextual error propagation without panics |

---

## 🚀 Getting Started

### Prerequisites

- [Rust SDK](https://rustup.rs/) (`1.88+` with `cargo`, `rustfmt`, and `clippy`)
- macOS, Linux, or Windows terminal

### Installation & Verification

```bash
# Clone the repository
git clone https://github.com/jpmasangkay/b3dup.git
cd b3dup

# Build the release binary
cargo build --release

# Run unit tests, formatting check, and Clippy lints
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

### Running b3dup

```bash
# 1. Generate a deterministic 2,000-file benchmark dataset across nested folders
cargo run --release -- bench-gen ./test_data

# 2. Run the 3-stage CLI scan and view the funnel + top duplicate groups
cargo run --release -- scan ./test_data

# 3. Launch the interactive full-screen TUI (or add --dry-run to preview safely)
cargo run --release -- tui ./test_data
cargo run --release -- scan ./test_data --tui --dry-run

# 4. Interactive CLI deletion checklist, dry-run preview, or JSON export
cargo run --release -- scan ./test_data -i --dry-run
cargo run --release -- scan ./test_data --top 20 --min-size 4096 --json report.json
```

---

## ⌨️ TUI Controls

### Global & Navigation

| Action | Shortcut |
|--------|----------|
| **Switch Groups / File Copies Pane** | `Tab`, `←` / `→`, or `h` / `l` |
| **Move Selection Up / Down** | `↑` / `↓` or `k` / `j` |
| **Page Up / Page Down** | `PageUp` / `PageDown` |
| **Jump to First / Last Item** | `g` / `Home` or `G` / `End` |
| **Toggle Help Modal** | `?` |
| **Quit (or close modal)** | `q`, `Esc`, or `Ctrl+C` |

### Selection, Cleanup & Modals

| Context | Action | Shortcut |
|---------|--------|----------|
| **File Copies** | Toggle deletion mark (never marks last remaining copy) | `Space` or `Enter` |
| **Group / Global** | Auto-mark all copies except first in current group / all groups | `a` / `A` |
| **Group / Global** | Unmark current group / unmark all groups | `u` / `U` |
| **Cleanup** | Open confirmation modal (or dry-run preview in `--dry-run`) | `d`, `Delete`, or `Backspace` |
| **Confirm Modal** | Delete marked files permanently / Cancel | `y` or `Enter` / `n` or `Esc` |

---

## 👥 Author

Built with ❤️ by **John Paul Masangkay** ([@jpmasangkay](https://github.com/jpmasangkay))

---

## 👏 Acknowledgements

- **BLAKE3 team** — For an extraordinarily fast cryptographic hash function in pure Rust
- **Rayon & WalkDir maintainers** — For effortless parallel iteration and safe directory traversal
- **Ratatui & Crossterm maintainers** — For a responsive, panic-safe Rust terminal UI stack
- **The Rust Community** — For `clap`, `indicatif`, `console`, `dialoguer`, `humansize`, `serde`, `serde_json`, and `anyhow`
