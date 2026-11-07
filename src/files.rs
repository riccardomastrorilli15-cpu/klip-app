//! The "Files" tab: find files, folders and apps by name. Runs only inside the picker
//! process, only while the window is open, and only reads directory listings.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_HITS: usize = 80;
const MAX_VISITED: usize = 150_000;
const BUDGET: Duration = Duration::from_millis(2500);
const SKIP: &[&str] = &[
    "node_modules",
    "target",
    "appdata",
    "library",
    "$recycle.bin",
    "system volume information",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub name: String,
    pub dir: bool,
    pub app: bool,
}

pub struct Root {
    pub path: PathBuf,
    pub app: bool,
    pub depth: usize,
}

pub fn roots() -> Vec<Root> {
    let mut r = Vec::new();
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    #[cfg(windows)]
    {
        for k in ["APPDATA", "ProgramData"] {
            if let Some(p) = env(k) {
                r.push(Root {
                    path: p.join(r"Microsoft\Windows\Start Menu\Programs"),
                    app: true,
                    depth: 3,
                });
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for p in ["/Applications", "/System/Applications"] {
            r.push(Root {
                path: p.into(),
                app: true,
                depth: 2,
            });
        }
    }
    if let Some(home) = env("USERPROFILE").or_else(|| env("HOME")) {
        #[cfg(target_os = "macos")]
        r.push(Root {
            path: home.join("Applications"),
            app: true,
            depth: 2,
        });
        for d in [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Music",
            "Videos",
            "Movies",
        ] {
            r.push(Root {
                path: home.join(d),
                app: false,
                depth: 6,
            });
        }
    }
    r.retain(|x| x.path.is_dir());
    r
}

/// Breadth-first search, shallow matches first. `cancelled` is polled often; `emit`
/// returns false to stop early.
pub fn search(
    roots: &[Root],
    query: &str,
    cancelled: impl Fn() -> bool,
    mut emit: impl FnMut(Hit) -> bool,
) {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return;
    }
    let start = Instant::now();
    let (mut visited, mut hits) = (0usize, 0usize);
    for root in roots {
        let mut queue = VecDeque::from([(root.path.clone(), 0usize)]);
        while let Some((dir, depth)) = queue.pop_front() {
            if cancelled() || start.elapsed() > BUDGET || visited > MAX_VISITED {
                return;
            }
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for entry in rd.flatten() {
                visited += 1;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || SKIP.contains(&name.to_lowercase().as_str()) {
                    continue;
                }
                let Ok(ft) = entry.file_type() else { continue };
                let is_dir = ft.is_dir(); // symlinks and junctions report false: no loops
                let lower = name.to_lowercase();
                if terms.iter().all(|t| lower.contains(t.as_str())) {
                    let hit = Hit {
                        path: entry.path(),
                        name: name.clone(),
                        dir: is_dir && !(root.app && name.ends_with(".app")),
                        app: root.app && !is_dir || (root.app && name.ends_with(".app")),
                    };
                    hits += 1;
                    if !emit(hit) || hits >= MAX_HITS {
                        return;
                    }
                }
                let is_bundle = name.ends_with(".app");
                if is_dir && !is_bundle && depth < root.depth {
                    queue.push_back((entry.path(), depth + 1));
                }
            }
        }
    }
}

fn spawn_quiet(mut cmd: Command) {
    // Null stdio so the launched program never holds the picker's pipes open.
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

pub fn open(path: &Path) {
    #[cfg(windows)]
    spawn_quiet({
        let mut c = Command::new("explorer");
        c.arg(path);
        c
    });
    #[cfg(target_os = "macos")]
    spawn_quiet({
        let mut c = Command::new("open");
        c.arg(path);
        c
    });
    #[cfg(all(unix, not(target_os = "macos")))]
    spawn_quiet({
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    });
}

pub fn reveal(path: &Path) {
    #[cfg(windows)]
    spawn_quiet({
        let mut c = Command::new("explorer");
        c.arg(format!("/select,{}", path.display()));
        c
    });
    #[cfg(target_os = "macos")]
    spawn_quiet({
        let mut c = Command::new("open");
        c.arg("-R").arg(path);
        c
    });
    #[cfg(all(unix, not(target_os = "macos")))]
    spawn_quiet({
        let mut c = Command::new("xdg-open");
        c.arg(path.parent().unwrap_or(path));
        c
    });
}

/// Parent folder for display, with the home directory shortened to `~`.
pub fn parent_label(path: &Path) -> String {
    let parent = path.parent().unwrap_or(path);
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home.as_ref().and_then(|h| parent.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => parent.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let base = std::env::temp_dir().join(format!("klip-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("docs/deep/er")).unwrap();
        fs::create_dir_all(base.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(base.join(".hidden")).unwrap();
        for f in [
            "docs/Report Q3.txt",
            "docs/deep/report-old.txt",
            "docs/deep/er/notes.md",
            "node_modules/pkg/report.js",
            ".hidden/report.txt",
            "top.txt",
        ] {
            fs::write(base.join(f), "x").unwrap();
        }
        base
    }

    fn run(base: &Path, q: &str) -> Vec<String> {
        let roots = [Root {
            path: base.to_path_buf(),
            app: false,
            depth: 6,
        }];
        let mut out = Vec::new();
        search(
            &roots,
            q,
            || false,
            |h| {
                out.push(h.name);
                true
            },
        );
        out
    }

    #[test]
    fn finds_case_insensitive_multi_term_and_shallow_first() {
        let base = tree();
        assert_eq!(run(&base, "report"), ["Report Q3.txt", "report-old.txt"]);
        assert_eq!(run(&base, "REPORT q3"), ["Report Q3.txt"]);
        assert_eq!(run(&base, "notes"), ["notes.md"]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn skips_hidden_and_vendor_dirs_and_empty_query() {
        let base = tree();
        assert!(!run(&base, "report").iter().any(|n| n == "report.js"));
        assert!(run(&base, "   ").is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn honours_cancellation_and_depth() {
        let base = tree();
        let roots = [Root {
            path: base.clone(),
            app: false,
            depth: 1,
        }];
        let mut names = Vec::new();
        search(
            &roots,
            "notes",
            || false,
            |h| {
                names.push(h.name);
                true
            },
        );
        assert!(names.is_empty(), "notes.md is deeper than depth 1");
        let mut n = 0;
        search(
            &roots,
            "t",
            || true,
            |_| {
                n += 1;
                true
            },
        );
        assert_eq!(n, 0);
        let _ = fs::remove_dir_all(&base);
    }
}
