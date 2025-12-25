use rusqlite::{params, Connection, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use walkdir::WalkDir;
use tracing::info;

#[allow(dead_code)]
struct FileRecord {
    path: String,
    size: i64,
    hash: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let conn = Connection::open("files.db")?;

    // Create the schema
    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (
            path TEXT NOT NULL PRIMARY KEY,
            size INTEGER NOT NULL,
            hash TEXT
        )",
        [],
    )?;

    // Step 1: Recursively scan folders
    let target_dirs = vec!["."]; // Add your paths here
    println!("Scanning folders...");

    let mut stmt = conn.prepare(
        "select path, size, hash from files"
    ).expect("select all files");

    let existing_files: Vec<FileRecord> = stmt.query_map([], |row| {
        Ok(FileRecord {
            path: row.get(0).expect("Should get path"),
            size: row.get(1).expect("should get size"),
            hash: row.get(2).ok(),
        })
    }).unwrap().flatten().collect();
    let mut map = HashMap::new();
    for record in existing_files.into_iter() {
        map.insert(record.path.clone(), record);
    }

    for dir in target_dirs {
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let path = entry.path().display().to_string();
                if !map.contains_key(&path) {
                    info!("Adding {}", &path);
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    conn.execute(
                        "INSERT INTO files (path, size) VALUES (?1, ?2)",
                        params![path, size as i64],
                    )?;
                }
            }
        }
    }

    // Step 2: Find files with duplicate sizes and compute hashes
    println!("Computing hashes for potential duplicates...");

    let mut stmt = conn.prepare(
        "SELECT path, size, hash FROM files WHERE size IN (
            SELECT size FROM files GROUP BY size HAVING COUNT(*) > 1
        )"
    )?;

    let candidate_iter = stmt.query_map([], |row| {
        Ok(FileRecord {
            path: row.get(0)?,
            size: row.get(1)?,
            hash: row.get(2).ok(),
        })
    })?;

    for file_ref in candidate_iter {
        let file_data = file_ref?;
        if file_data.hash.is_some() {
            continue
        }
        if let Ok(hash) = compute_md5(&file_data.path) {
            conn.execute(
                "UPDATE files SET hash = ?1 WHERE path = ?2",
                params![hash, file_data.path],
            )?;
        }
    }

    // Step 3: Final report
    println!("\n--- Duplicate Files Found ---");
    let mut stmt = conn.prepare(
        "SELECT path, size, hash FROM files WHERE hash IS NOT NULL AND hash IN (
            SELECT hash FROM files WHERE hash IS NOT NULL GROUP BY hash HAVING COUNT(*) > 1
        ) ORDER BY hash"
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
        if count == 0 { break; }
        context.consume(&buffer[..count]);
    }

    Ok(format!("{:x}", context.finalize()))
}
