use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

/// API Key record in the database
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApiKey {
    pub id: String,
    pub key_hash: String,
    pub name: String,
    pub user_id: Option<String>,
    pub rate_limit: i32,
    pub is_active: bool,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub total_calls: i64,
}

/// Stats for an API key
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApiKeyStats {
    pub id: String,
    pub name: String,
    pub user_id: Option<String>,
    pub total_calls: i64,
    pub last_used_at: Option<String>,
    pub is_active: bool,
}

/// Cached rate data for a chain
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CachedRates {
    pub chain: String,
    pub data_json: String,
    pub cached_at: String,
}

/// Database manager for API keys and rate cache
#[derive(Clone)]
pub struct Database {
    pub pool: SqlitePool,
}

impl Database {
    /// Create a new database connection and initialize tables
    pub async fn new(db_path: &str) -> Result<Self, sqlx::Error> {
        let options = SqliteConnectOptions::from_str(db_path)?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        // Create tables
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS api_keys (
                id TEXT PRIMARY KEY,
                key_hash TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                user_id TEXT,
                rate_limit INTEGER NOT NULL DEFAULT 100,
                is_active BOOLEAN NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                last_used_at TEXT,
                total_calls INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_api_keys_key_hash ON api_keys(key_hash);
            CREATE INDEX IF NOT EXISTS idx_api_keys_is_active ON api_keys(is_active);

            CREATE TABLE IF NOT EXISTS rate_cache (
                chain TEXT PRIMARY KEY,
                data_json TEXT NOT NULL,
                cached_at TEXT NOT NULL
            );
            "#,
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }

    /// Hash an API key for storage
    fn hash_key(key: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Create a new API key
    pub async fn create_key(
        &self,
        name: &str,
        user_id: Option<&str>,
        rate_limit: Option<i32>,
    ) -> Result<(ApiKey, String), sqlx::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let raw_key = format!("amcp_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let key_hash = Self::hash_key(&raw_key);
        let now = Utc::now().to_rfc3339();
        let rate_limit = rate_limit.unwrap_or(100);

        sqlx::query(
            r#"
            INSERT INTO api_keys (id, key_hash, name, user_id, rate_limit, is_active, created_at, total_calls)
            VALUES (?, ?, ?, ?, ?, 1, ?, 0)
            "#,
        )
        .bind(&id)
        .bind(&key_hash)
        .bind(name)
        .bind(user_id)
        .bind(rate_limit)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let api_key = ApiKey {
            id,
            key_hash,
            name: name.to_string(),
            user_id: user_id.map(|s| s.to_string()),
            rate_limit,
            is_active: true,
            created_at: now,
            last_used_at: None,
            total_calls: 0,
        };

        Ok((api_key, raw_key))
    }

    /// Validate an API key
    pub async fn validate_key(&self, raw_key: &str) -> Result<Option<ApiKey>, sqlx::Error> {
        let key_hash = Self::hash_key(raw_key);

        let api_key = sqlx::query_as::<_, ApiKey>(
            "SELECT * FROM api_keys WHERE key_hash = ? AND is_active = 1",
        )
        .bind(&key_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(api_key)
    }

    /// Record a usage event for an API key
    pub async fn record_usage(&self, key_id: &str) -> Result<(), sqlx::Error> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE api_keys SET last_used_at = ?, total_calls = total_calls + 1 WHERE id = ?",
        )
        .bind(&now)
        .bind(key_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List all API keys
    pub async fn list_keys(&self) -> Result<Vec<ApiKey>, sqlx::Error> {
        let keys = sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await?;
        Ok(keys)
    }

    /// Get stats for all API keys
    pub async fn get_stats(&self) -> Result<Vec<ApiKeyStats>, sqlx::Error> {
        let stats = sqlx::query_as::<_, ApiKeyStats>(
            "SELECT id, name, user_id, total_calls, last_used_at, is_active FROM api_keys ORDER BY total_calls DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(stats)
    }

    /// Deactivate an API key
    pub async fn deactivate_key(&self, key_id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE api_keys SET is_active = 0 WHERE id = ?")
            .bind(key_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Reactivate an API key
    pub async fn reactivate_key(&self, key_id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE api_keys SET is_active = 1 WHERE id = ?")
            .bind(key_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete an API key permanently
    pub async fn delete_key(&self, key_id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(key_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    // ── Rate Cache ─────────────────────────────────────────────────────

    /// Get cached rates for a chain if not expired
    pub async fn get_cached_rates(&self, chain: &str, ttl_secs: i64) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query_as::<_, CachedRates>(
            "SELECT chain, data_json, cached_at FROM rate_cache WHERE chain = ?",
        )
        .bind(chain)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(cached) => {
                // Check if cache is still valid
                if let Ok(cached_time) = chrono::DateTime::parse_from_rfc3339(&cached.cached_at) {
                    let now = Utc::now();
                    let age = now.signed_duration_since(cached_time);
                    if age.num_seconds() < ttl_secs {
                        tracing::debug!(chain = chain, age_secs = age.num_seconds(), "Cache hit");
                        return Ok(Some(cached.data_json));
                    }
                }
                tracing::debug!(chain = chain, "Cache expired");
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// Store rates in cache
    pub async fn set_cached_rates(&self, chain: &str, data_json: &str) -> Result<(), sqlx::Error> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT OR REPLACE INTO rate_cache (chain, data_json, cached_at) VALUES (?, ?, ?)",
        )
        .bind(chain)
        .bind(data_json)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear expired cache entries
    pub async fn clear_expired_cache(&self, ttl_secs: i64) -> Result<u64, sqlx::Error> {
        let cutoff = (Utc::now() - chrono::Duration::seconds(ttl_secs)).to_rfc3339();
        let result = sqlx::query("DELETE FROM rate_cache WHERE cached_at < ?")
            .bind(&cutoff)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}
