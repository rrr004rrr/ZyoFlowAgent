//! ZyoFlow Master — 跨平台 GUI（egui / eframe），Windows + macOS。
//!
//! - 背景執行緒跑 HTTP 伺服器（預設 :39218），收站台閘道轉發來的 Zentao webhook。
//! - 主執行緒跑 egui 視窗，即時顯示訊息列表（macOS 要求 GUI 在主執行緒）。
//! - 關視窗 → 縮到系統匣（隱藏圖示區），背景續收；點托盤圖示/選單叫回。
//!   視窗顯示/隱藏在 Windows 用 Win32 ShowWindow 直接控制（eframe 隱藏後會停迴圈，
//!   靠 OS 層級才叫得回來）。
//!
//! 設定（環境變數）：ZYOFLOW_MASTER_PORT（預設 39218）、ZYOFLOW_MASTER_DB。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // release 不開主控台視窗

use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use eframe::egui;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use unicode_width::UnicodeWidthStr;
use std::time::Duration;

mod lark;

const WINDOW_TITLE: &str = "ZyoFlow Master";
const SUB_PORT: u16 = 39219; // Sub 監聽埠固定（打包寫死）；IP 由使用者填
// 站台閘道位址（撈 Zentao 使用者名單）；由環境變數提供，例如 http://192.168.x.x:39217

#[derive(Clone)]
struct Msg {
    title: String,
    body: String,
    created_at: String,
    action: String, // 原始英文動作（篩選用）
    otype: String,  // 原始英文 objectType（篩選用）
    actor: String,  // 原始英文 actor 帳號（篩選用）
}

type Db = Arc<Mutex<Connection>>;

#[derive(Clone)]
struct AppState {
    db: Db,
    feed: Arc<Mutex<Vec<Msg>>>, // 時間正序（舊→新）
}

fn main() -> eframe::Result<()> {
    let port: u16 = std::env::var("ZYOFLOW_MASTER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(39218);
    let db_path = std::env::var("ZYOFLOW_MASTER_DB").unwrap_or_else(|_| default_db_path());

    let conn = Connection::open(&db_path).expect("開啟資料庫失敗");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS messages(
            id         INTEGER PRIMARY KEY,
            title      TEXT,
            body       TEXT,
            raw        TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
        );
        CREATE TABLE IF NOT EXISTS subs(
            id         INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            addr       TEXT,                -- 網段IP:port，如 192.168.x.x:39219
            owners     TEXT,                -- Zentao 負責人帳號，逗號分隔（一 Sub 可多負責人）
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
        );
        CREATE TABLE IF NOT EXISTS settings(
            key   TEXT PRIMARY KEY,
            value TEXT
        );",
    )
    .expect("建表失敗");
    let db: Db = Arc::new(Mutex::new(conn));
    let feed = Arc::new(Mutex::new(load_recent(&db)));
    let state = AppState {
        db: db.clone(),
        feed: feed.clone(),
    };

    // 背景執行緒：HTTP 伺服器
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("建立 tokio runtime 失敗");
        rt.block_on(serve(port, state));
    });

    // 主執行緒：GUI
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([560.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(MasterApp::new(cc, feed, db, port, db_path)))),
    )
}

// ----------------------------------------------------- HTTP 伺服器
async fn serve(port: u16, state: AppState) {
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .route("/webhook", post(webhook))
        .route("/messages", get(list_messages))
        .with_state(state);
    let addr = format!("0.0.0.0:{port}");
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            println!("ZyoFlow Master 收信中 http://{addr}");
            let _ = axum::serve(listener, app).await;
        }
        Err(e) => eprintln!("無法綁定 {addr}: {e}"),
    }
}

async fn webhook(State(st): State<AppState>, body: Bytes) -> (StatusCode, Json<Value>) {
    // ponytail: 收原始 bytes 再 lossy 解碼，永不因非 UTF-8 而拒收
    let raw = String::from_utf8_lossy(&body).into_owned();
    let data: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);

    // 站台轉來的 enriched task 帶 kind="task"；舊格式（測試送）沒有
    let is_task = data.get("kind").and_then(Value::as_str) == Some("task");
    let (title, text) = parse_zentao(&data, &raw);
    let action = str_field(&data, "action").unwrap_or_default();
    let otype = str_field(&data, "object_type")
        .or_else(|| str_field(&data, "objectType"))
        .unwrap_or_default();
    let assignee = str_field(&data, "assignee").unwrap_or_default();
    let actor = if is_task {
        assignee.clone() // 任務顯示用負責人當 actor
    } else {
        str_field(&data, "actor").unwrap_or_default()
    };
    println!("[webhook] {title}  (task={is_task} assignee={assignee})");

    let created_at = {
        let conn = st.db.lock().unwrap();
        if let Err(e) = conn.execute(
            "INSERT INTO messages(title, body, raw) VALUES(?1, ?2, ?3)",
            rusqlite::params![title, text, raw],
        ) {
            eprintln!("[webhook] 寫入失敗: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "ok": false })));
        }
        conn.query_row(
            "SELECT created_at FROM messages WHERE id = last_insert_rowid()",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    };

    {
        let mut feed = st.feed.lock().unwrap();
        feed.push(Msg {
            title: title.clone(),
            body: text.clone(),
            created_at: created_at.clone(),
            action,
            otype,
            actor,
        });
        let len = feed.len();
        if len > 500 {
            feed.drain(0..len - 500); // ponytail: 畫面只留最近 500 則
        }
    }

    if is_task {
        // 派發規則過濾：類型/動作/產品不符就只顯示、不派發
        let r_product = str_field(&data, "product").unwrap_or_default();
        let r_action = str_field(&data, "action").unwrap_or_default();
        let r_otype = str_field(&data, "object_type")
            .or_else(|| str_field(&data, "objectType"))
            .unwrap_or_default();
        if !task_passes_rules(&st.db, &r_otype, &r_action, &r_product) {
            println!("[rule] 任務不符派發規則，略過 (type={r_otype} action={r_action} product={r_product})");
            return (StatusCode::OK, Json(json!({ "ok": true, "filtered": true })));
        }
        // 依 assignee 路由：dispatch 給 owners 含該負責人的啟用 Sub
        let targets = subs_for_owner(&st.db, &assignee);
        let dispatch_payload = json!({
            "task_id": str_field(&data, "task_id").unwrap_or_default(),
            "type": str_field(&data, "type").unwrap_or_else(|| "feature".into()),
            "product": str_field(&data, "product").unwrap_or_default(),
            "module": str_field(&data, "module").unwrap_or_default(),
            "title": title,
            "content": text,
            "assignee": assignee,
        })
        .to_string();
        if targets.is_empty() {
            println!("[dispatch] 找不到負責此任務的 Sub（assignee 沒對到 owners），略過");
        } else {
            std::thread::spawn(move || {
                for addr in targets {
                    let url = format!("http://{addr}/dispatch");
                    if let Err(e) = ureq::post(&url)
                        .timeout(Duration::from_secs(5))
                        .set("Content-Type", "application/json")
                        .send_string(&dispatch_payload)
                    {
                        eprintln!("[dispatch→sub] {url} 失敗: {e}");
                    }
                }
            });
        }
    } else {
        // 舊格式（測試送）：廣播給所有啟用 Sub 顯示
        let push_payload =
            json!({ "title": title, "body": text, "created_at": created_at }).to_string();
        let sub_addrs = all_enabled_subs(&st.db);
        if !sub_addrs.is_empty() {
            std::thread::spawn(move || {
                for addr in sub_addrs {
                    let url = format!("http://{addr}/push");
                    let _ = ureq::post(&url)
                        .timeout(Duration::from_secs(5))
                        .set("Content-Type", "application/json")
                        .send_string(&push_payload);
                }
            });
        }
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

async fn list_messages(State(st): State<AppState>) -> Json<Value> {
    let conn = st.db.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT id, title, body, raw, created_at FROM messages ORDER BY id DESC LIMIT 50")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "title": r.get::<_, Option<String>>(1)?,
                "body": r.get::<_, Option<String>>(2)?,
                "raw": r.get::<_, Option<String>>(3)?,
                "created_at": r.get::<_, String>(4)?,
            }))
        })
        .unwrap();
    Json(json!(rows.filter_map(Result::ok).collect::<Vec<_>>()))
}

fn load_recent(db: &Db) -> Vec<Msg> {
    let conn = db.lock().unwrap();
    let mut stmt = match conn
        .prepare("SELECT title, body, raw, created_at FROM messages ORDER BY id DESC LIMIT 200")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| {
        let raw = r.get::<_, Option<String>>(2)?.unwrap_or_default();
        let data: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        Ok(Msg {
            title: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            body: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            created_at: r.get::<_, String>(3)?,
            action: str_field(&data, "action").unwrap_or_default(),
            otype: str_field(&data, "objectType").unwrap_or_default(),
            actor: str_field(&data, "actor").unwrap_or_default(),
        })
    });
    let mut v: Vec<Msg> = match rows {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    };
    v.reverse(); // DESC 取回後反轉成正序（舊→新）
    v
}

// ------------------------------------------------------------ GUI
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Tasks,
    Subs,
    Rules,
    Lark,
    Settings,
}

/// Sub 連線測試狀態（每個 Sub 一份）
#[derive(Clone)]
enum Ping {
    Testing,
    Ok,
    Fail(String),
}

struct MasterApp {
    feed: Arc<Mutex<Vec<Msg>>>,
    db: Db,
    users: Arc<Mutex<Vec<(String, String)>>>, // Zentao 使用者 (account, realname) 快取
    port: u16,
    db_path: String,
    tab: Tab,
    filter_kw: String,
    filter_action: String,
    filter_otype: String,
    filter_actor: String,
    new_name: String,
    new_ip: String,
    new_owners: String,
    owner_pick: String,
    editing: Option<i64>, // 正在編輯的 Sub id；None = 新增模式
    test_results: Arc<Mutex<HashMap<i64, Ping>>>, // Sub 連線測試結果（id → 狀態）
    // 飛書測試頁狀態
    lark_app_id: String,
    lark_app_secret: String,                  // 僅記憶體；不落地，不進 repo
    lark_target_chat: String,                 // 固定發送目標的 chat_id（oc_...）
    lark_target_name: String,                 // 目標群組顯示名
    lark_chats: Arc<Mutex<Vec<(String, String)>>>, // 載入的群組清單 (name, chat_id)
    lark_text: String,
    lark_card_title: String,
    lark_card_body: String,
    lark_log: Arc<Mutex<Vec<String>>>,        // 發送結果日誌（背景執行緒寫、GUI 讀）
    lark_token: Arc<lark::TokenCache>,        // 共享 token 快取
    lark_last_msg: Arc<Mutex<Option<String>>>, // 最後一張卡片的 message_id（給就地更新用）
    // 派發規則
    rule_types: HashSet<String>,
    rule_actions: HashSet<String>,
    rule_products: String,
    rules_saved: bool,
    _tray: tray_icon::TrayIcon, // 保持存活：drop 掉圖示就消失
}

impl MasterApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        feed: Arc<Mutex<Vec<Msg>>>,
        db: Db,
        port: u16,
        db_path: String,
    ) -> Self {
        install_cjk_font(&cc.egui_ctx);
        let tray = build_tray(cc.egui_ctx.clone());
        let users = Arc::new(Mutex::new(Vec::new()));
        spawn_user_fetch(users.clone(), cc.egui_ctx.clone()); // 啟動時撈一次 Zentao 名單
        let cfg = lark::load_config(); // 飛書憑證 + 固定群組（存於 appdata）
        let rule_types = load_rule_set(&db, "rule_types", &["task", "bug", "story"]);
        let rule_actions = load_rule_set(
            &db,
            "rule_actions",
            &["opened", "assigned", "edited", "finished", "closed", "commented"],
        );
        let rule_products = get_setting(&db, "rule_products").unwrap_or_default();
        Self {
            feed,
            db,
            users,
            port,
            db_path,
            tab: Tab::Tasks,
            filter_kw: String::new(),
            filter_action: String::new(),
            filter_otype: String::new(),
            filter_actor: String::new(),
            new_name: String::new(),
            new_ip: String::new(),
            new_owners: String::new(),
            owner_pick: String::new(),
            editing: None,
            test_results: Arc::new(Mutex::new(HashMap::new())),
            lark_app_id: if !cfg.app_id.is_empty() {
                cfg.app_id
            } else {
                std::env::var("ZYOFLOW_LARK_APP_ID").unwrap_or_default()
            },
            lark_app_secret: if !cfg.app_secret.is_empty() {
                cfg.app_secret
            } else {
                std::env::var("ZYOFLOW_LARK_APP_SECRET").unwrap_or_default()
            },
            lark_target_chat: cfg.chat_id,
            lark_target_name: cfg.chat_name,
            lark_chats: Arc::new(Mutex::new(Vec::new())),
            lark_text: "Hello from ZyoFlow Master 👋".into(),
            lark_card_title: "ZyoFlow 測試卡片".into(),
            lark_card_body: "**狀態**：測試中\n\n這是一張互動卡片。".into(),
            lark_log: Arc::new(Mutex::new(Vec::new())),
            lark_token: Arc::new(Mutex::new(None)),
            lark_last_msg: Arc::new(Mutex::new(None)),
            rule_types,
            rule_actions,
            rule_products,
            rules_saved: false,
            _tray: tray,
        }
    }

    fn tasks_view(&mut self, ui: &mut egui::Ui) {
        let feed: Vec<Msg> = self.feed.lock().unwrap().clone(); // 快照

        // 下拉選項：feed 內出現過的動作 / 類型
        let mut actions: Vec<String> =
            feed.iter().map(|m| m.action.clone()).filter(|s| !s.is_empty()).collect();
        actions.sort();
        actions.dedup();
        let mut otypes: Vec<String> =
            feed.iter().map(|m| m.otype.clone()).filter(|s| !s.is_empty()).collect();
        otypes.sort();
        otypes.dedup();
        let mut actors: Vec<String> =
            feed.iter().map(|m| m.actor.clone()).filter(|s| !s.is_empty()).collect();
        actors.sort();
        actors.dedup();
        let users = self.users.lock().unwrap().clone(); // account→中文名

        // 進階篩選列
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("關鍵字");
            ui.add(egui::TextEdit::singleline(&mut self.filter_kw).desired_width(140.0));
            ui.label("動作");
            egui::ComboBox::from_id_source("f_action")
                .selected_text(if self.filter_action.is_empty() {
                    "全部".to_string()
                } else {
                    zh_action(&self.filter_action).to_string()
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filter_action, String::new(), "全部");
                    for a in &actions {
                        ui.selectable_value(&mut self.filter_action, a.clone(), zh_action(a));
                    }
                });
            ui.label("類型");
            egui::ComboBox::from_id_source("f_otype")
                .selected_text(if self.filter_otype.is_empty() {
                    "全部".to_string()
                } else {
                    zh_object(&self.filter_otype).to_string()
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filter_otype, String::new(), "全部");
                    for t in &otypes {
                        ui.selectable_value(&mut self.filter_otype, t.clone(), zh_object(t));
                    }
                });
            ui.label("負責人");
            egui::ComboBox::from_id_source("f_actor")
                .selected_text(if self.filter_actor.is_empty() {
                    "全部".to_string()
                } else {
                    name_of(&self.filter_actor, &users)
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filter_actor, String::new(), "全部");
                    for a in &actors {
                        ui.selectable_value(&mut self.filter_actor, a.clone(), name_of(a, &users));
                    }
                });
            if ui.button("清除").clicked() {
                self.filter_kw.clear();
                self.filter_action.clear();
                self.filter_otype.clear();
                self.filter_actor.clear();
            }
        });
        ui.separator();

        // 套用篩選（AND）
        let kw = self.filter_kw.trim().to_lowercase();
        let fa = self.filter_action.clone();
        let ft = self.filter_otype.clone();
        let fac = self.filter_actor.clone();
        let filtered: Vec<&Msg> = feed
            .iter()
            .rev()
            .filter(|m| {
                (kw.is_empty()
                    || m.title.to_lowercase().contains(&kw)
                    || m.body.to_lowercase().contains(&kw))
                    && (fa.is_empty() || m.action == fa)
                    && (ft.is_empty() || m.otype == ft)
                    && (fac.is_empty() || m.actor == fac)
            })
            .collect();

        if filtered.is_empty() {
            ui.add_space(20.0);
            let msg = if feed.is_empty() {
                "尚無訊息，等待 Zentao 通知…"
            } else {
                "沒有符合篩選的訊息"
            };
            ui.vertical_centered(|ui| ui.weak(msg));
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for m in &filtered {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.strong(&m.title);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| ui.weak(&m.created_at),
                            );
                        });
                        ui.label(&m.body);
                    });
                    ui.add_space(6.0);
                }
            });
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        egui::Grid::new("status")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.strong("接收埠");
                ui.label(format!(":{}", self.port));
                ui.end_row();
                ui.strong("資料庫");
                ui.label(&self.db_path);
                ui.end_row();
                ui.strong("已收訊息");
                ui.label(format!("{} 則", self.feed.lock().unwrap().len()));
                ui.end_row();
            });
        ui.add_space(16.0);
        ui.weak("更多設定之後加入。");
    }

    /// 派發規則：控管哪些任務類型 / 動作 / 產品才派發給 Sub。
    fn rules_view(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label("只有符合下列全部條件的任務才會派發給 Sub；不符合的只在「任務」分頁顯示、不派發。");

        ui.add_space(10.0);
        ui.strong("任務類型");
        ui.horizontal_wrapped(|ui| {
            for (val, label) in [("task", "任務"), ("bug", "Bug"), ("story", "需求")] {
                let mut on = self.rule_types.contains(val);
                if ui.checkbox(&mut on, label).changed() {
                    self.rules_saved = false;
                    if on {
                        self.rule_types.insert(val.to_string());
                    } else {
                        self.rule_types.remove(val);
                    }
                }
            }
        });
        ui.weak("全部不勾 = 不限制（全收）。");

        ui.add_space(10.0);
        ui.strong("觸發動作");
        ui.horizontal_wrapped(|ui| {
            for (val, label) in [
                ("opened", "建立"),
                ("assigned", "指派"),
                ("edited", "編輯"),
                ("finished", "完成"),
                ("closed", "關閉"),
                ("commented", "評論"),
            ] {
                let mut on = self.rule_actions.contains(val);
                if ui.checkbox(&mut on, label).changed() {
                    self.rules_saved = false;
                    if on {
                        self.rule_actions.insert(val.to_string());
                    } else {
                        self.rule_actions.remove(val);
                    }
                }
            }
        });
        ui.weak("全部不勾 = 不限制（全收）。");

        ui.add_space(10.0);
        ui.strong("限定產品（product）");
        if ui
            .add(
                egui::TextEdit::singleline(&mut self.rule_products)
                    .desired_width(340.0)
                    .hint_text("留空 = 全部；或填產品值，逗號分隔"),
            )
            .changed()
        {
            self.rules_saved = false;
        }
        ui.weak("產品值就是 Sub 視窗顯示的 product（你的禪道可能是數字 ID）。");

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if ui.button("💾 儲存規則").clicked() {
                set_setting(&self.db, "rule_types", &csv_of(&self.rule_types));
                set_setting(&self.db, "rule_actions", &csv_of(&self.rule_actions));
                set_setting(&self.db, "rule_products", self.rule_products.trim());
                self.rules_saved = true;
            }
            if self.rules_saved {
                ui.colored_label(egui::Color32::from_rgb(46, 160, 67), "已儲存");
            } else {
                ui.weak("有變更，記得按儲存");
            }
        });
    }

    /// 飛書出站測試頁：填憑證 + 收件人，發 純文字 / 富文本 / 互動卡片，並可就地更新卡片。
    fn lark_view(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);

        // 憑證（App Secret 僅記憶體，預填自環境變數 ZYOFLOW_LARK_APP_SECRET）
        egui::Grid::new("lark_cred")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("App ID");
                ui.add(egui::TextEdit::singleline(&mut self.lark_app_id).desired_width(300.0));
                ui.end_row();
                ui.label("App Secret");
                ui.add(
                    egui::TextEdit::singleline(&mut self.lark_app_secret)
                        .password(true)
                        .desired_width(300.0),
                );
                ui.end_row();
            });
        ui.horizontal(|ui| {
            if self.lark_app_secret.trim().is_empty() {
                ui.colored_label(egui::Color32::RED, "未填 App Secret");
            } else {
                ui.colored_label(egui::Color32::from_rgb(46, 160, 67), "● Secret 已載入");
            }
            if ui.button("💾 記住憑證到本機").clicked() {
                let cfg = lark::LarkConfig {
                    app_id: self.lark_app_id.clone(),
                    app_secret: self.lark_app_secret.clone(),
                    chat_id: self.lark_target_chat.clone(),
                    chat_name: self.lark_target_name.clone(),
                };
                let msg = match lark::save_config(&cfg) {
                    Ok(()) => "✅ 憑證已存到 %APPDATA%\\ZyoFlow\\lark.json".to_string(),
                    Err(e) => format!("❌ {e}"),
                };
                self.lark_log.lock().unwrap().push(msg);
            }
        });

        ui.add_space(6.0);
        ui.separator();
        // 目標群組（只發群組）：下拉選，選好即存檔固定
        ui.horizontal(|ui| {
            ui.label("目標群組");
            egui::ComboBox::from_id_source("lark_target_chat")
                .width(260.0)
                .selected_text(if self.lark_target_name.is_empty() {
                    "（按右邊「重新整理群組」載入）".to_string()
                } else {
                    self.lark_target_name.clone()
                })
                .show_ui(ui, |ui| {
                    let chats = self.lark_chats.lock().unwrap().clone();
                    if chats.is_empty() {
                        ui.weak("（清單空，先按重新整理）");
                    }
                    for (name, cid) in &chats {
                        if ui
                            .selectable_label(&self.lark_target_chat == cid, name)
                            .clicked()
                        {
                            self.lark_target_chat = cid.clone();
                            self.lark_target_name = name.clone();
                            // 選好就固定：立刻存檔
                            let _ = lark::save_config(&lark::LarkConfig {
                                app_id: self.lark_app_id.clone(),
                                app_secret: self.lark_app_secret.clone(),
                                chat_id: cid.clone(),
                                chat_name: name.clone(),
                            });
                        }
                    }
                });
            if ui.button("重新整理群組").clicked() {
                let (id, sec, cache) = (
                    self.lark_app_id.clone(),
                    self.lark_app_secret.clone(),
                    self.lark_token.clone(),
                );
                let chats_arc = self.lark_chats.clone();
                let log = self.lark_log.clone();
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    let line = match lark::list_chats(&id, &sec, &cache) {
                        Ok(list) => {
                            let n = list.len();
                            *chats_arc.lock().unwrap() = list;
                            format!("✅ 載入 {n} 個群組（下拉選取）")
                        }
                        Err(e) => format!("❌ 載入群組失敗：{e}"),
                    };
                    log.lock().unwrap().push(line);
                    ctx.request_repaint();
                });
            }
        });
        if self.lark_target_chat.is_empty() {
            ui.weak("尚未選群組。載入清單需 im:chat:readonly 權限。");
        } else {
            ui.weak(format!(
                "固定發送到：{}（{}）",
                self.lark_target_name, self.lark_target_chat
            ));
        }

        ui.separator();
        let has_target = !self.lark_target_chat.is_empty();
        if !has_target {
            ui.weak("先選好目標群組才能發送。");
        }
        // 純文字
        ui.horizontal(|ui| {
            ui.label("純文字");
            ui.add(egui::TextEdit::singleline(&mut self.lark_text).desired_width(320.0));
            if ui.add_enabled(has_target, egui::Button::new("發送")).clicked() {
                let (id, sec, cache) = (
                    self.lark_app_id.clone(),
                    self.lark_app_secret.clone(),
                    self.lark_token.clone(),
                );
                let (chat, text) = (self.lark_target_chat.clone(), self.lark_text.clone());
                spawn_send(self.lark_log.clone(), move || {
                    lark::send_message(&id, &sec, &cache, "chat_id", &chat, "text", &lark::text_content(&text))
                        .map(|m| format!("純文字已送出 · message_id={m}"))
                });
            }
        });
        // 富文本（固定樣本）
        ui.horizontal(|ui| {
            ui.label("富文本");
            ui.weak("（固定樣本：標題＋段落＋連結）");
            if ui.add_enabled(has_target, egui::Button::new("發送")).clicked() {
                let (id, sec, cache) = (
                    self.lark_app_id.clone(),
                    self.lark_app_secret.clone(),
                    self.lark_token.clone(),
                );
                let chat = self.lark_target_chat.clone();
                spawn_send(self.lark_log.clone(), move || {
                    lark::send_message(&id, &sec, &cache, "chat_id", &chat, "post", &lark::post_content())
                        .map(|m| format!("富文本已送出 · message_id={m}"))
                });
            }
        });

        ui.separator();
        // 互動卡片
        ui.label("互動卡片（含按鈕）");
        egui::Grid::new("lark_card")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("標題");
                ui.add(egui::TextEdit::singleline(&mut self.lark_card_title).desired_width(320.0));
                ui.end_row();
                ui.label("內文(md)");
                ui.add(
                    egui::TextEdit::multiline(&mut self.lark_card_body)
                        .desired_width(320.0)
                        .desired_rows(2),
                );
                ui.end_row();
            });
        ui.horizontal(|ui| {
            if ui.add_enabled(has_target, egui::Button::new("發送卡片")).clicked() {
                let (id, sec, cache) = (
                    self.lark_app_id.clone(),
                    self.lark_app_secret.clone(),
                    self.lark_token.clone(),
                );
                let chat = self.lark_target_chat.clone();
                let (title, body) = (self.lark_card_title.clone(), self.lark_card_body.clone());
                let last = self.lark_last_msg.clone();
                spawn_send(self.lark_log.clone(), move || {
                    let m = lark::send_message(
                        &id,
                        &sec,
                        &cache,
                        "chat_id",
                        &chat,
                        "interactive",
                        &lark::card_content(&title, &body),
                    )?;
                    *last.lock().unwrap() = Some(m.clone());
                    Ok(format!("卡片已送出 · message_id={m}"))
                });
            }
            let last_mid = self.lark_last_msg.lock().unwrap().clone();
            if let Some(mid) = last_mid {
                if ui.button("就地更新剛才的卡片").clicked() {
                    let (id, sec, cache) = (
                        self.lark_app_id.clone(),
                        self.lark_app_secret.clone(),
                        self.lark_token.clone(),
                    );
                    let title = self.lark_card_title.clone();
                    spawn_send(self.lark_log.clone(), move || {
                        lark::update_card(&id, &sec, &cache, &mid, &lark::updated_card_content(&title))?;
                        Ok(format!("已就地更新 · message_id={mid}"))
                    });
                }
            } else {
                ui.weak("（先發一張卡片才能更新）");
            }
        });
        ui.weak("卡片的「開啟網頁」按鈕點了會開連結；可互動的回呼按鈕等 Stage 2 接好回調再加。");

        ui.separator();
        // 結果日誌
        ui.horizontal(|ui| {
            ui.strong("結果");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("清除").clicked() {
                    self.lark_log.lock().unwrap().clear();
                }
            });
        });
        let log = self.lark_log.lock().unwrap().clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if log.is_empty() {
                    ui.weak("尚無結果。填好收件人後按上面的發送。");
                }
                for line in log.iter().rev() {
                    ui.label(line);
                }
            });
    }

    fn subs_view(&mut self, ui: &mut egui::Ui) {
        let users = self.users.lock().unwrap().clone(); // (account, realname) 快取快照
        let test_results = self.test_results.clone(); // Arc：讀快照 + 點擊時寫入
        let tests = test_results.lock().unwrap().clone(); // 本幀連線測試狀態快照

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.strong("Sub 路由（Zentao 負責人 → Sub）");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("重新整理名單").clicked() {
                    spawn_user_fetch(self.users.clone(), ui.ctx().clone());
                }
            });
        });
        ui.weak(format!("Sub 監聽埠固定 :{SUB_PORT}（打包寫死）；填 Sub 的 IP，一個 Sub 可掛多個負責人。"));
        ui.add_space(6.0);

        let subs = load_subs(&self.db);
        egui::ScrollArea::vertical()
            .id_source("subs_list")
            .auto_shrink([false, false])
            .max_height(230.0)
            .show(ui, |ui| {
                if subs.is_empty() {
                    ui.weak("尚無 Sub，請在下方新增。");
                }
                for s in &subs {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let mut en = s.enabled;
                            if ui.checkbox(&mut en, "").changed() {
                                set_sub_enabled(&self.db, s.id, en);
                            }
                            ui.strong(&s.name);
                            ui.weak(&s.addr);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("刪除").clicked() {
                                        delete_sub(&self.db, s.id);
                                    }
                                    if ui.button("編輯").clicked() {
                                        self.new_name = s.name.clone();
                                        self.new_ip =
                                            s.addr.split(':').next().unwrap_or("").to_string();
                                        self.new_owners = s.owners.clone();
                                        self.owner_pick.clear();
                                        self.editing = Some(s.id);
                                    }
                                    if ui.button("測試").clicked() {
                                        // 送一則測試訊息到 Sub 的 /push，讓 Sub 畫面也看得到；結果寫回 test_results，GUI 不卡
                                        let addr = s.addr.clone();
                                        let id = s.id;
                                        let tr = test_results.clone();
                                        let now = {
                                            let conn = self.db.lock().unwrap();
                                            conn.query_row(
                                                "SELECT datetime('now','localtime')",
                                                [],
                                                |r| r.get::<_, String>(0),
                                            )
                                            .unwrap_or_default()
                                        };
                                        let payload = json!({
                                            "title": "🔔 連線測試",
                                            "body": "來自 Master 的連線測試，收到這則代表推播通道正常。",
                                            "created_at": now,
                                        })
                                        .to_string();
                                        tr.lock().unwrap().insert(id, Ping::Testing);
                                        std::thread::spawn(move || {
                                            let url = format!("http://{addr}/push");
                                            let r = match ureq::post(&url)
                                                .timeout(Duration::from_secs(3))
                                                .set("Content-Type", "application/json")
                                                .send_string(&payload)
                                            {
                                                Ok(_) => Ping::Ok,
                                                Err(e) => Ping::Fail(e.to_string()),
                                            };
                                            tr.lock().unwrap().insert(id, r);
                                        });
                                    }
                                },
                            );
                        });
                        ui.horizontal(|ui| {
                            if s.owners.trim().is_empty() {
                                ui.weak("（未設定負責人）");
                            } else {
                                ui.label(format!("負責人：{}", names_of(&s.owners, &users)));
                            }
                            // 連線測試結果（按過「測試」才顯示）
                            if let Some(p) = tests.get(&s.id) {
                                ui.separator();
                                match p {
                                    Ping::Testing => {
                                        ui.weak("連線測試中…");
                                    }
                                    Ping::Ok => {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(46, 160, 67),
                                            "● 測試已送達（Sub 畫面會顯示）",
                                        );
                                    }
                                    Ping::Fail(e) => {
                                        ui.colored_label(egui::Color32::RED, "● 連不上")
                                            .on_hover_text(e);
                                    }
                                }
                            }
                        });
                    });
                    ui.add_space(4.0);
                }
            });

        ui.add_space(10.0);
        ui.separator();
        ui.label(if self.editing.is_some() {
            "編輯 Sub："
        } else {
            "新增 Sub："
        });
        ui.horizontal(|ui| {
            ui.label("名稱");
            ui.text_edit_singleline(&mut self.new_name);
            // 顯示寬度 ≤ 20（10 全形 = 20 半形），超過就砍尾
            while self.new_name.as_str().width() > 20 {
                self.new_name.pop();
            }
            ui.weak(format!("{}/20", self.new_name.as_str().width()));
        });
        ui.horizontal(|ui| {
            ui.label("IP　");
            ui.text_edit_singleline(&mut self.new_ip);
            ui.weak(format!(":{SUB_PORT}"));
            let ip = self.new_ip.trim();
            if !ip.is_empty() && ip.parse::<std::net::Ipv4Addr>().is_err() {
                ui.colored_label(egui::Color32::RED, "IP 格式不正確");
            }
        });

        // 負責人下拉：Zentao 使用者（顯示中文名、值存 account）；撈不到退回收過的 actor
        let pool: Vec<(String, String)> = if users.is_empty() {
            distinct_owners(&self.db)
                .into_iter()
                .map(|a| (a.clone(), a))
                .collect()
        } else {
            users.clone()
        };
        ui.horizontal(|ui| {
            ui.label("負責人");
            egui::ComboBox::from_id_source("owner_pick")
                .width(240.0)
                .selected_text(if self.owner_pick.is_empty() {
                    "選擇…".to_string()
                } else {
                    name_of(&self.owner_pick, &users)
                })
                .show_ui(ui, |ui| {
                    if pool.is_empty() {
                        ui.weak("（名單空：確認站台 Zentao API，或按上方重新整理）");
                    }
                    for (acc, name) in &pool {
                        let label = if name == acc {
                            acc.clone()
                        } else {
                            format!("{name}（{acc}）")
                        };
                        ui.selectable_value(&mut self.owner_pick, acc.clone(), label);
                    }
                });
            if ui.button("加入").clicked() && !self.owner_pick.trim().is_empty() {
                let pick = self.owner_pick.trim().to_string();
                let mut list: Vec<String> = self
                    .new_owners
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !list.contains(&pick) {
                    list.push(pick);
                }
                self.new_owners = list.join(", ");
            }
        });
        // 已選負責人：每個點一下 ✕ 單獨移除
        let selected: Vec<String> = self
            .new_owners
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        ui.horizontal_wrapped(|ui| {
            ui.label("已選負責人：");
            if selected.is_empty() {
                ui.weak("（無）");
            }
            let mut remove: Option<String> = None;
            for acc in &selected {
                if ui
                    .button(format!("{} ×", name_of(acc, &users)))
                    .on_hover_text("移除")
                    .clicked()
                {
                    remove = Some(acc.clone());
                }
            }
            if let Some(r) = remove {
                self.new_owners = selected
                    .iter()
                    .filter(|a| **a != r)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
            }
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let editing = self.editing;
            let save = if editing.is_some() { "儲存修改" } else { "新增 Sub" };
            if ui.button(save).clicked() {
                let name = self.new_name.trim().to_string();
                let ip = self.new_ip.trim().to_string();
                let owners = self.new_owners.trim().to_string();
                if !name.is_empty() && ip.parse::<std::net::Ipv4Addr>().is_ok() {
                    let addr = format!("{ip}:{SUB_PORT}");
                    match editing {
                        Some(id) => update_sub(&self.db, id, &name, &addr, &owners),
                        None => add_sub(&self.db, &name, &addr, &owners),
                    }
                    self.new_name.clear();
                    self.new_ip.clear();
                    self.new_owners.clear();
                    self.owner_pick.clear();
                    self.editing = None;
                }
            }
            if editing.is_some() && ui.button("取消").clicked() {
                self.new_name.clear();
                self.new_ip.clear();
                self.new_owners.clear();
                self.owner_pick.clear();
                self.editing = None;
            }
        });
    }
}

/// 背景執行緒發飛書，結果(成功/失敗)寫進日誌；不卡 GUI。日誌只留最近 200 則。
fn spawn_send<F>(log: Arc<Mutex<Vec<String>>>, f: F)
where
    F: FnOnce() -> Result<String, String> + Send + 'static,
{
    std::thread::spawn(move || {
        let line = match f() {
            Ok(s) => format!("✅ {s}"),
            Err(e) => format!("❌ {e}"),
        };
        let mut g = log.lock().unwrap();
        g.push(line);
        let n = g.len();
        if n > 200 {
            g.drain(0..n - 200);
        }
    });
}

impl eframe::App for MasterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 按視窗右上角 X → 不結束，縮到系統匣（隱藏圖示區）
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            hide_window(ctx);
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("ZyoFlow Master");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(46, 160, 67),
                        format!("接收中 · :{}", self.port),
                    );
                });
            });
            // 頁籤
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Tasks, "任務");
                ui.selectable_value(&mut self.tab, Tab::Subs, "Sub 路由");
                ui.selectable_value(&mut self.tab, Tab::Rules, "派發規則");
                ui.selectable_value(&mut self.tab, Tab::Lark, "飛書測試");
                ui.selectable_value(&mut self.tab, Tab::Settings, "設定");
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Tasks => self.tasks_view(ui),
            Tab::Subs => self.subs_view(ui),
            Tab::Rules => self.rules_view(ui),
            Tab::Lark => self.lark_view(ui),
            Tab::Settings => self.settings_view(ui),
        });

        // eframe 認為視窗一直可見（隱藏是用 OS 層級做的），這個 tick 會一直跑、feed 保持更新
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

// -------------------------------------------------- Sub 路由表（設定）
struct Sub {
    id: i64,
    name: String,
    addr: String,
    owners: String,
    enabled: bool,
}

// 從站台閘道撈 Zentao 使用者 (account, realname)
fn fetch_users(url: &str) -> Vec<(String, String)> {
    let body = match ureq::get(url).timeout(Duration::from_secs(8)).call() {
        Ok(resp) => resp.into_string().unwrap_or_default(),
        Err(_) => return Vec::new(),
    };
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|u| {
                    let acc = u.get("account").and_then(Value::as_str)?.trim().to_string();
                    if acc.is_empty() {
                        return None;
                    }
                    let name = u
                        .get("realname")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(acc.as_str())
                        .to_string();
                    Some((acc, name))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn spawn_user_fetch(users: Arc<Mutex<Vec<(String, String)>>>, ctx: egui::Context) {
    std::thread::spawn(move || {
        // 站台位址由環境變數 ZYOFLOW_GATEWAY_URL 提供；沒設則撈不到、改用訊息裡出現過的負責人
        let base = std::env::var("ZYOFLOW_GATEWAY_URL").unwrap_or_default();
        let fetched = fetch_users(&format!("{base}/api/users"));
        if !fetched.is_empty() {
            *users.lock().unwrap() = fetched;
            ctx.request_repaint();
        }
    });
}

/// account → 中文名（查不到就回 account 本身）
fn name_of(acc: &str, users: &[(String, String)]) -> String {
    users
        .iter()
        .find(|(a, _)| a == acc)
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| acc.to_string())
}

/// 逗號分隔的 account 清單 → 中文名清單（頓號分隔）
fn names_of(csv: &str, users: &[(String, String)]) -> String {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|a| name_of(a, users))
        .collect::<Vec<_>>()
        .join("、")
}

fn distinct_owners(db: &Db) -> Vec<String> {
    let conn = db.lock().unwrap();
    let mut stmt = match conn.prepare("SELECT raw FROM messages ORDER BY id DESC LIMIT 500") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut set = std::collections::BTreeSet::new();
    if let Ok(rows) = stmt.query_map([], |r| r.get::<_, Option<String>>(0)) {
        for raw in rows.flatten().flatten() {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(a) = v.get("actor").and_then(Value::as_str) {
                    if !a.is_empty() {
                        set.insert(a.to_string());
                    }
                }
            }
        }
    }
    set.into_iter().collect()
}

/// 依負責人帳號找出 owners 含它的啟用 Sub 位址（任務路由用）。
fn subs_for_owner(db: &Db, assignee: &str) -> Vec<String> {
    if assignee.trim().is_empty() {
        return Vec::new();
    }
    let conn = db.lock().unwrap();
    let mut stmt = match conn.prepare("SELECT addr, owners FROM subs WHERE enabled = 1 AND addr <> ''") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default()))
    });
    let mut out = Vec::new();
    if let Ok(it) = rows {
        for (addr, owners) in it.flatten() {
            if owners.split(',').map(str::trim).any(|o| o == assignee) {
                out.push(addr);
            }
        }
    }
    out
}

/// 所有啟用且有位址的 Sub（舊格式廣播用）。
fn all_enabled_subs(db: &Db) -> Vec<String> {
    let conn = db.lock().unwrap();
    conn.prepare("SELECT addr FROM subs WHERE enabled = 1 AND addr <> ''")
        .and_then(|mut s| {
            let rows = s.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
}

// -------------------------------------------------- 派發規則（settings 表）
fn get_setting(db: &Db, key: &str) -> Option<String> {
    let conn = db.lock().unwrap();
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get::<_, String>(0))
        .ok()
}

fn set_setting(db: &Db, key: &str, value: &str) {
    let conn = db.lock().unwrap();
    let _ = conn.execute(
        "INSERT INTO settings(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
        rusqlite::params![key, value],
    );
}

/// 任務是否符合派發規則。三者皆「未設定或留空 = 不限制(全收)；否則須在清單內」。
fn task_passes_rules(db: &Db, otype: &str, action: &str, product: &str) -> bool {
    let allows = |key: &str, val: &str| match get_setting(db, key) {
        None => true,
        Some(csv) => {
            let t = csv.trim();
            t.is_empty() || t.split(',').map(str::trim).any(|x| x == val)
        }
    };
    allows("rule_types", otype) && allows("rule_actions", action) && allows("rule_products", product)
}

/// 從 settings 讀規則集合；未設定回傳「全部」當預設。
fn load_rule_set(db: &Db, key: &str, all: &[&str]) -> HashSet<String> {
    match get_setting(db, key) {
        Some(csv) => csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        None => all.iter().map(|s| s.to_string()).collect(),
    }
}

fn csv_of(set: &HashSet<String>) -> String {
    let mut v: Vec<&str> = set.iter().map(String::as_str).collect();
    v.sort_unstable();
    v.join(",")
}

fn load_subs(db: &Db) -> Vec<Sub> {
    let conn = db.lock().unwrap();
    let mut stmt = match conn.prepare("SELECT id, name, addr, owners, enabled FROM subs ORDER BY id")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| {
        Ok(Sub {
            id: r.get(0)?,
            name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            addr: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            owners: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            enabled: r.get::<_, i64>(4)? != 0,
        })
    });
    match rows {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

fn add_sub(db: &Db, name: &str, addr: &str, owners: &str) {
    let conn = db.lock().unwrap();
    let _ = conn.execute(
        "INSERT INTO subs(name, addr, owners, enabled) VALUES(?1, ?2, ?3, 1)",
        rusqlite::params![name, addr, owners],
    );
}

fn delete_sub(db: &Db, id: i64) {
    let conn = db.lock().unwrap();
    let _ = conn.execute("DELETE FROM subs WHERE id = ?1", [id]);
}

fn update_sub(db: &Db, id: i64, name: &str, addr: &str, owners: &str) {
    let conn = db.lock().unwrap();
    let _ = conn.execute(
        "UPDATE subs SET name = ?1, addr = ?2, owners = ?3 WHERE id = ?4",
        rusqlite::params![name, addr, owners, id],
    );
}

fn set_sub_enabled(db: &Db, id: i64, enabled: bool) {
    let conn = db.lock().unwrap();
    let _ = conn.execute(
        "UPDATE subs SET enabled = ?1 WHERE id = ?2",
        rusqlite::params![enabled as i64, id],
    );
}

// ---------------------------------------------------------- 系統匣
fn build_tray(ctx: egui::Context) -> tray_icon::TrayIcon {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    let menu = Menu::new();
    let show = MenuItem::new("顯示視窗", true, None);
    let quit = MenuItem::new("結束", true, None);
    let _ = menu.append(&show);
    let _ = menu.append(&quit);
    let show_id = show.id().clone();
    let quit_id = quit.id().clone();

    // 點托盤選單：顯示 / 結束
    let ctx_menu = ctx.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == show_id {
            show_window(&ctx_menu);
        } else if e.id == quit_id {
            std::process::exit(0);
        }
    }));

    // 左鍵點圖示 → 顯示視窗
    let ctx_tray = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = e
        {
            show_window(&ctx_tray);
        }
    }));

    TrayIconBuilder::new()
        .with_tooltip(WINDOW_TITLE)
        .with_icon(tray_icon_image())
        .with_menu(Box::new(menu))
        .build()
        .expect("建立系統匣圖示失敗")
}

fn show_window(ctx: &egui::Context) {
    #[cfg(windows)]
    {
        let _ = ctx;
        win::show();
    }
    #[cfg(not(windows))]
    {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
}

fn hide_window(ctx: &egui::Context) {
    #[cfg(windows)]
    {
        let _ = ctx;
        win::hide();
    }
    #[cfg(not(windows))]
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
}

/// Windows：用 Win32 直接控制視窗顯示/隱藏（依視窗標題找 HWND）。
#[cfg(windows)]
mod win {
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{FindWindowW, SetForegroundWindow, ShowWindow, SW_HIDE, SW_SHOW};

    fn find() -> HWND {
        let title: Vec<u16> = super::WINDOW_TITLE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) }
    }
    pub fn show() {
        let hwnd = find();
        if !hwnd.is_null() {
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
            }
        }
    }
    pub fn hide() {
        let hwnd = find();
        if !hwnd.is_null() {
            unsafe {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

fn tray_icon_image() -> tray_icon::Icon {
    let (w, h) = (32u32, 32u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&[0x4a, 0x90, 0xd9, 0xff]); // 實心藍方塊
    }
    tray_icon::Icon::from_rgba(rgba, w, h).expect("產生圖示失敗")
}

/// egui 預設字型沒有中日韓字，載入系統 CJK 字型，否則中文會變方框。
fn install_cjk_font(ctx: &egui::Context) {
    let candidates = [
        "C:/Windows/Fonts/msjh.ttc",            // Windows 正體中文（微軟正黑體）
        "C:/Windows/Fonts/msyh.ttc",            // Windows 簡體
        "C:/Windows/Fonts/simsun.ttc",
        "/System/Library/Fonts/PingFang.ttc",   // macOS
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(fam).or_default().insert(0, "cjk".to_owned());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

// -------------------------------------------------------- 共用工具
fn default_db_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("master.sqlite")))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "master.sqlite".into())
}

/// 從 Zentao webhook 取出顯示用的 (title, body)。實測欄位：
/// objectType / objectID / action / actor / text…；text 已是現成人話。
fn parse_zentao(data: &Value, raw: &str) -> (String, String) {
    let body = str_field(data, "text")
        .or_else(|| str_field(data, "comment"))
        .unwrap_or_else(|| raw.chars().take(500).collect());

    let otype_raw = str_field(data, "objectType").unwrap_or_default();
    let action_raw = str_field(data, "action").unwrap_or_default();
    let oid = match data.get("objectID") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };

    let mut title = String::from(zh_object(&otype_raw));
    if !oid.is_empty() {
        if !title.is_empty() {
            title.push(' ');
        }
        title.push('#');
        title.push_str(&oid);
    }
    let action = zh_action(&action_raw);
    if !action.is_empty() {
        if !title.is_empty() {
            title.push_str(" · ");
        }
        title.push_str(action);
    }
    if title.is_empty() {
        title = str_field(data, "title")
            .or_else(|| str_field(data, "subject"))
            .unwrap_or_else(|| "Zentao 通知".into());
    }
    (title, body)
}

fn str_field(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn zh_object(s: &str) -> &str {
    match s {
        "task" => "任務",
        "bug" => "Bug",
        "story" => "需求",
        "doc" => "文件",
        "execution" => "迭代",
        "project" => "專案",
        "testtask" => "測試單",
        "testcase" => "用例",
        "release" => "發布",
        other => other,
    }
}

fn zh_action(s: &str) -> &str {
    match s {
        "opened" => "建立",
        "edited" => "編輯",
        "assigned" => "指派",
        "commented" => "評論",
        "finished" => "完成",
        "closed" => "關閉",
        "activated" => "啟動",
        "started" => "開始",
        "deleted" => "刪除",
        "canceled" => "取消",
        "resolved" => "解決",
        other => other,
    }
}
