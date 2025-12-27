mod config;
mod file_record;

use anyhow::Context;
use clap::Parser;
use colored::Colorize;
use config::Config;
use file_record::FileRecord;
use rusqlite::{Connection, params};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::info;
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(author, version, about = "A tool to find duplicated files.")]
struct Args {
    /// List of folders to process
    #[arg(value_name = "FOLDER", num_args = 1.., required = true)]
    folders: Vec<PathBuf>,

    #[arg(short, long)]
    db_path: Option<PathBuf>,

    #[arg(short, long)]
    exclude_globs: Vec<String>,

    #[arg(short = 'E', long)]
    exclude_regex: Vec<String>,

    /// Walk the directory and report duplication states for each file.
    #[arg(long)]
    report_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let mut conn = match args.db_path {
        Some(path) => Connection::open(&path)?,
        None => Connection::open_in_memory()?,
    };
    let config = Config {
        excludes: args
            .exclude_globs
            .iter()
            .map(|x| glob::Pattern::new(x).expect("Valid pattern"))
            .collect(),
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
    };

    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (
            path TEXT NOT NULL PRIMARY KEY,
            size INTEGER NOT NULL,
            hash TEXT
        )",
        [],
    )?;

    for dir in config.includes.iter() {
        add_folder(&mut conn, &config, dir).expect("Failed to add folder");
    }

    hash_duplicated_candidates(&mut conn, &config)?;

    if let Some(report_dir) = args.report_dir {
        report_duplication_status_in_dir(&mut conn, &config, &report_dir)?;
    } else {
        report_all_duplicated_files(&mut conn, &config);
    }

    Ok(())
}

fn add_folder(conn: &mut Connection, config: &Config, path: &Path) -> anyhow::Result<()> {
    let map: HashSet<_> = fetch(conn)
        .into_iter()
        .map(|x| x.path.display().to_string())
        .collect();

    info!("Scanning files in '{}'", path.display());
    let mut tx = conn.transaction()?;
    let mut i = 0;
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let path = entry.path().canonicalize()?;
            let path_name = path.display().to_string();
            if !map.contains(&path_name) && config.is_included(&path) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
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
    Ok(())
}

fn fetch(conn: &mut Connection) -> Vec<FileRecord> {
    let mut stmt = conn
        .prepare("select path, size, hash from files")
        .expect("select all files");

    let existing_files: Vec<FileRecord> = stmt
        .query_map([], |row| {
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

fn hash_duplicated_candidates(conn: &mut Connection, config: &Config) -> anyhow::Result<()> {
    info!("Fetching files and filtering out those that don't exist");
    let files: Vec<_> = fetch(conn)
        .into_iter()
        .filter(|x| config.is_included(&x.path) && x.path.exists())
        .collect();

    info!("Group {} files by size", files.len());
    let to_check_hash: Vec<FileRecord> = find_duplicates_by(|a, b| a.size.cmp(&b.size), files)
        .into_iter()
        .flatten()
        .filter(|x| x.hash.is_none())
        .collect();

    let file_size_to_hash = to_check_hash.iter().fold(0, |agg, x| agg + x.size);

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
    for file_data in to_check_hash {
        assert!(file_data.hash.is_none());
        if let Ok(hash) = compute_md5(&file_data.path) {
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

fn find_duplicates_by<T, F>(cmp: F, mut files: Vec<T>) -> Vec<Vec<T>>
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

fn report_all_duplicated_files(conn: &mut Connection, config: &Config) {
    info!("Finding duplicates by hash");
    let files: Vec<_> = fetch(conn)
        .into_iter()
        .filter(|x| {
            config.is_included(Path::new(&x.path))
                && x.hash.is_some()
                && Path::new(&x.path).exists()
        })
        .collect();

    let mut duplicates = find_duplicates_by(|a, b| a.hash.cmp(&b.hash), files);
    duplicates.sort_by_key(|x| x[0].size);
    for duplicate in duplicates {
        let size = duplicate[0].size;
        println!(
            "\nDuplicated file with size {}",
            humanize_bytes(size as f64).bold().yellow(),
        );
        for file_record in duplicate {
            println!("  - {}", file_record.path.display());
        }
    }
}

fn report_duplication_status_in_dir(
    conn: &mut Connection,
    config: &Config,
    report_dir: &Path,
) -> anyhow::Result<()> {
    let report_dir = report_dir
        .canonicalize()
        .expect("Report dir could not be canonicalized.");
    let files: Vec<_> = fetch(conn)
        .into_iter()
        .filter(|x| config.is_included(&x.path) && x.path.exists())
        .collect();

    let files_by_hash = {
        let dups = find_duplicates_by(
            |x, y| x.hash.cmp(&y.hash),
            files.iter().filter(|x| x.hash.is_some()).collect(),
        );
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
    println!();
    for entry in WalkDir::new(&report_dir).sort_by_file_name() {
        let entry = entry?;
        let path = entry.path().canonicalize()?;
        if path.is_dir() {
            continue;
        } else if !config.is_included(&path) {
            eprintln!(
                "File {} in report dir is not included.",
                path.display().to_string().red()
            );
            std::process::exit(1);
        }
        let path_name = path
            .strip_prefix(&report_dir)
            .expect("File was found in report_dir")
            .display()
            .to_string();
        let file_record = &files_by_path
            .get(&path.display().to_string())
            .expect(&format!("Did not find {path:?}"));
        print!("{}: ", path_name.blue());
        if let Some(hash) = &file_record.hash {
            let other_files: Vec<_> = files_by_hash[hash]
                .iter()
                .filter(|x| !x.path.starts_with(&report_dir))
                .collect();
            if other_files.is_empty() {
                unique_files += 1;
                println!("Only copy");
                continue;
            }
            duplicated_files += 1;
            println!(
                "has {} other copies outside of the report dir",
                other_files.len().to_string().yellow().bold(),
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
        "{} duplicated files and {} unique files",
        duplicated_files.to_string().bold().yellow(),
        unique_files.to_string().bold().yellow(),
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

    let result = format!("{:.1}", unit.powf(base - base.floor()))
        .trim_end_matches(".0")
        .to_owned();

    format!("{} {}", result, suffixes[base.floor() as usize])
}

#[cfg(test)]
mod test {
    use std::path::Path;

    use crate::{find_duplicates_by, humanize_bytes};

    #[test]
    fn test_pattern() {
        let pattern = glob::Pattern::new("**/test/**").unwrap();
        let path = Path::new("a/b/c/test/a");
        assert!(pattern.matches_path(path));
    }

    #[test]
    fn test_find_duplicates_by() {
        let values = vec![1, 2, 3, 4];
        let duplicates = find_duplicates_by(|a, b| a.cmp(&b), values);
        assert_eq!(Vec::<Vec<i32>>::new(), duplicates);
    }

    #[test]
    fn test_find_duplicates_by_2() {
        let values = vec![1, 2, 3, 4, 3];
        let duplicates = find_duplicates_by(|a, b| a.cmp(&b), values);
        assert_eq!(vec![vec![3, 3]], duplicates);
    }

    #[test]
    fn test_find_duplicates_all_duplicated() {
        let values = vec![1, 1, 1];
        let duplicates = find_duplicates_by(|a, b| a.cmp(&b), values);
        assert_eq!(vec![vec![1, 1, 1]], duplicates);
    }

    #[test]
    fn test_humanize() {
        assert_eq!("34.5 KiB", humanize_bytes(34_500));
    }
}
