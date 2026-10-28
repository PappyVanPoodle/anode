pub mod agent;
pub mod backend;
pub mod baking;
pub mod cgen;
pub mod checker;
mod coff_archive;
pub mod effects;
pub mod fast;
pub mod fmt;
mod guard_facts;
pub mod holes;
pub mod hot_reload;
mod immutable_descriptors;
mod int_ranges;
pub mod ir;
pub mod lib_export;
mod methods;
pub mod model;
mod native_units;
pub mod navigation;
pub mod numeric;
pub mod object_cache;
mod predicates;
pub mod project;
pub mod storage;
pub mod syntax;
pub mod types;
pub mod verify;
pub mod views;

#[derive(Debug, Clone)]
pub struct Error {
    pub line: usize,
    pub message: String,
    pub path: Option<std::path::PathBuf>,
    pub details: Option<serde_json::Value>,
}
impl Error {
    pub fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
            path: None,
            details: None,
        }
    }
    pub fn in_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "{}:{}: {}", path.display(), self.line, self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
pub fn compile(source: &str) -> Result<String> {
    Ok(project::Project::from_source(source)?.compile(false)?.0)
}
