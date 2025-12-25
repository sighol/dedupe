use clap::Parser;
use rusqlite::{Connection, Result, params};
use std::collections::HashMap;
use std::error::Error;
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
    db: Option<PathBuf>,
}

#[allow(dead_code)]
struct FileRecord {
    path: String,
    size: i64,
    hash: Option<String>,
}


fn add_folder(
    conn: &mut Connection,
    path: &Path,
) -> anyhow::Result<()> {
    let mut stmt = conn
        .prepare("select path, size, hash from files where path like (?1 || '%')")
        .expect("select all files");

    let existing_files: Vec<FileRecord> = stmt
        .query_map([path.canonicalize().unwrap().to_str().unwrap()], |row| {
            Ok(FileRecord {
                path: row.get(0).expect("Should get path"),
                size: row.get(1).expect("should get size"),
                hash: row.get(2).ok(),
            })
        })
        .unwrap()
        .flatten()
        .collect();
    let mut map = HashMap::new();
    for record in existing_files.into_iter() {
        map.insert(record.path.clone(), record);
    }
    drop(stmt);

    info!("Found {} files", map.len());


    let mut tx = conn.transaction()?;
    let mut i = 0;

    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let path = entry.path().display().to_string();
            if !map.contains_key(&path) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                tx.execute(
                    "INSERT INTO files (path, size) VALUES (?1, ?2)",
                    params![path, size as i64],
                )?;
                i += 1;

                if i >= 500 {
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
    let path = args.db.unwrap_or(PathBuf::from("files.db"));
    let mut conn = Connection::open(&path)?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (
            path TEXT NOT NULL PRIMARY KEY,
            size INTEGER NOT NULL,
            hash TEXT
        )",
        [],
    )?;

    let folders: Vec<_> = args
        .folders
        .iter()
        .map(|x| x.canonicalize().expect("Canonicalize"))
        .collect();

    for dir in folders {
        add_folder(&mut conn, &dir).expect("Failed to add folder");
    }

    info!("Finding duplicates by size");
    let mut stmt = conn.prepare(
        "SELECT path, size, hash FROM files WHERE size IN (
            SELECT size FROM files GROUP BY size HAVING COUNT(*) > 1
        )",
    )?;

    let candidate_iter: Vec<_> = stmt.query_map([], |row| {
        Ok(FileRecord {
            path: row.get(0)?,
            size: row.get(1)?,
            hash: row.get(2).ok(),
        })
    })?.collect();
    drop(stmt);

    info!("Running md5sum on duplicates by size");
    let mut tx = conn.transaction()?;
    let mut i = 0;
    for file_ref in candidate_iter {
        let file_data = file_ref?;
        if file_data.hash.is_some() {
            continue;
        }
        if let Ok(hash) = compute_md5(&file_data.path) {
            tx.execute(
                "UPDATE files SET hash = ?1 WHERE path = ?2",
                params![hash, file_data.path],
            )?;
            i += 1;
            if i > 500 {
                info!("Updated {} hash values", i);
                tx.commit()?;
                tx = conn.transaction()?;
                i = 0;
            }
        }
    }
    tx.commit()?;

    println!("\n--- Duplicate Files Found ---");
    let mut stmt = conn.prepare(
        "SELECT path, size, hash FROM files WHERE hash IS NOT NULL AND hash IN (
            SELECT hash FROM files WHERE hash IS NOT NULL GROUP BY hash HAVING COUNT(*) > 1
        ) ORDER BY hash",
    )?;

    let mut rows = stmt.query([])?;
    let mut prev = "".to_string();
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        let size: i64 = row.get(1)?;
        let hash: String = row.get(2)?;
        if hash != prev {
            println!();
            prev = hash.clone();
            println!("\nNew file: {} with size {}", &prev, &size);
        }
        println!("- {}", path);
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
