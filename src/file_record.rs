use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    pub path: PathBuf,
    pub size: i64,
    pub hash: Option<String>,
    pub hash_4096: Option<String>,
}

impl FileRecord {
    pub fn path_name(&self) -> String {
        self.path.display().to_string()
    }
}
