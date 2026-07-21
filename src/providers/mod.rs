mod fanstudio;
mod fanstudio_protocol;
mod reconnect;
mod value;
mod wolfx;
mod wolfx_protocol;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub(crate) use fanstudio::FanStudioSource;
pub(crate) use wolfx::WolfxSource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderCursor {
    stream: String,
    value: String,
}

impl ProviderCursor {
    pub(crate) fn new(stream: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        let cursor = Self {
            stream: stream.into(),
            value: value.into(),
        };
        anyhow::ensure!(
            !cursor.stream.is_empty() && cursor.stream.len() <= 128,
            "provider cursor stream must contain 1..=128 bytes"
        );
        anyhow::ensure!(
            !cursor.value.is_empty() && cursor.value.len() <= 512,
            "provider cursor value must contain 1..=512 bytes"
        );
        Ok(cursor)
    }

    pub(crate) fn stream(&self) -> &str {
        &self.stream
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }
}
