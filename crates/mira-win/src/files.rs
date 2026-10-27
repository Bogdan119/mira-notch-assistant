//! Folder context and grep-style content search.
//!
//! A folder dropped on Mira (or the robot dropped on a folder) is walked once
//! into a list of file shortcuts. Searching runs on worker threads over the
//! same tree and streams hits back to the UI thread, which only renders them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Folders that are never worth walking: VCS metadata, dependency caches and
/// build output.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "$RECYCLE.BIN",
    "System Volume Information",
    "__pycache__",
    ".venv",
    "venv",
    ".idea",
    ".vs",
];

/// Files larger than this are skipped when searching contents.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_HITS: usize = 400;

pub struct FolderContext {
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
    /// True when the walk stopped at `limit` files.
    pub truncated: bool,
}

/// Walk `root` breadth-first, collecting up to `limit` files.
pub fn walk(root: &Path, limit: usize) -> FolderContext {
    let mut files = Vec::new();
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut truncated = false;
    'outer: while let Some(dir) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut subdirs = Vec::new();
        let mut here = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !SKIP_DIRS.contains(&name.as_ref()) && !name.starts_with('.') {
                    subdirs.push(entry.path());
                }
            } else if kind.is_file() {
                here.push(entry.path());
            }
        }
        here.sort();
        subdirs.sort();
        for f in here {
            if files.len() >= limit {
                truncated = true;
                break 'outer;
            }
            files.push(f);
        }
        queue.extend(subdirs);
    }
    FolderContext {
        root: root.to_path_buf(),
        files,
        truncated,
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: PathBuf,
    /// 1-based line, or 0 for a file-name match.
    pub line: usize,
    pub text: String,
}

/// A running (or finished) search. Dropping it cancels the workers.
pub struct Search {
    pub hits: Arc<Mutex<Vec<Hit>>>,
    pub scanned: Arc<AtomicUsize>,
    /// Files to scan; known once the walk finishes (0 until then).
    pub total: Arc<AtomicUsize>,
    pub done: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Search `root` (case-insensitive) for `query` in file names and contents.
/// Everything — including walking the tree — runs off the calling thread.
/// `notify` is called (from a worker) at most every ~80 ms while hits arrive,
/// and once when finished.
pub fn search(root: &Path, query: &str, notify: impl Fn() + Send + Sync + 'static) -> Search {
    let job = Search {
        hits: Arc::new(Mutex::new(Vec::new())),
        scanned: Arc::new(AtomicUsize::new(0)),
        total: Arc::new(AtomicUsize::new(0)),
        done: Arc::new(AtomicBool::new(false)),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let needle: Vec<u8> = query.to_lowercase().into_bytes();
    let (root, hits, scanned, total, done, cancel) = (
        root.to_path_buf(),
        job.hits.clone(),
        job.scanned.clone(),
        job.total.clone(),
        job.done.clone(),
        job.cancel.clone(),
    );
    let notify = Arc::new(notify);
    std::thread::spawn(move || {
        if needle.is_empty() {
            done.store(true, Ordering::Relaxed);
            notify();
            return;
        }
        let files = Arc::new(walk(&root, 200_000).files);
        total.store(files.len(), Ordering::Relaxed);
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 12);
        let next = AtomicUsize::new(0);
        let last_notify = Mutex::new(Instant::now());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    let mut buf = Vec::new();
                    while !cancel.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = files.get(i) else {
                            break;
                        };
                        let found = search_file(path, &needle, &mut buf);
                        scanned.fetch_add(1, Ordering::Relaxed);
                        if found.is_empty() {
                            continue;
                        }
                        let mut all = hits.lock().unwrap();
                        let room = MAX_HITS.saturating_sub(all.len());
                        all.extend(found.into_iter().take(room));
                        if all.len() >= MAX_HITS {
                            cancel.store(true, Ordering::Relaxed);
                        }
                        drop(all);
                        let mut last = last_notify.lock().unwrap();
                        if last.elapsed() > Duration::from_millis(80) {
                            *last = Instant::now();
                            notify();
                        }
                    }
                });
            }
        });
        done.store(true, Ordering::Relaxed);
        notify();
    });
    job
}

fn search_file(path: &Path, needle: &[u8], buf: &mut Vec<u8>) -> Vec<Hit> {
    let mut out = Vec::new();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if contains(name.as_bytes(), needle) {
        out.push(Hit {
            path: path.to_path_buf(),
            line: 0,
            text: String::new(),
        });
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return out;
    };
    if meta.len() == 0 || meta.len() > MAX_FILE_BYTES {
        return out;
    }
    buf.clear();
    let Ok(mut f) = std::fs::File::open(path) else {
        return out;
    };
    if std::io::Read::read_to_end(&mut f, buf).is_err() {
        return out;
    }
    // Binary files (NUL in the first 8 KiB) are skipped, like grep -I.
    if buf[..buf.len().min(8192)].contains(&0) {
        return out;
    }
    buf.make_ascii_lowercase();
    for (i, line) in buf.split(|b| *b == b'\n').enumerate() {
        if contains(line, needle) {
            // Snippets come from the original-case text, see fill_snippets.
            out.push(Hit {
                path: path.to_path_buf(),
                line: i + 1,
                text: String::new(),
            });
            if out.len() >= 20 {
                break;
            }
        }
    }
    if out.iter().any(|h| h.line > 0) {
        fill_snippets(path, &mut out);
    }
    out
}

/// Fill hit snippets from the file's original (not lower-cased) text.
fn fill_snippets(path: &Path, hits: &mut [Hit]) {
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.split('\n').collect();
    for h in hits.iter_mut().filter(|h| h.line > 0) {
        if let Some(l) = lines.get(h.line - 1) {
            let t = l.trim();
            h.text = t.chars().take(180).collect();
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return needle.is_empty();
    }
    let first = needle[0];
    let last = hay.len() - needle.len();
    let mut i = 0;
    while i <= last {
        match hay[i..=last].iter().position(|b| *b == first) {
            Some(p) => {
                i += p;
                if &hay[i..i + needle.len()] == needle {
                    return true;
                }
                i += 1;
            }
            None => return false,
        }
    }
    false
}

/// Path of `p` relative to `root`, for display.
pub fn relative(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_search() {
        assert!(contains(b"hello world", b"o w"));
        assert!(contains(b"abc", b"c"));
        assert!(!contains(b"abc", b"abcd"));
        assert!(!contains(b"aaab", b"ac"));
    }

    #[test]
    fn walks_and_searches_a_tree() {
        let dir = std::env::temp_dir().join(format!("mira-files-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("a.txt"), "first line\nNeedle here\n").unwrap();
        std::fs::write(dir.join("sub").join("needle.md"), "nothing").unwrap();
        std::fs::write(dir.join("node_modules").join("x.txt"), "needle").unwrap();
        std::fs::write(dir.join("bin.dat"), b"needle\0\0").unwrap();

        let ctx = walk(&dir, 100);
        assert_eq!(ctx.files.len(), 3); // node_modules skipped

        let s = search(&dir, "NEEDLE", || {});
        let deadline = Instant::now() + Duration::from_secs(5);
        while !s.done.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let hits = s.hits.lock().unwrap().clone();
        assert!(hits.iter().any(|h| h.line == 2 && h.text == "Needle here"));
        assert!(hits
            .iter()
            .any(|h| h.line == 0 && h.path.ends_with("needle.md")));
        assert!(!hits.iter().any(|h| h.path.ends_with("bin.dat")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
