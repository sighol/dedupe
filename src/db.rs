use crate::file_record::FileRecord;
use anyhow::Result;
use rusqlite::Connection;
use rusqlite::Transaction;
use rusqlite::params;
use std::path::Path;

pub fn setup(conn: &mut Connection) -> Result<()> {
    conn.execute("pragma synchronous = off; ", [])?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (
            path TEXT NOT NULL PRIMARY KEY,
            size INTEGER NOT NULL,
            hash_4096 TEXT,
            hash TEXT
        )",
        [],
    )?;

    Ok(())
}

pub struct FilesTransaction<'a> {
    tx: Transaction<'a>,
    pub num_commands: i32,
}

pub fn fetch(conn: &mut Connection, path: &Path) -> Vec<FileRecord> {
    let mut stmt = conn
        .prepare("select path, size, hash_4096, hash from files where path LIKE (?1 || '/%')")
        .expect("select all files");

    let existing_files: Vec<FileRecord> = stmt
        .query_map([path.display().to_string()], |row| {
            Ok(FileRecord {
                path: row.get::<_, String>(0).expect("Should get path").into(),
                size: row.get(1).expect("should get size"),
                hash_4096: row.get(2).ok(),
                hash: row.get(3).ok(),
            })
        })
        .unwrap()
        .flatten()
        .collect();
    existing_files
}

impl FilesTransaction<'_> {
    pub fn begin<'a>(conn: &'a mut Connection) -> Result<FilesTransaction<'a>> {
        let tx: Transaction<'a> = conn.transaction()?;
        Ok(FilesTransaction {
            tx,
            num_commands: 0,
        })
    }

    pub fn insert(&mut self, file: &FileRecord) -> anyhow::Result<i32> {
        self.tx.execute(
            "INSERT INTO files (path, size) VALUES (?1, ?2)",
            params![file.path_name(), file.size as i64],
        )?;
        self.num_commands += 1;
        Ok(self.num_commands)
    }

    pub fn update(&mut self, file: &FileRecord) -> anyhow::Result<i32> {
        self.tx.execute(
            "UPDATE files SET hash_4096 = ?1, hash = ?2 WHERE path = ?3",
            params![file.hash_4096, file.hash, file.path_name()],
        )?;
        self.num_commands += 1;
        Ok(self.num_commands)
    }

    pub fn commit(self) -> anyhow::Result<()> {
        self.tx.commit()?;
        Ok(())
    }
}
