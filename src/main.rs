mod config;
mod file_record;

use anyhow::Context;
use clap::Parser;
use config::Config;
use file_record::FileRecord;
use itertools::Itertools;
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
#[command(author, version, about = "A tool to process multiple folders")]
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

    #[arg(
        long,
        help = "Walk the directory and report duplication states on each file"
    )]
    report_dir: Option<PathBuf>,
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

fn add_folder(conn: &mut Connection, config: &Config, path: &Path) -> anyhow::Result<()> {
    let map: HashSet<_> = fetch(conn).into_iter().map(|x| x.path).collect();

    info!("Scanning files in '{}'", path.display());

    let mut tx = conn.transaction()?;
    let mut i = 0;
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let path = entry.path().canonicalize()?;
            let path_name = path.display().to_string();
            if !map.contains(&path) && config.is_included(&path) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                tx.execute(
                    "INSERT INTO files (path, size) VALUES (?1, ?2)",
                    params![path_name, size as i64],
                )?;
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

fn find_duplicates_by<T, F>(cmp: F, mut files: Vec<T>) -> Vec<T>
where
    T: Eq + Clone,
    F: Fn(&T, &T) -> Ordering + Clone,
{
    files.sort_unstable_by(cmp.clone());
    if files.len() == 0 {
        return vec![];
    }
    let mut duplicates: Vec<T> = vec![];
    let mut prev = &files[0];
    for file in files.iter().skip(1) {
        if cmp(prev, file).is_eq() {
            let prev_is_added = duplicates.last().map(|x| x == prev).unwrap_or(false);
            if !prev_is_added {
                duplicates.push(prev.clone());
            }
            duplicates.push(file.clone());
        }
        prev = file;
    }

    duplicates
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

    info!("Fetching files and filtering out those that don't exist");
    let files: Vec<_> = fetch(&mut conn)
        .into_iter()
        .filter(|x| config.is_included(Path::new(&x.path)) && Path::new(&x.path).exists())
        .sorted_by(|a, b| a.size.cmp(&b.size))
        .collect();

    info!("Group {} files by size", files.len());
    let to_check_hash: Vec<FileRecord> = find_duplicates_by(|a, b| a.size.cmp(&b.size), files)
        .into_iter()
        .filter(|x| x.hash.is_none())
        .collect();

    let file_size_to_hash = to_check_hash.iter().fold(0, |agg, x| agg + x.size);

    info!(
        "Running md5sum on {} duplicated files. Total size: {}",
        to_check_hash.len(),
        humanize_bytes(file_size_to_hash as f64)
    );
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
                info!(
                    "Computed hash for {} files. Total size: {}",
                    i,
                    humanize_bytes(bytes as f64)
                );
                tx.commit()?;
                tx = conn.transaction()?;
                i = 0;
                time = Instant::now();
                bytes = 0;
            }
        }
    }
    info!(
        "Computed hash for {} files. Total size: {}",
        i,
        humanize_bytes(bytes as f64)
    );
    tx.commit()?;

    info!("Finding duplicates by hash");
    let files: Vec<_> = fetch(&mut conn)
        .into_iter()
        .filter(|x| {
            config.is_included(Path::new(&x.path))
                && x.hash.is_some()
                && Path::new(&x.path).exists()
        })
        .collect();

    if let Some(report_dir) = args.report_dir {
        let files: Vec<_> = fetch(&mut conn)
            .into_iter()
            .filter(|x| config.is_included(Path::new(&x.path)) && Path::new(&x.path).exists())
            .collect();

        let mut files_by_path = HashMap::new();
        for file in files.iter() {
            files_by_path.insert(file.path.clone(), file.clone());
        }
        for entry in WalkDir::new(&report_dir).sort_by_file_name() {
            let entry = entry?;
            if entry.path().is_dir() {
                continue;
            }
            let path = entry.path().canonicalize()?;
            let path_name = path.display().to_string();
            let file_record = &files_by_path
                .get(&path)
                .context(format!("Did not find {path_name}"))?;
            print!("{path_name}: ");
            if let Some(hash) = &file_record.hash {
                let mut other_files = vec![];
                for file in files.iter() {
                    if Path::new(&file.path).starts_with(&report_dir) {
                        continue;
                    }
                    if let Some(other_hash) = &file.hash {
                        if hash == other_hash {
                            other_files.push(file.clone());
                        }
                    }
                }
                // let other_files_str = other_files.into_iter().map(|x| x.path).join(", ");
                println!("has {} other copies", other_files.len());
            } else {
                println!("Only copy");
            }
        }
    } else {
        let duplicates: Vec<FileRecord> = find_duplicates_by(|a, b| a.hash.cmp(&b.hash), files);
        let mut prev = "".to_string();
        for duplicate in duplicates {
            if duplicate.size < 1000 {
                continue;
            }
            if duplicate.hash.clone().unwrap() != prev {
                println!();
                prev = duplicate.hash.unwrap().clone();
                println!(
                    "\nNew file: {} with size {}",
                    &prev,
                    humanize_bytes(duplicate.size as f64)
                );
            }
            println!("- {}", duplicate.path.display());
        }
    }

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
        assert_eq!(Vec::<i32>::new(), duplicates);
    }

    #[test]
    fn test_find_duplicates_by_2() {
        let values = vec![1, 2, 3, 4, 3];
        let duplicates = find_duplicates_by(|a, b| a.cmp(&b), values);
        assert_eq!(vec![3, 3], duplicates);
    }

    #[test]
    fn test_humanize() {
        assert_eq!("34.5 KiB", humanize_bytes(34_500));
    }
}
