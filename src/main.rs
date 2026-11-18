mod tui;

use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::Hash;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, MultiSelect};
use humansize::{format_size, BINARY};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::Serialize;
use walkdir::WalkDir;

const PARTIAL_HASH_BYTES: usize = 4096;
const FULL_HASH_BUFFER_BYTES: usize = 65_536;

const BENCH_UNIQUE_COUNT: usize = 1_000;
const BENCH_DIFF_PREFIX_GROUPS: usize = 20;
const BENCH_SAME_PREFIX_GROUPS: usize = 20;
const BENCH_DIFF_COPIES: usize = 10;
const BENCH_DUP_GROUPS: usize = 120;
const BENCH_DUP_SMALL_GROUPS: usize = 10;
const BENCH_DUP_COPIES: usize = 5;

/// Fast, multithreaded CLI duplicate file finder and disk space analyzer.
#[derive(Debug, Parser)]
#[command(
    name = "b3dup",
    version,
    about = "Fast, multithreaded CLI duplicate file finder and disk space analyzer powered by BLAKE3"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scan a directory tree for duplicate files using a 3-stage BLAKE3 pipeline
    Scan(ScanArgs),
    /// Launch the interactive full-screen TUI duplicate explorer
    Tui(TuiArgs),
    /// Generate a deterministic 2,000-file benchmark dataset across nested folders
    BenchGen(BenchGenArgs),
}

#[derive(Debug, Args)]
struct ScanArgs {
    /// Root directory to scan for duplicate files
    path: PathBuf,

    /// Launch the full-screen TUI explorer instead of the static terminal report
    #[arg(long, conflicts_with = "interactive")]
    tui: bool,

    /// Interactively select duplicate files to delete after the report
    #[arg(short, long)]
    interactive: bool,

    /// Show what would be deleted and space freed without touching the disk
    #[arg(long)]
    dry_run: bool,

    /// Write the report as pretty-printed JSON to the given file
    #[arg(long, value_name = "OUT_FILE")]
    json: Option<PathBuf>,

    /// Maximum number of duplicate groups to display in the terminal report
    #[arg(long, default_value_t = 10, value_name = "N")]
    top: usize,

    /// Minimum file size in bytes to include (zero-byte files are always skipped)
    #[arg(long, default_value_t = 1, value_name = "BYTES")]
    min_size: u64,
}

#[derive(Debug, Args)]
struct TuiArgs {
    /// Root directory to scan and browse in the TUI
    path: PathBuf,

    /// Preview marked deletions without modifying disk
    #[arg(long)]
    dry_run: bool,

    /// Minimum file size in bytes to include (zero-byte files are always skipped)
    #[arg(long, default_value_t = 1, value_name = "BYTES")]
    min_size: u64,
}

#[derive(Debug, Args)]
struct BenchGenArgs {
    /// Target directory where the benchmark dataset will be generated
    target_dir: PathBuf,
}

/// A confirmed group of identical files sharing the same BLAKE3 hash and byte size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DuplicateGroup {
    pub(crate) hash: [u8; 32],
    pub(crate) size: u64,
    pub(crate) files: Vec<PathBuf>,
}

impl DuplicateGroup {
    /// Returns the lowercase hexadecimal representation of the BLAKE3 digest.
    pub(crate) fn hash_hex(&self) -> String {
        blake3::Hash::from(self.hash).to_hex().to_string()
    }

    /// Returns bytes reclaimable while keeping one copy: `(copies - 1) * size`.
    pub(crate) fn wasted_bytes(&self) -> u64 {
        let redundant = self.files.len().saturating_sub(1) as u64;
        redundant.saturating_mul(self.size)
    }
}

/// Candidate count and duration recorded for a single pipeline stage.
#[derive(Debug, Clone)]
pub(crate) struct StageMetrics {
    pub(crate) candidates: usize,
    pub(crate) elapsed_ms: u128,
}

/// Full summary of a completed 3-stage duplicate scan.
#[derive(Debug, Clone)]
pub(crate) struct ScanReport {
    pub(crate) scan_path: PathBuf,
    pub(crate) files_scanned: usize,
    pub(crate) skipped_files: usize,
    pub(crate) stage1: StageMetrics,
    pub(crate) stage2: StageMetrics,
    pub(crate) stage3: StageMetrics,
    #[allow(dead_code)]
    pub(crate) stage3_hashed_files: usize,
    pub(crate) total_elapsed_ms: u128,
    pub(crate) groups: Vec<DuplicateGroup>,
}

impl ScanReport {
    /// Total number of duplicate groups discovered.
    pub(crate) fn duplicate_groups(&self) -> usize {
        self.groups.len()
    }

    /// Total number of files participating in duplicate groups.
    pub(crate) fn duplicate_files(&self) -> usize {
        self.groups.iter().map(|group| group.files.len()).sum()
    }

    /// Total wasted bytes across all duplicate groups.
    fn wasted_bytes(&self) -> u64 {
        calculate_wasted_bytes(&self.groups)
    }
}

/// Serializable JSON report structure matching the CLI `--json` schema.
#[derive(Debug, Serialize)]
struct JsonReport {
    scan_path: String,
    files_scanned: usize,
    duplicate_groups: usize,
    duplicate_files: usize,
    wasted_bytes: u64,
    elapsed_ms: u128,
    groups: Vec<JsonGroup>,
}

/// Candidate file passed between pipeline stages.
#[derive(Debug, Clone)]
struct FileCandidate {
    size: u64,
    path: PathBuf,
}

/// Output of Stage 1 (filesystem walk and size grouping).
struct WalkStageOutput {
    size_groups: HashMap<u64, Vec<PathBuf>>,
    files_scanned: usize,
    skipped_files: usize,
    elapsed_ms: u128,
}

/// Output of Stage 2 (4 KB partial hashing).
struct PartialStageOutput {
    small_duplicates: Vec<DuplicateGroup>,
    stage3_candidates: Vec<FileCandidate>,
    skipped_files: usize,
    elapsed_ms: u128,
}

/// Output of Stage 3 (full-file streaming BLAKE3 hashing).
struct FullStageOutput {
    groups: Vec<DuplicateGroup>,
    skipped_files: usize,
    elapsed_ms: u128,
}

/// Serializable duplicate group entry inside `JsonReport`.
#[derive(Debug, Serialize)]
struct JsonGroup {
    hash: String,
    size: u64,
    files: Vec<String>,
}

/// Tiny deterministic xorshift64 PRNG for repeatable benchmark generation.
struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    /// Initializes the generator with a non-zero seed.
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    /// Advances internal state and returns the next pseudo-random `u64`.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Returns a deterministic integer in `[low, high_inclusive]`.
    fn gen_range(&mut self, low: u64, high_inclusive: u64) -> u64 {
        if low >= high_inclusive {
            return low;
        }
        let span = high_inclusive - low + 1;
        low + (self.next_u64() % span)
    }

    /// Fills `buf` deterministically from the PRNG stream.
    fn fill_bytes(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan(args) => run_scan(args),
        Command::Tui(args) => run_tui_cmd(args),
        Command::BenchGen(args) => run_bench_gen(args),
    }
}

/// Validates that `path` exists and is a readable directory.
fn validate_scan_dir(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Cannot access scan path: {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("Scan path is not a directory: {}", path.display());
    }
    Ok(())
}

/// Executes the `tui` subcommand, scanning `args.path` and launching the Ratatui explorer.
fn run_tui_cmd(args: TuiArgs) -> Result<()> {
    validate_scan_dir(&args.path)?;
    let report = scan_duplicates(args.path, args.min_size)?;
    tui::run_tui(report, args.dry_run)
}

/// Executes the `scan` subcommand, renders reports, and runs optional deletion flows.
fn run_scan(args: ScanArgs) -> Result<()> {
    validate_scan_dir(&args.path)?;

    let report = scan_duplicates(args.path, args.min_size)?;

    if let Some(out_path) = &args.json {
        write_json_report(&report, out_path)?;
        if !args.tui {
            println!(
                "{} {}",
                style("JSON report written to:").green().bold(),
                out_path.display()
            );
        }
    }

    if args.tui {
        return tui::run_tui(report, args.dry_run);
    }

    print_terminal_report(&report, args.top);
    handle_actions(&report, args.interactive, args.dry_run)
}

/// Runs the 3-stage duplicate detection pipeline on `root`.
fn scan_duplicates(root: PathBuf, min_size: u64) -> Result<ScanReport> {
    let total_start = Instant::now();

    let walk = stage_walk_by_size(&root, min_size)?;
    let stage1_candidates: usize = walk.size_groups.values().map(Vec::len).sum();

    let partial = stage_partial_hash(walk.size_groups)?;
    let stage2_small_duplicates: usize = partial
        .small_duplicates
        .iter()
        .map(|group| group.files.len())
        .sum();
    let stage2_candidates = stage2_small_duplicates + partial.stage3_candidates.len();
    let stage3_hashed_files = partial.stage3_candidates.len();

    let full = stage_full_hash(partial.stage3_candidates)?;
    let skipped_files = walk.skipped_files + partial.skipped_files + full.skipped_files;

    let mut final_groups = partial.small_duplicates;
    final_groups.extend(full.groups);
    sort_groups(&mut final_groups);

    let total_elapsed_ms = total_start.elapsed().as_millis();

    Ok(ScanReport {
        scan_path: root,
        files_scanned: walk.files_scanned,
        skipped_files,
        stage1: StageMetrics {
            candidates: stage1_candidates,
            elapsed_ms: walk.elapsed_ms,
        },
        stage2: StageMetrics {
            candidates: stage2_candidates,
            elapsed_ms: partial.elapsed_ms,
        },
        stage3: StageMetrics {
            candidates: final_groups.iter().map(|group| group.files.len()).sum(),
            elapsed_ms: full.elapsed_ms,
        },
        stage3_hashed_files,
        total_elapsed_ms,
        groups: final_groups,
    })
}

/// Stage 1: Walks `root` without following symlinks and groups eligible files by size.
fn stage_walk_by_size(root: &Path, min_size: u64) -> Result<WalkStageOutput> {
    let start = Instant::now();
    let effective_min = min_size.max(1);
    let spinner = make_spinner("[Stage 1/3] Walking filesystem...")?;

    let mut files_scanned: usize = 0;
    let mut skipped_files: usize = 0;
    let mut size_groups: HashMap<u64, Vec<PathBuf>> = HashMap::new();

    for entry_result in WalkDir::new(root).follow_links(false) {
        let entry = match entry_result {
            Ok(entry) => entry,
            Err(_) => {
                skipped_files += 1;
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped_files += 1;
                continue;
            }
        };

        files_scanned += 1;
        spinner.set_position(files_scanned as u64);

        let size = metadata.len();
        if size < effective_min {
            continue;
        }

        size_groups.entry(size).or_default().push(entry.into_path());
    }

    let size_groups = retain_non_singletons(size_groups);
    let elapsed_ms = start.elapsed().as_millis();
    spinner.finish_and_clear();

    Ok(WalkStageOutput {
        size_groups,
        files_scanned,
        skipped_files,
        elapsed_ms,
    })
}

/// Composite key `(file_size, partial_blake3_digest)` used in Stage 2 grouping.
type PartialKey = (u64, [u8; 32]);

/// Stage 2: Parallel 4 KB prefix hashing. Finalizes files `<= 4 KB` and forwards larger ties.
fn stage_partial_hash(size_groups: HashMap<u64, Vec<PathBuf>>) -> Result<PartialStageOutput> {
    let start = Instant::now();
    let total_candidates: usize = size_groups.values().map(Vec::len).sum();
    let pb = make_progress_bar(total_candidates, "[Stage 2/3] Partial hash (4 KB)")?;

    let inputs: Vec<FileCandidate> = size_groups
        .into_iter()
        .flat_map(|(size, paths)| {
            paths
                .into_iter()
                .map(move |path| FileCandidate { size, path })
        })
        .collect();

    let hashed: Vec<Option<(PartialKey, PathBuf)>> = inputs
        .into_par_iter()
        .map(|candidate| {
            let entry = hash_file_prefix(&candidate.path)
                .ok()
                .map(|prefix_hash| ((candidate.size, prefix_hash), candidate.path));
            pb.inc(1);
            entry
        })
        .collect();

    pb.finish_and_clear();

    let mut skipped_files = 0;
    let mut valid_entries = Vec::with_capacity(hashed.len());
    for item in hashed {
        match item {
            Some(entry) => valid_entries.push(entry),
            None => skipped_files += 1,
        }
    }

    let grouped = group_and_filter_duplicates(valid_entries);
    let mut small_duplicates = Vec::new();
    let mut stage3_candidates = Vec::new();

    for ((size, prefix_hash), mut files) in grouped {
        if size <= PARTIAL_HASH_BYTES as u64 {
            files.sort();
            small_duplicates.push(DuplicateGroup {
                hash: prefix_hash,
                size,
                files,
            });
        } else {
            for path in files {
                stage3_candidates.push(FileCandidate { size, path });
            }
        }
    }

    let elapsed_ms = start.elapsed().as_millis();
    Ok(PartialStageOutput {
        small_duplicates,
        stage3_candidates,
        skipped_files,
        elapsed_ms,
    })
}

/// Stage 3: Streams remaining candidates through a 64 KB buffer to compute full BLAKE3 hashes.
fn stage_full_hash(candidates: Vec<FileCandidate>) -> Result<FullStageOutput> {
    let start = Instant::now();
    let pb = make_progress_bar(candidates.len(), "[Stage 3/3] Full BLAKE3 hash")?;

    let hashed: Vec<Option<([u8; 32], FileCandidate)>> = candidates
        .into_par_iter()
        .map(|candidate| {
            let entry = hash_file_full(&candidate.path)
                .ok()
                .map(|full_hash| (full_hash, candidate));
            pb.inc(1);
            entry
        })
        .collect();

    pb.finish_and_clear();

    let mut skipped_files = 0;
    let mut valid_entries = Vec::with_capacity(hashed.len());
    for item in hashed {
        match item {
            Some(entry) => valid_entries.push(entry),
            None => skipped_files += 1,
        }
    }

    let grouped = group_and_filter_duplicates(valid_entries);
    let mut groups = Vec::new();
    for (hash, entries) in grouped {
        if let Some(first) = entries.first() {
            let size = first.size;
            let mut files: Vec<PathBuf> = entries.into_iter().map(|c| c.path).collect();
            files.sort();
            groups.push(DuplicateGroup { hash, size, files });
        }
    }

    let elapsed_ms = start.elapsed().as_millis();
    Ok(FullStageOutput {
        groups,
        skipped_files,
        elapsed_ms,
    })
}

/// Computes the BLAKE3 hash of at most the first `PARTIAL_HASH_BYTES` (4 KB) of `path`.
fn hash_file_prefix(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut buffer = [0u8; PARTIAL_HASH_BYTES];
    let mut total = 0;

    while total < buffer.len() {
        match file.read(&mut buffer[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(*blake3::hash(&buffer[..total]).as_bytes())
}

/// Streams `path` through a fixed 64 KB buffer to compute its full BLAKE3 hash.
fn hash_file_full(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; FULL_HASH_BUFFER_BYTES];

    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buffer[..n]);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(*hasher.finalize().as_bytes())
}

/// Drops every bucket containing fewer than 2 items.
fn retain_non_singletons<K: Eq + Hash, V>(mut groups: HashMap<K, Vec<V>>) -> HashMap<K, Vec<V>> {
    groups.retain(|_, items| items.len() > 1);
    groups
}

/// Groups `(key, value)` pairs by key and discards singletons.
fn group_and_filter_duplicates<K: Eq + Hash, V>(entries: Vec<(K, V)>) -> HashMap<K, Vec<V>> {
    let mut grouped: HashMap<K, Vec<V>> = HashMap::new();
    for (key, value) in entries {
        grouped.entry(key).or_default().push(value);
    }
    retain_non_singletons(grouped)
}

/// Sums `(copies - 1) * size` across all duplicate groups.
pub(crate) fn calculate_wasted_bytes(groups: &[DuplicateGroup]) -> u64 {
    groups.iter().map(DuplicateGroup::wasted_bytes).sum()
}

/// Sorts duplicate groups by wasted space descending, then size descending, then hash ascending.
fn sort_groups(groups: &mut [DuplicateGroup]) {
    groups.sort_by(|a, b| {
        b.wasted_bytes()
            .cmp(&a.wasted_bytes())
            .then_with(|| b.size.cmp(&a.size))
            .then_with(|| a.hash.cmp(&b.hash))
    });
}

/// Creates a styled `indicatif` spinner with a live file count placeholder.
fn make_spinner(label: &str) -> Result<ProgressBar> {
    let pb = ProgressBar::new_spinner();
    let template = format!("{{spinner:.cyan}} {label} {{pos}} files [{{elapsed_precise}}]");
    let style = ProgressStyle::with_template(&template)
        .context("Invalid spinner template")?
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ");
    pb.set_style(style);
    pb.enable_steady_tick(Duration::from_millis(80));
    Ok(pb)
}

/// Creates a styled `indicatif` progress bar for deterministic candidate stages.
fn make_progress_bar(len: usize, label: &str) -> Result<ProgressBar> {
    let pb = ProgressBar::new(len as u64);
    let style = ProgressStyle::with_template(
        "{spinner:.cyan} {prefix} [{bar:30.cyan/blue}] {pos}/{len} ({elapsed_precise})",
    )
    .context("Invalid progress bar template")?
    .progress_chars("█░ ");
    pb.set_style(style);
    pb.set_prefix(label.to_string());
    Ok(pb)
}

/// Prints the color-coded terminal summary, pipeline funnel, and top N duplicate groups.
fn print_terminal_report(report: &ScanReport, top: usize) {
    let wasted = report.wasted_bytes();
    let dup_groups = report.duplicate_groups();
    let dup_files = report.duplicate_files();

    println!();
    println!("{}", style("=== b3dup Scan Report ===").bold().cyan());
    println!(
        "{:<19}: {}",
        "Scan path",
        style(report.scan_path.display()).white()
    );
    println!(
        "{:<19}: {}",
        "Files scanned",
        style(report.files_scanned).bold().green()
    );
    let skipped_styled = if report.skipped_files > 0 {
        style(report.skipped_files).bold().yellow()
    } else {
        style(report.skipped_files).dim()
    };
    println!("{:<19}: {}", "Skipped/unreadable", skipped_styled);
    println!(
        "{:<19}: {} ms",
        "Total elapsed",
        style(report.total_elapsed_ms).bold()
    );

    println!();
    println!("{}", style("Pipeline Funnel:").bold().cyan());
    println!(
        "  {:<29}: {:>6} candidates ({} ms)",
        "Stage 1 (Size grouping)",
        style(report.stage1.candidates).bold(),
        report.stage1.elapsed_ms
    );
    println!(
        "  {:<29}: {:>6} candidates ({} ms)",
        "Stage 2 (4 KB partial hash)",
        style(report.stage2.candidates).bold(),
        report.stage2.elapsed_ms
    );
    println!(
        "  {:<29}: {:>6} duplicates ({} full-hashed, {} ms)",
        "Stage 3 (Full BLAKE3 hash)",
        style(report.stage3.candidates).bold().yellow(),
        report.stage3_hashed_files,
        report.stage3.elapsed_ms
    );

    println!();
    println!("{}", style("Duplicate Summary:").bold().cyan());
    println!(
        "  {:<19}: {}",
        "Duplicate groups",
        style(dup_groups).bold().yellow()
    );
    println!(
        "  {:<19}: {}",
        "Duplicate files",
        style(dup_files).bold().yellow()
    );
    println!(
        "  {:<19}: {} ({} bytes)",
        "Total wasted space",
        style(format_size(wasted, BINARY)).bold().red(),
        wasted
    );

    if report.groups.is_empty() || top == 0 {
        println!();
        return;
    }

    let shown = report.groups.len().min(top);
    println!();
    println!(
        "{}",
        style(format!(
            "Top {shown} of {dup_groups} Duplicate Groups (by wasted space):"
        ))
        .bold()
        .cyan()
    );

    for (idx, group) in report.groups.iter().take(shown).enumerate() {
        let hex = group.hash_hex();
        let short_hash = &hex[..16.min(hex.len())];
        println!(
            "  {}. {} {} {} {}",
            style(idx + 1).bold(),
            style(format!("[{}", format_size(group.size, BINARY))).green(),
            style(format!("x {} copies", group.files.len())).yellow(),
            style(format!(
                "| wasted: {}]",
                format_size(group.wasted_bytes(), BINARY)
            ))
            .red(),
            style(format!("blake3:{short_hash}...")).dim()
        );
        for path in &group.files {
            println!("       {}", style(path.display()).dim());
        }
    }
    println!();
}

/// Serializes the scan report as pretty-printed JSON to `out_path`.
fn write_json_report(report: &ScanReport, out_path: &Path) -> Result<()> {
    let json_groups: Vec<JsonGroup> = report
        .groups
        .iter()
        .map(|group| JsonGroup {
            hash: group.hash_hex(),
            size: group.size,
            files: group
                .files
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        })
        .collect();

    let payload = JsonReport {
        scan_path: report.scan_path.to_string_lossy().into_owned(),
        files_scanned: report.files_scanned,
        duplicate_groups: report.duplicate_groups(),
        duplicate_files: report.duplicate_files(),
        wasted_bytes: report.wasted_bytes(),
        elapsed_ms: report.total_elapsed_ms,
        groups: json_groups,
    };

    let json_text =
        serde_json::to_string_pretty(&payload).context("Failed to serialize JSON report")?;
    fs::write(out_path, format!("{json_text}\n"))
        .with_context(|| format!("Failed to write JSON report to {}", out_path.display()))?;
    Ok(())
}

/// Dispatches `--interactive` and `--dry-run` post-scan actions.
fn handle_actions(report: &ScanReport, interactive: bool, dry_run: bool) -> Result<()> {
    if !interactive && !dry_run {
        return Ok(());
    }

    if report.groups.is_empty() {
        println!("{}", style("No duplicate files found.").dim());
        return Ok(());
    }

    if interactive {
        let selected = prompt_interactive_selections(&report.groups)?;
        if selected.is_empty() {
            println!("{}", style("No files selected for deletion.").dim());
            return Ok(());
        }
        if dry_run {
            print_dry_run_selected(&selected);
            return Ok(());
        }
        return execute_deletions(&selected);
    }

    print_dry_run_automatic(&report.groups);
    Ok(())
}

/// Prompts per group via `MultiSelect` while ensuring at least one copy always remains.
fn prompt_interactive_selections(groups: &[DuplicateGroup]) -> Result<Vec<(PathBuf, u64)>> {
    let mut to_delete = Vec::new();
    let total_groups = groups.len();
    let theme = ColorfulTheme::default();

    for (idx, group) in groups.iter().enumerate() {
        let items: Vec<String> = group
            .files
            .iter()
            .map(|path| path.display().to_string())
            .collect();

        println!(
            "{}",
            style(format!(
                "Group {}/{} — {} per copy ({} copies, wasted: {})",
                idx + 1,
                total_groups,
                format_size(group.size, BINARY),
                group.files.len(),
                format_size(group.wasted_bytes(), BINARY)
            ))
            .bold()
            .cyan()
        );

        loop {
            let selection = MultiSelect::with_theme(&theme)
                .with_prompt(
                    "Select copies to DELETE (Space to toggle, Enter to confirm, Esc/q to skip)",
                )
                .items(&items)
                .interact_opt()
                .context("Interactive selection failed")?;

            match selection {
                None => {
                    println!("  {}", style("Skipped group (all copies kept).").dim());
                    break;
                }
                Some(indices) if indices.len() >= group.files.len() => {
                    println!(
                        "  {}",
                        style(
                            "Cannot delete every copy in a group — at least one file must remain."
                        )
                        .bold()
                        .red()
                    );
                }
                Some(indices) => {
                    for index in indices {
                        if let Some(path) = group.files.get(index) {
                            to_delete.push((path.clone(), group.size));
                        }
                    }
                    break;
                }
            }
        }
    }

    Ok(to_delete)
}

/// Prints what `--dry-run` (without `--interactive`) would delete while keeping one copy per group.
fn print_dry_run_automatic(groups: &[DuplicateGroup]) {
    let mut would_delete_count: usize = 0;
    let mut would_free_bytes: u64 = 0;

    println!(
        "{}",
        style("[DRY RUN] Proposed deletions (keeping first copy per group):")
            .bold()
            .yellow()
    );
    for group in groups {
        if let Some((keep, duplicates)) = group.files.split_first() {
            println!("  {} {}", style("Keep  :").green(), keep.display());
            for path in duplicates {
                println!(
                    "  {} {} ({})",
                    style("Delete:").red(),
                    path.display(),
                    format_size(group.size, BINARY)
                );
                would_delete_count += 1;
                would_free_bytes = would_free_bytes.saturating_add(group.size);
            }
        }
    }

    println!();
    println!(
        "{} Would delete {} file(s) and free {} (no files were touched).",
        style("[DRY RUN]").bold().yellow(),
        would_delete_count,
        style(format_size(would_free_bytes, BINARY)).bold().green()
    );
}

/// Prints the exact files chosen interactively during `--interactive --dry-run`.
fn print_dry_run_selected(selected: &[(PathBuf, u64)]) {
    let total_bytes: u64 = selected.iter().map(|(_, size)| *size).sum();

    println!();
    println!(
        "{}",
        style("[DRY RUN] Files selected for deletion:")
            .bold()
            .yellow()
    );
    for (path, size) in selected {
        println!(
            "  {} {} ({})",
            style("Would delete:").red(),
            path.display(),
            format_size(*size, BINARY)
        );
    }
    println!(
        "{} Would delete {} file(s) and free {} (no files were touched).",
        style("[DRY RUN]").bold().yellow(),
        selected.len(),
        style(format_size(total_bytes, BINARY)).bold().green()
    );
}

/// Requests final confirmation and deletes the selected duplicate files.
fn execute_deletions(selected: &[(PathBuf, u64)]) -> Result<()> {
    let planned_bytes: u64 = selected.iter().map(|(_, size)| *size).sum();

    println!();
    println!("{}", style("Selected files to delete:").bold().yellow());
    for (path, size) in selected {
        println!(
            "  {} {} ({})",
            style("-").red(),
            path.display(),
            format_size(*size, BINARY)
        );
    }

    let confirmed = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Permanently delete {} file(s) and free {}?",
            selected.len(),
            format_size(planned_bytes, BINARY)
        ))
        .default(false)
        .interact()
        .context("Confirmation prompt failed")?;

    if !confirmed {
        println!("{}", style("Aborted. No files were deleted.").dim());
        return Ok(());
    }

    let mut deleted_count: usize = 0;
    let mut freed_bytes: u64 = 0;
    let mut failed_count: usize = 0;

    for (path, size) in selected {
        match fs::remove_file(path) {
            Ok(()) => {
                deleted_count += 1;
                freed_bytes = freed_bytes.saturating_add(*size);
            }
            Err(err) => {
                failed_count += 1;
                eprintln!(
                    "{} Failed to delete {}: {err}",
                    style("Warning:").yellow().bold(),
                    path.display()
                );
            }
        }
    }

    println!(
        "{} Deleted {} file(s), freed {}.",
        style("Done:").green().bold(),
        deleted_count,
        style(format_size(freed_bytes, BINARY)).bold().green()
    );
    if failed_count > 0 {
        println!(
            "{} {} file(s) could not be deleted.",
            style("Warning:").yellow().bold(),
            failed_count
        );
    }
    Ok(())
}

/// Generates a deterministic 2,000-file benchmark dataset under `args.target_dir`.
fn run_bench_gen(args: BenchGenArgs) -> Result<()> {
    let start = Instant::now();
    ensure_empty_or_create_dir(&args.target_dir)?;

    let folders = create_nested_folders(&args.target_dir)?;
    let total_files = BENCH_UNIQUE_COUNT
        + (BENCH_DIFF_PREFIX_GROUPS + BENCH_SAME_PREFIX_GROUPS) * BENCH_DIFF_COPIES
        + BENCH_DUP_GROUPS * BENCH_DUP_COPIES;

    let pb = make_progress_bar(total_files, "[bench-gen] Writing test dataset")?;
    let mut rng = Xorshift64::new(0xB3D0_2025_0000_0001);
    let mut buffer = vec![0u8; 512 * 1024];
    let mut total_bytes: u64 = 0;
    let mut expected_wasted_bytes: u64 = 0;

    // 1. 1,000 unique files (1 KB .. 512 KB).
    for idx in 0..BENCH_UNIQUE_COUNT {
        let size = pick_varied_size(&mut rng, 1_024, 512 * 1024);
        let payload = &mut buffer[..size];
        rng.fill_bytes(payload);
        stamp_header(payload, b"UNIQ", idx as u32, 0);

        let folder = &folders[(rng.next_u64() as usize) % folders.len()];
        let file_path = folder.join(format!("unique_{idx:04}.bin"));
        write_payload(&file_path, payload)?;

        total_bytes += size as u64;
        pb.inc(1);
    }

    // 2a. 200 same-size files (20 groups x 10 copies) with differing first 4 KB (stopped at Stage 2).
    for group_idx in 0..BENCH_DIFF_PREFIX_GROUPS {
        let size = pick_varied_size(&mut rng, 8_192, 256 * 1024);
        for copy_idx in 0..BENCH_DIFF_COPIES {
            let payload = &mut buffer[..size];
            rng.fill_bytes(payload);
            stamp_header(payload, b"SAME", group_idx as u32, copy_idx as u32);

            let folder = &folders[(group_idx * BENCH_DIFF_COPIES + copy_idx) % folders.len()];
            let file_path = folder.join(format!("samesize_{group_idx:02}_{copy_idx:02}.bin"));
            write_payload(&file_path, payload)?;

            total_bytes += size as u64;
            pb.inc(1);
        }
    }

    // 2b. 200 same-size files (20 groups x 10 copies) sharing identical 4 KB prefix (stopped at Stage 3).
    let mut shared_prefix = [0u8; PARTIAL_HASH_BYTES];
    for group_idx in 0..BENCH_SAME_PREFIX_GROUPS {
        let size = pick_varied_size(&mut rng, 16_384, 384 * 1024);
        rng.fill_bytes(&mut shared_prefix);
        stamp_header(&mut shared_prefix, b"PREF", group_idx as u32, 0);

        for copy_idx in 0..BENCH_DIFF_COPIES {
            let payload = &mut buffer[..size];
            payload[..PARTIAL_HASH_BYTES].copy_from_slice(&shared_prefix);
            rng.fill_bytes(&mut payload[PARTIAL_HASH_BYTES..]);
            stamp_header(
                &mut payload[PARTIAL_HASH_BYTES..],
                b"TAIL",
                group_idx as u32,
                copy_idx as u32,
            );

            let folder = &folders[(group_idx * 7 + copy_idx * 3) % folders.len()];
            let file_path = folder.join(format!("prefixmatch_{group_idx:02}_{copy_idx:02}.bin"));
            write_payload(&file_path, payload)?;

            total_bytes += size as u64;
            pb.inc(1);
        }
    }

    // 3. 600 exact duplicates (120 groups x 5 copies) spread across distinct folders.
    for group_idx in 0..BENCH_DUP_GROUPS {
        let size = if group_idx < BENCH_DUP_SMALL_GROUPS {
            pick_varied_size(&mut rng, 1_024, PARTIAL_HASH_BYTES)
        } else {
            pick_varied_size(&mut rng, PARTIAL_HASH_BYTES + 1, 512 * 1024)
        };

        let payload = &mut buffer[..size];
        rng.fill_bytes(payload);
        stamp_header(payload, b"DUPL", group_idx as u32, 0);

        let chosen_folders = pick_distinct_folders(&mut rng, &folders, BENCH_DUP_COPIES);
        for (copy_idx, folder) in chosen_folders.iter().enumerate() {
            let file_path = folder.join(format!("dup_{group_idx:03}_copy_{copy_idx}.bin"));
            write_payload(&file_path, payload)?;
            total_bytes += size as u64;
            pb.inc(1);
        }

        expected_wasted_bytes += (BENCH_DUP_COPIES as u64 - 1) * (size as u64);
    }

    pb.finish_and_clear();
    let elapsed_ms = start.elapsed().as_millis();

    println!(
        "{}",
        style("=== b3dup Benchmark Dataset Created ===")
            .bold()
            .cyan()
    );
    println!("{:<24}: {}", "Target directory", args.target_dir.display());
    println!("{:<24}: {}", "Nested folders", folders.len());
    println!(
        "{:<24}: {}",
        "Total files created",
        style(total_files).bold().green()
    );
    println!("{:<24}: {}", "  Unique files", BENCH_UNIQUE_COUNT);
    println!(
        "{:<24}: {} ({} diff prefix + {} identical 4 KB prefix)",
        "  Same-size non-dups",
        (BENCH_DIFF_PREFIX_GROUPS + BENCH_SAME_PREFIX_GROUPS) * BENCH_DIFF_COPIES,
        BENCH_DIFF_PREFIX_GROUPS * BENCH_DIFF_COPIES,
        BENCH_SAME_PREFIX_GROUPS * BENCH_DIFF_COPIES
    );
    println!(
        "{:<24}: {} ({} groups x {} copies)",
        "  Exact duplicates",
        BENCH_DUP_GROUPS * BENCH_DUP_COPIES,
        BENCH_DUP_GROUPS,
        BENCH_DUP_COPIES
    );
    println!(
        "{:<24}: {}",
        "Total dataset size",
        format_size(total_bytes, BINARY)
    );
    println!(
        "{:<24}: {}",
        "Expected wasted space",
        style(format_size(expected_wasted_bytes, BINARY)).yellow()
    );
    println!("{:<24}: {} ms", "Generation time", elapsed_ms);

    Ok(())
}

/// Ensures `target_dir` does not exist or is an empty directory.
fn ensure_empty_or_create_dir(target_dir: &Path) -> Result<()> {
    match fs::symlink_metadata(target_dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                bail!(
                    "Refusing to run: '{}' exists and is not a directory",
                    target_dir.display()
                );
            }
            let has_entries = fs::read_dir(target_dir)
                .with_context(|| format!("Cannot read target directory: {}", target_dir.display()))?
                .next()
                .is_some();
            if has_entries {
                bail!(
                    "Refusing to run: target directory '{}' exists and is non-empty",
                    target_dir.display()
                );
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(target_dir).with_context(|| {
                format!(
                    "Failed to create target directory: {}",
                    target_dir.display()
                )
            })?;
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Cannot inspect target directory: {}", target_dir.display())
            });
        }
    }
    Ok(())
}

/// Builds a nested folder hierarchy 3 to 4 levels deep under `target_dir`.
fn create_nested_folders(target_dir: &Path) -> Result<Vec<PathBuf>> {
    let level1 = ["alpha", "beta", "gamma", "delta"];
    let level2 = ["assets", "builds", "cache", "docs"];
    let level3 = ["raw", "archive", "exports"];
    let level4 = ["v1", "v2"];

    let mut folders = Vec::new();
    for d1 in level1 {
        for d2 in level2 {
            for (idx, d3) in level3.iter().enumerate() {
                let path_l3 = target_dir.join(d1).join(d2).join(d3);
                if idx == 0 {
                    fs::create_dir_all(&path_l3)?;
                    folders.push(path_l3);
                } else {
                    for d4 in level4 {
                        let path_l4 = path_l3.join(d4);
                        fs::create_dir_all(&path_l4)?;
                        folders.push(path_l4);
                    }
                }
            }
        }
    }
    Ok(folders)
}

/// Picks a varied file size across small, medium, and large tiers within `[min_bytes, max_bytes]`.
fn pick_varied_size(rng: &mut Xorshift64, min_bytes: usize, max_bytes: usize) -> usize {
    if min_bytes >= max_bytes {
        return min_bytes;
    }
    let span = max_bytes - min_bytes;
    let roll = rng.next_u64() % 10;
    let upper = match roll {
        0..=3 => min_bytes + span / 16,
        4..=7 => min_bytes + span / 3,
        _ => max_bytes,
    };
    rng.gen_range(min_bytes as u64, upper.max(min_bytes) as u64) as usize
}

/// Stamps a deterministic 12-byte identifier tag into the beginning of `buf` when large enough.
fn stamp_header(buf: &mut [u8], tag: &[u8; 4], group_id: u32, copy_id: u32) {
    if buf.len() >= 12 {
        buf[0..4].copy_from_slice(tag);
        buf[4..8].copy_from_slice(&group_id.to_le_bytes());
        buf[8..12].copy_from_slice(&copy_id.to_le_bytes());
    }
}

/// Selects `count` distinct folder paths deterministically using `rng`.
fn pick_distinct_folders(rng: &mut Xorshift64, folders: &[PathBuf], count: usize) -> Vec<PathBuf> {
    let mut chosen_indices = Vec::with_capacity(count);
    while chosen_indices.len() < count && chosen_indices.len() < folders.len() {
        let idx = (rng.next_u64() as usize) % folders.len();
        if !chosen_indices.contains(&idx) {
            chosen_indices.push(idx);
        }
    }
    chosen_indices
        .into_iter()
        .map(|idx| folders[idx].clone())
        .collect()
}

/// Writes `buf` to `path` via a buffered writer.
fn write_payload(path: &Path, buf: &[u8]) -> Result<()> {
    let file =
        File::create(path).with_context(|| format!("Failed to create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(buf)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    writer
        .flush()
        .with_context(|| format!("Failed to flush {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("b3dup_{name}_{}_{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn grouping_drops_singletons_and_keeps_duplicates() {
        let entries = vec![
            (100_u64, PathBuf::from("a.bin")),
            (200_u64, PathBuf::from("b1.bin")),
            (200_u64, PathBuf::from("b2.bin")),
            (300_u64, PathBuf::from("c1.bin")),
            (300_u64, PathBuf::from("c2.bin")),
            (300_u64, PathBuf::from("c3.bin")),
        ];

        let grouped = group_and_filter_duplicates(entries);
        assert!(!grouped.contains_key(&100));
        assert_eq!(grouped.get(&200).unwrap().len(), 2);
        assert_eq!(grouped.get(&300).unwrap().len(), 3);
    }

    #[test]
    fn wasted_bytes_and_sorting_follow_spec() {
        let mut groups = vec![
            DuplicateGroup {
                hash: [1; 32],
                size: 1_000,
                files: vec![PathBuf::from("a"), PathBuf::from("b")],
            },
            DuplicateGroup {
                hash: [2; 32],
                size: 500,
                files: vec![
                    PathBuf::from("c"),
                    PathBuf::from("d"),
                    PathBuf::from("e"),
                    PathBuf::from("f"),
                ],
            },
            DuplicateGroup {
                hash: [3; 32],
                size: 2_000,
                files: vec![PathBuf::from("single")],
            },
        ];

        // (2 - 1) * 1000 + (4 - 1) * 500 + (1 - 1) * 2000 = 2500
        assert_eq!(calculate_wasted_bytes(&groups), 2_500);

        sort_groups(&mut groups);
        assert_eq!(groups[0].hash, [2; 32]);
        assert_eq!(groups[1].hash, [1; 32]);
        assert_eq!(groups[2].hash, [3; 32]);
    }

    #[test]
    fn pipeline_handles_zero_byte_small_and_prefix_colliding_files() {
        let dir = make_temp_dir("pipeline");

        // Zero-byte files must always be skipped even if identical.
        fs::write(dir.join("empty1.bin"), b"").unwrap();
        fs::write(dir.join("empty2.bin"), b"").unwrap();

        // <= 4 KB duplicate pair (finalized in Stage 2, skipping Stage 3).
        fs::write(dir.join("small_a.bin"), vec![42_u8; 2_048]).unwrap();
        fs::write(dir.join("small_b.bin"), vec![42_u8; 2_048]).unwrap();

        // > 4 KB pair sharing identical first 4 KB but differing afterwards (dropped in Stage 3).
        let mut prefix_a = vec![7_u8; 8_192];
        let mut prefix_b = vec![7_u8; 8_192];
        prefix_a[5_000] = 1;
        prefix_b[5_000] = 2;
        fs::write(dir.join("prefix_a.bin"), prefix_a).unwrap();
        fs::write(dir.join("prefix_b.bin"), prefix_b).unwrap();

        // > 4 KB true duplicate trio.
        let large_dup = vec![99_u8; 10_000];
        fs::write(dir.join("large_1.bin"), &large_dup).unwrap();
        fs::write(dir.join("large_2.bin"), &large_dup).unwrap();
        fs::write(dir.join("large_3.bin"), &large_dup).unwrap();

        let report = scan_duplicates(dir.clone(), 1).unwrap();
        assert_eq!(report.files_scanned, 9);
        assert_eq!(report.duplicate_groups(), 2);
        assert_eq!(report.duplicate_files(), 5);
        assert_eq!(report.wasted_bytes(), 2 * 10_000 + 2_048);
        assert_eq!(report.stage1.candidates, 7);
        assert_eq!(report.stage2.candidates, 7);
        assert_eq!(report.stage3_hashed_files, 5);
        assert_eq!(report.stage3.candidates, 5);

        fs::remove_dir_all(&dir).unwrap();
    }
}
