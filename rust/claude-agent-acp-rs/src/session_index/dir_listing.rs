//! File system reads of the session index that stay cheap with thousands of
//! transcripts (upstream `session-index/dir-listing.ts`): bounded batches of
//! stats, and directory listings that are read again only when the directory
//! changed.
//!
//! Upstream stats through the libuv thread pool with at most
//! [`STAT_CONCURRENCY`] in flight; the Rust index runs on its own worker thread,
//! so [`stat_files`] walks the paths in bounded batches of that size.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Stats in flight at once, across all callers of [`stat_files`].
pub const STAT_CONCURRENCY: usize = 512;

/// A listing read this soon after the directory changed is not trusted: some
/// file systems keep the mtime in whole seconds, so another change in the same
/// second would leave it unchanged.
const MTIME_RESOLUTION_MS: f64 = 2000.0;

/// The facts of a stat the index uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StatInfo {
    pub is_file: bool,
    pub is_dir: bool,
    pub size: u64,
    /// Epoch milliseconds, with the sub-millisecond part.
    pub mtime_ms: f64,
    /// Tells a file replaced by another apart from one that grew. The standard
    /// library has no portable inode; the creation time stands in for it.
    pub identity: u64,
}

fn system_time_ms(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64() * 1000.0,
        Err(e) => -(e.duration().as_secs_f64() * 1000.0),
    }
}

/// `fs.stat` of `path`; `None` when it is gone.
pub fn stat(path: &Path) -> Option<StatInfo> {
    let meta = std::fs::metadata(path).ok()?;
    Some(StatInfo {
        is_file: meta.is_file(),
        is_dir: meta.is_dir(),
        size: meta.len(),
        mtime_ms: meta.modified().map(system_time_ms).unwrap_or(0.0),
        identity: meta
            .created()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64),
    })
}

/// The stats of `paths`, in order (`None` for one that is gone), in batches of
/// [`STAT_CONCURRENCY`].
pub fn stat_files<P: AsRef<Path>>(paths: &[P]) -> Vec<Option<StatInfo>> {
    let mut out = Vec::with_capacity(paths.len());
    for batch in paths.chunks(STAT_CONCURRENCY) {
        out.extend(batch.iter().map(|path| stat(path.as_ref())));
    }
    out
}

#[derive(Clone, Debug)]
struct Listing {
    mtime_ms: f64,
    /// A directory replaced by another one with the same mtime differs here.
    identity: u64,
    /// When the listing was read, epoch ms.
    read_at: f64,
    names: Vec<String>,
}

/// Directory listings, read again only when the directory's mtime changed (an
/// entry was added, removed or renamed; an append to a file does not change it).
/// A project directory with thousands of transcripts is then one stat instead of
/// one `readdir` per list or mutation.
#[derive(Default)]
pub struct DirListings {
    listings: HashMap<PathBuf, Listing>,
    /// Reads of a directory that missed the cache, for tests and diagnostics.
    reads: usize,
}

fn now_ms() -> f64 {
    system_time_ms(SystemTime::now())
}

impl DirListings {
    /// How many times a directory was actually read.
    pub fn reads(&self) -> usize {
        self.reads
    }

    /// The entry names of `dir`, empty when it cannot be read.
    pub fn names(&mut self, dir: &Path) -> Vec<String> {
        self.listing(dir, stat(dir))
    }

    /// The entry names of each of `dirs`, one bounded batch of stats.
    pub fn names_of_all(&mut self, dirs: &[PathBuf]) -> Vec<Vec<String>> {
        let stats = stat_files(dirs);
        dirs.iter()
            .zip(stats)
            .map(|(dir, stats)| self.listing(dir, stats))
            .collect()
    }

    fn listing(&mut self, dir: &Path, stats: Option<StatInfo>) -> Vec<String> {
        let Some(stats) = stats.filter(|s| s.is_dir) else {
            self.listings.remove(dir);
            return Vec::new();
        };
        if let Some(cached) = self.listings.get(dir) {
            if cached.mtime_ms == stats.mtime_ms
                && cached.identity == stats.identity
                && cached.read_at - cached.mtime_ms > MTIME_RESOLUTION_MS
            {
                return cached.names.clone();
            }
        }
        let read_at = now_ms();
        self.reads += 1;
        let Ok(read) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let names: Vec<String> = read
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        self.listings.insert(
            dir.to_path_buf(),
            Listing {
                mtime_ms: stats.mtime_ms,
                identity: stats.identity,
                read_at,
                names: names.clone(),
            },
        );
        names
    }
}

/// A small LRU: insertion order is recency, a hit is re-inserted. Every
/// operation is O(1) amortised (a map of generations plus an ordered index),
/// like the JS `Map` upstream uses for the 50k-entry metadata cache: a linear
/// scan per hit made a cold list quadratic (review T2B item 4).
pub struct Lru<K, V> {
    entries: HashMap<K, V>,
    /// The generation of each key in `order`; re-inserting a key gets a new one.
    generations: HashMap<K, u64>,
    /// `generation -> key`, oldest first.
    order: std::collections::BTreeMap<u64, K>,
    next_generation: u64,
    /// Order entries looked at, for tests: O(1) per operation (the linear
    /// version counted a scan of up to `len`).
    inspections: usize,
    capacity: usize,
}

impl<K: Clone + Eq + std::hash::Hash, V> Lru<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            generations: HashMap::new(),
            order: std::collections::BTreeMap::new(),
            next_generation: 0,
            inspections: 0,
            capacity,
        }
    }

    fn touch(&mut self, key: &K) {
        self.inspections += 1;
        if let Some(previous) = self.generations.insert(key.clone(), self.next_generation) {
            self.inspections += 1;
            self.order.remove(&previous);
        }
        self.order.insert(self.next_generation, key.clone());
        self.next_generation += 1;
    }

    /// The value of `key`, made the most recent.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        if self.entries.contains_key(key) {
            self.touch(key);
        }
        self.entries.get(key)
    }

    /// The value of `key`, without making it recent.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }

    pub fn set(&mut self, key: K, value: V) {
        self.touch(&key);
        self.entries.insert(key, value);
        while self.entries.len() > self.capacity {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.entries.remove(&oldest);
            self.generations.remove(&oldest);
        }
    }

    pub fn delete(&mut self, key: &K) -> Option<V> {
        if let Some(generation) = self.generations.remove(key) {
            self.order.remove(&generation);
        }
        self.entries.remove(key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acp-t2b-dl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn age(dir: &Path, seconds: u64) {
        let file = std::fs::File::open(dir).expect("open dir");
        let old = SystemTime::now() - Duration::from_secs(seconds);
        file.set_modified(old).expect("set mtime");
    }

    /// directory listings and stats > re-reads a listing when an entry is added or removed, not otherwise
    #[test]
    fn rereads_a_listing_only_when_an_entry_changes() {
        let dir = temp_dir("reread");
        std::fs::write(dir.join("a"), b"1").expect("write");
        age(&dir, 3600);
        let mut listings = DirListings::default();
        assert_eq!(listings.names(&dir), vec!["a".to_string()]);
        assert_eq!(listings.names(&dir), vec!["a".to_string()]);
        assert_eq!(
            listings.reads(),
            1,
            "an unchanged directory is not read again"
        );
        // An append to a file does not change the directory.
        std::fs::write(dir.join("a"), b"22").expect("write");
        assert_eq!(listings.names(&dir).len(), 1);
        assert_eq!(listings.reads(), 1);
        std::fs::write(dir.join("b"), b"1").expect("write");
        age(&dir, 3000);
        let mut names = listings.names(&dir);
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(listings.reads(), 2);
        // A listing read right after the directory changed is not trusted.
        let fresh = temp_dir("fresh");
        std::fs::write(fresh.join("x"), b"1").expect("write");
        let mut quick = DirListings::default();
        quick.names(&fresh);
        quick.names(&fresh);
        assert_eq!(quick.reads(), 2);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh);
    }

    /// directory listings and stats > re-reads a directory replaced by another with the same mtime
    #[test]
    fn rereads_a_replaced_directory_with_the_same_mtime() {
        let dir = temp_dir("replace");
        std::fs::write(dir.join("a"), b"1").expect("write");
        age(&dir, 3600);
        let mut listings = DirListings::default();
        assert_eq!(listings.names(&dir), vec!["a".to_string()]);
        let mtime = stat(&dir).map(|s| s.mtime_ms);
        std::fs::remove_dir_all(&dir).expect("remove");
        std::thread::sleep(Duration::from_millis(30));
        std::fs::create_dir_all(&dir).expect("create");
        std::fs::write(dir.join("z"), b"1").expect("write");
        let file = std::fs::File::open(&dir).expect("open");
        file.set_modified(UNIX_EPOCH + Duration::from_secs_f64(mtime.unwrap_or(0.0) / 1000.0))
            .expect("mtime");
        assert_eq!(listings.names(&dir), vec!["z".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// directory listings and stats > stats many files in order, bounded, and reports missing ones
    #[test]
    fn stats_many_files_in_order_and_reports_missing_ones() {
        let dir = temp_dir("stats");
        let mut paths = Vec::new();
        for i in 0..(STAT_CONCURRENCY + 40) {
            let path = dir.join(format!("f{i}"));
            if i % 7 != 0 {
                std::fs::write(&path, vec![b'x'; i % 5 + 1]).expect("write");
            }
            paths.push(path);
        }
        let stats = stat_files(&paths);
        assert_eq!(stats.len(), paths.len());
        for (i, stat) in stats.iter().enumerate() {
            match (i % 7 == 0, stat) {
                (true, None) => {}
                (false, Some(s)) => assert_eq!(s.size, (i % 5 + 1) as u64),
                other => panic!("unexpected stat at {i}: {other:?}"),
            }
        }
        assert!(stat_files::<PathBuf>(&[]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lru_drops_the_least_recently_used() {
        let mut lru = Lru::new(2);
        lru.set("a", 1);
        lru.set("b", 2);
        assert_eq!(lru.get(&"a"), Some(&1));
        lru.set("c", 3);
        assert_eq!(lru.peek(&"b"), None);
        assert_eq!(lru.peek(&"a"), Some(&1));
        assert_eq!(lru.len(), 2);
    }

    /// directory listings and stats > the LRU does O(1) work per operation at 20k entries
    #[test]
    fn the_lru_is_o1_at_20k_entries() {
        const N: usize = 20_000;
        let mut lru = Lru::new(N);
        for i in 0..N {
            lru.set(format!("k{i}"), i);
        }
        assert_eq!(lru.len(), N);
        for i in 0..N {
            assert_eq!(lru.get(&format!("k{i}")), Some(&i));
        }
        // The linear version scanned the whole order on every set and hit:
        // ~2 * 20k * 20k path comparisons. O(1) touches at most two entries.
        assert!(
            lru.inspections <= 4 * N,
            "{} order entries inspected for {N} entries",
            lru.inspections
        );
        // Recency still decides the eviction.
        lru.set("extra".into(), N);
        assert_eq!(lru.len(), N);
        assert_eq!(lru.peek(&"k0".into()), None);
        assert_eq!(lru.peek(&"k1".into()), Some(&1));
        assert_eq!(lru.delete(&"k1".into()), Some(1));
        assert_eq!(lru.peek(&"k1".into()), None);
    }
}
