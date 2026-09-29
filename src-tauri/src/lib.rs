use std::sync::Arc;

mod compat_setup;

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
/// 归一化对外代理地址与令牌：0.0.0.0 等监听地址对外不可直连，改写为 127.0.0.1；
/// 本地令牌为空时用占位值（代理未启用鉴权时任意义字符串均可）。
fn compat_target(state: &AppState) -> (String, String) {
    let cfg = state.config.read();
    let host = match cfg.server.host.trim() {
        "" | "0.0.0.0" | "::" => "127.0.0.1",
        h => h,
    };
    let token = match cfg.server.local_api_key.trim() {
        "" => compat_setup::PLACEHOLDER_TOKEN.to_string(),
        t => t.to_string(),
    };
    (format!("http://{}:{}", host, cfg.server.port), token)
}

/// 代理当前可服务的模型名列表（显式路由在前，渠道模型列表在后，去重）。
///
/// 供一键接入写 CLI 的模型槽位（Claude Code `/model` 选择器、Codex 默认模型），
/// 与 `/v1/models` 的口径一致（额外包含路由项，路由可直接指向未在渠道
/// models 里列出的上游模型名）。
fn proxy_model_names(cfg: &AppConfig) -> Vec<String> {
    fn push(distinct: &mut Vec<String>, m: &str) {
        if !m.is_empty() && !distinct.iter().any(|x| x == m) {
            distinct.push(m.to_string());
        }
    }
    let mut out = Vec::new();
    for r in &cfg.routes {
        if r.disabled {
            continue;
        }
        if cfg.providers.iter().any(|p| p.id == r.provider && p.enabled) {
            push(&mut out, &r.model);
        }
    }
    for p in cfg.providers.iter().filter(|p| p.enabled) {
        for m in &p.models {
            push(&mut out, m);
        }
    }
    out
}

/// 组装两个 CLI 工具的接入状态
fn compat_status_value(state: &Arc<AppState>) -> Result<serde_json::Value, String> {
    let (base, _) = compat_target(state);
    let home = compat_setup::home_dir()?;
    let claude = compat_setup::claude_status(&home, &base);
    let codex = compat_setup::codex_status(&home, &format!("{base}/v1"));
    Ok(serde_json::json!({
        "base_url": base,
        "claude": claude,
        "codex": codex,
    }))
}

/// 一键接入状态：Claude Code / Codex 是否已指向本代理
#[tauri::command]
fn compat_integration_status(state: tauri::State<Arc<AppState>>) -> Result<serde_json::Value, String> {
    compat_status_value(&state)
}

/// 一键接入：把 Claude Code / Codex 的本地配置改写为指向本代理
#[tauri::command]
fn compat_integrate(
    state: tauri::State<Arc<AppState>>,
    target: String,
) -> Result<serde_json::Value, String> {
    let (base, token) = compat_target(&state);
    let home = compat_setup::home_dir()?;
    match target.as_str() {
        "claude" => {
            let (models, fallback, max_ctx) = {
                let cfg = state.config.read();
                (
                    proxy_model_names(&cfg),
                    cfg.compat.claude_code_default_model.clone(),
                    cfg.compat.claude_code_max_context_tokens,
                )
            };
            compat_setup::integrate_claude(&home, &base, &token, &models, &fallback, max_ctx)?;
        }
        "codex" => {
            // 默认模型 = 兜底模型；未配置兜底时取代理第一个模型
            let default = {
                let cfg = state.config.read();
                match cfg.compat.codex_default_model.trim() {
                    "" => proxy_model_names(&cfg).first().cloned().unwrap_or_default(),
                    d => d.to_string(),
                }
            };
            compat_setup::integrate_codex(&home, &format!("{base}/v1"), &token, &default)?;
        }
        other => return Err(format!("未知接入目标：{other}")),
    }
    compat_status_value(&state)
}

/// 断开接入：从快照恢复 CLI 原配置（无快照时仅移除本代理写入的键）
#[tauri::command]
fn compat_disconnect(
    state: tauri::State<Arc<AppState>>,
    target: String,
) -> Result<serde_json::Value, String> {
    let home = compat_setup::home_dir()?;
    match target.as_str() {
        "claude" => {
            compat_setup::disconnect_claude(&home)?;
        }
        "codex" => {
            compat_setup::disconnect_codex(&home)?;
        }
        other => return Err(format!("未知接入目标：{other}")),
    }
    compat_status_value(&state)
}

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
        lan_ips: lan_ipv4_addrs(),
    })
}

/// 本机对外的局域网 IPv4 地址，供总览页展示访问入口。
///
/// 过滤规则：排除回环与 `169.254.x.x` 链路本地地址（APIPA，来自断开或
/// 虚拟网卡，不可用于局域网访问），只保留私有网段（10/8、172.16/12、192.168/16）。
/// 默认路由（当前实际联网网卡）的地址排在首位。
fn lan_ipv4_addrs() -> Vec<String> {
    let mut v: Vec<String> = local_ip_address::list_afinet_netifas()
        .map(|ifs| {
            let mut v: Vec<String> = ifs
                .into_iter()
                .filter_map(|(_, ip)| match ip {
                    std::net::IpAddr::V4(v4)
                        if !v4.is_loopback()
                            && !v4.is_link_local()
                            && v4.is_private() =>
                    {
                        Some(v4.to_string())
                    }
                    _ => None,
                })
                .collect();
            v.sort();
            v.dedup();
            v
        })
        .unwrap_or_default();
    // 默认路由地址（出网流量实际使用的网卡）置顶
    if let Ok(primary) = local_ip_address::local_ip() {
        if let std::net::IpAddr::V4(p) = primary {
            let s = p.to_string();
            if let Some(pos) = v.iter().position(|x| *x == s) {
                v.remove(pos);
                v.insert(0, s);
            }
        }
    }
    v
}

#[tauri::command]
fn get_metrics(
    state: tauri::State<Arc<AppState>>,
) -> Result<token_proxy_core::proxy::state::MetricsSnapshot, String> {
    Ok(state.metrics.snapshot())
}

#[tauri::command]
fn reset_breaker(state: tauri::State<Arc<AppState>>, provider_id: String) -> Result<(), String> {
    // 熔断器按 route 维度隔离，key = `{model}|{provider_id}|{upstream_model}`；
    // 重置某渠道 = 重置其名下全部路由熔断器（立即恢复全量流量，不触发慢启动）
    for entry in state.breakers.iter() {
        if is_route_of_provider(entry.key(), &provider_id) {
            entry.value().reset();
        }
    }
    Ok(())
}

/// 判断 route_key（`model|provider_id|upstream_model`）是否属于指定 provider。
///
/// provider id 不含 `|` 且为 key 的第 2 段；model 含 `|` 的极端情况下
/// 会导致误判失败（重置不到），但不会误伤其它渠道。
fn is_route_of_provider(route_key: &str, provider_id: &str) -> bool {
    let mut parts = route_key.splitn(3, '|');
    parts.next(); // model
    parts.next() == Some(provider_id)
}

#[tauri::command]
fn get_breaker_status(state: tauri::State<Arc<AppState>>) -> Result<serde_json::Value, String> {
    // route 级熔断器按 provider 聚合展示：状态取最差（Open > HalfOpen > Closed），
    // 连续失败取最大值。key 仍为 provider_id，与旧版前端接口兼容。
    use std::collections::HashMap;
    use token_proxy_core::proxy::circuit::BreakerState;

    let cb_cfg = state.config.read().retry.circuit_breaker.clone();
    let mut per_provider: HashMap<String, (u8, u32)> = HashMap::new();
    for entry in state.breakers.iter() {
        let Some(pid) = route_key_provider(entry.key()) else {
            continue;
        };
        let s = entry.value().status();
        let rank = match s.state {
            BreakerState::Closed => 0u8,
            BreakerState::HalfOpen => 1,
            BreakerState::Open => 2,
        };
        let e = per_provider.entry(pid.to_string()).or_insert((0, 0));
        if rank > e.0 {
            e.0 = rank;
        }
        e.1 = e.1.max(s.consecutive_failures);
    }

    let mut map = serde_json::Map::new();
    for (pid, (rank, failures)) in per_provider {
        // 熔断中（open）给出最长剩余冷却秒数，供前端告警栏倒计时
        let cooldown = if rank == 2 {
            state
                .breakers
                .iter()
                .filter(|e| route_key_provider(e.key()) == Some(pid.as_str()))
                .filter_map(|e| e.value().cooldown_remaining_secs(&cb_cfg))
                .filter(|s| *s > 0)
                .min()
        } else {
            None
        };
        map.insert(
            pid,
            serde_json::json!({
                "state": match rank {
                    2 => "open",
                    1 => "half_open",
                    _ => "closed",
                },
                "consecutive_failures": failures,
                "cooldown_remaining_secs": cooldown,
            }),
        );
    }
    Ok(serde_json::Value::Object(map))
}

/// 从 route_key 中提取 provider_id（第 2 段）；格式不合法时返回 None。
fn route_key_provider(route_key: &str) -> Option<&str> {
    let mut parts = route_key.splitn(3, '|');
    parts.next()?; // model
    let pid = parts.next()?;
    if pid.is_empty() {
        None
    } else {
        Some(pid)
    }
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
            compat_integration_status,
            compat_integrate,
            compat_disconnect,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
