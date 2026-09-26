//! One-time VACUUM for databases created before incremental auto-vacuum. It locks
//! out writers, so it runs only while the app is idle.

use super::storage::database_path;
use rusqlite::Connection;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MIN_FREE_BYTES: u64 = 64 * 1024 * 1024;
const MIN_FREE_PERCENT: u64 = 25;
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(120);
const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// A snapshot with "null" inside a string looks like one that still needs the rewrite.
const CREATOR_SNAPSHOTS_DONE_KEY: &str = "maintenance.creator_snapshots_without_nulls";

static APP_HIDDEN: AtomicBool = AtomicBool::new(false);

pub fn set_app_hidden(hidden: bool) {
    APP_HIDDEN.store(hidden, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageStats {
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_count: u64,
    /// SQLite's mode: 0 none, 1 full, 2 incremental.
    pub auto_vacuum: i64,
}

impl PageStats {
    pub fn read(connection: &Connection) -> rusqlite::Result<Self> {
        let pragma = |name: &str| {
            connection.query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
        };
        Ok(Self {
            page_size: pragma("page_size")?.max(0) as u64,
            page_count: pragma("page_count")?.max(0) as u64,
            freelist_count: pragma("freelist_count")?.max(0) as u64,
            auto_vacuum: pragma("auto_vacuum")?,
        })
    }

    fn free_bytes(&self) -> u64 {
        self.freelist_count.saturating_mul(self.page_size)
    }

    fn used_bytes(&self) -> u64 {
        self.page_count
            .saturating_sub(self.freelist_count)
            .saturating_mul(self.page_size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Nothing,
    Incremental,
    Full,
}

pub fn plan(stats: &PageStats) -> Plan {
    let worth_it = stats.free_bytes() >= MIN_FREE_BYTES
        && stats.freelist_count.saturating_mul(100)
            >= stats.page_count.saturating_mul(MIN_FREE_PERCENT);
    match (worth_it, stats.auto_vacuum) {
        (false, _) => Plan::Nothing,
        (true, 2) => Plan::Incremental,
        (true, _) => Plan::Full,
    }
}

/// VACUUM writes the live data to a temp file and again into the WAL.
fn has_room_for_full(stats: &PageStats, database: &Path) -> bool {
    let used = stats.used_bytes();
    let database_dir = database.parent().unwrap_or(database);
    let enough = |dir: &Path, needed: u64| available_space(dir).is_some_and(|free| free >= needed);
    enough(database_dir, used.saturating_mul(2)) && enough(&std::env::temp_dir(), used)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    NotNeeded,
    NotEnoughDiskSpace,
    Compacted { before: u64, after: u64 },
}

fn creator_snapshots_pending(connection: &Connection) -> rusqlite::Result<bool> {
    let has_tables: bool = connection.query_row(
        "SELECT COUNT(*) = 2 FROM sqlite_master
         WHERE type = 'table' AND name IN ('creators', 'app_settings')",
        [],
        |row| row.get(0),
    )?;
    if !has_tables {
        return Ok(false);
    }
    connection.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM app_settings WHERE key = ?1)",
        [CREATOR_SNAPSHOTS_DONE_KEY],
        |row| row.get(0),
    )
}

fn strip_creator_snapshot_nulls(connection: &Connection) -> rusqlite::Result<()> {
    let tx = connection.unchecked_transaction()?;
    tx.execute(
        "UPDATE creators SET snapshot_json = json_patch('{}', snapshot_json)
         WHERE instr(snapshot_json, 'null') > 0 AND json_valid(snapshot_json)",
        [],
    )?;
    tx.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, '1', CURRENT_TIMESTAMP)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP",
        [CREATOR_SNAPSHOTS_DONE_KEY],
    )?;
    tx.commit()
}

pub fn compact(database: &Path) -> Result<Outcome, String> {
    let connection = Connection::open(database).map_err(|e| e.to_string())?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| e.to_string())?;
    let before = PageStats::read(&connection).map_err(|e| e.to_string())?;
    let rewrite_creators = creator_snapshots_pending(&connection).map_err(|e| e.to_string())?;
    let plan = match (plan(&before), rewrite_creators, before.auto_vacuum) {
        (Plan::Nothing, false, _) => return Ok(Outcome::NotNeeded),
        (Plan::Nothing, true, 2) => Plan::Incremental,
        (Plan::Nothing, true, _) => Plan::Full,
        (plan, _, _) => plan,
    };
    if (plan == Plan::Full || rewrite_creators) && !has_room_for_full(&before, database) {
        return Ok(Outcome::NotEnoughDiskSpace);
    }
    if rewrite_creators {
        strip_creator_snapshot_nulls(&connection).map_err(|e| e.to_string())?;
    }
    let batch = match plan {
        Plan::Full => "PRAGMA auto_vacuum=INCREMENTAL; VACUUM;",
        _ => "PRAGMA incremental_vacuum;",
    };
    connection.execute_batch(batch).map_err(|e| e.to_string())?;
    // Otherwise the WAL file keeps the reclaimed space.
    let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let after = PageStats::read(&connection).map_err(|e| e.to_string())?;
    Ok(Outcome::Compacted {
        before: before.page_count.saturating_mul(before.page_size),
        after: after.page_count.saturating_mul(after.page_size),
    })
}

fn app_is_idle(app: &tauri::AppHandle) -> bool {
    use tauri::Manager;
    let Some(state) = app.try_state::<crate::AppState>() else {
        return false;
    };
    if state.download_manager.is_busy() || state.sync_manager.is_syncing() {
        return false;
    }
    if APP_HIDDEN.load(Ordering::Relaxed) {
        return true;
    }
    #[cfg(desktop)]
    if let Some(window) = app.get_webview_window("main") {
        return window.is_minimized().unwrap_or(false) || !window.is_visible().unwrap_or(true);
    }
    false
}

pub fn spawn(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            if app_is_idle(&app) {
                let outcome = tokio::task::spawn_blocking(|| compact(&database_path()))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|result| result);
                match outcome {
                    Ok(Outcome::Compacted { before, after }) => {
                        tracing::info!(before, after, "Database compacted")
                    }
                    Ok(Outcome::NotNeeded) => {}
                    Ok(Outcome::NotEnoughDiskSpace) => {
                        tracing::info!("Database compaction skipped: not enough free disk space")
                    }
                    Err(error) => tracing::warn!(%error, "Database compaction failed"),
                }
                return;
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

#[cfg(windows)]
fn available_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

#[cfg(unix)]
fn available_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(page_count: u64, freelist_count: u64, auto_vacuum: i64) -> PageStats {
        PageStats {
            page_size: 4096,
            page_count,
            freelist_count,
            auto_vacuum,
        }
    }

    #[test]
    fn small_or_mostly_full_databases_are_left_alone() {
        assert_eq!(plan(&stats(20_000, 10_000, 0)), Plan::Nothing);
        assert_eq!(plan(&stats(500_000, 49_000, 0)), Plan::Nothing);
    }

    #[test]
    fn bloated_databases_get_the_right_kind_of_vacuum() {
        assert_eq!(plan(&stats(145_411, 49_006, 0)), Plan::Full);
        assert_eq!(plan(&stats(145_411, 49_006, 2)), Plan::Incremental);
    }

    fn temp_database() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("pawstash-compact-{}.db", uuid::Uuid::new_v4()))
    }

    fn remove_database(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn creator_snapshots_lose_their_nulls_once() {
        let path = temp_database();
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                r#"PRAGMA journal_mode=WAL;
                   CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT);
                   CREATE TABLE creators(service TEXT, creator_id TEXT, snapshot_json TEXT NOT NULL);
                   INSERT INTO creators VALUES
                     ('patreon', '1', '{"id":"1","name":"a:null","avatar_url":null,"extra":{"x":null,"y":1},"tags":[null]}'),
                     ('patreon', '2', 'not json');"#,
            )
            .unwrap();

        let first = compact(&path).unwrap();
        let second = compact(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        let snapshots: Vec<String> = connection
            .prepare("SELECT snapshot_json FROM creators ORDER BY creator_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        drop(connection);
        remove_database(&path);

        assert!(matches!(first, Outcome::Compacted { .. }));
        assert_eq!(second, Outcome::NotNeeded);
        assert_eq!(
            snapshots,
            vec![
                r#"{"id":"1","name":"a:null","extra":{"y":1},"tags":[null]}"#.to_string(),
                "not json".to_string()
            ]
        );
    }

    #[test]
    fn stored_snapshots_match_the_one_time_rewrite() {
        let json = serde_json::json!({
            "id": "1", "name": "a:null", "avatar_url": null,
            "extra": {"x": null, "y": 1}, "tags": [null]
        });
        assert_eq!(
            crate::db::storage::snapshot_json(&json).unwrap(),
            r#"{"extra":{"y":1},"id":"1","name":"a:null","tags":[null]}"#
        );
    }

    #[test]
    fn free_space_is_reported_for_the_temp_dir() {
        assert!(available_space(&std::env::temp_dir()).is_some_and(|free| free > 0));
    }

    #[test]
    fn compaction_switches_mode_and_returns_the_space() {
        let path = temp_database();
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "PRAGMA journal_mode=WAL;
                     CREATE TABLE blobs(data BLOB);
                     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1100)
                     INSERT INTO blobs SELECT zeroblob(100000) FROM n;
                     DELETE FROM blobs WHERE rowid > 100;
                     PRAGMA wal_checkpoint(TRUNCATE);",
                )
                .unwrap();
            assert_eq!(plan(&PageStats::read(&connection).unwrap()), Plan::Full);
        }

        let outcome = compact(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        let after = PageStats::read(&connection).unwrap();
        drop(connection);
        remove_database(&path);

        match outcome {
            Outcome::Compacted {
                before,
                after: size,
            } => assert!(size < before / 4),
            other => panic!("expected compaction, got {other:?}"),
        }
        assert_eq!(after.auto_vacuum, 2);
        assert_eq!(after.freelist_count, 0);
    }
}
