use anyhow::Context;
use clap::Parser;
use itertools::Itertools;
use rusqlite::{Connection, Result, params};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
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
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct FileRecord {
    path: String,
    size: i64,
    hash: Option<String>,
}

#[derive(Debug)]
struct Config {
    includes: Vec<PathBuf>,
    excludes: Vec<glob::Pattern>,
}

impl Config {
    fn is_included(&self, path: &Path) -> bool {
        let mut is_included = false;
        for inc in self.includes.iter() {
            if path.starts_with(inc) {
                is_included = true;
                break;
            }
        }
        if !is_included {
            return false;
        }

        for exc in self.excludes.iter() {
            if exc.matches_path(path) {
                return false;
            }
        }

        true
    }
}

fn fetch(conn: &mut Connection) -> Vec<FileRecord> {
    let mut stmt = conn
        .prepare("select path, size, hash from files")
        .expect("select all files");

    let existing_files: Vec<FileRecord> = stmt
        .query_map([], |row| {
            Ok(FileRecord {
                path: row.get(0).expect("Should get path"),
                size: row.get(1).expect("should get size"),
                hash: row.get(2).ok(),
            })
        })
        .unwrap()
        .flatten()
        .collect();
    return existing_files;
}

fn add_folder(conn: &mut Connection, config: &Config, path: &Path) -> anyhow::Result<()> {
    let map = {
        let existing_files = fetch(conn);
        let mut map = HashMap::new();
        for record in existing_files.into_iter() {
            map.insert(record.path.clone(), record);
        }
        map
    };

    info!("Found {} files", map.len());

    let mut tx = conn.transaction()?;
    let mut i = 0;

    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let path = entry.path();
            let path_name = path.display().to_string();
            if !map.contains_key(&path_name) && config.is_included(path) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                tx.execute(
                    "INSERT INTO files (path, size) VALUES (?1, ?2)",
                    params![path_name, size as i64],
                )?;
                i += 1;

                if i >= 10_000 {
                    info!("Committing {} items", i);
                    i = 0;
                    tx.commit().unwrap();
                    tx = conn.transaction().unwrap();
                }
            }
        }
    }
    tx.commit().unwrap();
    Ok(())
}

fn main() -> Result<()> {
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

    info!("Group by size");
    let files: Vec<_> = fetch(&mut conn)
        .into_iter()
        .filter(|x| config.is_included(Path::new(&x.path)))
        .sorted_by(|a, b| a.size.cmp(&b.size))
        .collect();

    let mut to_check_hash: Vec<FileRecord> = vec![];

    let mut prev = &files[0];
    for file in files.iter().skip(1) {
        if file.size == prev.size {
            let prev_is_added = to_check_hash
                .last()
                .map(|x| x.path == prev.path)
                .unwrap_or(false);
            if !prev_is_added {
                to_check_hash.push(prev.clone());
            }
            to_check_hash.push(file.clone());
        }
        prev = file;
    }

    info!("Running md5sum on duplicates by size");
    let mut tx = conn.transaction()?;
    let mut i = 0;
    for file_ref in to_check_hash {
        let file_data = file_ref;
        if file_data.hash.is_some() {
            continue;
        }
        if let Ok(hash) = compute_md5(&file_data.path) {
            tx.execute(
                "UPDATE files SET hash = ?1 WHERE path = ?2",
                params![hash, file_data.path],
            )?;
            i += 1;
            if i >= 5_000 {
                info!("Updated {} hash values", i);
                tx.commit()?;
                tx = conn.transaction()?;
                i = 0;
            }
        }
    }
    tx.commit()?;

    info!("Finding duplicates by hash");    
    let files: Vec<_> = fetch(&mut conn)
        .into_iter()
        .filter(|x| config.is_included(Path::new(&x.path)))
        .sorted_by(|a, b| a.size.cmp(&b.size))
        .collect();

    let mut duplicates: Vec<FileRecord> = vec![];

    let mut prev = &files[0];
    for file in files.iter().skip(1) {
        if file.size == prev.size && file.hash == prev.hash {
            let prev_is_added = duplicates
                .last()
                .map(|x| x.path == prev.path)
                .unwrap_or(false);
            if !prev_is_added {
                duplicates.push(prev.clone());
            }
            duplicates.push(file.clone());
        }
        prev = file;
    }

    let mut prev = "".to_string();
    for duplicate in duplicates {
        if duplicate.size < 1000 {
            continue;
        }
        if duplicate.hash.clone().unwrap() != prev {
            println!();
            prev = duplicate.hash.unwrap().clone();
            println!("\nNew file: {} with size {}", &prev, &duplicate.size);
        }
        println!("- {}", duplicate.path);
    }

    Ok(())
}

fn compute_md5(path: &str) -> io::Result<String> {
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

#[cfg(test)]
mod test {
    use std::path::Path;

    #[test]
    fn test_pattern() {
        let pattern = glob::Pattern::new("**/test/**").unwrap();
        let path = Path::new("a/b/c/test/a");
        assert!(pattern.matches_path(path));
    }
}
