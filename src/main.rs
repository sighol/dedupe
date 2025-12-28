mod config;
mod file_record;

use anyhow::Context;
use clap::Parser;
use colored::Colorize;
use config::Config;
use file_record::FileRecord;
use rusqlite::{Connection, params};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::info;
use walkdir::WalkDir;

use crate::config::Score;

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
    #[arg(long)]
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

    /// Delete lowest-scoring duplicates.
    #[arg(long)]
    delete: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let mut conn = match args.db_path {
        Some(path) => Connection::open(&path)?,
        None => Connection::open_in_memory()?,
    };
    let config = Config {
        min_size: args.min_size.unwrap_or(0),
        exclude_regex: args
            .exclude_regex
            .iter()
            .map(|x| regex::Regex::new(x).expect("Valid regex pattern"))
            .collect(),
        includes: args
            .folders
            .iter()
            .map(|x| {
                x.canonicalize()
                    .context(format!("Could not find file {x:?}"))
            })
            .collect::<Result<Vec<PathBuf>, anyhow::Error>>()
            .unwrap(),
        scores: args
            .score
            .iter()
            .map(|x| Score::parse(x).expect("Bad score"))
            .collect(),
    };

    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (
            path TEXT NOT NULL PRIMARY KEY,
            size INTEGER NOT NULL,
            hash TEXT
        )",
        [],
    )?;

    let mut files = vec![];
    for dir in config.includes.iter() {
        for f in add_folder(&mut conn, &config, dir).expect("Failed to add folder") {
            files.push(f);
        }
    }
    info!("Found {} files", files.len());

    hash_duplicated_candidates(&mut conn, &mut files)?;

    if let Some(report_dir) = args.report_dir {
        report_duplication_status_in_dir(&config, files, &report_dir)?;
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
    for file in fetch(conn, path) {
        map.insert(file.path.display().to_string(), file);
    }
    info!(
        "Found {} files in the db cache starting at path {}",
        map.len(),
        path.display()
    );

    info!("Scanning files in '{}'", path.display());
    let mut tx = conn.transaction()?;
    let mut i = 0;
    let mut files = vec![];
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let path = entry.path();
            let path_name = path.display().to_string();
            if !config.is_included(&path) {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if size < config.min_size {
                continue;
            }

            if let Some(existing) = map.get(&path_name) {
                files.push(existing.clone());
            } else {
                let file_record = FileRecord {
                    path: path.to_owned(),
                    size: size as i64,
                    hash: None,
                };
                files.push(file_record);

                tx.execute(
                    "INSERT INTO files (path, size) VALUES (?1, ?2)",
                    params![path_name, size as i64],
                )
                .context(format!(
                    "Failed to insert {}, {}",
                    &path_name,
                    path.display()
                ))?;
                i += 1;

                if i >= 10_000 {
                    info!("Adding {} files", i);
                    i = 0;
                    tx.commit().unwrap();
                    tx = conn.transaction().unwrap();
                }
            }
        }
    }
    info!("Adding {} files", i);
    tx.commit().unwrap();
    Ok(files)
}

fn fetch(conn: &mut Connection, path: &Path) -> Vec<FileRecord> {
    let mut stmt = conn
        .prepare("select path, size, hash from files where path LIKE (?1 || '/%')")
        .expect("select all files");

    let existing_files: Vec<FileRecord> = stmt
        .query_map([path.display().to_string()], |row| {
            Ok(FileRecord {
                path: row.get::<_, String>(0).expect("Should get path").into(),
                size: row.get(1).expect("should get size"),
                hash: row.get(2).ok(),
            })
        })
        .unwrap()
        .flatten()
        .collect();
    existing_files
}

fn hash_duplicated_candidates(
    conn: &mut Connection,
    files: &mut [FileRecord],
) -> anyhow::Result<()> {
    info!("Group {} files by size", files.len());
    let to_check_hash: Vec<usize> = find_duplicates_indexes_by(|a, b| a.size.cmp(&b.size), files)
        .into_iter()
        .flatten()
        .filter(|i| {
            // Only compute hash for files that don't have a hash in the db, and files that are not empty.
            // Duplicated empty files don't count.
            files[*i].hash.is_none() && files[*i].size > 0
        })
        .collect();

    let file_size_to_hash = to_check_hash.iter().fold(0, |acc, i| acc + files[*i].size);

    info!(
        "Running md5sum on {} duplicated files. Total size: {}",
        to_check_hash.len(),
        humanize_bytes(file_size_to_hash as f64)
    );

    fn log(count: usize, bytes: i64) {
        info!(
            "Computed hash for {} files. Total size: {}",
            count,
            humanize_bytes(bytes as f64)
        );
    }

    let mut tx = conn.transaction()?;
    let mut i = 0;
    let mut bytes = 0;
    let mut time = Instant::now();
    for index in to_check_hash {
        let file_data: &mut FileRecord = files
            .get_mut(index)
            .expect("Could not find index in files array");
        if let Ok(hash) = compute_md5(&file_data.path) {
            file_data.hash = Some(hash.clone());
            tx.execute(
                "UPDATE files SET hash = ?1 WHERE path = ?2",
                params![hash, file_data.path.display().to_string()],
            )?;
            i += 1;
            bytes += file_data.size;
            if i >= 5_000 || time.elapsed() > Duration::from_secs(5) {
                log(i, bytes);
                tx.commit()?;
                tx = conn.transaction()?;
                i = 0;
                time = Instant::now();
                bytes = 0;
            }
        }
    }

    log(i, bytes);
    tx.commit()?;

    Ok(())
}

fn find_duplicates_by<T, F>(cmp: F, files: &mut [T]) -> Vec<Vec<T>>
where
    T: Eq + Clone + std::fmt::Debug,
    F: Fn(&T, &T) -> Ordering + Clone,
{
    files.sort_unstable_by(cmp.clone());
    if files.is_empty() {
        return vec![];
    }
    let mut duplicate_groups: Vec<Vec<T>> = vec![];
    let mut duplicates: Vec<T> = vec![];
    for i in 1..files.len() {
        let prev = &files[i - 1];
        let next = &files[i];
        if cmp(prev, next).is_eq() {
            if duplicates.is_empty() {
                duplicates.push(prev.clone());
            }
            duplicates.push(next.clone());
        } else if !duplicates.is_empty() {
            duplicate_groups.push(duplicates);
            duplicates = vec![];
        }
    }
    if !duplicates.is_empty() {
        duplicate_groups.push(duplicates);
    }

    duplicate_groups
}

fn find_duplicates_indexes_by<T, F>(cmp: F, files: &mut [T]) -> Vec<Vec<usize>>
where
    T: Eq + Clone + std::fmt::Debug,
    F: Fn(&T, &T) -> Ordering + Clone,
{
    files.sort_unstable_by(|a, b| cmp(a, b));
    if files.is_empty() {
        return vec![];
    }
    let mut duplicate_groups: Vec<Vec<usize>> = vec![];
    let mut duplicates: Vec<usize> = vec![];
    for i in 1..files.len() {
        let prev = &files[i - 1];
        let next = &files[i];
        if cmp(prev, next).is_eq() {
            if duplicates.is_empty() {
                duplicates.push(i - 1);
            }
            duplicates.push(i);
        } else if !duplicates.is_empty() {
            duplicate_groups.push(duplicates);
            duplicates = vec![];
        }
    }
    if !duplicates.is_empty() {
        duplicate_groups.push(duplicates);
    }

    duplicate_groups
}

fn report_all_duplicated_files(config: &Config, files: Vec<FileRecord>, delete: bool) {
    info!("Finding duplicates by hash");
    let mut files: Vec<_> = files.into_iter().filter(|x| x.hash.is_some()).collect();

    let mut redundant_size = 0;
    let mut duplicates = find_duplicates_by(|a, b| a.hash.cmp(&b.hash), &mut files);
    duplicates.sort_unstable_by_key(|x| x[0].size);
    for duplicate in duplicates {
        let size = duplicate[0].size;
        let hash = duplicate[0]
            .hash
            .clone()
            .expect("Duplicates have hash values");
        redundant_size += size * (duplicate.len() as i64 - 1);
        println!(
            "\nDuplicated file with size {}",
            humanize_bytes(size as f64).bold().yellow(),
        );
        let mut scored_file_records = vec![];
        let mut lowest_score = i64::MAX;
        for file_record in duplicate {
            assert!(
                file_record.size == size && file_record.hash.as_ref() == Some(&hash),
                "The hash and size must be identical in all groups."
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

        if scored_file_records.iter().any(|(_, score)| score.is_some()) {
            // Can only delete if all files are scored and not all share the same score.
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
            scored_file_records.sort_by_key(|(_, score)| score.unwrap_or(i64::MAX));
            scored_file_records.reverse();
            for (file_record, score) in scored_file_records {
                let score_fmt = match score {
                    Some(value) => {
                        format!("{}: {}", "Score".green(), value.to_string().bold().blue())
                    }
                    None => format!("{}", "No score".bold().red()),
                };
                let lowest_score_fmt = if score == Some(lowest_score) && has_deletables {
                    // Deleting while computing the lowest_score_fmt is so ugly.
                    if delete {
                        match std::fs::remove_file(&file_record.path) {
                            Ok(()) => format!("{}", " Deleted".bold().green()),
                            Err(e) => format!("{} {:?}", " Failed to delete".red(), e),
                        }
                    } else {
                        format!("{}", " to be deleted".bold().red())
                    }
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
            for (file_record, _) in scored_file_records {
                println!("  - {}", file_record.path.display());
            }
        }
    }

    println!(
        "\nRedundant size: {}",
        humanize_bytes(redundant_size as f64).yellow().bold()
    );
}

fn report_duplication_status_in_dir(
    config: &Config,
    files: Vec<FileRecord>,
    report_dir: &Path,
) -> anyhow::Result<()> {
    let report_dir = report_dir
        .canonicalize()
        .expect("Report dir could not be canonicalized.");

    let files_by_hash = {
        let mut files_with_hash: Vec<_> = files.iter().filter(|x| x.hash.is_some()).collect();
        let dups = find_duplicates_by(|x, y| x.hash.cmp(&y.hash), &mut files_with_hash);
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
        if path.is_dir() {
            continue;
        } else if !config.is_included(&path) {
            ignored_files += 1;
            continue;
        }
        let path_name = path
            .strip_prefix(&report_dir)
            .expect("File was found in report_dir")
            .display()
            .to_string();
        let file_record = &files_by_path
            .get(&path.display().to_string())
            .expect(&format!("Did not find {path:?}"));
        if file_record.size < config.min_size as i64 {
            ignored_files += 1;
            continue;
        }
        print!("{}: ", path_name.blue());
        if let Some(hash) = &file_record.hash {
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
                humanize_bytes(file_record.size as f64).yellow().bold(),
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

fn compute_md5(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut context = md5::Context::new();
    let mut buffer = [0; 1024];

    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        context.consume(&buffer[..count]);
    }

    Ok(format!("{:x}", context.finalize()))
}

fn humanize_bytes<T: Into<f64>>(bytes: T) -> String {
    let suffixes = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
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
    use crate::{find_duplicates_by, humanize_bytes};

    #[test]
    fn test_find_duplicates_by() {
        let mut values = vec![1, 2, 3, 4];
        let duplicates = find_duplicates_by(|a, b| a.cmp(&b), &mut values);
        assert_eq!(Vec::<Vec<i32>>::new(), duplicates);
    }

    #[test]
    fn test_find_duplicates_by_2() {
        let mut values = vec![1, 2, 3, 4, 3];
        let duplicates = find_duplicates_by(|a, b| a.cmp(&b), &mut values);
        assert_eq!(vec![vec![3, 3]], duplicates);
    }

    #[test]
    fn test_find_duplicates_all_duplicated() {
        let mut values = vec![1, 1, 1];
        let duplicates = find_duplicates_by(|a, b| a.cmp(&b), &mut values);
        assert_eq!(vec![vec![1, 1, 1]], duplicates);
    }

    #[test]
    fn test_humanize() {
        assert_eq!("34.5 KiB", humanize_bytes(34_500));
    }
}
