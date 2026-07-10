use crate::models::{GeoHashIndex, Subscription, mask_bark_id};
use crate::utils::geohash;
use anyhow::{Context, Result, anyhow};
use sled::Db;
use sled::transaction::{ConflictableTransactionError, TransactionError, TransactionalTree};
use std::collections::HashSet;

#[derive(Clone)]
pub struct SubscriptionStore {
    db: Db,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreErrorKind {
    NotFound,
    Internal,
}

impl SubscriptionStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn upsert_subscription(&self, subscription: Subscription) -> Result<()> {
        let bark_id = subscription.bark_id.clone();
        let new_geohashes = subscription_geohashes(&subscription);

        let old_subscription = self.get_subscription_optional(&bark_id)?;
        let is_new_subscription = old_subscription.is_none();

        let old_geohashes = old_subscription
            .as_ref()
            .map(subscription_geohashes)
            .unwrap_or_default();
        let primary_key = format!("sub:{}", bark_id);
        let primary_value = serde_json::to_vec(&subscription)?;

        run_transaction(self.db.transaction(|tx| {
            for geohash_str in &old_geohashes {
                remove_from_geohash_index_tx(tx, &bark_id, geohash_str)?;
            }
            for geohash_str in old_geohashes.difference(&new_geohashes) {
                remove_from_geohash_subscription_tx(tx, &bark_id, geohash_str)?;
            }
            tx.insert(primary_key.as_bytes(), primary_value.clone())?;
            for geohash_str in &new_geohashes {
                insert_geohash_subscription_tx(tx, &bark_id, geohash_str, primary_value.clone())?;
            }
            Ok(())
        }))?;

        tracing::info!(
            event = "subscription.stored",
            action = if is_new_subscription { "insert" } else { "update" },
            bark_id = %mask_bark_id(&bark_id),
            geohash_count = new_geohashes.len(),
            "subscription.stored"
        );

        Ok(())
    }

    pub fn delete_subscription(&self, bark_id: &str) -> Result<()> {
        let subscription = self.get_subscription(bark_id)?;
        let geohashes = subscription_geohashes(&subscription);
        let primary_key = format!("sub:{}", bark_id);

        run_transaction(self.db.transaction(|tx| {
            for geohash_str in &geohashes {
                remove_from_geohash_index_tx(tx, bark_id, geohash_str)?;
                remove_from_geohash_subscription_tx(tx, bark_id, geohash_str)?;
            }
            tx.remove(primary_key.as_bytes())?;
            Ok(())
        }))?;

        tracing::info!(
            event = "subscription.deleted",
            bark_id = %mask_bark_id(bark_id),
            "subscription.deleted"
        );
        Ok(())
    }

    pub fn get_subscription(&self, bark_id: &str) -> Result<Subscription> {
        self.get_subscription_optional(bark_id)?
            .ok_or_else(|| anyhow!("订阅不存在"))
    }

    pub fn classify_error(error: &anyhow::Error) -> StoreErrorKind {
        if error.to_string().contains("订阅不存在") {
            StoreErrorKind::NotFound
        } else {
            StoreErrorKind::Internal
        }
    }

    fn get_subscription_optional(&self, bark_id: &str) -> Result<Option<Subscription>> {
        let key = format!("sub:{}", bark_id);
        let Some(value) = self.db.get(key.as_bytes())? else {
            return Ok(None);
        };

        let subscription: Subscription = serde_json::from_slice(&value)
            .with_context(|| format!("订阅数据格式错误: {}", mask_bark_id(bark_id)))?;
        Ok(Some(subscription))
    }

    /// 按 geohash 集合返回匹配的订阅。
    ///
    /// 热路径使用 `geo_sub:{geohash}:{bark_id}` 桶化索引，通知时只扫描目标
    /// geohash 的连续 key range，避免先读 `geo:` ID 列表再对每个 bark_id 做
    /// 一次随机 `get`。旧版 `geo:` 索引作为兼容 fallback 保留。
    pub fn for_each_subscription_by_geohashes<F>(
        &self,
        geohashes: &[String],
        mut visitor: F,
    ) -> Result<()>
    where
        F: FnMut(Subscription) -> Result<()>,
    {
        if geohashes.is_empty() {
            return Ok(());
        }

        let mut seen_bark_ids = HashSet::new();
        let mut fallback_bark_ids = Vec::new();

        for geohash in geohashes {
            let prefix = format!("geo_sub:{geohash}:");
            for item in self.db.scan_prefix(prefix.as_bytes()) {
                let (_key, value) = item?;
                let subscription: Subscription = serde_json::from_slice(&value)
                    .with_context(|| format!("GeoHash 订阅桶数据格式错误: {geohash}"))?;
                if seen_bark_ids.insert(subscription.bark_id.clone()) {
                    visitor(subscription)?;
                }
            }

            if let Some(index) = self.get_geohash_index_optional(geohash)? {
                fallback_bark_ids.extend(index.bark_ids);
            }
        }

        fallback_bark_ids.sort();
        fallback_bark_ids.dedup();

        for bark_id in fallback_bark_ids {
            if !seen_bark_ids.insert(bark_id.clone()) {
                continue;
            }
            match self.get_subscription_optional(&bark_id)? {
                Some(subscription) => visitor(subscription)?,
                None => tracing::warn!(
                    event = "subscription.index_dangling",
                    bark_id = %mask_bark_id(&bark_id),
                    "subscription.index_dangling"
                ),
            }
        }

        Ok(())
    }

    pub fn get_total_count(&self) -> Result<usize> {
        let mut count = 0usize;
        for item in self.db.scan_prefix(b"sub:") {
            let (_key, _value) = item?;
            count += 1;
        }
        Ok(count)
    }

    fn get_geohash_index_optional(&self, geohash: &str) -> Result<Option<GeoHashIndex>> {
        let key = format!("geo:{geohash}");
        let Some(value) = self.db.get(key.as_bytes())? else {
            return Ok(None);
        };

        let index: GeoHashIndex = serde_json::from_slice(&value)
            .with_context(|| format!("GeoHash 索引数据格式错误: {geohash}"))?;
        Ok(Some(index))
    }
}

#[derive(Debug, Clone)]
enum TxError {
    Serde(String),
}

impl std::fmt::Display for TxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxError::Serde(message) => write!(formatter, "事务序列化失败: {message}"),
        }
    }
}

fn run_transaction(result: std::result::Result<(), TransactionError<TxError>>) -> Result<()> {
    result.map_err(|error| match error {
        TransactionError::Abort(error) => anyhow!("{error}"),
        TransactionError::Storage(error) => {
            anyhow::Error::new(error).context("storage transaction failed")
        }
    })
}

fn remove_from_geohash_index_tx(
    tx: &TransactionalTree,
    bark_id: &str,
    geohash: &str,
) -> std::result::Result<(), ConflictableTransactionError<TxError>> {
    let key = format!("geo:{geohash}");
    let Some(mut index) = geohash_index_from_tx(tx, &key, geohash)? else {
        return Ok(());
    };
    index.remove(bark_id);
    if index.bark_ids.is_empty() {
        tx.remove(key.as_bytes())?;
    } else {
        let value = serde_json::to_vec(&index).map_err(|error| {
            ConflictableTransactionError::Abort(TxError::Serde(error.to_string()))
        })?;
        tx.insert(key.as_bytes(), value)?;
    }
    Ok(())
}

fn insert_geohash_subscription_tx(
    tx: &TransactionalTree,
    bark_id: &str,
    geohash: &str,
    subscription_value: Vec<u8>,
) -> std::result::Result<(), ConflictableTransactionError<TxError>> {
    let key = geohash_subscription_key(geohash, bark_id);
    tx.insert(key.as_bytes(), subscription_value)?;
    Ok(())
}

fn remove_from_geohash_subscription_tx(
    tx: &TransactionalTree,
    bark_id: &str,
    geohash: &str,
) -> std::result::Result<(), ConflictableTransactionError<TxError>> {
    let key = geohash_subscription_key(geohash, bark_id);
    tx.remove(key.as_bytes())?;
    Ok(())
}

fn geohash_index_from_tx(
    tx: &TransactionalTree,
    key: &str,
    geohash: &str,
) -> std::result::Result<Option<GeoHashIndex>, ConflictableTransactionError<TxError>> {
    let Some(value) = tx.get(key.as_bytes())? else {
        return Ok(None);
    };
    let index = serde_json::from_slice(&value).map_err(|error| {
        ConflictableTransactionError::Abort(TxError::Serde(format!(
            "GeoHash 索引数据格式错误: {geohash}: {error}"
        )))
    })?;
    Ok(Some(index))
}

fn subscription_geohashes(subscription: &Subscription) -> HashSet<String> {
    subscription
        .normalized_locations()
        .into_iter()
        .filter_map(|location| geohash::try_encode(location.latitude, location.longitude))
        .collect()
}

fn geohash_subscription_key(geohash: &str, bark_id: &str) -> String {
    format!("geo_sub:{geohash}:{bark_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{NotificationBand, SubscriptionLocation};

    fn temporary_store() -> Result<SubscriptionStore> {
        let db = sled::Config::new().temporary(true).open()?;
        Ok(SubscriptionStore::new(db))
    }

    fn subscription(bark_id: &str, lat: f64, lon: f64) -> Subscription {
        let mut subscription = Subscription::new(bark_id.to_string(), lat, lon);
        subscription.locations = vec![SubscriptionLocation {
            name: "home".to_string(),
            latitude: lat,
            longitude: lon,
        }];
        subscription.notify_bands = vec![NotificationBand {
            min: 1,
            max: 99,
            level: "critical".to_string(),
            label: String::new(),
        }];
        subscription
    }

    fn collect_by_geohashes(
        store: &SubscriptionStore,
        geohashes: &[String],
    ) -> Result<Vec<Subscription>> {
        let mut subscriptions = Vec::new();
        store.for_each_subscription_by_geohashes(geohashes, |subscription| {
            subscriptions.push(subscription);
            Ok(())
        })?;
        Ok(subscriptions)
    }

    #[test]
    fn bucket_index_tracks_insert_update_and_delete() -> Result<()> {
        let store = temporary_store()?;
        let beijing = subscription("abc123", 39.9042, 116.4074);
        let shanghai = subscription("abc123", 31.2397, 121.4999);

        let Some(beijing_geohash) = geohash::try_encode(beijing.latitude, beijing.longitude) else {
            anyhow::bail!("failed to geohash beijing")
        };
        let Some(shanghai_geohash) = geohash::try_encode(shanghai.latitude, shanghai.longitude)
        else {
            anyhow::bail!("failed to geohash shanghai")
        };

        store.upsert_subscription(beijing)?;
        let found = collect_by_geohashes(&store, std::slice::from_ref(&beijing_geohash))?;
        anyhow::ensure!(found.len() == 1, "expected one beijing subscription");
        anyhow::ensure!(found[0].bark_id == "abc123", "unexpected bark id");

        store.upsert_subscription(shanghai)?;
        let old_bucket = collect_by_geohashes(&store, &[beijing_geohash])?;
        anyhow::ensure!(old_bucket.is_empty(), "old geohash bucket must be empty");

        let new_bucket = collect_by_geohashes(&store, std::slice::from_ref(&shanghai_geohash))?;
        anyhow::ensure!(new_bucket.len() == 1, "expected one shanghai subscription");
        anyhow::ensure!(new_bucket[0].longitude == 121.4999, "unexpected longitude");

        store.delete_subscription("abc123")?;
        let after_delete = collect_by_geohashes(&store, &[shanghai_geohash])?;
        anyhow::ensure!(
            after_delete.is_empty(),
            "deleted subscription must not be returned"
        );

        Ok(())
    }

    #[test]
    fn lookup_can_read_legacy_geohash_index() -> Result<()> {
        let store = temporary_store()?;
        let subscription = subscription("legacy1", 39.9042, 116.4074);
        let Some(geohash) = geohash::try_encode(subscription.latitude, subscription.longitude)
        else {
            anyhow::bail!("failed to geohash legacy subscription")
        };
        let bark_id = subscription.bark_id.clone();

        let subscription_value = serde_json::to_vec(&subscription)?;
        store
            .db
            .insert(format!("sub:{bark_id}").as_bytes(), subscription_value)?;
        let legacy_index = GeoHashIndex {
            bark_ids: vec![bark_id],
        };
        store.db.insert(
            format!("geo:{geohash}").as_bytes(),
            serde_json::to_vec(&legacy_index)?,
        )?;

        let found = collect_by_geohashes(&store, &[geohash])?;
        anyhow::ensure!(found.len() == 1, "expected one legacy subscription");
        anyhow::ensure!(found[0].bark_id == "legacy1", "unexpected legacy bark id");

        Ok(())
    }
}
