use std::sync::Arc;

use tauri::Manager;
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

#[tauri::command]
fn get_config(state: tauri::State<Arc<AppState>>) -> Result<AppConfig, String> {
    Ok(state.config.read().clone())
}

#[tauri::command]
fn save_config(state: tauri::State<Arc<AppState>>, cfg: AppConfig) -> Result<(), String> {
    validate(&cfg)?;
    let path = state.config_path.clone();
    persist_config(&path, &cfg)?;
    *state.config.write() = cfg;
    Ok(())
}

#[tauri::command]
fn reload_config(state: tauri::State<Arc<AppState>>) -> Result<AppConfig, String> {
    reload(&state)?;
    Ok(state.config.read().clone())
}

#[tauri::command]
fn get_status(state: tauri::State<Arc<AppState>>) -> Result<StatusDto, String> {
    let cfg = state.config.read();
    Ok(StatusDto {
        running: true,
        host: cfg.server.host.clone(),
        port: cfg.server.port,
        config_path: state.config_path.display().to_string(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        providers: cfg.providers.len(),
        routes: cfg.routes.len(),
        total_requests: state.metrics.snapshot().total,
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

            // 启动代理服务器（后台任务）
            let server_state = state.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = token_proxy_core::proxy::serve(server_state).await {
                    tracing::error!("proxy server exited: {e}");
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
