//! UUID Chunker —— 包一层 `adk_rag::Chunker`，把每个 chunk.id 改写为 UUID v7。
//!
//! 原因：adk_rag 三个 chunker 都把 chunk.id 设为 `{document.id}_{i}`，而 Qdrant 的
//! point id 必须是 UUID 或无符号整数，拼接 id 会被 Qdrant 拒绝。删除走 payload
//! `document_id` 过滤，不依赖 chunk id，故直接重写为合法 UUID。

use std::sync::Arc;

use adk_rag::chunking::Chunker;
use adk_rag::document::{Chunk, Document};

use uuid::Uuid;

pub struct UuidChunker {
    inner: Arc<dyn Chunker>,
}

impl UuidChunker {
    pub fn new(inner: Arc<dyn Chunker>) -> Self {
        Self { inner }
    }
}

impl Chunker for UuidChunker {
    fn chunk(&self, document: &Document) -> Vec<Chunk> {
        let mut chunks = self.inner.chunk(document);
        for c in chunks.iter_mut() {
            c.id = Uuid::now_v7().to_string();
        }
        chunks
    }
}
