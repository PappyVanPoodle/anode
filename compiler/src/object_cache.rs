//! Shared LRU storage for native objects and baked executables. One total budget.
use crate::{Error, Result};
use std::{
    collections::BTreeMap,
    fs::{self, File, FileTimes, OpenOptions},
    hash::{Hash, Hasher},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn io(e: std::io::Error) -> Error {
    Error::new(0, e.to_string())
}
fn digest(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}
fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}
fn entry(path: &Path) -> bool {
    let object = path.extension().is_some_and(|e| e == "zno")
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    // Interrupted atomic publications still count toward the exact budget.
    let temporary = path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix(".zn-cache-publish-"))
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'-'));
    object || temporary
}
pub struct Cache {
    root: PathBuf,
    limit: u64,
    _lock: File,
    contents: Mutex<Contents>,
}
struct Cached {
    path: PathBuf,
    size: u64,
    age: u64,
}
#[derive(Default)]
struct Contents {
    entries: BTreeMap<String, Cached>,
    pending: Vec<(PathBuf, fs::Metadata)>,
    bytes: u64,
    indexed: bool,
}
impl Contents {
    fn remove(&mut self, key: &str) -> Option<Cached> {
        let old = self.entries.remove(key)?;
        self.bytes = self.bytes.saturating_sub(old.size);
        Some(old)
    }
    fn insert(&mut self, key: String, value: Cached) {
        self.remove(&key);
        self.bytes = self.bytes.saturating_add(value.size);
        self.entries.insert(key, value);
    }
}
impl Cache {
    pub fn open(root: &Path, limit: u64) -> Result<Self> {
        fs::create_dir_all(root).map_err(io)?;
        // Refuse junction/symlink redirection of the managed cache directory.
        let expected = root
            .parent()
            .ok_or_else(|| Error::new(0, "cache has no parent"))?
            .canonicalize()
            .map_err(io)?
            .join(root.file_name().unwrap());
        let root = root.canonicalize().map_err(io)?;
        if root != expected {
            return Err(Error::new(0, "module cache must not be a link or junction"));
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("cache.lock"))
            .map_err(io)?;
        lock.lock().map_err(io)?;
        let marker = root.join("owner");
        const OWNER: &str = "zenith-module-cache-v1\n";
        if marker.exists() {
            if fs::read_to_string(&marker).map_err(io)? != OWNER {
                return Err(Error::new(0, "unrecognized module cache owner"));
            }
        } else {
            fs::write(marker, OWNER).map_err(io)?;
        }
        // Cap-only openings need lengths, but not key strings, LRU timestamps
        // or payloads. Populate the detailed index lazily for real get/put or
        // when this exact (possibly lowered) cap requires eviction.
        let mut contents = Contents::default();
        for item in fs::read_dir(&root).map_err(io)? {
            let item = item.map_err(io)?;
            let path = item.path();
            if !entry(&path) {
                continue;
            }
            if !item.file_type().map_err(io)?.is_file() {
                return Err(Error::new(0, "unexpected module cache entry type"));
            }
            let metadata = item.metadata().map_err(io)?;
            contents.bytes = contents.bytes.saturating_add(metadata.len());
            contents.pending.push((path, metadata));
        }
        let over = contents.bytes > limit.saturating_sub(OWNER.len() as u64);
        let cache = Self {
            root,
            limit: limit.saturating_sub(OWNER.len() as u64),
            _lock: lock,
            contents: Mutex::new(contents),
        };
        if over {
            let mut state = cache.state()?;
            cache.evict(&mut state, 0)?;
        }
        Ok(cache)
    }
    fn state(&self) -> Result<MutexGuard<'_, Contents>> {
        let mut state = self
            .contents
            .lock()
            .map_err(|_| Error::new(0, "module cache index lock poisoned"))?;
        if !state.indexed {
            let mut contents = Contents {
                indexed: true,
                ..Contents::default()
            };
            // The lifetime OS lock excludes compliant writers. Reuse the
            // already validated metadata rather than scanning the directory a
            // second time when this handle first performs a get or put.
            for (path, meta) in std::mem::take(&mut state.pending) {
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos().min(u64::MAX as u128) as u64);
                let mut key = path.file_stem().unwrap().to_str().unwrap().to_owned();
                if cfg!(windows) {
                    key.make_ascii_lowercase();
                }
                contents.insert(
                    key,
                    Cached {
                        path,
                        size: meta.len(),
                        age,
                    },
                );
            }
            *state = contents;
        }
        Ok(state)
    }
    /// Diagnostic only: cap maintenance under budget leaves this false.
    #[doc(hidden)]
    pub fn has_lru_index(&self) -> Result<bool> {
        self.contents
            .lock()
            .map(|state| state.indexed)
            .map_err(|_| Error::new(0, "module cache index lock poisoned"))
    }
    pub fn get(&self, key: u64, destination: &Path) -> Result<bool> {
        let Some(bytes) = self.get_bytes(key)? else {
            return Ok(false);
        };
        fs::write(destination, bytes).map_err(io)?;
        Ok(true)
    }
    pub(crate) fn get_bytes(&self, key: u64) -> Result<Option<Vec<u8>>> {
        let mut state = self.state()?;
        let key = format!("{key:016x}");
        let path = state
            .entries
            .get(&key)
            .map(|e| e.path.clone())
            .unwrap_or_else(|| self.root.join(format!("{key}.zno")));
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                state.remove(&key);
                return Ok(None);
            }
            Err(e) => return Err(io(e)),
        };
        if metadata.file_type().is_symlink() {
            return Err(Error::new(0, "redirected module cache entry"));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(io)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        if bytes.len() < 16
            || u64::from_le_bytes(bytes[..8].try_into().unwrap()) != digest(&bytes[16..])
        {
            drop(file);
            fs::remove_file(path).map_err(io)?;
            state.remove(&key);
            return Ok(None);
        }
        let age = timestamp();
        file.seek(SeekFrom::Start(8)).map_err(io)?;
        file.write_all(&age.to_le_bytes()).map_err(io)?;
        // Windows can coalesce automatic last-write timestamps. Set the exact
        // persisted LRU time so reopening the metadata index preserves order.
        file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_nanos(age)))
            .map_err(io)?;
        state.insert(
            key,
            Cached {
                path,
                size: bytes.len() as u64,
                age,
            },
        );
        self.evict(&mut state, 0)?;
        bytes.drain(..16);
        Ok(Some(bytes))
    }
    pub fn put(&self, key: u64, object: &Path) -> Result<bool> {
        let data = fs::read(object).map_err(io)?;
        self.put_bytes(key, &data)
    }
    pub(crate) fn put_bytes(&self, key: u64, data: &[u8]) -> Result<bool> {
        let needed = data.len() as u64 + 16;
        if needed > self.limit {
            // An object that cannot be retained must not evict useful entries.
            return Ok(false);
        }
        let mut state = self.state()?;
        let key = format!("{key:016x}");
        let replaced = state.entries.get(&key).map_or(0, |old| old.size);
        self.evict_except(&mut state, needed.saturating_sub(replaced), Some(&key))?;
        let path = self.root.join(format!("{key}.zno"));
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temporary = self.root.join(format!(
            ".zn-cache-publish-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let age = timestamp();
        let write = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&digest(data).to_le_bytes())?;
            file.write_all(&age.to_le_bytes())?;
            file.write_all(data)?;
            file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_nanos(age)))?;
            drop(file);
            // Publish the complete checksum/header/payload together. Replacing
            // an entry never truncates through a pre-existing hardlink.
            fs::rename(&temporary, &path)
        })();
        if let Err(e) = write {
            if fs::remove_file(&temporary).is_err() {
                if let Ok(meta) = fs::symlink_metadata(&temporary) {
                    state.insert(
                        temporary
                            .file_stem()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        Cached {
                            path: temporary,
                            size: meta.len(),
                            age: 0,
                        },
                    );
                }
            }
            return Err(io(e));
        }
        state.insert(
            key,
            Cached {
                path,
                size: needed,
                age,
            },
        );
        Ok(true)
    }
    fn evict(&self, state: &mut Contents, needed: u64) -> Result<()> {
        self.evict_except(state, needed, None)
    }
    fn evict_except(
        &self,
        state: &mut Contents,
        needed: u64,
        protected: Option<&str>,
    ) -> Result<()> {
        if state.bytes.saturating_add(needed) <= self.limit {
            return Ok(());
        }
        let mut entries = state
            .entries
            .iter()
            .filter(|(key, _)| Some(key.as_str()) != protected)
            .map(|(key, value)| (value.age, key.clone()))
            .collect::<Vec<_>>();
        entries.sort_by_key(|e| e.0);
        for (_, key) in entries {
            if state.bytes.saturating_add(needed) <= self.limit {
                break;
            }
            let path = &state.entries[&key].path;
            if path.parent() != Some(self.root.as_path()) {
                return Err(Error::new(0, "invalid cache removal path"));
            }
            fs::remove_file(path).map_err(io)?;
            state.remove(&key);
        }
        Ok(())
    }
}
pub fn size(root: &Path) -> u64 {
    fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.metadata().map_or(0, |m| m.len()))
        .sum()
}
