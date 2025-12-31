use crate::db::FilesTransaction;
use crate::file_record::FileRecord;
use crate::humanize_bytes;
use anyhow::Result;
use rusqlite::Connection;
use std::cmp::Ordering;
use std::fs::File;
use std::hash::Hasher;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::info;
use twox_hash::XxHash64;

pub fn step1(conn: &mut Connection, mut files: Vec<FileRecord>) -> Result<Vec<FileRecord>> {
    files.retain(|x| x.size > 0);
    files = find_duplicates_by(|a, b| a.size.cmp(&b.size), files)
        .into_iter()
        .flatten()
        .collect();
    let to_check_hash: Vec<_> = files.iter_mut().filter(|x| x.hash_4096.is_none()).collect();
    if to_check_hash.is_empty() {
        return Ok(files);
    }
    info!(
        "[short] Hashing first 4096 bytes of {} files",
        to_check_hash.len()
    );

    fn log(count: i32) {
        info!("[short] Hashed {:>4} files", count);
    }

    let mut tx = FilesTransaction::begin(conn)?;
    let mut time = Instant::now();
    for file in to_check_hash {
        if let Ok(hash) = compute_xxhash_4096(&file.path) {
            file.hash_4096 = Some(hash.clone());
            if file.size <= 4096 {
                file.hash = Some(hash);
            }
            let num_updated = tx.update(file)?;
            if num_updated >= 15_000 || time.elapsed() > Duration::from_secs(5) {
                log(num_updated);
                tx.commit()?;
                tx = FilesTransaction::begin(conn)?;
                time = Instant::now();
            }
        }
    }

    log(tx.num_commands);
    tx.commit()?;

    Ok(files)
}

pub fn step2(conn: &mut Connection, mut files: Vec<FileRecord>) -> Result<Vec<FileRecord>> {
    files.retain(|f| f.hash_4096.is_some());
    files = find_duplicates_by(|a, b| a.hash_4096.cmp(&b.hash_4096), files)
        .into_iter()
        .flatten()
        .collect();
    let mut to_check_hash = vec![];
    let mut file_size_to_hash = 0;
    for file in files.iter_mut() {
        if file.hash.is_none() {
            file_size_to_hash += file.size;
            to_check_hash.push(file);
        }
    }
    if to_check_hash.is_empty() {
        return Ok(files);
    }

    info!(
        "[full] Hashing {} duplicated files. Total size: {}",
        to_check_hash.len(),
        humanize_bytes(file_size_to_hash as f64)
    );

    fn log(count: i32, bytes: i64, duration: Duration) {
        let bytes_per_second = (bytes as f64) / duration.as_secs_f64();
        info!(
            "[full] Hashed {:>4} files, {:>8}, {:>8}/s",
            count,
            humanize_bytes(bytes as f64),
            humanize_bytes(bytes_per_second),
        );
    }

    let mut tx = FilesTransaction::begin(conn)?;
    let mut bytes = 0;
    let mut time = Instant::now();
    for file in to_check_hash {
        if let Ok(hash) = compute_xxhash(&file.path) {
            file.hash = Some(hash);
            let num_updated = tx.update(file)?;
            bytes += file.size;
            if num_updated >= 5_000 || time.elapsed() > Duration::from_secs(5) {
                log(num_updated, bytes, time.elapsed());
                tx.commit()?;
                tx = FilesTransaction::begin(conn)?;
                time = Instant::now();
                bytes = 0;
            }
        }
    }

    log(tx.num_commands, bytes, time.elapsed());
    tx.commit()?;

    Ok(files)
}

pub fn find_duplicates_by<T, F>(cmp: F, mut files: Vec<T>) -> Vec<Vec<T>>
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
fn compute_xxhash_4096(path: &Path) -> anyhow::Result<String> {
    let mut hash = XxHash64::with_seed(0);
    let mut file = File::open(path)?;
    let mut buffer = [0; 4096];

    let count = file.read(&mut buffer)?;
    if count != 0 {
        hash.write(&buffer[..count]);
    }

    Ok(format!("{:x}", hash.finish()))
}

fn compute_xxhash(path: &Path) -> anyhow::Result<String> {
    let mut hash = XxHash64::with_seed(0);
    let mut file = File::open(path)?;
    let mut buffer = [0; 4096];

    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.write(&buffer[..count]);
    }

    Ok(format!("{:x}", hash.finish()))
}

#[cfg(test)]
mod test {
    use super::*;

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
}
