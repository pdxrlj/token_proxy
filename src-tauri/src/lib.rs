use std::sync::Arc;

use tauri::Manager;
use tauri_plugin_dialog::DialogExt;
use token_proxy_core::proxy::config::AppConfig;
use token_proxy_core::proxy::db::{default_db_path, init_db, Database, DatasetRecord};
use token_proxy_core::proxy::state::{AppState, EventSink, RequestLog};
use token_proxy_core::proxy::store::save_config as persist_config;
use token_proxy_core::proxy::store::{config_path, load_config, validate, StatusDto};

/// 事件推送：core → 前端
struct TauriSink {
    app: tauri::AppHandle,
}

impl EventSink for TauriSink {
    fn emit_log(&self, log: &RequestLog) {
        use tauri::Emitter;
        let _ = self.app.emit("proxy-log", log);
    }
    fn emit_config_changed(&self) {
        use tauri::Emitter;
        let _ = self.app.emit("config-changed", ());
    }
}

/// 从磁盘重新加载配置（热重载/手动刷新）
fn reload(state: &Arc<AppState>) -> Result<(), String> {
    let path = state.config_path.clone();
    let cfg = load_config(&path)?;
    *state.config.write() = cfg;
    Ok(())
}

/// 配置变更后同步服务监听：仅当 host/port 变化时重启监听，
/// 其余配置由 AppState 中的 RwLock 热生效。
async fn sync_server(state: &Arc<AppState>) {
    match token_proxy_core::proxy::apply_server_config(state).await {
        Ok(true) => tracing::info!("proxy server restarted on new listen address"),
        Ok(false) => {}
        Err(e) => tracing::error!("failed to restart proxy server: {e}"),
    }
}

#[tauri::command]
fn get_config(state: tauri::State<Arc<AppState>>) -> Result<AppConfig, String> {
    Ok(state.config.read().clone())
}

#[tauri::command]
async fn save_config(
    state: tauri::State<'_, Arc<AppState>>,
    cfg: AppConfig,
) -> Result<(), String> {
    validate(&cfg)?;
    let path = state.config_path.clone();
    persist_config(&path, &cfg)?;
    *state.config.write() = cfg;
    // 监听地址（host/port）变化时重启监听，其余配置热生效
    let s = state.inner().clone();
    sync_server(&s).await;
    Ok(())
}

/// 导出当前完整配置为 JSON 字符串（含 API Key）。
///
/// 保留供脚本 / 调试使用；应用内的「导出配置」走
/// [`export_config_to_file`]，由用户选择保存位置。
#[tauri::command]
fn export_config(state: tauri::State<Arc<AppState>>) -> Result<String, String> {
    let cfg = state.config.read().clone();
    serde_json::to_string_pretty(&cfg).map_err(|e| format!("序列化配置失败：{e}"))
}

/// 弹出「另存为」对话框，把配置写入用户选择的位置。
///
/// 返回实际写入路径；用户取消时返回 `Ok(None)`。
///
/// 落盘由 Rust 直接完成（而非交给 WebView 下载）：
/// WebView 的程序化下载在桌面端常被静默丢弃，用户无从得知文件去了哪里，
/// 而对话框能明确给出位置，且无需给前端开放任意文件写权限。
#[tauri::command]
async fn export_config_to_file(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    default_name: Option<String>,
) -> Result<Option<String>, String> {
    let json = export_config(state)?;

    let file_name = default_name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "token-proxy-config.json".to_string());

    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app
        .dialog()
        .file()
        .set_file_name(file_name)
        .add_filter("JSON 配置文件", &["json"]);
    // 默认落在下载目录；取不到时退回系统对话框自己的默认位置
    if let Ok(dir) = app.path().download_dir() {
        dialog = dialog.set_directory(dir);
    }
    dialog.save_file(move |chosen| {
        let path = chosen.and_then(|p| p.into_path().ok());
        let _ = tx.send(path);
    });

    let chosen = rx
        .await
        .map_err(|_| "文件对话框已关闭，未导出".to_string())?;
    let Some(path) = chosen else {
        // 用户点了取消，不是错误
        return Ok(None);
    };

    // 用户没写后缀时补上 .json，避免生成"无类型"文件
    let path = match path.extension() {
        Some(_) => path,
        None => {
            let mut p = path.into_os_string();
            p.push(".json");
            std::path::PathBuf::from(p)
        }
    };

    std::fs::write(&path, json).map_err(|e| format!("写入文件失败：{e}"))?;
    Ok(Some(path.display().to_string()))
}

/// 导入配置：校验 → 写盘 → 热生效（含按需重启监听）。
///
/// 会整体替换当前配置，因此导入前前端需二次确认。
#[tauri::command]
async fn import_config(
    state: tauri::State<'_, Arc<AppState>>,
    json: String,
) -> Result<AppConfig, String> {
    let cfg: AppConfig =
        serde_json::from_str(&json).map_err(|e| format!("配置文件格式不正确：{e}"))?;
    // 基本结构校验：确保是本项目导出的配置而非任意 JSON
    if cfg.server.host.trim().is_empty() {
        return Err("配置内容不完整：缺少 server.host 字段".into());
    }
    validate(&cfg)?;

    let path = state.config_path.clone();
    persist_config(&path, &cfg)?;
    *state.config.write() = cfg;

    let s = state.inner().clone();
    sync_server(&s).await;

    Ok(state.config.read().clone())
}

/// 重置用户的已用请求数（传 null 表示重置全部）
#[tauri::command]
fn reset_user_usage(
    state: tauri::State<Arc<AppState>>,
    id: Option<String>,
) -> Result<(), String> {
    state.reset_user_usage(id.as_deref());
    Ok(())
}

/// 各用户的已用请求数快照：`{ id: 已用次数 }`
#[tauri::command]
fn get_user_usage(
    state: tauri::State<Arc<AppState>>,
) -> Result<std::collections::HashMap<String, u64>, String> {
    let mut map = std::collections::HashMap::new();
    for entry in state.user_usage.iter() {
        map.insert(entry.key().clone(), entry.value().load(std::sync::atomic::Ordering::Relaxed));
    }
    Ok(map)
}

#[tauri::command]
async fn reload_config(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AppConfig, String> {
    reload(&state)?;
    let s = state.inner().clone();
    sync_server(&s).await;
    Ok(state.config.read().clone())
}

#[tauri::command]
async fn get_status(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<StatusDto, String> {
    let (host, port, providers, routes) = {
        let cfg = state.config.read();
        (
            cfg.server.host.clone(),
            cfg.server.port,
            cfg.providers.len(),
            cfg.routes.len(),
        )
    };
    // 服务实际监听的端口（端口为 0 时为内核分配的真实端口）
    let port = state
        .server
        .bound_addr()
        .await
        .map(|a| a.port())
        .unwrap_or(port);
    Ok(StatusDto {
        running: state.server.is_running().await,
        host,
        port,
        config_path: state.config_path.display().to_string(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        providers,
        routes,
        total_requests: state.metrics.snapshot().total,
        server_error: state.server.last_error().await,
    })
}

#[tauri::command]
fn get_metrics(
    state: tauri::State<Arc<AppState>>,
) -> Result<token_proxy_core::proxy::state::MetricsSnapshot, String> {
    Ok(state.metrics.snapshot())
}

#[tauri::command]
fn reset_breaker(state: tauri::State<Arc<AppState>>, provider_id: String) -> Result<(), String> {
    if let Some(b) = state.breakers.get(&provider_id) {
        b.reset();
    }
    Ok(())
}

#[tauri::command]
fn get_breaker_status(state: tauri::State<Arc<AppState>>) -> Result<serde_json::Value, String> {
    let mut map = serde_json::Map::new();
    for entry in state.breakers.iter() {
        let s = entry.value().status();
        map.insert(
            entry.key().clone(),
            serde_json::json!({
                "state": match s.state {
                    token_proxy_core::proxy::circuit::BreakerState::Closed => "closed",
                    token_proxy_core::proxy::circuit::BreakerState::Open => "open",
                    token_proxy_core::proxy::circuit::BreakerState::HalfOpen => "half_open",
                },
                "consecutive_failures": s.consecutive_failures,
            }),
        );
    }
    Ok(serde_json::Value::Object(map))
}

#[tauri::command]
fn get_dataset(
    state: tauri::State<Arc<AppState>>,
    limit: Option<i64>,
    offset: Option<i64>,
    model_filter: Option<String>,
) -> Result<Vec<DatasetRecord>, String> {
    let db_guard = state.db.lock();
    let db = db_guard.as_ref().ok_or("dataset DB not initialized")?;
    let records = db
        .query_records(
            limit.unwrap_or(50),
            offset.unwrap_or(0),
            model_filter.as_deref(),
        )
        .map_err(|e| e.to_string())?;
    Ok(records)
}

#[tauri::command]
fn get_dataset_count(
    state: tauri::State<Arc<AppState>>,
    model_filter: Option<String>,
) -> Result<i64, String> {
    let db_guard = state.db.lock();
    let db = db_guard.as_ref().ok_or("dataset DB not initialized")?;
    db.count(model_filter.as_deref()).map_err(|e| e.to_string())
}

#[tauri::command]
fn export_dataset_jsonl(
    state: tauri::State<Arc<AppState>>,
    model_filter: Option<String>,
) -> Result<String, String> {
    let db_guard = state.db.lock();
    let db = db_guard.as_ref().ok_or("dataset DB not initialized")?;
    db.export_jsonl(model_filter.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn clear_dataset(state: tauri::State<Arc<AppState>>) -> Result<(), String> {
    let db_guard = state.db.lock();
    let db = db_guard.as_ref().ok_or("dataset DB not initialized")?;
    db.clear().map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_dataset_record(state: tauri::State<Arc<AppState>>, id: i64) -> Result<(), String> {
    let db_guard = state.db.lock();
    let db = db_guard.as_ref().ok_or("dataset DB not initialized")?;
    db.delete(id).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info,token_proxy=debug".into()),
                )
                .try_init();

            // 加载配置
            let path = config_path();
            let cfg = load_config(&path).unwrap_or_else(|e| {
                tracing::error!("failed to load config at {}: {e}", path.display());
                AppConfig::default()
            });
            tracing::info!("config at {}", path.display());

            let state = Arc::new(AppState::new(cfg, path));
            *state.sink.lock() = Some(Arc::new(TauriSink {
                app: app.handle().clone(),
            }));

            // 初始化 SQLite 数据存储
            let db_path = default_db_path();
            tracing::info!("dataset db at {}", db_path.display());
            match init_db(&db_path) {
                Ok(conn) => {
                    let database = Arc::new(Database::new(conn));
                    *state.db.lock() = Some(database);
                }
                Err(e) => {
                    tracing::warn!("failed to init dataset db: {e}");
                }
            }

            app.manage(state.clone());

            // 启动代理服务器（可重启，监听地址变更时自动重新绑定）
            let server_state = state.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = server_state.server.start(server_state.clone()).await {
                    tracing::error!("proxy server failed to start: {e}");
                }
            });

            // 配置文件热重载监听
            let watch_state = state.clone();
            tauri::async_runtime::spawn(async move {
                use notify::{recommended_watcher, RecursiveMode, Watcher};
                let watch_dir = watch_state.config_path.parent().map(|p| p.to_path_buf());
                let mut watcher =
                    match recommended_watcher(move |res: notify::Result<notify::Event>| {
                        if let Ok(ev) = res {
                            if ev.kind.is_modify() || ev.kind.is_create() {
                                if let Err(e) = reload(&watch_state) {
                                    tracing::warn!("config hot-reload failed: {e}");
                                } else {
                                    tracing::info!("config hot-reloaded");
                                    if let Some(s) = watch_state.sink.lock().as_ref() {
                                        s.emit_config_changed();
                                    }
                                    // 监听地址变化时重启服务
                                    let s = watch_state.clone();
                                    tauri::async_runtime::spawn(async move {
                                        sync_server(&s).await;
                                    });
                                }
                            }
                        }
                    }) {
                        Ok(w) => w,
                        Err(e) => {
                            tracing::warn!("watcher setup failed: {e}");
                            return;
                        }
                    };
                if let Some(dir) = watch_dir {
                    let _ = watcher.watch(&dir, RecursiveMode::NonRecursive);
                }
                // 保持任务存活
                std::future::pending::<()>().await;
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            reload_config,
            export_config,
            export_config_to_file,
            import_config,
            reset_user_usage,
            get_user_usage,
            get_status,
            get_metrics,
            reset_breaker,
            get_breaker_status,
            get_dataset,
            get_dataset_count,
            export_dataset_jsonl,
            clear_dataset,
            delete_dataset_record,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
