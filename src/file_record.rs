use std::path::PathBuf;

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    pub path: PathBuf,
    pub size: i64,
    pub hash: Option<String>,
    pub hash_4096: Option<String>,
}
