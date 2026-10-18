use ignore::WalkBuilder;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};
use tauri::{Emitter, Manager, State};
use thiserror::Error;
mod symbols;
use notify::{EventKind, RecursiveMode, Watcher};
use std::process::Command;
use std::str::FromStr;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

const MAX_FILE_BYTES: u64 = 1_000_000;
const MAX_RESULTS: usize = 250;

#[derive(Debug, Error)]
enum IndexError {
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("File operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid project folder: {0}")]
    Invalid(String),
}

struct Store(Arc<Mutex<Connection>>);
#[derive(Clone)]
struct IndexObserver(std::sync::mpsc::Sender<WatchMessage>);
#[derive(Clone)]
enum WatchMessage {
    Watch(PathBuf),
    Unwatch(PathBuf),
    Changed(PathBuf),
    Recover,
}

fn record_watcher_error(db: &Arc<Mutex<Connection>>, error: Option<&str>) {
    if let Ok(connection) = db.lock() {
        match error {
            Some(message) => {
                let _=connection.execute("INSERT INTO settings(key,value) VALUES('watcher_error',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[message]);
            }
            None => {
                let _ = connection.execute("DELETE FROM settings WHERE key='watcher_error'", []);
            }
        }
    }
}

fn start_observer(
    db: Arc<Mutex<Connection>>,
    app: tauri::AppHandle,
    initial: Vec<PathBuf>,
) -> IndexObserver {
    let (request_tx, request_rx) = std::sync::mpsc::channel::<WatchMessage>();
    std::thread::spawn(move || {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(
            move |result: Result<notify::Event, notify::Error>| match result {
                Ok(event)
                    if matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) =>
                {
                    for path in event.paths {
                        let _ = event_tx.send(WatchMessage::Changed(path));
                    }
                }
                Ok(event) if matches!(event.kind, EventKind::Other) => {
                    let _ = event_tx.send(WatchMessage::Recover);
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = event_tx.send(WatchMessage::Recover);
                }
            },
        ) {
            Ok(watcher) => watcher,
            Err(error) => {
                eprintln!("File watcher unavailable: {error}");
                let message = format!("Automatic index updates are unavailable: {error}");
                record_watcher_error(&db, Some(&message));
                let _ = app.emit("index-error", message);
                return;
            }
        };
        record_watcher_error(&db, None);
        let mut roots = HashSet::<PathBuf>::new();
        for root in initial {
            if roots.insert(root.clone()) {
                if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
                    eprintln!("Could not watch {}: {error}", root.display());
                    let message = format!("Could not watch {}: {error}", root.display());
                    record_watcher_error(&db, Some(&message));
                    let _ = app.emit("index-error", message);
                }
            }
        }
        loop {
            while let Ok(message) = request_rx.try_recv() {
                if let WatchMessage::Watch(root) = message {
                    if roots.insert(root.clone()) {
                        if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
                            let message = format!("Could not watch {}: {error}", root.display());
                            record_watcher_error(&db, Some(&message));
                            let _ = app.emit("index-error", message);
                        }
                    }
                } else if let WatchMessage::Unwatch(root) = message {
                    if roots.remove(&root) {
                        let _ = watcher.unwatch(&root);
                    }
                }
            }
            let first = match event_rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(e) => e,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            let mut changed = HashSet::new();
            let mut recover = matches!(first, WatchMessage::Recover);
            if let WatchMessage::Changed(path) = first {
                changed.insert(path);
            }
            while let Ok(event) = event_rx.try_recv() {
                match event {
                    WatchMessage::Changed(path) => {
                        changed.insert(path);
                    }
                    WatchMessage::Recover => recover = true,
                    WatchMessage::Watch(_) => {}
                    WatchMessage::Unwatch(_) => {}
                }
            }
            if let Ok(connection) = db.lock() {
                if let Ok(projects) = list_projects_db(&connection) {
                    for project in projects {
                        let root = PathBuf::from(project.path);
                        if recover || changed.iter().any(|path| path.starts_with(&root)) {
                            let _ = app.emit("index-started", project.id);
                        }
                    }
                }
            }
            let results = match db.lock() {
                Ok(mut connection) => refresh_watched_projects(&mut connection, &changed, recover),
                Err(error) => {
                    let _ = app.emit("index-error", error.to_string());
                    continue;
                }
            };
            for result in results {
                match result {
                    Ok(updated) => {
                        record_watcher_error(&db, None);
                        let _ = app.emit("index-updated", updated);
                    }
                    Err(error) => {
                        let _ = app.emit("index-error", error);
                    }
                }
            }
        }
    });
    IndexObserver(request_tx)
}

#[derive(Debug, Serialize, Clone)]
struct Project {
    id: i64,
    name: String,
    path: String,
    file_count: i64,
    indexed_at: Option<i64>,
}
#[derive(Debug, Serialize)]
struct SearchResult {
    path: String,
    relative_path: String,
    name: String,
    extension: String,
    line: usize,
    snippet: String,
    project_id: i64,
    project_name: String,
    result_type: String,
    symbol_kind: Option<String>,
    symbol_name: Option<String>,
}
#[derive(Debug, Serialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
    total: usize,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchOptions {
    query: String,
    project_id: Option<i64>,
    extension: Option<String>,
    case_sensitive: bool,
    #[serde(default)]
    mode: String,
}

fn init_db(path: &Path) -> Result<Connection, IndexError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = Connection::open(path)?;
    initialize_schema(&db)?;
    Ok(db)
}

fn initialize_schema(db: &Connection) -> Result<(), IndexError> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
        CREATE TABLE IF NOT EXISTS projects(id INTEGER PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL UNIQUE, file_count INTEGER NOT NULL DEFAULT 0, indexed_at INTEGER);
        CREATE TABLE IF NOT EXISTS files(id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE, path TEXT NOT NULL, relative_path TEXT NOT NULL, name TEXT NOT NULL, extension TEXT NOT NULL, content TEXT NOT NULL, hash TEXT NOT NULL, modified INTEGER NOT NULL, UNIQUE(project_id,path));
        CREATE INDEX IF NOT EXISTS files_project_extension ON files(project_id,extension);
        CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(content, content='files', content_rowid='id', tokenize='trigram');
        CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN INSERT INTO files_fts(rowid,content) VALUES(new.id,new.content); END;
        CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN INSERT INTO files_fts(files_fts,rowid,content) VALUES('delete',old.id,old.content); END;
        CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN INSERT INTO files_fts(files_fts,rowid,content) VALUES('delete',old.id,old.content); INSERT INTO files_fts(rowid,content) VALUES(new.id,new.content); END;
        CREATE TABLE IF NOT EXISTS index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS symbols(id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE, name TEXT NOT NULL, kind TEXT NOT NULL, line INTEGER NOT NULL, UNIQUE(file_id,name,kind,line)); CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name COLLATE NOCASE); CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
    let fts_ready: Option<String> = db
        .query_row("SELECT value FROM index_meta WHERE key='fts5'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if fts_ready.is_none() {
        db.execute("INSERT INTO files_fts(files_fts) VALUES('rebuild')", [])?;
        db.execute(
            "INSERT INTO index_meta(key,value) VALUES('fts5','trigram-v1')",
            [],
        )?;
    }
    let symbols_ready: Option<String> = db
        .query_row(
            "SELECT value FROM index_meta WHERE key='symbols'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if symbols_ready.is_none() {
        let mut stmt = db.prepare("SELECT id,path,content FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let files = rows.collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (file_id, path, content) in files {
            for symbol in symbols::parse(Path::new(&path), &content) {
                db.execute(
                    "INSERT OR IGNORE INTO symbols(file_id,name,kind,line) VALUES(?1,?2,?3,?4)",
                    params![file_id, symbol.name, symbol.kind, symbol.line],
                )?;
            }
        }
        db.execute(
            "INSERT INTO index_meta(key,value) VALUES('symbols','treesitter-v1')",
            [],
        )?;
    }
    Ok(())
}

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        file_count: row.get(3)?,
        indexed_at: row.get(4)?,
    })
}
fn list_projects_db(db: &Connection) -> Result<Vec<Project>, IndexError> {
    let mut q = db.prepare(
        "SELECT id,name,path,file_count,indexed_at FROM projects ORDER BY name COLLATE NOCASE",
    )?;
    let projects = q
        .query_map([], project_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(projects)
}
fn get_project(db: &Connection, id: i64) -> Result<Project, IndexError> {
    db.query_row(
        "SELECT id,name,path,file_count,indexed_at FROM projects WHERE id=?1",
        [id],
        project_from_row,
    )
    .map_err(Into::into)
}
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn supported(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cs"
            | "php"
            | "rb"
            | "swift"
            | "scala"
            | "sh"
            | "bash"
            | "zsh"
            | "sql"
            | "html"
            | "css"
            | "scss"
            | "sass"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "md"
            | "vue"
            | "svelte"
            | "xml"
            | "gradle"
            | "lua"
            | "ex"
            | "exs"
            | "clj"
            | "cljs"
            | "fs"
            | "fsx"
            | "r"
            | "dart"
            | "pl"
            | "pm"
    )
}

fn should_skip_dir(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str().unwrap_or("").to_ascii_lowercase().as_str(),
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "out"
            | "bin"
            | "obj"
            | ".next"
            | ".nuxt"
            | ".turbo"
            | ".cache"
            | "coverage"
            | ".venv"
            | "venv"
            | "pods"
    )
}

fn index_project(db: &mut Connection, root: &Path) -> Result<Project, IndexError> {
    if !root.is_dir() {
        return Err(IndexError::Invalid(
            "The selected folder does not exist or is not a directory.".into(),
        ));
    }
    let canonical = root.canonicalize()?;
    let root_text = canonical.to_string_lossy().to_string();
    db.execute(
        "INSERT OR IGNORE INTO projects(name,path) VALUES(?1,?2)",
        params![
            canonical
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("Project"),
            root_text
        ],
    )?;
    let project_id: i64 =
        db.query_row("SELECT id FROM projects WHERE path=?1", [&root_text], |r| {
            r.get(0)
        })?;
    let mut existing = std::collections::HashMap::<String, String>::new();
    {
        let mut q = db.prepare("SELECT path,hash FROM files WHERE project_id=?1")?;
        for row in q.query_map([project_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (p, h) = row?;
            existing.insert(p, h);
        }
    }
    let mut seen = HashSet::new();
    let mut skipped = 0u64;
    let tx = db.transaction()?;
    for entry in WalkBuilder::new(&canonical)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .filter_entry(|entry| {
            !entry
                .file_type()
                .is_some_and(|kind| kind.is_dir() && should_skip_dir(entry.file_name()))
        })
        .build()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) || !supported(path) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if metadata.len() > MAX_FILE_BYTES {
            skipped += 1;
            continue;
        }
        let full = path.to_string_lossy().to_string();
        seen.insert(full.clone());
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let Some(content) = String::from_utf8(bytes).ok() else {
            skipped += 1;
            continue;
        };
        let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        if existing.get(&full).is_some_and(|old| old == &hash) {
            continue;
        }
        let relative = path
            .strip_prefix(&canonical)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let modified = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();
        tx.execute("INSERT INTO files(project_id,path,relative_path,name,extension,content,hash,modified) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(project_id,path) DO UPDATE SET relative_path=excluded.relative_path,name=excluded.name,extension=excluded.extension,content=excluded.content,hash=excluded.hash,modified=excluded.modified", params![project_id,full,relative,name,ext,content,hash,modified])?;
        let file_id: i64 = tx.query_row(
            "SELECT id FROM files WHERE project_id=?1 AND path=?2",
            params![project_id, full],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM symbols WHERE file_id=?1", [file_id])?;
        for symbol in symbols::parse(path, &content) {
            tx.execute(
                "INSERT OR IGNORE INTO symbols(file_id,name,kind,line) VALUES(?1,?2,?3,?4)",
                params![file_id, symbol.name, symbol.kind, symbol.line],
            )?;
        }
    }
    // Remove deleted files and files that exceed the supported size limit.
    {
        let mut q = tx.prepare("SELECT path FROM files WHERE project_id=?1")?;
        let paths: Vec<String> = q
            .query_map([project_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let stale: Vec<String> = paths.into_iter().filter(|p| !seen.contains(p)).collect();
        drop(q);
        for path in stale {
            tx.execute(
                "DELETE FROM files WHERE project_id=?1 AND path=?2",
                params![project_id, path],
            )?;
        }
    }
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM files WHERE project_id=?1",
        [project_id],
        |r| r.get(0),
    )?;
    tx.execute(
        "UPDATE projects SET file_count=?1,indexed_at=?2 WHERE id=?3",
        params![count, now_epoch(), project_id],
    )?;
    tx.commit()?;
    let project = get_project(db, project_id)?;
    // Partial unreadable/oversized files do not fail the whole project refresh.
    let _ = skipped;
    Ok(project)
}

fn refresh_watched_projects(
    db: &mut Connection,
    changed: &HashSet<PathBuf>,
    recover: bool,
) -> Vec<Result<Project, String>> {
    let projects = match list_projects_db(db) {
        Ok(items) => items,
        Err(error) => return vec![Err(error.to_string())],
    };
    let mut results = Vec::new();
    for project in projects {
        let root = PathBuf::from(&project.path);
        let paths = changed
            .iter()
            .map(|path| normalize_watch_path(path))
            .filter(|path| path.starts_with(&root) && !ignored_watch_event(&root, path))
            .collect::<Vec<_>>();
        if paths.is_empty() && !recover {
            continue;
        }
        let settings_changed = paths.iter().any(|path| {
            path.file_name()
                .is_some_and(|name| name == ".gitignore" || name == ".gitmodules")
                || path.strip_prefix(&root).ok().is_some_and(|relative| {
                    relative.to_string_lossy().replace('\\', "/") == ".git/info/exclude"
                })
        });
        let directory_changed = paths.iter().any(|path| path.is_dir());
        let mut full_scan = recover || settings_changed || directory_changed;
        if !full_scan {
            for path in paths.iter().filter(|path| !supported(path)) {
                let tracked: Vec<String> = match db
                    .prepare("SELECT path FROM files WHERE project_id=?1")
                    .and_then(|mut stmt| stmt.query_map([project.id], |row| row.get(0))?.collect())
                {
                    Ok(items) => items,
                    Err(error) => {
                        results.push(Err(error.to_string()));
                        full_scan = true;
                        break;
                    }
                };
                if tracked
                    .iter()
                    .any(|indexed| Path::new(indexed).starts_with(path))
                {
                    full_scan = true;
                    break;
                }
            }
        }
        let result = if full_scan {
            index_project(db, &root).map_err(|error| error.to_string())
        } else {
            let id = project.id;
            let one = paths
                .iter()
                .filter(|path| supported(path))
                .try_for_each(|path| {
                    refresh_one_file(db, id, &root, path).map_err(|error| error.to_string())
                });
            one.and_then(|_| {
                let count: i64 = db
                    .query_row(
                        "SELECT count(*) FROM files WHERE project_id=?1",
                        [id],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                db.execute(
                    "UPDATE projects SET file_count=?1,indexed_at=?2 WHERE id=?3",
                    params![count, now_epoch(), id],
                )
                .map_err(|error| error.to_string())?;
                get_project(db, id).map_err(|error| error.to_string())
            })
        };
        results.push(result);
    }
    results
}

fn ignored_watch_event(root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let components = relative.components().collect::<Vec<_>>();
    if components.len() == 3
        && components[0].as_os_str() == ".git"
        && components[1].as_os_str() == "info"
        && components[2].as_os_str() == "exclude"
    {
        return false;
    }
    components.iter().enumerate().any(|(index, component)| {
        let name = component.as_os_str();
        if should_skip_dir(name) {
            return true;
        }
        let Some(text) = name.to_str() else {
            return false;
        };
        let is_ignore_file =
            index + 1 == components.len() && (text == ".gitignore" || text == ".gitmodules");
        text.starts_with('.') && !is_ignore_file
    })
}

fn refresh_one_file(
    db: &mut Connection,
    project_id: i64,
    root: &Path,
    path: &Path,
) -> Result<(), IndexError> {
    if !path.starts_with(root) {
        return Ok(());
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    if relative.components().any(|component| {
        let name = component.as_os_str();
        name.to_str()
            .is_some_and(|text| text.starts_with('.') && text != ".")
            || should_skip_dir(name)
    }) {
        db.execute(
            "DELETE FROM files WHERE project_id=?1 AND path=?2",
            params![project_id, path.to_string_lossy()],
        )?;
        return Ok(());
    }
    let mut included = false;
    for entry in WalkBuilder::new(path)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .build()
    {
        let Ok(entry) = entry else {
            continue;
        };
        if entry.path() == path && entry.file_type().is_some_and(|kind| kind.is_file()) {
            included = true;
            break;
        }
    }
    if !included {
        db.execute(
            "DELETE FROM files WHERE project_id=?1 AND path=?2",
            params![project_id, path.to_string_lossy()],
        )?;
        return Ok(());
    }
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_FILE_BYTES {
        db.execute(
            "DELETE FROM files WHERE project_id=?1 AND path=?2",
            params![project_id, path.to_string_lossy()],
        )?;
        return Ok(());
    }
    let content = String::from_utf8(std::fs::read(path)?)
        .map_err(|error| IndexError::Invalid(format!("Source file is not UTF-8: {error}")))?;
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    let full = path.to_string_lossy().to_string();
    let previous: Option<String> = db
        .query_row(
            "SELECT hash FROM files WHERE project_id=?1 AND path=?2",
            params![project_id, full],
            |r| r.get(0),
        )
        .optional()?;
    if previous.as_deref() == Some(hash.as_str()) {
        return Ok(());
    }
    let relative = relative.to_string_lossy().replace('\\', "/");
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string();
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64)
        .unwrap_or_default();
    let tx = db.transaction()?;
    tx.execute("INSERT INTO files(project_id,path,relative_path,name,extension,content,hash,modified) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(project_id,path) DO UPDATE SET relative_path=excluded.relative_path,name=excluded.name,extension=excluded.extension,content=excluded.content,hash=excluded.hash,modified=excluded.modified",params![project_id,full,relative,name,extension,content,hash,modified])?;
    let file_id: i64 = tx.query_row(
        "SELECT id FROM files WHERE project_id=?1 AND path=?2",
        params![project_id, full],
        |r| r.get(0),
    )?;
    tx.execute("DELETE FROM symbols WHERE file_id=?1", [file_id])?;
    for symbol in symbols::parse(path, &content) {
        tx.execute(
            "INSERT OR IGNORE INTO symbols(file_id,name,kind,line) VALUES(?1,?2,?3,?4)",
            params![file_id, symbol.name, symbol.kind, symbol.line],
        )?;
    }
    tx.execute(
        "UPDATE projects SET indexed_at=?1 WHERE id=?2",
        params![now_epoch(), project_id],
    )?;
    tx.commit()?;
    Ok(())
}

fn normalize_watch_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(parent) = parent.canonicalize() {
            return parent.join(name);
        }
    }
    path.to_path_buf()
}

#[tauri::command]
fn list_projects(state: State<'_, Store>) -> Result<Vec<Project>, String> {
    let db = state.0.lock().map_err(|e| e.to_string())?;
    list_projects_db(&db).map_err(|e| e.to_string())
}
#[tauri::command]
async fn add_project(
    path: String,
    state: State<'_, Store>,
    observer: State<'_, IndexObserver>,
) -> Result<Project, String> {
    let db = state.0.clone();
    let project = tauri::async_runtime::spawn_blocking(move || {
        let mut connection = db.lock().map_err(|e| e.to_string())?;
        index_project(&mut connection, Path::new(&path)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = observer
        .0
        .send(WatchMessage::Watch(PathBuf::from(&project.path)));
    Ok(project)
}
#[tauri::command]
async fn refresh_project(id: i64, state: State<'_, Store>) -> Result<Project, String> {
    let path = {
        let connection = state.0.lock().map_err(|e| e.to_string())?;
        get_project(&connection, id)
            .map_err(|e| e.to_string())?
            .path
    };
    let db = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut connection = db.lock().map_err(|e| e.to_string())?;
        index_project(&mut connection, Path::new(&path)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn remove_project(
    id: i64,
    state: State<'_, Store>,
    observer: State<'_, IndexObserver>,
) -> Result<(), String> {
    let db = state.0.lock().map_err(|e| e.to_string())?;
    let project = get_project(&db, id).map_err(|e| e.to_string())?;
    db.execute("DELETE FROM projects WHERE id=?1", [id])
        .map_err(|e| e.to_string())?;
    let _ = observer
        .0
        .send(WatchMessage::Unwatch(PathBuf::from(project.path)));
    Ok(())
}
#[tauri::command]
async fn search(options: SearchOptions, state: State<'_, Store>) -> Result<SearchResponse, String> {
    let database = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let db = database.lock().map_err(|e| e.to_string())?;
        search_db(&db, options)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn search_db(db: &Connection, options: SearchOptions) -> Result<SearchResponse, String> {
    let query = options.query.trim();
    if query.is_empty() {
        return Ok(SearchResponse {
            results: vec![],
            total: 0,
        });
    }
    let use_fts = !options.case_sensitive && query.chars().count() >= 3;
    let normalized_query = query.to_lowercase();
    if options.mode == "symbols" {
        let mut stmt = db.prepare("SELECT f.path,f.relative_path,f.name,f.extension,p.id,p.name,s.name,s.kind,s.line,f.content FROM symbols s JOIN files f ON f.id=s.file_id JOIN projects p ON p.id=f.project_id WHERE (?1 IS NULL OR p.id=?1) AND (?2 IS NULL OR f.extension=?2) LIMIT 20000").map_err(|e|e.to_string())?;
        let rows = stmt
            .query_map(
                params![options.project_id, options.extension.as_deref()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, usize>(8)?,
                        r.get::<_, String>(9)?,
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
        let mut ranked = Vec::new();
        for row in rows {
            let (
                path,
                relative,
                name,
                extension,
                project_id,
                project_name,
                symbol_name,
                kind,
                line,
                content,
            ) = row.map_err(|e| e.to_string())?;
            let score = fuzzy_score(query, &symbol_name, options.case_sensitive);
            if score >= 0 {
                let snippet = content
                    .lines()
                    .nth(line.saturating_sub(1))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                ranked.push((
                    score,
                    SearchResult {
                        path,
                        relative_path: relative,
                        name,
                        extension,
                        line,
                        snippet,
                        project_id,
                        project_name,
                        result_type: "symbol".into(),
                        symbol_kind: Some(kind),
                        symbol_name: Some(symbol_name),
                    },
                ));
            }
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        let total = ranked.len();
        return Ok(SearchResponse {
            results: ranked
                .into_iter()
                .take(MAX_RESULTS)
                .map(|(_, r)| r)
                .collect(),
            total,
        });
    }
    if options.mode == "files" {
        let mut stmt=db.prepare("SELECT f.path,f.relative_path,f.name,f.extension,p.id,p.name FROM files f JOIN projects p ON p.id=f.project_id WHERE (?1 IS NULL OR p.id=?1) AND (?2 IS NULL OR f.extension=?2) LIMIT 20000").map_err(|e|e.to_string())?;
        let rows = stmt
            .query_map(
                params![options.project_id, options.extension.as_deref()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
        let mut ranked = Vec::new();
        for row in rows {
            let (path, relative_path, name, extension, project_id, project_name) =
                row.map_err(|e| e.to_string())?;
            let score = fuzzy_score(
                query,
                &format!("{name} {relative_path}"),
                options.case_sensitive,
            );
            if score >= 0 {
                ranked.push((
                    score,
                    SearchResult {
                        path,
                        relative_path,
                        name,
                        extension,
                        line: 1,
                        snippet: String::new(),
                        project_id,
                        project_name,
                        result_type: "file".into(),
                        symbol_kind: None,
                        symbol_name: None,
                    },
                ));
            }
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        let total = ranked.len();
        return Ok(SearchResponse {
            results: ranked
                .into_iter()
                .take(MAX_RESULTS)
                .map(|(_, r)| r)
                .collect(),
            total,
        });
    }
    let needle = if use_fts {
        format!("\"{}\"", query.replace('"', "\"\""))
    } else if options.case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let sql = if use_fts {
        "SELECT f.path,f.relative_path,f.name,f.extension,f.content,p.id,p.name FROM files_fts JOIN files f ON f.id=files_fts.rowid JOIN projects p ON p.id=f.project_id WHERE files_fts MATCH ?1 AND (?2 IS NULL OR p.id=?2) AND (?3 IS NULL OR f.extension=?3) ORDER BY f.path"
    } else if options.case_sensitive {
        "SELECT f.path,f.relative_path,f.name,f.extension,f.content,p.id,p.name FROM files f JOIN projects p ON p.id=f.project_id WHERE instr(f.content,?1)>0 AND (?2 IS NULL OR p.id=?2) AND (?3 IS NULL OR f.extension=?3) ORDER BY f.path"
    } else {
        "SELECT f.path,f.relative_path,f.name,f.extension,f.content,p.id,p.name FROM files f JOIN projects p ON p.id=f.project_id WHERE instr(lower(f.content),?1)>0 AND (?2 IS NULL OR p.id=?2) AND (?3 IS NULL OR f.extension=?3) ORDER BY f.path"
    };
    let mut stmt = db.prepare(sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            params![needle, options.project_id, options.extension.as_deref()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    let mut total = 0;
    for row in rows {
        let (path, relative, name, extension, content, project_id, project_name) =
            row.map_err(|e| e.to_string())?;
        for (line_idx, line) in content.lines().enumerate() {
            if if options.case_sensitive {
                line.contains(query)
            } else {
                line.to_lowercase().contains(&normalized_query)
            } {
                total += 1;
                if results.len() < MAX_RESULTS {
                    let shown = line.trim();
                    let snippet = if shown.chars().count() > 280 {
                        format!("{}…", shown.chars().take(277).collect::<String>())
                    } else {
                        shown.to_string()
                    };
                    results.push(SearchResult {
                        path: path.clone(),
                        relative_path: relative.clone(),
                        name: name.clone(),
                        extension: extension.clone(),
                        line: line_idx + 1,
                        snippet,
                        project_id,
                        project_name: project_name.clone(),
                        result_type: "text".into(),
                        symbol_kind: None,
                        symbol_name: None,
                    });
                }
            }
        }
    }
    drop(stmt);
    let mut names=db.prepare("SELECT f.path,f.relative_path,f.name,f.extension,p.id,p.name FROM files f JOIN projects p ON p.id=f.project_id WHERE (?1 IS NULL OR p.id=?1) AND (?2 IS NULL OR f.extension=?2) LIMIT 20000").map_err(|e|e.to_string())?;
    let rows = names
        .query_map(
            params![options.project_id, options.extension.as_deref()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (path, relative_path, name, extension, project_id, project_name) =
            row.map_err(|e| e.to_string())?;
        if fuzzy_score(query, &name, options.case_sensitive) >= 0
            && !results.iter().any(|hit| hit.path == path)
        {
            total += 1;
            if results.len() < MAX_RESULTS {
                results.push(SearchResult {
                    path,
                    relative_path,
                    name,
                    extension,
                    line: 1,
                    snippet: String::new(),
                    project_id,
                    project_name,
                    result_type: "file".into(),
                    symbol_kind: None,
                    symbol_name: None,
                });
            }
        }
    }
    Ok(SearchResponse { results, total })
}

fn fuzzy_score(query: &str, candidate: &str, case_sensitive: bool) -> i32 {
    let q = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    let c = if case_sensitive {
        candidate.to_string()
    } else {
        candidate.to_lowercase()
    };
    if q == c {
        return 1000;
    }
    if c.split_whitespace().any(|part| part.starts_with(&q)) {
        return 850 - c.len().min(100) as i32;
    }
    if c.contains(&q) {
        return 700 - c.find(&q).unwrap_or(0) as i32;
    }
    let compact = |value: &str| {
        value
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
    };
    let q = compact(&q);
    let c = compact(&c);
    if q.is_empty() {
        return -1;
    }
    let mut qi = 0usize;
    let mut gaps = 0usize;
    let mut previous = None;
    for (index, ch) in c.chars().enumerate() {
        if q.chars().nth(qi) == Some(ch) {
            if let Some(last) = previous {
                gaps += index.saturating_sub(last + 1);
            }
            previous = Some(index);
            qi += 1;
            if qi == q.chars().count() {
                return 520 - gaps.min(400) as i32 - c.len().min(80) as i32;
            }
        }
    }
    let qchars = q.chars().collect::<Vec<_>>();
    let cchars = c.chars().collect::<Vec<_>>();
    if qchars.len() < 3 || cchars.len() > 128 || qchars.len().abs_diff(cchars.len()) > 2 {
        return -1;
    }
    let mut prev = (0..=cchars.len()).collect::<Vec<usize>>();
    for (i, qc) in qchars.iter().enumerate() {
        let mut curr = vec![i + 1; cchars.len() + 1];
        for (j, cc) in cchars.iter().enumerate() {
            curr[j + 1] = (curr[j] + 1)
                .min(prev[j + 1] + 1)
                .min(prev[j] + usize::from(qc != cc));
        }
        prev = curr;
    }
    let distance = prev[cchars.len()];
    if distance <= 2 {
        400 - distance as i32 * 100
    } else {
        -1
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct EditorPreferences {
    editor: String,
    executable: String,
    shortcut: String,
    shortcut_error: Option<String>,
    watcher_error: Option<String>,
}

#[tauri::command]
fn get_preferences(state: State<'_, Store>) -> Result<EditorPreferences, String> {
    let db = state.0.lock().map_err(|e| e.to_string())?;
    let read = |key: &str, default: &str| -> Result<String, String> {
        Ok(db
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| default.to_string()))
    };
    Ok(EditorPreferences {
        editor: read("editor", "auto")?,
        executable: read("editor_executable", "")?,
        shortcut: read("global_shortcut", "CTRL+SHIFT+SPACE")?,
        shortcut_error: db
            .query_row(
                "SELECT value FROM settings WHERE key='shortcut_error'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?,
        watcher_error: db
            .query_row(
                "SELECT value FROM settings WHERE key='watcher_error'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?,
    })
}

#[tauri::command]
fn save_preferences(
    preferences: EditorPreferences,
    state: State<'_, Store>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let shortcut: Shortcut = preferences
        .shortcut
        .parse()
        .map_err(|e| format!("Atalho inválido: {e}"))?;
    let old = get_preferences(state.clone())?;
    app.global_shortcut()
        .unregister_all()
        .map_err(|e| format!("Falha ao liberar atalho anterior: {e}"))?;
    if let Err(error) = app.global_shortcut().register(shortcut) {
        if let Ok(previous) = old.shortcut.parse::<Shortcut>() {
            let _ = app.global_shortcut().register(previous);
        }
        return Err(format!(
            "Não foi possível registrar o atalho (talvez esteja em uso): {error}"
        ));
    }
    let db = state.0.lock().map_err(|e| e.to_string())?;
    for (key, value) in [
        ("editor", preferences.editor),
        ("editor_executable", preferences.executable),
        ("global_shortcut", preferences.shortcut),
    ] {
        db.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value]).map_err(|e|e.to_string())?;
    }
    db.execute("DELETE FROM settings WHERE key='shortcut_error'", [])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn open_file(path: String, line: usize, state: State<'_, Store>) -> Result<Option<String>, String> {
    let prefs = get_preferences(state)?;
    let file = PathBuf::from(&path);
    if prefs.editor == "auto" && prefs.executable.trim().is_empty() {
        if open_auto_editor(&file, line) {
            return Ok(None);
        }
        return open_with_system(
            &file,
            "Nenhum editor compatível foi encontrado. Arquivo aberto no aplicativo padrão.",
        );
    }
    if prefs.editor == "visualstudio" {
        if let Some(executable) = prefs
            .executable
            .trim()
            .is_empty()
            .then(find_visual_studio)
            .flatten()
        {
            if launch_editor(Command::new(executable).args(visual_studio_arguments(&file, line)))
                .is_ok()
            {
                return Ok(None);
            }
        } else if !prefs.executable.trim().is_empty()
            && launch_editor(
                Command::new(&prefs.executable).args(visual_studio_arguments(&file, line)),
            )
            .is_ok()
        {
            return Ok(None);
        }
        return open_with_system(
            &file,
            "Visual Studio não foi encontrado. Arquivo aberto no aplicativo padrão.",
        );
    }
    let executable = if prefs.executable.trim().is_empty() {
        if prefs.editor == "nvim" {
            "nvim".to_string()
        } else {
            "code".to_string()
        }
    } else {
        prefs.executable
    };
    #[cfg(target_os = "windows")]
    let result = if prefs.editor == "nvim" {
        launch_nvim(&executable, &file, line)
    } else {
        launch_editor(Command::new(&executable).args(editor_arguments(&prefs.editor, &file, line)))
    };
    #[cfg(not(target_os = "windows"))]
    let result =
        launch_editor(Command::new(&executable).args(editor_arguments(&prefs.editor, &file, line)));
    if result.is_ok() {
        return Ok(None);
    }
    open_with_system(&file, &format!("Editor '{}' não encontrado. Arquivo aberto no aplicativo padrão; configure o executável em Settings.", executable))
}

fn open_auto_editor(file: &Path, line: usize) -> bool {
    let mut code_candidates = vec![PathBuf::from("code")];
    #[cfg(target_os = "windows")]
    {
        for variable in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = std::env::var_os(variable) {
                let root = PathBuf::from(root);
                code_candidates.push(root.join("Programs/Microsoft VS Code/Code.exe"));
                code_candidates.push(root.join("Microsoft VS Code/Code.exe"));
            }
        }
    }
    for executable in code_candidates {
        if launch_editor(Command::new(executable).args(editor_arguments("vscode", file, line)))
            .is_ok()
        {
            return true;
        }
    }
    #[cfg(target_os = "windows")]
    if let Some(executable) = find_visual_studio() {
        if launch_editor(Command::new(executable).args(visual_studio_arguments(file, line))).is_ok()
        {
            return true;
        }
    }
    #[cfg(not(target_os = "windows"))]
    if launch_nvim("nvim", file, line).is_ok() {
        return true;
    }
    false
}

#[cfg(target_os = "windows")]
fn find_visual_studio() -> Option<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles(x86)")?;
    let vswhere =
        PathBuf::from(program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
    let output = Command::new(vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-find",
            "Common7\\IDE\\devenv.exe",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

fn visual_studio_arguments(file: &Path, line: usize) -> Vec<std::ffi::OsString> {
    vec![
        "/Edit".into(),
        file.as_os_str().to_owned(),
        "/Command".into(),
        format!("Edit.GoTo {}", line.max(1)).into(),
    ]
}

fn launch_editor(command: &mut Command) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn()
}

#[cfg(target_os = "windows")]
fn launch_nvim(executable: &str, file: &Path, line: usize) -> std::io::Result<std::process::Child> {
    // Prefer a GUI frontend when available. The plain nvim binary needs a terminal,
    // so launch it in Windows Terminal instead of popping a legacy console window.
    let mut candidates = Vec::new();
    if executable == "nvim" {
        candidates.extend(["neovide.exe", "nvim-qt.exe"].map(PathBuf::from));
    }
    candidates.push(PathBuf::from(executable));
    let args = editor_arguments("nvim", file, line);
    for candidate in candidates {
        if let Ok(child) = launch_editor(Command::new(candidate).args(args.clone())) {
            return Ok(child);
        }
    }
    let mut terminal = Command::new("wt.exe");
    terminal
        .arg("new-tab")
        .arg("--")
        .arg(executable)
        .arg(format!("+{}", line.max(1)));
    terminal.arg(file);
    launch_editor(&mut terminal)
}

#[cfg(not(target_os = "windows"))]
fn launch_nvim(executable: &str, file: &Path, line: usize) -> std::io::Result<std::process::Child> {
    launch_editor(Command::new(executable).args(editor_arguments("nvim", file, line)))
}

#[cfg(not(target_os = "windows"))]
fn find_visual_studio() -> Option<PathBuf> {
    None
}

fn open_with_system(file: &Path, message: &str) -> Result<Option<String>, String> {
    #[cfg(target_os = "windows")]
    let fallback = launch_editor(Command::new("explorer").arg(&file));
    #[cfg(target_os = "linux")]
    let fallback = launch_editor(Command::new("xdg-open").arg(&file));
    #[cfg(target_os = "macos")]
    let fallback = launch_editor(Command::new("open").arg(&file));
    fallback
        .map(|_| Some(message.to_string()))
        .map_err(|e| format!("{message} Não foi possível abrir o arquivo pelo sistema: {e}"))
}

fn editor_arguments(editor: &str, path: &Path, line: usize) -> Vec<std::ffi::OsString> {
    if editor == "nvim" {
        vec![
            format!("+{}", line.max(1)).into(),
            path.as_os_str().to_owned(),
        ]
    } else {
        vec![
            "--goto".into(),
            format!("{}:{}", path.display(), line.max(1)).into(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::str::FromStr;

    fn fixture(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("index-test-{}-{}", label, unique));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn memory_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        initialize_schema(&db).unwrap();
        db
    }

    #[test]
    fn indexes_and_searches_source_with_line_numbers() {
        let root = fixture("search");
        fs::write(
            root.join("main.rs"),
            "fn main() {\n    let needle = 42;\n}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("node_modules/dependency")).unwrap();
        fs::write(
            root.join("node_modules/dependency/hidden.ts"),
            "const needle = true;",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), "ignored.py\n").unwrap();
        fs::write(root.join("ignored.py"), "needle\n").unwrap();
        let mut db = memory_db();
        let project = index_project(&mut db, &root).unwrap();
        let (indexed_path, content): (String, String) = db
            .query_row(
                "SELECT path,content FROM files WHERE project_id=?1",
                [project.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            indexed_path.ends_with("main.rs"),
            "indexed path was {indexed_path}"
        );
        let found = content
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("needle"))
            .unwrap();
        assert_eq!(found.0 + 1, 2, "indexed content was {content:?}");
        assert_eq!(project.file_count, 1);
        let insensitive = search_db(
            &db,
            SearchOptions {
                query: "NEEDLE".into(),
                project_id: Some(project.id),
                extension: Some("rs".into()),
                case_sensitive: false,
                mode: "content".into(),
            },
        )
        .unwrap();
        assert_eq!(insensitive.total, 1);
        assert_eq!(insensitive.results[0].line, 2);
        assert_eq!(insensitive.results[0].relative_path, "main.rs");
        let sensitive = search_db(
            &db,
            SearchOptions {
                query: "NEEDLE".into(),
                project_id: Some(project.id),
                extension: None,
                case_sensitive: true,
                mode: "content".into(),
            },
        )
        .unwrap();
        assert_eq!(sensitive.total, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn files_and_symbols_modes_return_their_own_kinds_of_matches() {
        let root = fixture("modes");
        fs::write(
            root.join("lookup.rs"),
            "let lookup = 1;\npub fn lookup_value() {}\n",
        )
        .unwrap();
        fs::write(root.join("other.rs"), "fn unrelated() {}\n").unwrap();
        let mut db = memory_db();
        let project = index_project(&mut db, &root).unwrap();
        let files = search_db(
            &db,
            SearchOptions {
                query: "lookup".into(),
                project_id: Some(project.id),
                extension: None,
                case_sensitive: false,
                mode: "files".into(),
            },
        )
        .unwrap();
        assert_eq!(files.total, 1);
        assert_eq!(files.results[0].name, "lookup.rs");
        let symbols = search_db(
            &db,
            SearchOptions {
                query: "lookup".into(),
                project_id: Some(project.id),
                extension: None,
                case_sensitive: false,
                mode: "symbols".into(),
            },
        )
        .unwrap();
        assert_eq!(symbols.total, 2);
        assert!(symbols
            .results
            .iter()
            .any(|hit| hit.line == 2 && hit.symbol_kind.as_deref() == Some("function")));
        assert!(symbols
            .results
            .iter()
            .any(|hit| hit.line == 1 && hit.symbol_kind.as_deref() == Some("variable")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn incremental_refresh_updates_changes_and_removes_deleted_files() {
        let root = fixture("incremental");
        let file = root.join("lib.ts");
        fs::write(&file, "const value = 1;\n").unwrap();
        let mut db = memory_db();
        let first = index_project(&mut db, &root).unwrap();
        assert_eq!(
            search_db(
                &db,
                SearchOptions {
                    query: "value = 1".into(),
                    project_id: Some(first.id),
                    extension: None,
                    case_sensitive: false,
                    mode: "content".into(),
                },
            )
            .unwrap()
            .total,
            1
        );
        let file_id: i64 = db
            .query_row("SELECT id FROM files", [], |r| r.get(0))
            .unwrap();
        let second = index_project(&mut db, &root).unwrap();
        let unchanged_id: i64 = db
            .query_row("SELECT id FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(file_id, unchanged_id);
        fs::write(&file, "const value = 2;\n").unwrap();
        index_project(&mut db, &root).unwrap();
        let updated: String = db
            .query_row("SELECT content FROM files", [], |r| r.get(0))
            .unwrap();
        assert!(updated.contains("= 2"));
        assert_eq!(
            search_db(
                &db,
                SearchOptions {
                    query: "value = 1".into(),
                    project_id: Some(first.id),
                    extension: None,
                    case_sensitive: false,
                    mode: "content".into(),
                },
            )
            .unwrap()
            .total,
            0
        );
        assert_eq!(
            search_db(
                &db,
                SearchOptions {
                    query: "value = 2".into(),
                    project_id: Some(first.id),
                    extension: None,
                    case_sensitive: false,
                    mode: "content".into(),
                },
            )
            .unwrap()
            .total,
            1
        );
        fs::remove_file(&file).unwrap();
        let last = index_project(&mut db, &root).unwrap();
        assert_eq!(last.file_count, 0);
        assert_eq!(
            search_db(
                &db,
                SearchOptions {
                    query: "value = 2".into(),
                    project_id: Some(first.id),
                    extension: None,
                    case_sensitive: false,
                    mode: "content".into(),
                },
            )
            .unwrap()
            .total,
            0
        );
        assert_eq!(first.id, second.id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removing_project_cascades_its_files() {
        let root = fixture("remove");
        fs::write(root.join("a.py"), "print('hello')").unwrap();
        let mut db = memory_db();
        let project = index_project(&mut db, &root).unwrap();
        db.execute("DELETE FROM projects WHERE id=?1", [project.id])
            .unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fuzzy_matching_prioritizes_exact_then_prefix_and_supports_subsequence() {
        assert!(
            fuzzy_score("HookController", "HookController.lua", false)
                > fuzzy_score("HookCtrl", "HookController.lua", false)
        );
        assert!(fuzzy_score("HookCtrl", "HookController.lua", false) >= 0);
        assert!(fuzzy_score("HokController", "HookController", false) >= 0);
        assert!(fuzzy_score("HxkZzz", "HookController.lua", false) < 0);
    }

    #[test]
    fn symbols_are_persisted_and_fuzzy_search_returns_symbol_type_and_line() {
        let root = fixture("symbols");
        fs::write(
            root.join("hooks.lua"),
            "local HookController = {}\nfunction HookController:install() end\n",
        )
        .unwrap();
        let mut db = memory_db();
        let project = index_project(&mut db, &root).unwrap();
        let result = search_db(
            &db,
            SearchOptions {
                query: "HookCtrl".into(),
                project_id: Some(project.id),
                extension: None,
                case_sensitive: false,
                mode: "symbols".into(),
            },
        )
        .unwrap();
        assert!(result.results.iter().any(|hit| hit.name == "hooks.lua"
            && hit.symbol_kind.as_deref() == Some("variable")
            && hit.symbol_name.as_deref() == Some("HookController")
            && hit.line == 1));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn global_shortcut_parser_rejects_invalid_values() {
        assert!(Shortcut::from_str("CTRL+SHIFT+SPACE").is_ok());
        assert!(Shortcut::from_str("not a shortcut").is_err());
    }

    #[test]
    fn watcher_reconciliation_handles_create_modify_rename_and_delete() {
        let root = fixture("watcher");
        let original = root.join("one.rs");
        fs::write(&original, "fn one() {}\n").unwrap();
        let mut db = memory_db();
        let project = index_project(&mut db, &root).unwrap();
        let gitignore = root.join(".gitignore");
        let ignored = root.join("ignored.ts");
        fs::write(&gitignore, "ignored.ts\n").unwrap();
        fs::write(&ignored, "const shouldNotIndex = true;\n").unwrap();
        assert!(
            refresh_watched_projects(&mut db, &HashSet::from([gitignore, ignored]), false)[0]
                .is_ok()
        );
        assert_eq!(get_project(&db, project.id).unwrap().file_count, 1);
        let created = root.join("two.ts");
        fs::write(&created, "const value = 1;\n").unwrap();
        let changed = HashSet::from([created.clone()]);
        assert_eq!(refresh_watched_projects(&mut db, &changed, false).len(), 1);
        assert_eq!(get_project(&db, project.id).unwrap().file_count, 2);
        fs::write(&created, "const value = 2;\n").unwrap();
        assert!(
            refresh_watched_projects(&mut db, &HashSet::from([created.clone()]), false)[0].is_ok()
        );
        let renamed = root.join("renamed.ts");
        fs::rename(&created, &renamed).unwrap();
        assert!(refresh_watched_projects(
            &mut db,
            &HashSet::from([created, renamed.clone()]),
            false
        )[0]
        .is_ok());
        assert_eq!(get_project(&db, project.id).unwrap().file_count, 2);
        fs::remove_file(&renamed).unwrap();
        assert!(refresh_watched_projects(&mut db, &HashSet::from([renamed]), false)[0].is_ok());
        assert_eq!(get_project(&db, project.id).unwrap().file_count, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn editor_arguments_keep_paths_with_spaces_as_single_arguments() {
        let path = Path::new("C:/Users/test user/project file/src/main.rs");
        let args = editor_arguments("vscode", path, 37);
        assert_eq!(args.len(), 2);
        assert_eq!(args[1], "C:/Users/test user/project file/src/main.rs:37");
        let nvim_args = editor_arguments("nvim", path, 37);
        assert_eq!(nvim_args[0], "+37");
        assert_eq!(nvim_args[1], path.as_os_str());
        let visual_studio_args = visual_studio_arguments(path, 37);
        assert_eq!(visual_studio_args[0], "/Edit");
        assert_eq!(visual_studio_args[1], path.as_os_str());
        assert_eq!(visual_studio_args[2], "/Command");
        assert_eq!(visual_studio_args[3], "Edit.GoTo 37");
        let goto = format!("{}:{}", path.display(), 37);
        assert_eq!(goto, "C:/Users/test user/project file/src/main.rs:37");
        let nvim_arg = format!("+{}", 37);
        assert_eq!(nvim_arg, "+37");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window)=app.get_webview_window("main") { let _=window.show(); let _=window.set_focus(); let _=window.emit("global-focus-search",()); }
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().with_handler(|app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                if let Some(window)=app.get_webview_window("main") { let _=window.show(); let _=window.set_focus(); let _=window.emit("global-focus-search",()); }
            }
        }).build())
        .setup(|app| {
            let db_path: PathBuf = app.path().app_data_dir()?.join("index.sqlite3");
            let db=Arc::new(Mutex::new(init_db(&db_path).expect("initialize Index database")));
            let shortcut: String = { let connection=db.lock().map_err(|e|e.to_string())?; connection.query_row("SELECT value FROM settings WHERE key='global_shortcut'",[],|r|r.get(0)).optional()?.unwrap_or_else(||"CTRL+SHIFT+SPACE".into()) };
            let shortcut_error=match Shortcut::from_str(&shortcut) {
                Ok(shortcut)=>app.global_shortcut().register(shortcut).err().map(|error|format!("Atalho global indisponível ({shortcut}): {error}")),
                Err(error)=>Some(format!("Atalho global inválido ({shortcut}): {error}"))
            };
            if let Some(error)=shortcut_error { if let Ok(connection)=db.lock() { let _=connection.execute("INSERT INTO settings(key,value) VALUES('shortcut_error',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[error]); } }
            app.manage(Store(db));
            let db=app.state::<Store>().0.clone();
            let roots={ let connection=db.lock().map_err(|e|e.to_string())?; list_projects_db(&connection).map_err(|e|e.to_string())?.into_iter().map(|p|PathBuf::from(p.path)).collect() };
            app.manage(start_observer(db,app.handle().clone(),roots));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_projects,
            add_project,
            refresh_project,
            remove_project,
            search,
            open_file,
            get_preferences,
            save_preferences
        ])
        .run(tauri::generate_context!())
        .expect("error while running Index");
}
