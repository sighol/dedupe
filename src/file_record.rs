#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    pub path: String,
    pub size: i64,
    pub hash: Option<String>,
}
