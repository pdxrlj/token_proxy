//! SQLite 持久化：把成功请求的 prompt + completion 存为微调数据集。
//!
//! 表结构设计兼容 OpenAI 微调格式（messages 数组），同时保留原始请求/响应
//! JSON 以便后续灵活处理。

use chrono::Utc;
use rusqlite::{params, Connection, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

/// 一条微调数据集记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetRecord {
    pub id: i64,
    pub ts: i64,
    /// 客户端请求的模型名
    pub model: String,
    /// 实际转发给上游的模型名（经路由改写后）
    pub upstream_model: Option<String>,
    pub provider: Option<String>,
    pub stream: bool,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cache_hit: bool,
    /// 缓存命中的 token 数量
    pub cached_tokens: Option<u64>,
    /// 请求体 JSON（完整 OpenAI 格式，含 messages）
    pub request_body: String,
    /// 响应体 JSON（非流式为完整响应；流式为聚合后的 choices 数组）
    pub response_body: String,
    /// 微调格式：{"messages": [...]} 可直接用于 OpenAI fine-tuning
    pub finetune_jsonl: String,
}

/// 初始化数据库（建表 + 索引）
pub fn init_db(db_path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dataset (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            ts          INTEGER NOT NULL,
            model       TEXT NOT NULL,
            upstream_model TEXT,
            provider    TEXT,
            stream      INTEGER NOT NULL DEFAULT 0,
            tokens_in   INTEGER,
            tokens_out  INTEGER,
            cache_hit   INTEGER NOT NULL DEFAULT 0,
            cached_tokens INTEGER,
            request_body  TEXT NOT NULL,
            response_body TEXT NOT NULL,
            finetune_jsonl TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_dataset_ts     ON dataset(ts);
        CREATE INDEX IF NOT EXISTS idx_dataset_model  ON dataset(model);
        CREATE INDEX IF NOT EXISTS idx_dataset_provider ON dataset(provider);
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;",
    )?;
    // 迁移：为旧表补充 cached_tokens 列（CREATE TABLE IF NOT EXISTS 不会修改已有表）
    let has_col: bool = conn
        .prepare("PRAGMA table_info(dataset)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "cached_tokens");
    if !has_col {
        conn.execute("ALTER TABLE dataset ADD COLUMN cached_tokens INTEGER", [])?;
    }
    Ok(conn)
}

/// 线程安全的 DB 句柄（rusqlite::Connection 本身非 Sync，用 Mutex 保护）
pub struct Database {
    conn: parking_lot::Mutex<Connection>,
}

impl Database {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: parking_lot::Mutex::new(conn),
        }
    }

    /// 写入一条成功请求记录
    pub fn insert_record(
        &self,
        ts: i64,
        model: &str,
        upstream_model: Option<&str>,
        provider: Option<&str>,
        stream: bool,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        cache_hit: bool,
        cached_tokens: Option<u64>,
        request_body: &str,
        response_body: &str,
        finetune_jsonl: &str,
    ) -> rusqlite::Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO dataset
                (ts, model, upstream_model, provider, stream, tokens_in, tokens_out,
                 cache_hit, cached_tokens, request_body, response_body, finetune_jsonl)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                ts,
                model,
                upstream_model,
                provider,
                stream as i64,
                tokens_in.map(|v| v as i64),
                tokens_out.map(|v| v as i64),
                cache_hit as i64,
                cached_tokens.map(|v| v as i64),
                request_body,
                response_body,
                finetune_jsonl,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 分页查询记录（最新在前）
    pub fn query_records(
        &self,
        limit: i64,
        offset: i64,
        model_filter: Option<&str>,
    ) -> rusqlite::Result<Vec<DatasetRecord>> {
        let conn = self.conn.lock();
        let mut sql = String::from(
            "SELECT id, ts, model, upstream_model, provider, stream, tokens_in,
                    tokens_out, cache_hit, cached_tokens, request_body, response_body, finetune_jsonl
             FROM dataset",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(m) = model_filter {
            sql.push_str(" WHERE model LIKE ?");
            params_vec.push(Box::new(format!("%{m}%")));
        }
        sql.push_str(" ORDER BY ts DESC LIMIT ? OFFSET ?");
        params_vec.push(Box::new(limit));
        params_vec.push(Box::new(offset));
        let params_ref: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_ref.as_slice(), map_record)?;
        rows.collect()
    }

    /// 统计记录总数
    pub fn count(&self, model_filter: Option<&str>) -> rusqlite::Result<i64> {
        let conn = self.conn.lock();
        if let Some(m) = model_filter {
            conn.query_row(
                "SELECT COUNT(*) FROM dataset WHERE model LIKE ?1",
                params![format!("%{m}%")],
                |row| row.get(0),
            )
        } else {
            conn.query_row("SELECT COUNT(*) FROM dataset", [], |row| row.get(0))
        }
    }

    /// 导出为 JSONL 字符串（微调格式，每行一条）
    pub fn export_jsonl(&self, model_filter: Option<&str>) -> rusqlite::Result<String> {
        let conn = self.conn.lock();
        let mut sql = String::from("SELECT finetune_jsonl FROM dataset");
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(m) = model_filter {
            sql.push_str(" WHERE model LIKE ?");
            params_vec.push(Box::new(format!("%{m}%")));
        }
        sql.push_str(" ORDER BY ts ASC");
        let params_ref: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_ref.as_slice(), |row| row.get::<_, String>(0))?;
        let mut out = String::new();
        for line in rows {
            if let Ok(l) = line {
                out.push_str(&l);
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// 清空数据集
    pub fn clear(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM dataset", [])?;
        conn.execute("VACUUM", [])?;
        Ok(())
    }

    /// 删除指定 ID
    pub fn delete(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM dataset WHERE id = ?1", params![id])?;
        Ok(())
    }
}

fn map_record(row: &Row) -> rusqlite::Result<DatasetRecord> {
    Ok(DatasetRecord {
        id: row.get(0)?,
        ts: row.get(1)?,
        model: row.get(2)?,
        upstream_model: row.get(3)?,
        provider: row.get(4)?,
        stream: row.get::<_, i64>(5)? != 0,
        tokens_in: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        tokens_out: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        cache_hit: row.get::<_, i64>(8)? != 0,
        cached_tokens: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        request_body: row.get(10)?,
        response_body: row.get(11)?,
        finetune_jsonl: row.get(12)?,
    })
}

/// 构建微调格式 JSONL：{"messages": [{"role":"system","content":"..."}, ...]}
/// 从请求体提取 messages 数组，从响应提取 assistant 回复内容。
pub fn build_finetune_jsonl(
    request_body: &serde_json::Value,
    response_body: &serde_json::Value,
    stream: bool,
) -> String {
    // 取请求中的 messages
    let messages = request_body
        .get("messages")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![]));

    // 取 assistant 回复
    let assistant_content = if stream {
        // 流式聚合的 response_body 是 {"choices":[{"message":{"content":"..."}}]} 格式
        response_body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        // 非流式标准响应
        response_body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string()
    };

    // 构建微调样本：messages + assistant 回复
    let mut finetune_messages = messages.clone();
    if let Some(arr) = finetune_messages.as_array_mut() {
        arr.push(serde_json::json!({
            "role": "assistant",
            "content": assistant_content,
        }));
    }

    let sample = serde_json::json!({ "messages": finetune_messages });
    serde_json::to_string(&sample).unwrap_or_default()
}

/// 从流式 SSE chunks 聚合出完整响应 JSON
pub fn aggregate_stream_response(
    chunks: &[serde_json::Value],
    model: &str,
    usage: Option<(u64, u64)>,
) -> serde_json::Value {
    let mut content = String::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();

    for chunk in chunks {
        if let Some(delta) = chunk
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
        {
            if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
                content.push_str(t);
            }
            if let Some(tc) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                tool_calls.extend_from_slice(tc);
            }
        }
    }

    let mut message = serde_json::json!({
        "role": "assistant",
        "content": if content.is_empty() { serde_json::Value::Null } else { serde_json::json!(content) },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = serde_json::Value::Array(tool_calls);
    }

    let mut response = serde_json::json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": "stop"
        }],
    });

    if let Some((input, output)) = usage {
        response["usage"] = serde_json::json!({
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": input + output,
        });
    }

    response
}

/// 异步写入辅助：在 blocking task 中执行 DB 写入，避免阻塞 async 运行时
pub fn spawn_insert(db: Arc<Database>, record: InsertRecord) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = db.insert_record(
            record.ts,
            &record.model,
            record.upstream_model.as_deref(),
            record.provider.as_deref(),
            record.stream,
            record.tokens_in,
            record.tokens_out,
            record.cache_hit,
            record.cached_tokens,
            &record.request_body,
            &record.response_body,
            &record.finetune_jsonl,
        ) {
            warn!(error = %e, "failed to insert dataset record");
        }
    });
}

/// 写入所需的所有字段（owned，方便跨 await 传递）
pub struct InsertRecord {
    pub ts: i64,
    pub model: String,
    pub upstream_model: Option<String>,
    pub provider: Option<String>,
    pub stream: bool,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cache_hit: bool,
    pub cached_tokens: Option<u64>,
    pub request_body: String,
    pub response_body: String,
    pub finetune_jsonl: String,
}

/// 获取默认 DB 路径（配置目录下 dataset.db）
pub fn default_db_path() -> std::path::PathBuf {
    let dir = directories::ProjectDirs::from("dev", "tokenproxy", "token-proxy")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.join("dataset.db")
}

/// 获取当前时间戳（毫秒）
pub fn now_ts() -> i64 {
    Utc::now().timestamp_millis()
}
