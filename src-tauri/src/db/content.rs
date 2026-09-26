use crate::api::models::{Creator, CreatorProfile, Favorite, Post, PostRevision};
#[cfg(test)]
use crate::db::storage::prepare_connection;
use crate::db::storage::{
    content_cache_path, open_database, sanitize_cache_key, thumbs_cache_path,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

fn map_post_row(json: String, preview: Option<String>) -> Result<Post, String> {
    let mut post = Post::from_json_str(&json).map_err(|e| e.to_string())?;
    if let Some(path) = preview {
        if std::path::Path::new(&path).is_file() {
            post.preview_path = Some(path.clone());
            post.extra
                .insert("local_preview_path".into(), serde_json::Value::String(path));
        }
    }
    Ok(post)
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CreatorsQuery {
    pub query: Option<String>,
    pub services: Option<Vec<String>>,
    pub providers: Option<Vec<String>>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
    pub subscribed_only: Option<bool>,
    pub hide_ai: Option<bool>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatorsPageResult {
    pub items: Vec<Creator>,
    pub total: u64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    pub total_bytes: u64,
    pub metadata_bytes: u64,
    pub protected_bytes: u64,
    pub reclaimable_bytes: u64,
    pub preview_bytes: u64,
    pub avatar_bytes: u64,
    pub banner_bytes: u64,
    pub thumbnail_bytes: u64,
    pub other_bytes: u64,
    pub file_count: u64,
}

struct CacheFile {
    path: PathBuf,
    size: u64,
    modified_at: u64,
    protected: bool,
}

const CACHE_PRUNE_MIN_INTERVAL: Duration = Duration::from_secs(120);

const CACHE_PRUNE_FORCE_OVERSHOOT: u64 = 256 * 1024 * 1024;

pub struct ContentRepository {
    connection: Mutex<Connection>,
    cache_limit_bytes: AtomicU64,
    cache_bytes_since_prune: AtomicU64,
    last_prune_at: Mutex<Option<Instant>>,
}

impl ContentRepository {
    pub fn new(cache_limit_mb: u64) -> Result<Self, String> {
        Ok(Self::from_connection(open_database()?, cache_limit_mb))
    }

    fn from_connection(connection: Connection, cache_limit_mb: u64) -> Self {
        Self {
            connection: Mutex::new(connection),
            cache_limit_bytes: AtomicU64::new(cache_limit_mb.saturating_mul(1024 * 1024)),
            cache_bytes_since_prune: AtomicU64::new(0),
            last_prune_at: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn in_memory(cache_limit_mb: u64) -> Result<Self, String> {
        let mut connection = Connection::open_in_memory().map_err(|e| e.to_string())?;
        prepare_connection(&mut connection)?;
        Ok(Self::from_connection(connection, cache_limit_mb))
    }

    pub fn cache_stats(&self) -> Result<CacheStats, String> {
        let protected = self.protected_cache_paths()?;
        let files = scan_cache_files(&content_cache_path(), &protected)?;
        let mut stats = cache_stats_from_files(&files);
        stats.metadata_bytes = self.cached_metadata_bytes()?;
        stats.total_bytes = stats.total_bytes.saturating_add(stats.metadata_bytes);
        stats.reclaimable_bytes = stats.reclaimable_bytes.saturating_add(stats.metadata_bytes);
        Ok(stats)
    }

    pub fn set_cache_limit_mb(&self, max_mb: u64) -> Result<CacheStats, String> {
        let limit = max_mb.clamp(64, 2048).saturating_mul(1024 * 1024);
        self.cache_limit_bytes.store(limit, Ordering::Release);
        self.note_pruned();
        self.prune_cache(limit)
    }

    fn note_pruned(&self) {
        self.cache_bytes_since_prune.store(0, Ordering::Release);
        if let Ok(mut last) = self.last_prune_at.lock() {
            *last = Some(Instant::now());
        }
    }

    pub fn clear_cached_images(&self) -> Result<CacheStats, String> {
        self.remove_cached_images()?;
        self.compact();
        self.cache_stats()
    }

    fn compact(&self) {
        let Ok(connection) = self.connection.lock() else {
            return;
        };
        if let Err(error) =
            connection.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; PRAGMA optimize; VACUUM;")
        {
            tracing::warn!(%error, "Database compaction failed");
        }
    }

    fn remove_cached_images(&self) -> Result<(), String> {
        let root = content_cache_path();
        let files = scan_cache_files(&root, &HashSet::new())?;
        for file in files {
            if !file.path.starts_with(&root) {
                return Err("Refusing to remove a cache file outside the cache root".to_string());
            }
            match std::fs::remove_file(&file.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        let connection = self.connection.lock().map_err(|error| error.to_string())?;
        connection
            .execute("UPDATE posts SET preview_path=NULL", [])
            .map_err(|error| error.to_string())?;
        connection
            .execute("UPDATE creators SET avatar_path=NULL, banner_path=NULL", [])
            .map_err(|error| error.to_string())?;
        let _ = connection.execute("DELETE FROM thumb_refs", []);
        let _ = connection.execute("DELETE FROM thumb_blobs", []);
        drop(connection);
        let legacy_previews = root.join("previews");
        if legacy_previews.exists() {
            let _ = std::fs::remove_dir_all(&legacy_previews);
        }
        let thumbs = thumbs_cache_path();
        if thumbs.exists() {
            let _ = std::fs::remove_dir_all(&thumbs);
        }
        remove_empty_cache_dirs(&root)
    }

    pub fn clear_all_cache(&self) -> Result<CacheStats, String> {
        self.remove_cached_images()?;
        let mut connection = self.connection.lock().map_err(|error| error.to_string())?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute("DELETE FROM content_lists", [])
            .map_err(|error| error.to_string())?;
        transaction
            .execute("DELETE FROM content_documents", [])
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM posts AS post
                 WHERE NOT EXISTS (
                   SELECT 1 FROM content_pins pin
                   WHERE pin.entity_kind='post' AND pin.service=post.service
                     AND pin.creator_id=post.creator_id AND pin.post_id=post.post_id
                 )
                 AND NOT EXISTS (
                   SELECT 1 FROM collection_posts item
                   WHERE item.service=post.service AND item.creator_id=post.creator_id
                     AND item.post_id=post.post_id
                 )
                 AND NOT EXISTS (
                   SELECT 1 FROM download_jobs job
                   WHERE job.service=post.service AND job.creator_id=post.creator_id
                     AND job.post_id=post.post_id
                 )
                 AND NOT EXISTS (
                   SELECT 1 FROM subscriptions sub
                   WHERE sub.service=post.service AND sub.creator_id=post.creator_id
                 )",
                [],
            )
            .map_err(|error| error.to_string())?;
        // Creators without posts are the Creators tab directory, not cache.
        transaction.commit().map_err(|error| error.to_string())?;
        drop(connection);
        self.compact();
        self.cache_stats()
    }

    pub fn wipe_all_data(&self) -> Result<CacheStats, String> {
        let root = content_cache_path();
        if root.exists() {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::create_dir_all(&root);
        }
        let mut connection = self.connection.lock().map_err(|error| error.to_string())?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute("DELETE FROM collection_posts", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM collections WHERE is_system = 0", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM subscription_seen_posts", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM subscriptions", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM content_pins", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM content_lists", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM content_documents", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM download_blob_refs", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM download_jobs", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM media_blobs", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM posts", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM creators", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM sync_conflicts", [])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM sync_records", [])
            .map_err(|e| e.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        drop(connection);
        self.compact();
        self.cache_stats()
    }

    fn cached_metadata_bytes(&self) -> Result<u64, String> {
        let connection = self.connection.lock().map_err(|error| error.to_string())?;
        connection
            .query_row(
                "SELECT
                   COALESCE((SELECT SUM(length(CAST(identities_json AS BLOB))) FROM content_lists), 0) +
                   COALESCE((SELECT SUM(length(CAST(snapshot_json AS BLOB))) FROM content_documents), 0) +
                   COALESCE((
                     SELECT SUM(
                       length(CAST(snapshot_json AS BLOB)) +
                       length(CAST(title AS BLOB)) +
                       length(CAST(COALESCE(content,'') AS BLOB))
                     )
                     FROM posts post
                     WHERE NOT EXISTS (
                       SELECT 1 FROM content_pins pin
                       WHERE pin.entity_kind='post' AND pin.service=post.service
                         AND pin.creator_id=post.creator_id AND pin.post_id=post.post_id
                     )
                     AND NOT EXISTS (
                       SELECT 1 FROM collection_posts item
                       WHERE item.service=post.service AND item.creator_id=post.creator_id
                         AND item.post_id=post.post_id
                     )
                     AND NOT EXISTS (
                       SELECT 1 FROM download_jobs job
                       WHERE job.service=post.service AND job.creator_id=post.creator_id
                         AND job.post_id=post.post_id
                     )
                     AND NOT EXISTS (
                       SELECT 1 FROM subscriptions sub
                       WHERE sub.service=post.service AND sub.creator_id=post.creator_id
                     )
                   ), 0)",
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| error.to_string())
    }

    fn enforce_cache_limit_after_write(&self, written_bytes: u64) {
        let pending = self
            .cache_bytes_since_prune
            .fetch_add(written_bytes, Ordering::AcqRel)
            .saturating_add(written_bytes);

        if !self.should_prune_now(pending) {
            return;
        }

        self.cache_bytes_since_prune.store(0, Ordering::Release);
        self.enforce_cache_limit();
    }

    fn should_prune_now(&self, pending_bytes: u64) -> bool {
        let Ok(mut last) = self.last_prune_at.lock() else {
            return false;
        };
        let now = Instant::now();
        let due = match *last {
            Some(previous) => now.duration_since(previous) >= CACHE_PRUNE_MIN_INTERVAL,
            None => true,
        };
        if due || pending_bytes >= CACHE_PRUNE_FORCE_OVERSHOOT {
            *last = Some(now);
            true
        } else {
            false
        }
    }

    fn enforce_cache_limit(&self) {
        let limit = self.cache_limit_bytes.load(Ordering::Acquire);
        let _ = self.prune_cache(limit);
    }

    fn prune_cache(&self, target_bytes: u64) -> Result<CacheStats, String> {
        let root = content_cache_path();
        let protected = self.protected_cache_paths()?;
        let mut files = scan_cache_files(&root, &protected)?;
        let files_bytes = files.iter().map(|file| file.size).sum::<u64>();
        let metadata_bytes = self.cached_metadata_bytes().unwrap_or(0);
        let mut total = files_bytes.saturating_add(metadata_bytes);
        files.sort_by_key(|file| file.modified_at);

        let mut removed = Vec::new();
        for file in files.iter().filter(|file| !file.protected) {
            if total <= target_bytes {
                break;
            }
            if !file.path.starts_with(&root) {
                return Err("Refusing to remove a cache file outside the cache root".to_string());
            }
            match std::fs::remove_file(&file.path) {
                Ok(()) => {
                    total = total.saturating_sub(file.size);
                    removed.push(file.path.clone());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    total = total.saturating_sub(file.size);
                    removed.push(file.path.clone());
                }
                Err(error) => return Err(error.to_string()),
            }
        }

        if !removed.is_empty() {
            let connection = self.connection.lock().map_err(|error| error.to_string())?;
            let thumbs_dir = thumbs_cache_path();
            for path in removed {
                let value = path.to_string_lossy();
                connection
                    .execute(
                        "UPDATE posts SET preview_path=NULL WHERE preview_path=?1",
                        [&value],
                    )
                    .map_err(|error| error.to_string())?;
                connection
                    .execute(
                        "UPDATE creators SET avatar_path=NULL WHERE avatar_path=?1",
                        [&value],
                    )
                    .map_err(|error| error.to_string())?;
                connection
                    .execute(
                        "UPDATE creators SET banner_path=NULL WHERE banner_path=?1",
                        [&value],
                    )
                    .map_err(|error| error.to_string())?;

                if let Ok(rel) = path.strip_prefix(&thumbs_dir) {
                    let rel_norm = rel.to_string_lossy().replace('\\', "/");
                    let _ = connection.execute(
                        "DELETE FROM thumb_blobs WHERE relative_path=?1",
                        [&rel_norm],
                    );
                }
            }
            drop(connection);
            remove_empty_cache_dirs(&root)?;
        }

        if total > target_bytes || metadata_bytes > (target_bytes / 2) {
            let mut connection = self.connection.lock().map_err(|error| error.to_string())?;
            let tx = connection
                .transaction()
                .map_err(|error| error.to_string())?;
            let _ = tx.execute(
                "DELETE FROM content_lists WHERE datetime(cached_at) < datetime('now', '-7 days')",
                [],
            );
            let _ = tx.execute(
                "DELETE FROM posts WHERE rowid IN (
                    SELECT post.rowid FROM posts post
                    WHERE NOT EXISTS (
                        SELECT 1 FROM content_pins pin
                        WHERE pin.entity_kind='post' AND pin.service=post.service
                          AND pin.creator_id=post.creator_id AND pin.post_id=post.post_id
                    )
                    AND NOT EXISTS (
                        SELECT 1 FROM collection_posts item
                        WHERE item.service=post.service AND item.creator_id=post.creator_id
                          AND item.post_id=post.post_id
                    )
                    AND NOT EXISTS (
                        SELECT 1 FROM download_jobs job
                        WHERE job.service=post.service AND job.creator_id=post.creator_id
                          AND job.post_id=post.post_id
                    )
                    AND NOT EXISTS (
                        SELECT 1 FROM subscriptions sub
                        WHERE sub.service=post.service AND sub.creator_id=post.creator_id
                    )
                    ORDER BY post.cached_at ASC
                    LIMIT 5000
                )",
                [],
            );
            tx.commit().map_err(|error| error.to_string())?;
            let _ = connection.execute_batch("PRAGMA optimize; PRAGMA incremental_vacuum;");
        }

        self.cache_stats()
    }

    fn protected_cache_paths(&self) -> Result<HashSet<PathBuf>, String> {
        let connection = self.connection.lock().map_err(|error| error.to_string())?;
        let mut statement = connection
            .prepare(
                "SELECT p.preview_path FROM posts p
                 WHERE p.preview_path IS NOT NULL AND (
                   EXISTS (SELECT 1 FROM content_pins pin WHERE pin.service=p.service AND pin.creator_id=p.creator_id AND pin.entity_kind='post' AND pin.post_id=p.post_id)
                   OR EXISTS (SELECT 1 FROM collection_posts cp WHERE cp.service=p.service AND cp.creator_id=p.creator_id AND cp.post_id=p.post_id)
                   OR EXISTS (SELECT 1 FROM download_jobs job WHERE job.service=p.service AND job.creator_id=p.creator_id AND job.post_id=p.post_id)
                   OR EXISTS (SELECT 1 FROM subscriptions sub WHERE sub.service=p.service AND sub.creator_id=p.creator_id)
                 )
                 UNION ALL
                 SELECT c.avatar_path FROM creators c
                 WHERE c.avatar_path IS NOT NULL AND (
                   EXISTS (SELECT 1 FROM content_pins pin WHERE pin.service=c.service AND pin.creator_id=c.creator_id)
                   OR EXISTS (SELECT 1 FROM collection_posts cp WHERE cp.service=c.service AND cp.creator_id=c.creator_id)
                   OR EXISTS (SELECT 1 FROM download_jobs job WHERE job.service=c.service AND job.creator_id=c.creator_id)
                   OR EXISTS (SELECT 1 FROM subscriptions sub WHERE sub.service=c.service AND sub.creator_id=c.creator_id)
                 )
                 UNION ALL
                 SELECT c.banner_path FROM creators c
                 WHERE c.banner_path IS NOT NULL AND (
                   EXISTS (SELECT 1 FROM content_pins pin WHERE pin.service=c.service AND pin.creator_id=c.creator_id)
                   OR EXISTS (SELECT 1 FROM collection_posts cp WHERE cp.service=c.service AND cp.creator_id=c.creator_id)
                   OR EXISTS (SELECT 1 FROM download_jobs job WHERE job.service=c.service AND job.creator_id=c.creator_id)
                   OR EXISTS (SELECT 1 FROM subscriptions sub WHERE sub.service=c.service AND sub.creator_id=c.creator_id)
                 )",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        let mut paths: HashSet<PathBuf> = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(PathBuf::from)
            .collect();

        let mut thumb_stmt = connection
            .prepare(
                "SELECT b.relative_path FROM thumb_refs r
                 JOIN thumb_blobs b ON r.content_key = b.content_key
                 WHERE (
                   EXISTS (SELECT 1 FROM content_pins pin WHERE pin.service=r.service AND pin.creator_id=r.creator_id AND pin.post_id=r.post_id)
                   OR EXISTS (SELECT 1 FROM collection_posts cp WHERE cp.service=r.service AND cp.creator_id=r.creator_id AND cp.post_id=r.post_id)
                   OR EXISTS (SELECT 1 FROM download_jobs job WHERE job.service=r.service AND job.creator_id=r.creator_id AND job.post_id=r.post_id)
                   OR EXISTS (SELECT 1 FROM content_pins pin WHERE pin.service=r.service AND pin.creator_id=r.creator_id AND pin.entity_kind='creator')
                   OR EXISTS (SELECT 1 FROM download_jobs job WHERE job.media_id=r.media_id)
                   OR EXISTS (SELECT 1 FROM subscriptions sub WHERE sub.service=r.service AND sub.creator_id=r.creator_id)
                 )",
            )
            .map_err(|error| error.to_string())?;
        let thumb_rows = thumb_stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        let thumbs_dir = thumbs_cache_path();
        for rel in thumb_rows.flatten() {
            paths.insert(thumbs_dir.join(rel));
        }

        Ok(paths)
    }

    pub fn save_posts(&self, posts: &[Post]) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let tx = connection.transaction().map_err(|e| e.to_string())?;
        for post in posts {
            let mut clean_post = post.clone();
            clean_post.clean_extra();
            tx.execute(
                "INSERT INTO creators(service, creator_id, name, snapshot_json)
                 VALUES(?1, ?2, ?2, json_object('id', ?2, 'name', ?2, 'service', ?1))
                 ON CONFLICT(service, creator_id) DO NOTHING",
                params![clean_post.service, clean_post.user],
            )
            .map_err(|e| e.to_string())?;
            let snapshot = serde_json::to_string(&clean_post).map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO posts(service, creator_id, post_id, title, content, published_at, snapshot_json, preview_path, last_checked_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,CURRENT_TIMESTAMP)
                 ON CONFLICT(service, creator_id, post_id) DO UPDATE SET
                    title=excluded.title, content=excluded.content, published_at=excluded.published_at,
                    preview_path=COALESCE(posts.preview_path, excluded.preview_path),
                    snapshot_json=CASE
                        WHEN json_extract(posts.snapshot_json, '$.detail_fetched') = 1 AND (json_extract(excluded.snapshot_json, '$.detail_fetched') IS NULL OR json_extract(excluded.snapshot_json, '$.detail_fetched') = 0)
                        THEN posts.snapshot_json
                        ELSE excluded.snapshot_json
                    END,
                    remote_state='active',
                    cached_at=CURRENT_TIMESTAMP, last_checked_at=CURRENT_TIMESTAMP",
                params![clean_post.service, clean_post.user, clean_post.id, clean_post.title, clean_post.content, clean_post.published, snapshot, clean_post.preview_path],
            ).map_err(|e| e.to_string())?;

            let _ = tx.execute(
                "INSERT OR IGNORE INTO content_pins(entity_kind, service, creator_id, post_id, reason)
                 SELECT 'post', ?1, ?2, ?3, 'subscription'
                 FROM subscriptions WHERE service=?1 AND creator_id=?2",
                params![clean_post.service, clean_post.user, clean_post.id],
            );

            let _ = tx.execute(
                "INSERT OR IGNORE INTO content_pins(entity_kind, service, creator_id, post_id, reason)
                 SELECT 'post', ?1, ?2, ?3, 'library'
                 FROM collection_posts WHERE service=?1 AND creator_id=?2 AND post_id=?3",
                params![clean_post.service, clean_post.user, clean_post.id],
            );
        }
        tx.commit().map_err(|e| e.to_string())
    }

    pub fn get_post(
        &self,
        service: &str,
        creator_id: &str,
        post_id: &str,
    ) -> Result<Option<Post>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let row: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT snapshot_json,preview_path FROM posts WHERE service=?1 AND creator_id=?2 AND post_id=?3",
                params![service, creator_id, post_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        row.map(|(value, preview)| map_post_row(value, preview))
            .transpose()
    }

    pub fn get_post_raw_json(
        &self,
        service: &str,
        creator_id: &str,
        post_id: &str,
    ) -> Result<Option<String>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .query_row(
                "SELECT snapshot_json FROM posts WHERE service=?1 AND creator_id=?2 AND post_id=?3",
                params![service, creator_id, post_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn find_post_identity(
        &self,
        service: &str,
        post_id: &str,
        preferred_creator_id: Option<&str>,
    ) -> Result<Option<(String, String, String)>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .query_row(
                "SELECT service,creator_id,post_id FROM posts
                 WHERE service=?1 AND post_id=?2
                 ORDER BY CASE WHEN creator_id=COALESCE(?3,'') THEN 0 ELSE 1 END
                 LIMIT 1",
                params![service, post_id, preferred_creator_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn find_creator_by_alias(
        &self,
        service: &str,
        alias: &str,
    ) -> Result<Option<String>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .query_row(
                "SELECT creator_id FROM creators
                 WHERE service=?1 AND (
                   creator_id=?2 OR
                   name=?2 COLLATE NOCASE OR
                   CAST(json_extract(snapshot_json,'$.name') AS TEXT)=?2 COLLATE NOCASE OR
                   CAST(json_extract(snapshot_json,'$.public_id') AS TEXT)=?2 OR
                   CAST(json_extract(snapshot_json,'$.relation_id') AS TEXT)=?2
                 ) LIMIT 1",
                params![service, alias],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn list_creator_posts(
        &self,
        service: &str,
        creator_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Post>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut statement = connection
            .prepare(
                "SELECT snapshot_json, preview_path FROM posts WHERE service=?1 AND creator_id=?2
             ORDER BY published_at DESC, post_id DESC LIMIT ?3 OFFSET ?4",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![service, creator_id, limit, offset], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| e.to_string())?;
        rows.map(|row| {
            row.map_err(|e| e.to_string())
                .and_then(|(json, preview)| map_post_row(json, preview))
        })
        .collect()
    }

    pub fn search_posts(&self, query: &str) -> Result<Vec<Post>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let pattern = format!("%{query}%");
        let mut statement = connection.prepare(
            "SELECT snapshot_json, preview_path FROM posts WHERE title LIKE ?1 OR content LIKE ?1 ORDER BY published_at DESC LIMIT 100"
        ).map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![pattern], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| e.to_string())?;
        rows.map(|row| {
            row.map_err(|e| e.to_string())
                .and_then(|(json, preview)| map_post_row(json, preview))
        })
        .collect()
    }

    pub fn list_recent_posts(&self, offset: u32, limit: u32) -> Result<Vec<Post>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut statement = connection.prepare("SELECT snapshot_json, preview_path FROM posts ORDER BY published_at DESC, post_id DESC LIMIT ?1 OFFSET ?2").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![limit, offset], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(|e| e.to_string())?;
        rows.map(|row| {
            row.map_err(|e| e.to_string())
                .and_then(|(json, preview)| map_post_row(json, preview))
        })
        .collect()
    }

    pub fn save_post_list(
        &self,
        list_key: &str,
        offset: u32,
        posts: &[Post],
    ) -> Result<(), String> {
        self.save_posts(posts)?;
        let identities: Vec<(&str, &str, &str)> = posts
            .iter()
            .map(|post| (post.service.as_str(), post.user.as_str(), post.id.as_str()))
            .collect();
        let json = serde_json::to_string(&identities).map_err(|e| e.to_string())?;
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .execute(
                "INSERT INTO content_lists(list_key,page_offset,identities_json) VALUES(?1,?2,?3)
                 ON CONFLICT(list_key,page_offset) DO UPDATE SET
                   identities_json=excluded.identities_json,cached_at=CURRENT_TIMESTAMP",
                params![list_key, offset, json],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_post_list_age_secs(
        &self,
        list_key: &str,
        offset: u32,
    ) -> Result<Option<i64>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .query_row(
                "SELECT CAST(strftime('%s', 'now') - strftime('%s', cached_at) AS INTEGER)
                 FROM content_lists WHERE list_key=?1 AND page_offset=?2",
                params![list_key, offset],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn load_post_list(&self, list_key: &str, offset: u32) -> Result<Vec<Post>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let json: Option<String> = connection
            .query_row(
                "SELECT identities_json FROM content_lists WHERE list_key=?1 AND page_offset=?2",
                params![list_key, offset],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some(json) = json else {
            return Ok(Vec::new());
        };
        let identities: Vec<(String, String, String)> =
            serde_json::from_str(&json).map_err(|e| e.to_string())?;
        if identities.is_empty() {
            return Ok(Vec::new());
        }

        let mut map: HashMap<(String, String, String), Post> =
            HashMap::with_capacity(identities.len());
        for chunk in identities.chunks(50) {
            let mut sql = String::from(
                "SELECT service, creator_id, post_id, snapshot_json, preview_path FROM posts WHERE ",
            );
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push_str(" OR ");
                }
                sql.push_str("(service = ? AND creator_id = ? AND post_id = ?)");
            }
            let mut stmt = connection.prepare(&sql).map_err(|e| e.to_string())?;
            let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() * 3);
            for (s, c, p) in chunk {
                params_vec.push(s);
                params_vec.push(c);
                params_vec.push(p);
            }
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params_vec), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (service, creator_id, post_id, snapshot, preview) =
                    row.map_err(|e| e.to_string())?;
                if let Ok(post) = map_post_row(snapshot, preview) {
                    map.insert((service, creator_id, post_id), post);
                }
            }
        }

        let mut posts = Vec::with_capacity(identities.len());
        for identity in identities {
            if let Some(post) = map.remove(&identity) {
                posts.push(post);
            }
        }
        Ok(posts)
    }

    pub fn save_document<T: serde::Serialize>(
        &self,
        document_kind: &str,
        service: &str,
        creator_id: &str,
        post_id: &str,
        value: &T,
    ) -> Result<(), String> {
        let snapshot = serde_json::to_string(value).map_err(|e| e.to_string())?;
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .execute(
                "INSERT INTO content_documents(document_kind,service,creator_id,post_id,snapshot_json)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(document_kind,service,creator_id,post_id) DO UPDATE SET
                   snapshot_json=excluded.snapshot_json,cached_at=CURRENT_TIMESTAMP",
                params![document_kind, service, creator_id, post_id, snapshot],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_document<T: DeserializeOwned>(
        &self,
        document_kind: &str,
        service: &str,
        creator_id: &str,
        post_id: &str,
    ) -> Result<Option<T>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let snapshot: Option<String> = connection
            .query_row(
                "SELECT snapshot_json FROM content_documents
                 WHERE document_kind=?1 AND service=?2 AND creator_id=?3 AND post_id=?4",
                params![document_kind, service, creator_id, post_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        snapshot
            .map(|json| serde_json::from_str(&json).map_err(|e| e.to_string()))
            .transpose()
    }

    pub fn save_post_revisions(
        &self,
        service: &str,
        creator_id: &str,
        post_id: &str,
        provider_id: &str,
        revisions: &[PostRevision],
    ) -> Result<(), String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        for rev in revisions {
            let snapshot = serde_json::to_string(&rev.post).map_err(|e| e.to_string())?;
            connection
                .execute(
                    "INSERT INTO post_revisions(service,creator_id,post_id,revision_id,provider_id,imported_at,edited_at,snapshot_json)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                     ON CONFLICT(service,creator_id,post_id,revision_id,provider_id) DO UPDATE SET
                       snapshot_json=excluded.snapshot_json,
                       imported_at=excluded.imported_at,
                       edited_at=excluded.edited_at",
                    params![
                        service,
                        creator_id,
                        post_id,
                        rev.revision_id,
                        provider_id,
                        rev.post.added,
                        rev.post.edited,
                        snapshot
                    ],
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn load_post_revisions(
        &self,
        service: &str,
        creator_id: &str,
        post_id: &str,
    ) -> Result<Vec<PostRevision>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut statement = connection
            .prepare(
                "SELECT revision_id, snapshot_json, provider_id FROM post_revisions
                 WHERE service=?1 AND creator_id=?2 AND post_id=?3
                 ORDER BY revision_id DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![service, creator_id, post_id], |row| {
                let revision_id: i64 = row.get(0)?;
                let json: String = row.get(1)?;
                let provider_id: Option<String> = row.get(2).ok();
                let mut post: Post = Post::from_json_str(&json).unwrap_or_else(|_| Post {
                    id: post_id.to_string(),
                    user: creator_id.to_string(),
                    service: service.to_string(),
                    title: String::new(),
                    content: None,
                    substring: None,
                    published: None,
                    added: None,
                    edited: None,
                    embed: None,
                    shared_file: None,
                    attachments: None,
                    file: None,
                    poll: None,
                    captions: None,
                    tags: None,
                    origin: None,
                    preview_state: None,
                    has_full: None,
                    detail_fetched: None,
                    next: None,
                    prev: None,
                    favorite_count: None,
                    attachment_count: None,
                    thumbnail_url: None,
                    media_url: None,
                    page_url: None,
                    preview_path: None,
                    cloud_urls: Vec::new(),
                    extra: Default::default(),
                });
                if let Some(pid) = provider_id {
                    post.extra
                        .entry("provider_id".to_string())
                        .or_insert_with(|| serde_json::Value::String(pid));
                }
                Ok(PostRevision { revision_id, post })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn save_creator(&self, creator: &CreatorProfile) -> Result<(), String> {
        self.save_creator_json(&creator.service, &creator.id, &creator.name, creator)
    }

    fn extract_creator_favorited(creator: &Creator) -> i64 {
        if let Some(f) = creator.favorited {
            return f as i64;
        }
        if let Some(v) = creator
            .extra
            .get("favorited")
            .or_else(|| creator.extra.get("kemono_favorited"))
            .or_else(|| creator.extra.get("favorite_count"))
        {
            if let Some(num) = v.as_i64() {
                return num;
            }
            if let Some(num) = v.as_u64() {
                return num as i64;
            }
            if let Some(s) = v.as_str() {
                if let Ok(num) = s.parse::<i64>() {
                    return num;
                }
            }
        }
        0
    }

    fn extract_creator_timestamp(t: Option<i64>) -> i64 {
        match t {
            Some(val) if val > 10_000_000_000 => val / 1000,
            Some(val) => val,
            None => 0,
        }
    }

    fn is_ai_creator(creator: &Creator) -> bool {
        let lower_name = creator.name.to_lowercase();
        if lower_name.contains("[ai]") || lower_name.contains("(ai)") {
            return true;
        }
        if let Some(serde_json::Value::Array(tags)) = creator.extra.get("tags") {
            for t in tags {
                if let Some(s) = t.as_str() {
                    let l = s.to_lowercase();
                    if l == "ai"
                        || l.contains("ai generated")
                        || l.contains("artificial intelligence")
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn save_creators(&self, creators: &[Creator]) -> Result<(), String> {
        struct PreparedCreator {
            service: String,
            id: String,
            name: String,
            snapshot: String,
            favorited: i64,
            updated_at: i64,
            indexed_at: i64,
            is_ai: i64,
        }

        let mut prepared = Vec::with_capacity(creators.len());
        for c in creators {
            let snapshot = crate::db::storage::snapshot_json(c)?;
            let favorited = Self::extract_creator_favorited(c);
            let updated_at = Self::extract_creator_timestamp(c.updated);
            let indexed_at = Self::extract_creator_timestamp(c.indexed);
            let is_ai = if Self::is_ai_creator(c) { 1 } else { 0 };

            prepared.push(PreparedCreator {
                service: c.service.clone(),
                id: c.id.clone(),
                name: c.name.clone(),
                snapshot,
                favorited,
                updated_at,
                indexed_at,
                is_ai,
            });
        }

        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let tx = connection.transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO creators(service, creator_id, name, snapshot_json, favorited, updated_at, indexed_at, is_ai, last_checked_at)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, CURRENT_TIMESTAMP)
                     ON CONFLICT(service, creator_id) DO UPDATE SET
                       name = excluded.name,
                       snapshot_json = excluded.snapshot_json,
                       favorited = excluded.favorited,
                       updated_at = excluded.updated_at,
                       indexed_at = excluded.indexed_at,
                       is_ai = excluded.is_ai,
                       cached_at = CURRENT_TIMESTAMP,
                       last_checked_at = CURRENT_TIMESTAMP",
                )
                .map_err(|e| e.to_string())?;

            for p in &prepared {
                stmt.execute(params![
                    &p.service,
                    &p.id,
                    &p.name,
                    &p.snapshot,
                    p.favorited,
                    p.updated_at,
                    p.indexed_at,
                    p.is_ai,
                ])
                .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list_creators(&self) -> Result<Vec<Creator>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut statement = connection
            .prepare("SELECT snapshot_json, creator_id, name, service FROM creators ORDER BY name COLLATE NOCASE")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |r| {
                let json_str: String = r.get(0)?;
                if let Ok(creator) = serde_json::from_str::<Creator>(&json_str) {
                    Ok(creator)
                } else {
                    Ok(Creator {
                        id: r.get(1)?,
                        name: r.get(2)?,
                        service: r.get(3)?,
                        public_id: None,
                        relation_id: None,
                        indexed: None,
                        updated: None,
                        favorited: None,
                        ever_imported: None,
                        avatar_url: None,
                        avatar_path: None,
                        banner_url: None,
                        banner_path: None,
                        page_url: None,
                        extra: Default::default(),
                    })
                }
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn query_creators(&self, query: &CreatorsQuery) -> Result<CreatorsPageResult, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;

        let is_subscribed = query.subscribed_only.unwrap_or(false);
        let from_clause = if is_subscribed {
            "FROM creators c INNER JOIN subscriptions s ON s.service = c.service AND s.creator_id = c.creator_id"
        } else {
            "FROM creators c"
        };

        let mut where_conditions = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(services) = &query.services {
            let active_services: Vec<&String> =
                services.iter().filter(|s| !s.trim().is_empty()).collect();
            if !active_services.is_empty() {
                let placeholders = active_services
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");
                where_conditions.push(format!("c.service IN ({placeholders})"));
                for s in active_services {
                    params.push(Box::new(s.to_lowercase()));
                }
            }
        }

        if let Some(providers) = &query.providers {
            let active_providers: Vec<&String> =
                providers.iter().filter(|p| !p.trim().is_empty()).collect();
            if !active_providers.is_empty() {
                let placeholders = active_providers
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");
                where_conditions.push(format!(
                    "COALESCE(json_extract(c.snapshot_json, '$.provider_id'), json_extract(c.snapshot_json, '$.extra.provider_id'), 'pawchive') IN ({placeholders})"
                ));
                for p in active_providers {
                    params.push(Box::new(p.to_lowercase()));
                }
            }
        }

        if let Some(q) = &query.query {
            let trimmed = q.trim();
            if !trimmed.is_empty() {
                where_conditions.push("(c.name LIKE ? OR c.creator_id LIKE ?)".to_string());
                let pattern = format!("%{trimmed}%");
                params.push(Box::new(pattern.clone()));
                params.push(Box::new(pattern));
            }
        }

        if query.hide_ai.unwrap_or(false) {
            where_conditions.push("c.is_ai = 0".to_string());
        }

        let where_clause = if where_conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_conditions.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) {from_clause} {where_clause}");
        let count_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: u64 = connection
            .query_row(&count_sql, count_params.as_slice(), |r| r.get(0))
            .unwrap_or(0);

        let sort_by = query.sort_by.as_deref().unwrap_or("favorited");
        let sort_order = query.sort_order.as_deref().unwrap_or("desc").to_lowercase();
        let order_dir = if sort_order == "asc" { "ASC" } else { "DESC" };

        let order_clause = match sort_by {
            "name" => format!("ORDER BY c.name COLLATE NOCASE {order_dir}"),
            "updated" => format!("ORDER BY c.updated_at {order_dir}, c.name COLLATE NOCASE ASC"),
            "indexed" => format!("ORDER BY c.indexed_at {order_dir}, c.name COLLATE NOCASE ASC"),
            _ => format!(
                "ORDER BY c.favorited {order_dir}, c.updated_at DESC, c.name COLLATE NOCASE ASC"
            ),
        };

        let limit = query.limit.unwrap_or(80).clamp(1, 200);
        let offset = query.offset.unwrap_or(0);

        let select_sql = format!(
            "SELECT c.snapshot_json, c.creator_id, c.name, c.service, c.avatar_path, c.banner_path {from_clause} {where_clause} {order_clause} LIMIT ? OFFSET ?"
        );

        let mut query_params: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        query_params.push(&limit);
        query_params.push(&offset);

        let mut stmt = connection.prepare(&select_sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(query_params.as_slice(), |r| {
                let json_str: String = r.get(0)?;
                let raw_avatar_path: Option<String> = r.get(4)?;
                let raw_banner_path: Option<String> = r.get(5)?;

                let mut creator = if let Ok(c) = serde_json::from_str::<Creator>(&json_str) {
                    c
                } else {
                    Creator {
                        id: r.get(1)?,
                        name: r.get(2)?,
                        service: r.get(3)?,
                        public_id: None,
                        relation_id: None,
                        indexed: None,
                        updated: None,
                        favorited: None,
                        ever_imported: None,
                        avatar_url: None,
                        avatar_path: None,
                        banner_url: None,
                        banner_path: None,
                        page_url: None,
                        extra: Default::default(),
                    }
                };

                if let Some(path_str) = raw_avatar_path {
                    if std::path::Path::new(&path_str).is_file() {
                        creator.avatar_path = Some(path_str);
                    }
                }
                if let Some(path_str) = raw_banner_path {
                    if std::path::Path::new(&path_str).is_file() {
                        creator.banner_path = Some(path_str);
                    }
                }

                Ok(creator)
            })
            .map_err(|e| e.to_string())?;

        let items: Vec<_> = rows.flatten().collect();

        let has_more = (offset as u64 + items.len() as u64) < total;

        Ok(CreatorsPageResult {
            items,
            total,
            has_more,
        })
    }

    pub fn list_creator_services(&self) -> Result<Vec<String>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut stmt = connection
            .prepare(
                "SELECT DISTINCT service FROM creators WHERE service != '' ORDER BY service ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let services: Vec<String> = rows.flatten().collect();
        Ok(services)
    }

    pub fn list_creator_names(&self) -> Result<HashMap<String, String>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let mut stmt = connection
            .prepare("SELECT service, creator_id, name FROM creators")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                let service: String = r.get(0)?;
                let id: String = r.get(1)?;
                let name: String = r.get(2)?;
                Ok((
                    format!("{}:{}", service.to_lowercase(), id.to_lowercase()),
                    name,
                ))
            })
            .map_err(|e| e.to_string())?;

        let map: HashMap<String, String> = rows.flatten().collect();
        Ok(map)
    }

    pub fn get_creator_name(
        &self,
        service: &str,
        creator_id: &str,
    ) -> Result<Option<String>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let name: Option<String> = connection
            .query_row(
                "SELECT name FROM creators WHERE service = ?1 AND creator_id = ?2",
                params![service, creator_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(name)
    }

    fn save_creator_json<T: serde::Serialize>(
        &self,
        service: &str,
        id: &str,
        name: &str,
        creator: &T,
    ) -> Result<(), String> {
        let snapshot = crate::db::storage::snapshot_json(creator)?;
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection.execute(
            "INSERT INTO creators(service,creator_id,name,snapshot_json,last_checked_at)
             VALUES(?1,?2,?3,?4,CURRENT_TIMESTAMP)
             ON CONFLICT(service,creator_id) DO UPDATE SET name=excluded.name,
               snapshot_json=excluded.snapshot_json,cached_at=CURRENT_TIMESTAMP,last_checked_at=CURRENT_TIMESTAMP",
            params![service,id,name,snapshot]
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_creator(
        &self,
        service: &str,
        creator_id: &str,
    ) -> Result<Option<CreatorProfile>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let row: Option<(String, Option<String>, Option<String>)> = connection
            .query_row(
                "SELECT snapshot_json, avatar_path, banner_path FROM creators WHERE service=?1 AND creator_id=?2",
                params![service, creator_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;

        if let Some((json, avatar_path, banner_path)) = row {
            let mut profile: CreatorProfile =
                serde_json::from_str(&json).map_err(|e| e.to_string())?;
            if let Some(path_str) = avatar_path {
                if std::path::Path::new(&path_str).is_file() {
                    profile.avatar_path = Some(path_str);
                }
            }
            if let Some(path_str) = banner_path {
                if std::path::Path::new(&path_str).is_file() {
                    profile.banner_path = Some(path_str);
                }
            }
            Ok(Some(profile))
        } else {
            Ok(None)
        }
    }

    pub fn pin_post(&self, post: &Post, reason: &str, account_id: &str) -> Result<(), String> {
        self.save_posts(std::slice::from_ref(post))?;
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection.execute(
            "INSERT OR IGNORE INTO content_pins(entity_kind,service,creator_id,post_id,reason,account_id) VALUES('post',?1,?2,?3,?4,?5)",
            params![post.service,post.user,post.id,reason,account_id]
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_pin(
        &self,
        kind: &str,
        service: &str,
        creator_id: &str,
        post_id: Option<&str>,
        reason: &str,
        account_id: &str,
        active: bool,
    ) -> Result<(), String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let post_id = post_id.unwrap_or("");
        if active {
            connection.execute(
                "INSERT OR IGNORE INTO content_pins(entity_kind,service,creator_id,post_id,reason,account_id) VALUES(?1,?2,?3,?4,?5,?6)",
                params![kind,service,creator_id,post_id,reason,account_id]
            ).map_err(|e| e.to_string())?;
        } else {
            connection.execute(
                "DELETE FROM content_pins WHERE entity_kind=?1 AND service=?2 AND creator_id=?3 AND post_id=?4 AND reason=?5 AND (account_id=?6 OR account_id='' OR ?6='')",
                params![kind,service,creator_id,post_id,reason,account_id]
            ).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn list_favorites(&self, kind: &str, account_id: &str) -> Result<Vec<Favorite>, String> {
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let (sql, entity_kind) = if kind == "artist" {
            (
                "SELECT c.snapshot_json, MAX(pin.created_at) AS faved_at, c.avatar_path, c.banner_path, c.service, c.creator_id FROM content_pins pin JOIN creators c USING(service,creator_id) WHERE pin.entity_kind=?1 AND pin.reason='favorite' AND (pin.account_id=?2 OR (?2 != '' AND pin.account_id='')) GROUP BY c.service, c.creator_id ORDER BY faved_at DESC",
                "creator",
            )
        } else {
            (
                "SELECT p.snapshot_json, MAX(pin.created_at) AS faved_at, p.preview_path, NULL, p.service, p.creator_id FROM content_pins pin JOIN posts p USING(service,creator_id,post_id) WHERE pin.entity_kind=?1 AND pin.reason='favorite' AND (pin.account_id=?2 OR (?2 != '' AND pin.account_id='')) GROUP BY p.service, p.creator_id, p.post_id ORDER BY faved_at DESC",
                "post",
            )
        };
        let mut statement = connection.prepare(sql).map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![entity_kind, account_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut favorites = Vec::new();
        let cache_root = content_cache_path();
        let avatars_dir = cache_root.join("avatars");
        for item in rows.flatten() {
            let (json, created_at_str, avatar_or_preview, banner_path, srv, creator_id) = item;
            match serde_json::from_str::<Favorite>(&json) {
                Ok(mut fav) => {
                    if let Some(created_str) = created_at_str {
                        fav.extra.insert(
                            "faved_at".to_string(),
                            serde_json::Value::String(created_str),
                        );
                    }
                    if kind == "artist" {
                        let valid_avatar = if let Some(ref av) = avatar_or_preview {
                            if std::path::Path::new(av).is_file() {
                                Some(av.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        let resolved_avatar = valid_avatar.or_else(|| {
                            let s = sanitize_cache_key(&srv);
                            let id = sanitize_cache_key(&creator_id);
                            for ext in &["jpg", "jpeg", "png", "webp", "gif"] {
                                let direct = avatars_dir.join(format!("{s}_{id}_avatar.{ext}"));
                                if direct.is_file() {
                                    return Some(direct.to_string_lossy().into_owned());
                                }
                            }
                            if let Ok(entries) = std::fs::read_dir(&avatars_dir) {
                                let prefix = format!("{s}_{id}_");
                                for entry in entries.flatten() {
                                    let name = entry.file_name().to_string_lossy().into_owned();
                                    if name.starts_with(&prefix) && name.contains("avatar") {
                                        let p = entry.path();
                                        if p.is_file() {
                                            return Some(p.to_string_lossy().into_owned());
                                        }
                                    }
                                }
                            }
                            None
                        });

                        if let Some(av) = resolved_avatar {
                            fav.extra
                                .insert("avatar_path".to_string(), serde_json::Value::String(av));
                        }
                        if let Some(ref bn) = banner_path {
                            if std::path::Path::new(bn).is_file() {
                                fav.extra.insert(
                                    "banner_path".to_string(),
                                    serde_json::Value::String(bn.clone()),
                                );
                            }
                        }
                    } else if let Some(preview) = avatar_or_preview {
                        if std::path::Path::new(&preview).is_file() {
                            fav.extra.insert(
                                "preview_path".to_string(),
                                serde_json::Value::String(preview),
                            );
                        }
                    }
                    favorites.push(fav);
                }
                Err(e) => {
                    tracing::warn!("Failed to deserialize favorite {}: {}", entity_kind, e);
                }
            }
        }
        Ok(favorites)
    }

    pub fn remove_account_favorites(&self, account_id: &str) -> Result<usize, String> {
        if account_id.trim().is_empty() {
            return Ok(0);
        }
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        let affected = connection
            .execute(
                "DELETE FROM content_pins WHERE account_id = ?1 AND reason = 'favorite'",
                params![account_id.trim()],
            )
            .map_err(|e| e.to_string())?;
        Ok(affected)
    }

    pub fn store_artwork_data_url(
        &self,
        service: &str,
        creator_id: &str,
        kind: &str,
        data_url: &str,
    ) -> Result<PathBuf, String> {
        let (header, encoded) = data_url.split_once(',').ok_or("Invalid artwork data URL")?;
        let extension = if header.contains("png") {
            "png"
        } else if header.contains("webp") {
            "webp"
        } else if header.contains("gif") {
            "gif"
        } else {
            "jpg"
        };
        let bytes = BASE64_STANDARD.decode(encoded).map_err(|e| e.to_string())?;
        let dir = content_cache_path().join(if kind == "banner" {
            "banners"
        } else {
            "avatars"
        });
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let safe = format!(
            "{}_{}_{}.{}",
            sanitize_cache_key(service),
            sanitize_cache_key(creator_id),
            kind,
            extension
        );
        let path = dir.join(safe);
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        let column = if kind == "banner" {
            "banner_path"
        } else {
            "avatar_path"
        };
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .execute(
                &format!("UPDATE creators SET {column}=?3 WHERE service=?1 AND creator_id=?2"),
                params![service, creator_id, path.to_string_lossy()],
            )
            .map_err(|e| e.to_string())?;
        drop(connection);
        self.enforce_cache_limit_after_write(bytes.len() as u64);
        Ok(path)
    }

    pub fn artwork_path(
        &self,
        service: &str,
        creator_id: &str,
        kind: &str,
    ) -> Result<Option<String>, String> {
        let column = if kind == "banner" {
            "banner_path"
        } else {
            "avatar_path"
        };
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .query_row(
                &format!("SELECT {column} FROM creators WHERE service=?1 AND creator_id=?2"),
                params![service, creator_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())
            .map(|v| v.flatten())
    }

    pub fn artwork_data_url(
        &self,
        service: &str,
        creator_id: &str,
        kind: &str,
    ) -> Result<Option<String>, String> {
        let Some(path) = self.artwork_path(service, creator_id, kind)? else {
            return Ok(None);
        };
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Ok(None);
        }
        let mime = mime_guess::from_path(&path).first_or_octet_stream();
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        Ok(Some(format!(
            "data:{mime};base64,{}",
            BASE64_STANDARD.encode(bytes)
        )))
    }

    pub fn store_thumbnail_data_url(&self, key: &str, data_url: &str) -> Result<PathBuf, String> {
        let (_header, encoded) = data_url
            .split_once(',')
            .ok_or("Invalid thumbnail data URL")?;
        let bytes = BASE64_STANDARD.decode(encoded).map_err(|e| e.to_string())?;
        if bytes.len() < 256 {
            return Err("Thumbnail data is too small to be valid".to_string());
        }

        let hash = format!("{:x}", Sha256::digest(key.as_bytes()));
        let safe_name = sanitize_cache_key(key);
        let filename = if safe_name.len() > 100 {
            format!("{}_{}.webp", &safe_name[..40], &hash[..16])
        } else {
            format!("{safe_name}.webp")
        };
        let dir = thumbs_cache_path();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(&filename);
        let relative = filename.clone();
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;

        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .execute(
                "INSERT INTO thumb_blobs (content_key, size, relative_path, last_access_at)
                 VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
                 ON CONFLICT(content_key) DO UPDATE SET size = ?2, relative_path = ?3, last_access_at = CURRENT_TIMESTAMP",
                params![hash, bytes.len() as u64, relative],
            )
            .map_err(|e| e.to_string())?;

        let path_str = path.to_string_lossy();
        if let Some(post_key) = key.strip_prefix("post:") {
            let parts: Vec<&str> = post_key.split(':').collect();
            if parts.len() >= 3 {
                let service = parts[0];
                let creator_id = parts[1];
                let post_id = parts[2];
                let media_id = parts.get(3).copied().unwrap_or("");
                let _ = connection.execute(
                    "INSERT INTO thumb_refs (service, creator_id, post_id, media_id, content_key, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, CURRENT_TIMESTAMP)
                     ON CONFLICT(service, creator_id, post_id, media_id) DO UPDATE SET content_key = excluded.content_key",
                    params![service, creator_id, post_id, media_id, hash],
                );
                let _ = connection.execute(
                    "UPDATE posts SET preview_path=?1 WHERE service=?2 AND creator_id=?3 AND post_id=?4",
                    params![path_str, service, creator_id, post_id],
                );
            }
        } else {
            let _ = connection.execute(
                "INSERT INTO thumb_refs (service, creator_id, post_id, media_id, content_key, created_at)
                 VALUES ('', '', '', ?1, ?2, CURRENT_TIMESTAMP)
                 ON CONFLICT(service, creator_id, post_id, media_id) DO UPDATE SET content_key = excluded.content_key",
                params![key, hash],
            );
        }
        drop(connection);

        self.enforce_cache_limit_after_write(bytes.len() as u64);
        Ok(path)
    }

    pub fn thumbnail_path(&self, key: &str) -> Option<String> {
        let hash = format!("{:x}", Sha256::digest(key.as_bytes()));
        if let Ok(connection) = self.connection.lock() {
            let rel_opt: Option<String> = connection
                .query_row(
                    "SELECT relative_path FROM thumb_blobs WHERE content_key = ?1",
                    params![hash],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None);

            if let Some(rel) = rel_opt {
                let full = thumbs_cache_path().join(rel);
                if full.is_file() {
                    return Some(full.to_string_lossy().into_owned());
                }
            }

            if let Some(post_key) = key.strip_prefix("post:") {
                let parts: Vec<&str> = post_key.split(':').collect();
                if parts.len() == 3 {
                    let rel_ref: Option<String> = connection
                        .query_row(
                            "SELECT b.relative_path FROM thumb_refs r
                             JOIN thumb_blobs b ON r.content_key = b.content_key
                             WHERE r.service = ?1 AND r.creator_id = ?2 AND r.post_id = ?3
                             LIMIT 1",
                            params![parts[0], parts[1], parts[2]],
                            |r| r.get(0),
                        )
                        .optional()
                        .unwrap_or(None);

                    if let Some(rel) = rel_ref {
                        let full = thumbs_cache_path().join(rel);
                        if full.is_file() {
                            return Some(full.to_string_lossy().into_owned());
                        }
                    }
                }
            } else {
                let rel_ref: Option<String> = connection
                    .query_row(
                        "SELECT b.relative_path FROM thumb_refs r
                         JOIN thumb_blobs b ON r.content_key = b.content_key
                         WHERE r.media_id = ?1
                         LIMIT 1",
                        params![key],
                        |r| r.get(0),
                    )
                    .optional()
                    .unwrap_or(None);

                if let Some(rel) = rel_ref {
                    let full = thumbs_cache_path().join(rel);
                    if full.is_file() {
                        return Some(full.to_string_lossy().into_owned());
                    }
                }
            }
        }

        let dir = thumbs_cache_path();
        for ext in &["webp", "jpg", "png"] {
            let path = dir.join(format!("{}.{}", sanitize_cache_key(key), ext));
            if path.is_file() {
                return Some(path.to_string_lossy().into_owned());
            }
        }
        None
    }

    pub fn thumbnail_data_url(&self, key: &str) -> Result<Option<String>, String> {
        let Some(path) = self.thumbnail_path(key) else {
            return Ok(None);
        };
        let mime = mime_guess::from_path(&path).first_or_octet_stream();
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        Ok(Some(format!(
            "data:{mime};base64,{}",
            BASE64_STANDARD.encode(bytes)
        )))
    }

    pub async fn cache_post_preview(
        &self,
        post: &Post,
        url: &str,
        client: &reqwest::Client,
    ) -> Result<PathBuf, String> {
        let response = client.get(url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("Preview HTTP {}", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|n| n > 16 * 1024 * 1024)
        {
            return Err("Post preview is too large".into());
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err("Post preview is too large".into());
        }
        let ext = if mime.contains("png") {
            "png"
        } else if mime.contains("webp") {
            "webp"
        } else if mime.contains("gif") {
            "gif"
        } else if mime.contains("mp4") {
            "mp4"
        } else if mime.contains("webm") {
            "webm"
        } else {
            "jpg"
        };
        let dir = content_cache_path().join("thumbnails");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!(
            "post_{}_{}_{}.{}",
            sanitize_cache_key(&post.service),
            sanitize_cache_key(&post.user),
            sanitize_cache_key(&post.id),
            ext
        ));
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        let c = self.connection.lock().map_err(|e| e.to_string())?;
        c.execute(
            "UPDATE posts SET preview_path=?4 WHERE service=?1 AND creator_id=?2 AND post_id=?3",
            params![post.service, post.user, post.id, path.to_string_lossy()],
        )
        .map_err(|e| e.to_string())?;
        drop(c);
        self.enforce_cache_limit_after_write(bytes.len() as u64);
        Ok(path)
    }

    pub async fn revalidate_creator_artwork(
        &self,
        service: &str,
        creator_id: &str,
        kind: &str,
        url: &str,
        _provider_id: Option<&str>,
        client: &reqwest::Client,
    ) -> Result<Option<PathBuf>, String> {
        let dir = content_cache_path().join(if kind == "banner" {
            "banners"
        } else {
            "avatars"
        });
        let s_san = sanitize_cache_key(service);
        let id_san = sanitize_cache_key(creator_id);

        let existing = self.artwork_path(service, creator_id, kind)?.or_else(|| {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let prefix = format!("{s_san}_{id_san}_");
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with(&prefix) && name.contains(kind) {
                        return Some(entry.path().to_string_lossy().into_owned());
                    }
                }
            }
            None
        });

        let mut req = client.get(url);
        if let Some(ref path_str) = existing {
            let path = PathBuf::from(path_str);
            if path.is_file() {
                if let Ok(metadata) = std::fs::metadata(&path) {
                    if let Ok(mtime) = metadata.modified() {
                        let dt = chrono::DateTime::<chrono::Utc>::from(mtime);
                        req = req.header(reqwest::header::IF_MODIFIED_SINCE, dt.to_rfc2822());
                    }
                }
            }
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("Artwork revalidation error: {e}"))?;

        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            let c = self.connection.lock().map_err(|e| e.to_string())?;
            let _ = c.execute(
                "UPDATE creators SET last_checked_at=CURRENT_TIMESTAMP WHERE service=?1 AND creator_id=?2",
                params![service, creator_id],
            );
            return Ok(existing.map(PathBuf::from));
        }

        if !resp.status().is_success() {
            return Ok(existing.map(PathBuf::from));
        }

        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;

        let ext = if mime.contains("png") {
            "png"
        } else if mime.contains("webp") {
            "webp"
        } else if mime.contains("gif") {
            "gif"
        } else {
            "jpg"
        };

        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let safe = format!("{s_san}_{id_san}_{kind}.{ext}");
        let path = dir.join(&safe);
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;

        if let Ok(entries) = std::fs::read_dir(&dir) {
            let prefix = format!("{s_san}_{id_san}_");
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) && name.contains(kind) && name != safe {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        let column = if kind == "banner" {
            "banner_path"
        } else {
            "avatar_path"
        };
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .execute(
                &format!("UPDATE creators SET {column}=?3, last_checked_at=CURRENT_TIMESTAMP WHERE service=?1 AND creator_id=?2"),
                params![service, creator_id, path.to_string_lossy()],
            )
            .map_err(|e| e.to_string())?;
        drop(connection);

        self.enforce_cache_limit_after_write(bytes.len() as u64);
        Ok(Some(path))
    }
}

fn scan_cache_files(root: &Path, protected: &HashSet<PathBuf>) -> Result<Vec<CacheFile>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let metadata = entry.metadata().map_err(|error| error.to_string())?;
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                let modified_at = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs())
                    .unwrap_or_default();
                files.push(CacheFile {
                    protected: protected.contains(&path),
                    path,
                    size: metadata.len(),
                    modified_at,
                });
            }
        }
    }
    Ok(files)
}

fn cache_stats_from_files(files: &[CacheFile]) -> CacheStats {
    let protected_bytes = files
        .iter()
        .filter(|file| file.protected)
        .map(|file| file.size)
        .sum::<u64>();
    let total_bytes = files.iter().map(|file| file.size).sum::<u64>();
    let bytes_for = |directory: &str| {
        files
            .iter()
            .filter(|file| file.path.components().any(|c| c.as_os_str() == directory))
            .map(|file| file.size)
            .sum::<u64>()
    };
    let preview_bytes = bytes_for("previews");
    let avatar_bytes = bytes_for("avatars");
    let banner_bytes = bytes_for("banners");
    let thumbnail_bytes = bytes_for("thumbnails");
    CacheStats {
        total_bytes,
        metadata_bytes: 0,
        protected_bytes,
        reclaimable_bytes: total_bytes.saturating_sub(protected_bytes),
        preview_bytes,
        avatar_bytes,
        banner_bytes,
        thumbnail_bytes,
        other_bytes: total_bytes
            .saturating_sub(preview_bytes)
            .saturating_sub(avatar_bytes)
            .saturating_sub(banner_bytes)
            .saturating_sub(thumbnail_bytes),
        file_count: files.len() as u64,
    }
}

fn remove_empty_cache_dirs(root: &Path) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    let directories = std::fs::read_dir(root)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    for directory in directories {
        remove_empty_cache_dirs(&directory)?;
        if std::fs::read_dir(&directory)
            .map_err(|error| error.to_string())?
            .next()
            .is_none()
        {
            std::fs::remove_dir(&directory).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_prune_is_debounced_after_writes() {
        let repo = ContentRepository::in_memory(128).unwrap();

        assert!(repo.should_prune_now(1024));
        assert!(!repo.should_prune_now(1024));
        assert!(!repo.should_prune_now(1024));
    }

    #[test]
    fn cache_prune_is_forced_by_a_large_overshoot() {
        let repo = ContentRepository::in_memory(128).unwrap();
        assert!(repo.should_prune_now(0));
        assert!(!repo.should_prune_now(1024));

        assert!(repo.should_prune_now(CACHE_PRUNE_FORCE_OVERSHOOT));
    }

    #[test]
    fn note_pruned_resets_pending_byte_counter() {
        let repo = ContentRepository::in_memory(128).unwrap();
        repo.cache_bytes_since_prune.store(4096, Ordering::Release);
        repo.note_pruned();
        assert_eq!(repo.cache_bytes_since_prune.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cache_stats_separate_protected_files() {
        let root = std::env::temp_dir().join(format!(
            "pawstash-content-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = root.join("previews");
        std::fs::create_dir_all(&nested).unwrap();
        let kept = nested.join("kept.jpg");
        let reclaimable = nested.join("old.jpg");
        std::fs::write(&kept, [0_u8; 7]).unwrap();
        std::fs::write(&reclaimable, [0_u8; 11]).unwrap();

        let files = scan_cache_files(&root, &HashSet::from([kept])).unwrap();
        let stats = cache_stats_from_files(&files);
        assert_eq!(stats.total_bytes, 18);
        assert_eq!(stats.metadata_bytes, 0);
        assert_eq!(stats.protected_bytes, 7);
        assert_eq!(stats.reclaimable_bytes, 11);
        assert_eq!(stats.preview_bytes, 18);
        assert_eq!(stats.avatar_bytes, 0);
        assert_eq!(stats.banner_bytes, 0);
        assert_eq!(stats.thumbnail_bytes, 0);
        assert_eq!(stats.other_bytes, 0);
        assert_eq!(stats.file_count, 2);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn test_list_favorites_attaches_faved_at() {
        let repo = ContentRepository::in_memory(512).unwrap();
        let post: Post = serde_json::from_str(
            r#"{
                "id": "100",
                "user": "creator_a",
                "service": "patreon",
                "title": "Test Post",
                "content": "Hello",
                "published": "2026-08-28 10:00:00",
                "added": "2026-08-28 10:00:00"
            }"#,
        )
        .unwrap();

        repo.pin_post(&post, "favorite", "my_account").unwrap();
        let favorites = repo.list_favorites("post", "my_account").unwrap();
        assert!(favorites.iter().any(|f| f.id == "100"
            && f.service.as_deref() == Some("patreon")
            && f.extra.contains_key("faved_at")));
    }

    #[test]
    fn test_list_favorites_isolates_accounts_and_cleans_pins() {
        let repo = ContentRepository::in_memory(512).unwrap();
        let creator_a = CreatorProfile {
            id: "creator_a_test".into(),
            name: "Creator A".into(),
            service: "patreon".into(),
            public_id: None,
            relation_id: None,
            indexed: None,
            updated: None,
            favorited: None,
            ever_imported: None,
            avatar_url: None,
            avatar_path: None,
            banner_url: None,
            banner_path: None,
            page_url: None,
            extra: Default::default(),
        };
        let creator_guest = CreatorProfile {
            id: "creator_guest_test".into(),
            name: "Creator Guest".into(),
            service: "fanbox".into(),
            public_id: None,
            relation_id: None,
            indexed: None,
            updated: None,
            favorited: None,
            ever_imported: None,
            avatar_url: None,
            avatar_path: None,
            banner_url: None,
            banner_path: None,
            page_url: None,
            extra: Default::default(),
        };
        repo.save_creator(&creator_a).unwrap();
        repo.save_creator(&creator_guest).unwrap();

        repo.set_pin(
            "creator",
            "patreon",
            "creator_a_test",
            None,
            "favorite",
            "favkeep",
            true,
        )
        .unwrap();
        repo.set_pin(
            "creator",
            "fanbox",
            "creator_guest_test",
            None,
            "favorite",
            "",
            true,
        )
        .unwrap();

        let logged_in_favs = repo.list_favorites("artist", "favkeep").unwrap();
        assert!(logged_in_favs.iter().any(|f| f.id == "creator_a_test"));
        assert!(logged_in_favs.iter().any(|f| f.id == "creator_guest_test"));

        let guest_favs = repo.list_favorites("artist", "").unwrap();
        assert!(guest_favs.iter().any(|f| f.id == "creator_guest_test"));
        assert!(!guest_favs.iter().any(|f| f.id == "creator_a_test"));

        let other_favs = repo.list_favorites("artist", "other_user_test").unwrap();
        assert!(other_favs.iter().any(|f| f.id == "creator_guest_test"));
        assert!(!other_favs.iter().any(|f| f.id == "creator_a_test"));

        repo.set_pin(
            "creator",
            "fanbox",
            "creator_guest_test",
            None,
            "favorite",
            "",
            false,
        )
        .unwrap();
        let guest_favs_after = repo.list_favorites("artist", "").unwrap();
        assert!(!guest_favs_after
            .iter()
            .any(|f| f.id == "creator_guest_test"));

        repo.set_pin(
            "creator",
            "patreon",
            "creator_a_test",
            None,
            "favorite",
            "favkeep",
            false,
        )
        .unwrap();
    }

    #[test]
    fn test_creators_query_pagination_and_lookup() {
        let repo = ContentRepository::in_memory(512).unwrap();
        let creators = vec![
            Creator {
                id: "c1".to_string(),
                name: "Alpha Artist".to_string(),
                service: "patreon".to_string(),
                public_id: None,
                relation_id: None,
                indexed: Some(100),
                updated: Some(200),
                favorited: Some(50),
                ever_imported: None,
                avatar_url: None,
                avatar_path: None,
                banner_url: None,
                banner_path: None,
                page_url: None,
                extra: Default::default(),
            },
            Creator {
                id: "c2".to_string(),
                name: "Beta Generator [AI]".to_string(),
                service: "fanbox".to_string(),
                public_id: None,
                relation_id: None,
                indexed: Some(300),
                updated: Some(400),
                favorited: Some(100),
                ever_imported: None,
                avatar_url: None,
                avatar_path: None,
                banner_url: None,
                banner_path: None,
                page_url: None,
                extra: Default::default(),
            },
            Creator {
                id: "c3".to_string(),
                name: "Gamma Creator".to_string(),
                service: "patreon".to_string(),
                public_id: None,
                relation_id: None,
                indexed: Some(500),
                updated: Some(600),
                favorited: Some(20),
                ever_imported: None,
                avatar_url: None,
                avatar_path: None,
                banner_url: None,
                banner_path: None,
                page_url: None,
                extra: Default::default(),
            },
        ];

        repo.save_creators(&creators).unwrap();

        let services = repo.list_creator_services().unwrap();
        assert_eq!(services, vec!["fanbox", "patreon"]);

        let names = repo.list_creator_names().unwrap();
        assert_eq!(
            names.get("patreon:c1").map(|s| s.as_str()),
            Some("Alpha Artist")
        );
        assert_eq!(
            names.get("fanbox:c2").map(|s| s.as_str()),
            Some("Beta Generator [AI]")
        );

        let single_name = repo.get_creator_name("patreon", "c1").unwrap();
        assert_eq!(single_name, Some("Alpha Artist".to_string()));

        let page = repo
            .query_creators(&CreatorsQuery {
                sort_by: Some("favorited".to_string()),
                sort_order: Some("desc".to_string()),
                limit: Some(2),
                offset: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].id, "c2");
        assert_eq!(page.items[1].id, "c1");
        assert!(page.has_more);

        let page_no_ai = repo
            .query_creators(&CreatorsQuery {
                hide_ai: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page_no_ai.total, 2);
        assert!(!page_no_ai.items.iter().any(|c| c.id == "c2"));

        let page_service = repo
            .query_creators(&CreatorsQuery {
                services: Some(vec!["fanbox".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page_service.total, 1);
        assert_eq!(page_service.items[0].id, "c2");

        let page_search = repo
            .query_creators(&CreatorsQuery {
                query: Some("Gamma".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page_search.total, 1);
        assert_eq!(page_search.items[0].id, "c3");

        let page_prov = repo
            .query_creators(&CreatorsQuery {
                providers: Some(vec!["pawchive".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page_prov.total, 3);

        let page_nonexistent = repo
            .query_creators(&CreatorsQuery {
                providers: Some(vec!["onlyhaven".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page_nonexistent.total, 0);
    }
}
