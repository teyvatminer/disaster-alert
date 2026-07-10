use anyhow::{Context, Result};
use sled::Db;
use std::path::Path;

mod subscription_store;

pub use subscription_store::{StoreErrorKind, SubscriptionStore};

/// 数据库封装
#[derive(Clone)]
pub struct Database {
    db: Db,
}

impl Database {
    /// 打开数据库
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let db = sled::open(path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;
        Ok(Self { db })
    }

    /// 获取订阅存储
    pub fn subscriptions(&self) -> SubscriptionStore {
        SubscriptionStore::new(self.db.clone())
    }
}
