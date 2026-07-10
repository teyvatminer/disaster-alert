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
            for geohash_str in old_geohashes.difference(&new_geohashes) {
                remove_from_geohash_index_tx(tx, &bark_id, geohash_str)?;
            }
            tx.insert(primary_key.as_bytes(), primary_value.clone())?;
            for geohash_str in &new_geohashes {
                add_to_geohash_index_tx(tx, &bark_id, geohash_str)?;
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

    pub fn get_subscriptions_by_geohashes(
        &self,
        geohashes: &[String],
    ) -> Result<Vec<Subscription>> {
        let mut all_bark_ids = Vec::new();

        for gh in geohashes {
            if let Some(index) = self.get_geohash_index_optional(gh)? {
                all_bark_ids.extend(index.bark_ids);
            }
        }

        all_bark_ids.sort();
        all_bark_ids.dedup();

        let mut subscriptions = Vec::new();
        for bark_id in all_bark_ids {
            match self.get_subscription_optional(&bark_id)? {
                Some(sub) => subscriptions.push(sub),
                None => tracing::warn!(
                    event = "subscription.index_dangling",
                    bark_id = %mask_bark_id(&bark_id),
                    "subscription.index_dangling"
                ),
            }
        }

        Ok(subscriptions)
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
        let key = format!("geo:{}", geohash);
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
        TransactionError::Storage(error) => anyhow::Error::new(error).context("storage transaction failed"),
    })
}

fn add_to_geohash_index_tx(
    tx: &TransactionalTree,
    bark_id: &str,
    geohash: &str,
) -> std::result::Result<(), ConflictableTransactionError<TxError>> {
    let key = format!("geo:{geohash}");
    let mut index = geohash_index_from_tx(tx, &key, geohash)?.unwrap_or_default();
    index.add(bark_id.to_string());
    let value = serde_json::to_vec(&index)
        .map_err(|error| ConflictableTransactionError::Abort(TxError::Serde(error.to_string())))?;
    tx.insert(key.as_bytes(), value)?;
    Ok(())
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

fn geohash_index_from_tx(
    tx: &TransactionalTree,
    key: &str,
    geohash: &str,
) -> std::result::Result<Option<GeoHashIndex>, ConflictableTransactionError<TxError>> {
    let Some(value) = tx.get(key.as_bytes())? else {
        return Ok(None);
    };
    let index = serde_json::from_slice(&value)
        .map_err(|error| {
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
