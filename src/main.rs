mod config;
mod db;
mod file_record;
mod filter_duplicates;

use crate::config::{DuplicateGroupFilter, Score};
use crate::db::FilesTransaction;
use crate::filter_duplicates::find_duplicates_by;
use clap::Parser;
use colored::Colorize;
use config::Config;
use file_record::FileRecord;
use rusqlite::Connection;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::info;
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(author, version, about = "A tool to find duplicated files.")]
struct Args {
    /// List of folders to process.
    #[arg(value_name = "FOLDER", num_args = 1.., required = true)]
    folders: Vec<PathBuf>,

    /// Database cache file for checksums.
    ///
    /// The file will be created if it does not exist. For big file hierarchies, this greatly
    /// improves the performance.
    #[arg(short, long)]
    db_path: Option<PathBuf>,

    /// Exclude files where the regex matches any part of the full file path.
    #[arg(short = 'e', long)]
    exclude_regex: Vec<String>,

    /// Exclude files smaller than this file size in bytes.
    #[arg(short, long)]
    min_size: Option<u64>,

    /// Walk the directory and report duplication states for each file.
    #[arg(short, long)]
    report_dir: Option<PathBuf>,

    /// Add scores to path patterns for cleanup.
    ///
    /// The scores will be processed for each group of duplicated files. Only the files that have a
    /// score equal to the lowest score in the group will be considered for deletion. If all the
    /// files in the group have the same score, nothing will be deleted.
    ///
    /// Files will only be deleted if the --delete flag is set.
    ///
    /// Format is --score SCORE=REGEX.
    /// Example: --score 10=/duplicates/
    #[arg(short, long)]
    score: Vec<String>,

    /// Only show duplication groups that have at least one scored file.
    ///
    /// Specify twice to only show duplication groups containing files that are candidates for
    /// deletion.
    #[arg(short, long, action=clap::ArgAction::Count)]
    only_show_groups_with_scores: u8,

    /// Delete lowest-scoring duplicates.
    #[arg(long)]
    delete: bool,

    #[arg(value_enum, long="color", default_value_t=ColorChoice::Always)]
    color: ColorChoice,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum ColorChoice {
    Auto,
    Never,
    Always,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    match args.color {
        ColorChoice::Always => colored::control::set_override(true),
        ColorChoice::Never => colored::control::set_override(false),
        _ => {}
    };
    if let Some(report_dir) = &args.report_dir {
        match report_dir.canonicalize() {
            Ok(_) => (),
            Err(e) => {
                eprintln!(
                    "Failed to read report dir '{}': {}",
                    report_dir.display().to_string().blue(),
                    e
                );
                std::process::exit(1);
            }
        }
    }
    let mut conn = match args.db_path {
        Some(path) => Connection::open(&path)?,
        None => Connection::open_in_memory()?,
    };
    db::setup(&mut conn)?;

    let config = Config {
        groups_filter: match args.only_show_groups_with_scores {
            0 => DuplicateGroupFilter::All,
            1 => DuplicateGroupFilter::OnlyGroupsWithScores,
            _ => DuplicateGroupFilter::OnlyGroupsWithDeletables,
        },
        min_size: args.min_size.unwrap_or(0),
        exclude_regex: args
            .exclude_regex
            .iter()
            .map(|x| {
                regex::Regex::new(x).unwrap_or_else(|e| {
                    eprintln!("Invalid regex pattern: {}", e);
                    std::process::exit(1);
                })
            })
            .collect(),
        includes: args
            .folders
            .iter()
            .map(|x| {
                x.canonicalize().unwrap_or_else(|e| {
                    eprintln!(
                        "Failed to read directory '{}': {}",
                        x.display().to_string().blue(),
                        e
                    );
                    std::process::exit(1);
                })
            })
            .collect::<Vec<PathBuf>>(),
        scores: args
            .score
            .iter()
            .map(|x| {
                Score::parse(x).unwrap_or_else(|e| {
                    eprintln!("Could not parse score: {}", e);
                    std::process::exit(1);
                })
            })
            .collect(),
    };

    let mut files = vec![];
    for dir in config.includes.iter() {
        for f in add_folder(&mut conn, &config, dir).expect("Failed to add folder") {
            files.push(f);
        }
    }
    info!("Found {} files in search folders", files.len());

    files = filter_duplicates::step1(&mut conn, files)?;
    files = filter_duplicates::step2(&mut conn, files)?;

    if let Some(report_dir) = args.report_dir {
        report_duplication_status_in_dir(&config, &report_dir, files)?;
    } else {
        report_all_duplicated_files(&config, files, args.delete);
    }

    Ok(())
}

fn add_folder(
    conn: &mut Connection,
    config: &Config,
    path: &Path,
) -> anyhow::Result<Vec<FileRecord>> {
    let mut map = HashMap::new();
    for file in db::fetch(conn, path) {
        map.insert(file.path.display().to_string(), file);
    }
    if !map.is_empty() {
        info!(
            "Found {} files in the db cache starting at path {}",
            map.len(),
            path.display()
        );
    }
    info!("Scanning files in '{}'", path.display());
    let mut tx = FilesTransaction::begin(conn)?;
    let mut files = vec![];
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let path = entry.path();
            if !config.is_included(path) {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if size < config.min_size {
                continue;
            }

            let path_name = path.display().to_string();
            if let Some(existing) = map.get(&path_name) {
                files.push(existing.clone());
            } else {
                let file_record = FileRecord {
                    path: path.to_owned(),
                    size: size as i64,
                    hash_4096: None,
                    hash: None,
                };
                let num_inserts = tx.insert(&file_record)?;
                if num_inserts >= 10_000 {
                    tx.commit()?;
                    tx = FilesTransaction::begin(conn)?;
                }
                files.push(file_record);
            }
        }
    }
    tx.commit()?;
    Ok(files)
}

fn report_all_duplicated_files(config: &Config, files: Vec<FileRecord>, delete: bool) {
    info!("Finding duplicates by hash");
    let files: Vec<_> = files.into_iter().filter(|x| x.hash.is_some()).collect();

    let mut redundant_size = 0;
    let mut duplicate_groups = find_duplicates_by(|a, b| a.hash.cmp(&b.hash), files);
    let mut num_duplicate_groups = 0;
    duplicate_groups.sort_unstable_by_key(|x| x[0].size);
    let mut to_delete = vec![];
    for duplicate_group in duplicate_groups {
        let file_size = duplicate_group[0].size;
        let hash = duplicate_group[0].hash.clone().unwrap();
        let mut scored_file_records = vec![];
        let mut lowest_score = i64::MAX;
        for file_record in duplicate_group {
            assert!(
                file_record.size == file_size && file_record.hash.as_ref() == Some(&hash),
                "The hash and size must be identical within the group."
            );
            let path_name = file_record.path.display().to_string();
            let score = config.get_score(&path_name);
            scored_file_records.push((file_record, score));
            if let Some(score) = score
                && score < lowest_score
            {
                lowest_score = score;
            }
        }

        let scores_in_group = scored_file_records.iter().any(|(_, score)| score.is_some());
        let has_deletables = {
            let scored_files = scored_file_records
                .iter()
                .filter(|(_, score)| score.is_some())
                .count();
            let files_with_lowest_score = scored_file_records
                .iter()
                .filter(|(_, score)| *score == Some(lowest_score))
                .count();
            scored_files == scored_file_records.len() && files_with_lowest_score != scored_files
        };

        let should_show = match config.groups_filter {
            DuplicateGroupFilter::All => true,
            DuplicateGroupFilter::OnlyGroupsWithScores => scores_in_group,
            DuplicateGroupFilter::OnlyGroupsWithDeletables => has_deletables,
        };
        if !should_show {
            continue;
        }

        redundant_size += file_size * (scored_file_records.len() as i64 - 1);
        num_duplicate_groups += 1;
        let header = format!(
            "\nDuplicated file with size {}",
            humanize_bytes(file_size as f64).bold().yellow(),
        );

        if scores_in_group {
            println!("{}", header);
            scored_file_records.sort_by_key(|(_, score)| score.unwrap_or(i64::MAX));
            scored_file_records.reverse();
            for (file_record, score) in scored_file_records {
                let score_fmt = match score {
                    Some(value) => {
                        format!("{}: {}", "Score".green(), value.to_string().bold().blue())
                    }
                    None => format!("{}", "No score".bold().red()),
                };
                let is_delete_candidate = score == Some(lowest_score) && has_deletables;
                if is_delete_candidate {
                    to_delete.push(file_record.clone());
                }
                let lowest_score_fmt = if is_delete_candidate {
                    format!("{}", " to be deleted".bold().red())
                } else {
                    String::new()
                };
                println!(
                    "  - {} {}{}",
                    file_record.path.display(),
                    score_fmt,
                    lowest_score_fmt
                );
            }
        } else {
            scored_file_records.sort_by(|(a, _), (b, _)| a.path.cmp(&b.path));
            println!("{}", header);
            for (file_record, _) in scored_file_records {
                println!("  - {}", file_record.path.display());
            }
        }
    }

    println!(
        "\nRedundant size: {}",
        humanize_bytes(redundant_size as f64).yellow().bold()
    );
    println!(
        "Number of duplicate groups: {}",
        num_duplicate_groups.to_string().yellow().bold()
    );

    if !to_delete.is_empty() && delete {
        confirm_and_delete(to_delete);
    }
}

fn confirm_and_delete(to_delete: Vec<FileRecord>) {
    let total_size: i64 = to_delete.iter().map(|x| x.size).sum();
    print!(
        "\nAre you sure you want to delete {} files ({})? [y/n]: ",
        to_delete.len().to_string().bold().yellow(),
        humanize_bytes(total_size as f64).bold().yellow(),
    );
    std::io::stdout().flush().unwrap();
    let mut response = String::new();
    std::io::stdin()
        .read_line(&mut response)
        .expect("Failed to read from stdin");
    if response.trim().to_lowercase() == "y" {
        for file in to_delete {
            print!("- {}", file.path.display());
            match std::fs::remove_file(&file.path) {
                Ok(()) => println!(" {}", "deleted".green()),
                Err(e) => println!(" {} {}", "failed to delete".red().bold(), e),
            }
        }
    }
}

fn report_duplication_status_in_dir(
    config: &Config,
    report_dir: &Path,
    files: Vec<FileRecord>,
) -> anyhow::Result<()> {
    let report_dir = report_dir
        .canonicalize()
        .expect("Report dir could not be canonicalized.");

    let files_by_hash = {
        let files_with_hash: Vec<_> = files.iter().filter(|x| x.hash.is_some()).collect();
        let dups = find_duplicates_by(|x, y| x.hash.cmp(&y.hash), files_with_hash);
        let mut map = HashMap::new();
        for group in dups.into_iter() {
            let hash = group[0].hash.clone().unwrap();
            map.insert(hash, group);
        }
        map
    };

    let mut files_by_path = HashMap::new();
    for file in files.iter() {
        files_by_path.insert(file.path.display().to_string(), file.clone());
    }
    let mut unique_files = 0;
    let mut duplicated_files = 0;
    let mut ignored_files = 0;
    println!();
    for entry in WalkDir::new(&report_dir).sort_by_file_name() {
        let entry = entry?;
        let path = entry.path();
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if path.is_dir() {
            continue;
        } else if size == 0 || size < config.min_size || !config.is_included(path) {
            ignored_files += 1;
            continue;
        }
        let path_name = path
            .strip_prefix(&report_dir)
            .expect("File was found in report_dir")
            .display()
            .to_string();

        let file_record = &files_by_path.get(&path.display().to_string());
        let file_record_hash = file_record.and_then(|x| x.hash.clone());
        print!("{}: ", path_name.blue());
        if let Some(hash) = &file_record_hash {
            let other_files: Vec<_> = match files_by_hash.get(hash) {
                Some(vec) => vec
                    .iter()
                    .filter(|x| !x.path.starts_with(&report_dir))
                    .collect(),
                None => vec![],
            };
            if other_files.is_empty() {
                unique_files += 1;
                println!("Only copy");
                continue;
            }
            duplicated_files += 1;
            println!(
                "{} other copies. File size: {}.",
                other_files.len().to_string().yellow().bold(),
                humanize_bytes(size as f64).yellow().bold(),
            );
            for f in other_files {
                println!("  - {}", f.path.display());
            }
            println!();
        } else {
            unique_files += 1;
            println!("Only copy");
        }
    }

    println!(
        "\n{} duplicated files, {} unique files, and {} ignored files",
        duplicated_files.to_string().bold().yellow(),
        unique_files.to_string().bold().yellow(),
        ignored_files.to_string().bold().yellow(),
    );
    Ok(())
}

fn humanize_bytes<T: Into<f64>>(bytes: T) -> String {
    let suffixes = ["B", "KB", "MB", "GB", "TB", "PB"];
    let bytes = bytes.into();
    if bytes <= 0.0 {
        return "0 B".to_string();
    }

    let unit: f64 = 1000.0;

    let base = bytes.log10() / unit.log10();

    let value = bytes / unit.powf(base.floor());
    let result = format!("{:.1}", value).trim_end_matches(".0").to_owned();

    format!("{} {}", result, suffixes[base.floor() as usize])
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_humanize() {
        assert_eq!("34.5 KB", humanize_bytes(34_500));
    }
}
