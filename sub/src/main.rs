//! ZyoFlow Sub — 桌面常駐接收端（egui / eframe），Windows + macOS。
//!
//! 流程：收站台派發(/dispatch) → 設 git worktree → 跑 Claude headless →
//!       遇決策 Claude 吐 `@@ASK@@ {questions}` → 發站台(/lark/ask)發飛書問題卡 →
//!       使用者在飛書點選 → 站台把答案 POST 回本端(/answer) → `claude --resume` 續跑。
//!
//! 設定：exe 旁 sub.json {station, self_addr, claude_bin, worktree_root, repos:{產品→路徑}}。
//!       self_addr 留空會自動偵測本機 LAN IP + 埠。
//! 環境變數：ZYOFLOW_SUB_PORT（預設 39219）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // release 不開主控台視窗

use axum::{
    body::Bytes,
    extract::State,
    routing::{get, post},
    Json, Router,
};
use eframe::egui;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

const WINDOW_TITLE: &str = "ZyoFlow Sub";

#[derive(Clone)]
struct Msg {
    title: String,
    body: String,
    created_at: String,
}

type Feed = Arc<Mutex<Vec<Msg>>>;
type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>; // cid → 等答案的 runner

/// 站台回來的本機綁定狀態（給 GUI「綁定狀態」分頁顯示）。
#[derive(Clone, Default)]
struct Binding {
    zentao_account: String,
    feishu_name: String,
    enabled: bool,
    registered: bool,   // 是否成功向站台報到過
    last_error: String, // 報到失敗原因
}
type BindingState = Arc<Mutex<Binding>>;

#[derive(Clone)]
struct AppState {
    feed: Feed,
    pending: Pending,
    cfg: Arc<SubConfig>,
}

// ------------------------------------------------------------ 設定
struct Repo {
    path: String,
    keywords: String, // 描述/關鍵字，AI 依此 + 任務內容挑這個 repo
    name: String,     // 顯示用，可省
}

struct SubConfig {
    station: String,       // 站台 host:port（發 /lark/ask）
    self_addr: String,     // 本端 host:port（站台回 /answer 用）
    claude_bin: String,    // 預設 "claude"
    worktree_root: String, // worktree 根目錄
    repos: Vec<Repo>,      // 多個程式庫；派任務時由 AI 依關鍵字選一個
}

fn config_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sub.json")))
}

fn load_config() -> SubConfig {
    let v: Value = config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let s = |k: &str, d: &str| v.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
    let mut repos = Vec::new();
    if let Some(arr) = v.get("repos").and_then(Value::as_array) {
        for r in arr {
            let path = r.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            if path.is_empty() {
                continue;
            }
            repos.push(Repo {
                path,
                keywords: r.get("keywords").and_then(Value::as_str).unwrap_or("").to_string(),
                name: r.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            });
        }
    }
    SubConfig {
        station: s("station", ""),
        self_addr: s("self_addr", ""),
        claude_bin: s("claude_bin", "claude"),
        worktree_root: s("worktree_root", "zyoflow-worktrees"),
        repos,
    }
}

/// 偵測本機對外的 LAN IP（連個 UDP 不送資料，讀 local_addr）。
fn local_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    s.local_addr().ok().map(|a| a.ip().to_string())
}

/// 電腦名稱（站台綁定用的穩定鍵）：先試 hostname 指令，再退環境變數。
fn hostname() -> String {
    if let Ok(out) = Command::new("hostname").output() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "sub".into())
}

/// 開機起每 60 秒向站台 /api/register 報到 {hostname, addr}，並把回來的綁定寫進共享狀態。
fn register_loop(station: String, self_addr: String, host: String, state: BindingState) {
    if station.trim().is_empty() || self_addr.trim().is_empty() {
        state.lock().unwrap().last_error = "sub.json 未設定 station，或偵測不到 self_addr".into();
        return;
    }
    let url = format!("http://{station}/api/register");
    let payload = json!({ "hostname": host, "addr": self_addr }).to_string();
    loop {
        match ureq::post(&url)
            .timeout(Duration::from_secs(5))
            .set("Content-Type", "application/json")
            .send_string(&payload)
        {
            Ok(resp) => {
                let body = resp.into_string().unwrap_or_default();
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let b = v.get("binding").cloned().unwrap_or(Value::Null);
                let mut g = state.lock().unwrap();
                g.zentao_account = sv(&b, "zentao_account");
                g.feishu_name = sv(&b, "feishu_name");
                g.enabled = b.get("enabled").and_then(Value::as_bool).unwrap_or(false);
                g.registered = true;
                g.last_error.clear();
            }
            Err(e) => {
                let mut g = state.lock().unwrap();
                g.registered = false;
                g.last_error = e.to_string();
            }
        }
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn main() -> eframe::Result<()> {
    let port: u16 = std::env::var("ZYOFLOW_SUB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(39219);
    let mut cfg = load_config();
    if cfg.self_addr.trim().is_empty() {
        if let Some(ip) = local_ip() {
            cfg.self_addr = format!("{ip}:{port}");
        }
    }
    let host = hostname();
    let station = cfg.station.clone();
    let self_addr = cfg.self_addr.clone();
    println!(
        "[sub] host={} station={} self_addr={} repos={}",
        host,
        station,
        self_addr,
        cfg.repos.len()
    );
    let feed: Feed = Arc::new(Mutex::new(Vec::new()));
    let binding: BindingState = Arc::new(Mutex::new(Binding::default()));
    let state = AppState {
        feed: feed.clone(),
        pending: Arc::new(Mutex::new(HashMap::new())),
        cfg: Arc::new(cfg),
    };

    // 背景執行緒：向站台定期報到（送 IP，收綁定）
    {
        let (s, a, h, b) = (station.clone(), self_addr.clone(), host.clone(), binding.clone());
        std::thread::spawn(move || register_loop(s, a, h, b));
    }

    // 背景執行緒：HTTP 伺服器
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("建立 tokio runtime 失敗");
        rt.block_on(serve(port, state));
    });

    // 主執行緒：GUI
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([720.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            Ok(Box::new(SubApp::new(cc, feed, port, binding, station, self_addr, host)))
        }),
    )
}

// ----------------------------------------------------- HTTP 伺服器
async fn serve(port: u16, state: AppState) {
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .route("/push", post(push))
        .route("/dispatch", post(dispatch))
        .route("/answer", post(answer))
        .with_state(state);
    let addr = format!("0.0.0.0:{port}");
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            println!("ZyoFlow Sub 接收中 http://{addr}");
            let _ = axum::serve(listener, app).await;
        }
        Err(e) => eprintln!("無法綁定 {addr}: {e}"),
    }
}

/// 站台廣播的舊格式訊息（測試/通知），純顯示。
async fn push(State(st): State<AppState>, body: Bytes) -> Json<Value> {
    let raw = String::from_utf8_lossy(&body).into_owned();
    let (title, text, created_at) = extract(&raw);
    println!("[push] {title}");
    push_feed(&st.feed, Msg { title, body: text, created_at });
    Json(json!({ "ok": true }))
}

/// 站台派發任務 → 背景跑 Claude。
async fn dispatch(State(st): State<AppState>, body: Bytes) -> Json<Value> {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let task = Task {
        task_id: sv(&v, "task_id"),
        ttype: sv(&v, "type"),
        product: sv(&v, "product"),
        module: sv(&v, "module"),
        title: sv(&v, "title"),
        content: sv(&v, "content"),
        assignee: sv(&v, "assignee"),
    };
    println!(
        "[dispatch] 任務 #{} {} (product={} module={})",
        task.task_id, task.title, task.product, task.module
    );
    report(
        &st.feed,
        &task,
        format!(
            "已收到派發 · product={} module={}（這兩個值就是 sub.json repos 的 key）· 準備跑 Claude…",
            task.product, task.module
        ),
    );
    let (cfg, pending, feed) = (st.cfg.clone(), st.pending.clone(), st.feed.clone());
    std::thread::spawn(move || run_task(task, cfg, pending, feed));
    Json(json!({ "ok": true }))
}

/// 站台回送飛書答案 → 喚醒對應的 runner。
async fn answer(State(st): State<AppState>, body: Bytes) -> Json<Value> {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let cid = sv(&v, "correlation_id");
    let answers = v.get("answers").cloned().unwrap_or(Value::Null);
    println!("[answer] cid={cid}");
    let tx = st.pending.lock().unwrap().remove(&cid);
    match tx {
        Some(tx) => {
            let _ = tx.send(answers);
            Json(json!({ "ok": true }))
        }
        None => Json(json!({ "ok": false, "reason": "no pending task for cid" })),
    }
}

// ----------------------------------------------------- Claude 任務執行
struct Task {
    task_id: String,
    ttype: String,
    product: String,
    module: String,
    title: String,
    content: String,
    #[allow(dead_code)]
    assignee: String,
}

fn run_task(task: Task, cfg: Arc<SubConfig>, pending: Pending, feed: Feed) {
    let _ = std::fs::create_dir_all(&cfg.worktree_root);
    // 多個 repo（前端/後台/API…）時，用 AI 依任務內容 + 各 repo 關鍵字挑一個，不對照 Zentao product
    let repo = if cfg.repos.is_empty() {
        None
    } else {
        let idx = pick_repo_index(&cfg, &task).unwrap_or(1);
        let r = &cfg.repos[idx - 1];
        report(
            &feed,
            &task,
            format!("🧭 選定程式庫：{}", if r.name.is_empty() { &r.path } else { &r.name }),
        );
        Some(PathBuf::from(&r.path))
    };
    let workdir = setup_worktree(&cfg, &task, repo.as_deref());
    report(&feed, &task, format!("工作目錄：{}", workdir.display()));
    if cfg.self_addr.trim().is_empty() {
        report(&feed, &task, "⚠️ 不知道自己的位址(self_addr)，飛書回答將收不到");
    }

    let mut prompt = build_initial_prompt(&task);
    let mut session: Option<String> = None;
    let mut seq: u32 = 0;
    loop {
        match run_claude(&workdir, &prompt, &session, &cfg.claude_bin) {
            Err(e) => {
                report(&feed, &task, format!("❌ {e}"));
                break;
            }
            Ok((result, sid)) => {
                if !sid.is_empty() {
                    session = Some(sid);
                }
                match parse_ask_marker(&result) {
                    Some(questions) => {
                        seq += 1;
                        let cid = format!("t{}-{}", task.task_id, seq);
                        let (tx, rx) = mpsc::channel::<Value>();
                        pending.lock().unwrap().insert(cid.clone(), tx);
                        report(
                            &feed,
                            &task,
                            format!("🙋 Claude 提問 {} 題，已發飛書，等回答…", questions.len()),
                        );
                        if !post_ask(&cfg, &cid, &task, &questions) {
                            report(&feed, &task, "❌ 發問題卡到站台失敗");
                            pending.lock().unwrap().remove(&cid);
                            break;
                        }
                        // 阻塞等站台把答案送回（最多 1 小時，免執行緒永久卡住）
                        match rx.recv_timeout(Duration::from_secs(3600)) {
                            Ok(answers) => {
                                report(&feed, &task, format!("✅ 收到回答：{answers}"));
                                prompt = format_answers(&answers);
                            }
                            Err(_) => {
                                report(&feed, &task, "⏰ 一小時未回答，停止此任務");
                                pending.lock().unwrap().remove(&cid);
                                break;
                            }
                        }
                    }
                    None => {
                        report(&feed, &task, format!("🎉 完成：{}", truncate(&result, 800)));
                        break;
                    }
                }
            }
        }
    }
}

/// 跑一次 claude headless（讀 prompt 由 stdin），回 (result 文字, session_id)。
fn run_claude(
    workdir: &Path,
    prompt: &str,
    session: &Option<String>,
    bin: &str,
) -> Result<(String, String), String> {
    // ponytail: 先用 acceptEdits（可改檔、破壞性工具仍會被擋）；要跑測試/指令再依 repo 配 --allowedTools。
    let mut cmd = Command::new(bin);
    cmd.current_dir(workdir)
        .args(["-p", "--output-format", "json", "--permission-mode", "acceptEdits"]);
    if let Some(sid) = session {
        cmd.args(["-r", sid]);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("啟動 claude 失敗（claude 在 PATH 嗎）：{e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes()); // 寫完即關閉 → claude 收到完整 prompt
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("等 claude 失敗：{e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // --output-format json 應吐單一 JSON；保險再退而取最後一行非空 JSON
    let parsed = serde_json::from_str::<Value>(stdout.trim()).ok().or_else(|| {
        stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| serde_json::from_str::<Value>(l.trim()).ok())
    });
    let v = match parsed {
        Some(v) => v,
        None => {
            return Err(format!(
                "解析 claude 輸出失敗；stderr={}",
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    };
    let result = v.get("result").and_then(Value::as_str).unwrap_or_default().to_string();
    let sid = v.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string();
    if result.is_empty() && sid.is_empty() {
        return Err(format!(
            "claude 無有效輸出；stderr={}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok((result, sid))
}

/// 從 claude 輸出找 `@@ASK@@ {json}` 標記，回傳 questions 陣列。
fn parse_ask_marker(result: &str) -> Option<Vec<Value>> {
    for line in result.lines() {
        if let Some(rest) = line.trim().strip_prefix("@@ASK@@") {
            if let Ok(v) = serde_json::from_str::<Value>(rest.trim()) {
                if let Some(qs) = v.get("questions").and_then(Value::as_array) {
                    return Some(qs.clone());
                }
            }
        }
    }
    None
}

fn post_ask(cfg: &SubConfig, cid: &str, task: &Task, questions: &[Value]) -> bool {
    let url = format!("http://{}/lark/ask", cfg.station);
    let payload = json!({
        "reply_to": cfg.self_addr,
        "correlation_id": cid,
        "title": format!("任務 #{} 需要你決定", task.task_id),
        "questions": questions,
    })
    .to_string();
    ureq::post(&url)
        .timeout(Duration::from_secs(8))
        .set("Content-Type", "application/json")
        .send_string(&payload)
        .is_ok()
}

/// 把飛書回來的答案 {question: label} 組成續跑 prompt。
fn format_answers(answers: &Value) -> String {
    let mut s = String::from("使用者的決定：\n");
    if let Some(obj) = answers.as_object() {
        for (q, a) in obj {
            s.push_str(&format!("- {q} → {}\n", a.as_str().unwrap_or_default()));
        }
    }
    s.push_str("\n請依這些決定繼續執行任務；若還有需要我決定的，再用 @@ASK@@ 格式問。");
    s
}

fn build_initial_prompt(task: &Task) -> String {
    format!(
        "你是自動化開發代理，請完成下列任務。\n\n\
任務 #{id}（{ttype}）：{title}\n\
產品：{product}　模組：{module}\n\
內容：\n{content}\n\n\
規則：\n\
- 遇到需要人類拍板、有多個合理選項的設計決策時，不要自己假設。請輸出「恰好一行」：\n  \
@@ASK@@ {{\"questions\": [{{\"question\": \"...\", \"options\": [\"A\", \"B\"]}}]}}\n  \
（questions 可放 1~4 題）輸出後立刻結束你的回合，等我回答。\n\
- 沒有需要決策時就直接動手完成任務，完成後簡述你做了什麼。",
        id = task.task_id,
        ttype = task.ttype,
        title = task.title,
        product = task.product,
        module = task.module,
        content = task.content
    )
}

/// 設 worktree：給定 repo 就 `git worktree add`，失敗退回 repo 目錄；沒 repo 就用一個工作資料夾。
fn setup_worktree(cfg: &SubConfig, task: &Task, repo: Option<&Path>) -> PathBuf {
    if let Some(repo) = repo {
        if repo.is_dir() {
            let wt = PathBuf::from(&cfg.worktree_root).join(format!("task-{}", task.task_id));
            let branch = format!("zyoflow/task-{}", task.task_id);
            let try_add = |extra: &[&str]| {
                Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(["worktree", "add", "--force"])
                    .arg(&wt)
                    .args(extra)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            };
            if try_add(&["-b", &branch]) || try_add(&[]) {
                return wt;
            }
            return repo.to_path_buf(); // worktree 失敗 → 直接用 repo 目錄
        }
    }
    let dir = PathBuf::from(&cfg.worktree_root).join(format!("task-{}", task.task_id));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// AI 依各 repo 關鍵字 + 任務內容選一個程式庫，回 1-based 編號；AI 失敗退回關鍵字比對。
fn pick_repo_index(cfg: &SubConfig, task: &Task) -> Option<usize> {
    let n = cfg.repos.len();
    if n == 0 {
        return None;
    }
    if n == 1 {
        return Some(1);
    }
    let mut list = String::new();
    for (i, r) in cfg.repos.iter().enumerate() {
        let name = if r.name.is_empty() { &r.path } else { &r.name };
        list.push_str(&format!("{}. {} — {}\n", i + 1, name, r.keywords));
    }
    let prompt = format!(
        "任務標題：{title}\n產品：{product}　模組：{module}\n任務內容：\n{content}\n\n\
可選的程式庫：\n{list}\n\
請依任務內容判斷該在哪一個程式庫執行。最後輸出「恰好一行」：@@REPO@@ N（N 是上面的編號）。",
        title = task.title,
        product = task.product,
        module = task.module,
        content = truncate(&task.content, 2000),
        list = list
    );
    let workdir = PathBuf::from(&cfg.worktree_root);
    run_claude(&workdir, &prompt, &None, &cfg.claude_bin)
        .ok()
        .and_then(|(result, _)| parse_repo_pick(&result, n))
        .or_else(|| keyword_pick(&cfg.repos, task))
}

fn parse_repo_pick(result: &str, count: usize) -> Option<usize> {
    for line in result.lines() {
        if let Some(rest) = line.trim().strip_prefix("@@REPO@@") {
            if let Some(n) = first_number(rest) {
                if n >= 1 && n <= count {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn first_number(s: &str) -> Option<usize> {
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else if !num.is_empty() {
            break;
        }
    }
    num.parse().ok()
}

/// AI 失敗時的後備：哪個 repo 的關鍵字在任務文字裡命中最多就選它。
fn keyword_pick(repos: &[Repo], task: &Task) -> Option<usize> {
    let hay = format!("{} {} {} {}", task.title, task.content, task.product, task.module).to_lowercase();
    let mut best: Option<usize> = None;
    let mut best_score = 0usize;
    for (i, r) in repos.iter().enumerate() {
        let score = r
            .keywords
            .split([',', '，', ' ', '、'])
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .filter(|k| hay.contains(&k.to_lowercase()))
            .count();
        if score > best_score {
            best_score = score;
            best = Some(i + 1);
        }
    }
    best
}

// -------------------------------------------------------- 共用工具
fn sv(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

fn push_feed(feed: &Feed, m: Msg) {
    let mut f = feed.lock().unwrap();
    f.push(m);
    let len = f.len();
    if len > 500 {
        f.drain(0..len - 500);
    }
}

fn report(feed: &Feed, task: &Task, msg: impl Into<String>) {
    push_feed(
        feed,
        Msg {
            title: format!("任務 #{}", task.task_id),
            body: msg.into(),
            created_at: String::new(),
        },
    );
}

/// 從站台推來的 JSON 取 (title, body, created_at)，缺欄位回空字串。
fn extract(raw: &str) -> (String, String, String) {
    let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let f = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    (f("title"), f("body"), f("created_at"))
}

// ------------------------------------------------------------ GUI
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Feed,    // 接收訊息
    Binding, // 綁定狀態
}

struct SubApp {
    feed: Feed,
    port: u16,
    tab: Tab,
    binding: BindingState,
    station: String,
    self_addr: String,
    host: String,
    _tray: tray_icon::TrayIcon, // 保持存活：drop 掉圖示就消失
}

impl SubApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        feed: Feed,
        port: u16,
        binding: BindingState,
        station: String,
        self_addr: String,
        host: String,
    ) -> Self {
        install_cjk_font(&cc.egui_ctx);
        let tray = build_tray(cc.egui_ctx.clone());
        Self { feed, port, tab: Tab::Feed, binding, station, self_addr, host, _tray: tray }
    }

    /// 「接收訊息」分頁：站台派發 / 通知列表。
    fn feed_view(&self, ui: &mut egui::Ui) {
        let feed: Vec<Msg> = self.feed.lock().unwrap().clone(); // 快照
        if feed.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| ui.weak("等待站台派發…"));
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for m in feed.iter().rev() {
                    // 新→舊
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.strong(&m.title);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| ui.weak(&m.created_at),
                            );
                        });
                        if !m.body.is_empty() {
                            ui.label(&m.body);
                        }
                    });
                    ui.add_space(6.0);
                }
            });
    }

    /// 「綁定狀態」分頁：本機在站台的綁定（每分鐘由報到更新）。
    fn binding_view(&self, ui: &mut egui::Ui) {
        let b = self.binding.lock().unwrap().clone();
        let green = egui::Color32::from_rgb(46, 160, 67);
        ui.add_space(10.0);
        egui::Grid::new("binding")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.strong("電腦名稱");
                ui.label(&self.host);
                ui.end_row();
                ui.strong("本機位址");
                ui.label(if self.self_addr.is_empty() { "（偵測失敗）" } else { &self.self_addr });
                ui.end_row();
                ui.strong("站台 Stage");
                ui.label(if self.station.is_empty() { "（未設定）" } else { &self.station });
                ui.end_row();
                ui.strong("報到狀態");
                if b.registered {
                    ui.colored_label(green, "● 已向站台報到");
                } else {
                    ui.colored_label(egui::Color32::RED, format!("● 未報到　{}", b.last_error))
                        .on_hover_text("開機起每 60 秒自動重試");
                }
                ui.end_row();
                ui.strong("Zentao 負責人");
                ui.label(if b.zentao_account.is_empty() {
                    "（未綁定）".to_string()
                } else {
                    b.zentao_account.clone()
                });
                ui.end_row();
                ui.strong("飛書用戶");
                ui.label(if b.feishu_name.is_empty() {
                    "（未綁定）".to_string()
                } else {
                    b.feishu_name.clone()
                });
                ui.end_row();
                ui.strong("啟用");
                ui.label(if b.enabled { "是" } else { "否" });
                ui.end_row();
            });
        ui.add_space(10.0);
        ui.weak("綁定在站台控管網頁設定；本頁每分鐘自動更新。");
    }
}

impl eframe::App for SubApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 按視窗右上角 X → 不結束，縮到系統匣
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            hide_window(ctx);
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("ZyoFlow Sub");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(46, 160, 67),
                        format!("接收中 · :{}", self.port),
                    );
                });
            });
            // 頁籤（保留後續擴充）
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Feed, "接收訊息");
                ui.selectable_value(&mut self.tab, Tab::Binding, "綁定狀態");
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Feed => self.feed_view(ui),
            Tab::Binding => self.binding_view(ui),
        });

        // 隱藏是 OS 層級做的，eframe 認為視窗一直可見，這個 tick 會一直跑、feed 保持更新
        ctx.request_repaint_after(Duration::from_secs(1));
    }
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

    let ctx_menu = ctx.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == show_id {
            show_window(&ctx_menu);
        } else if e.id == quit_id {
            std::process::exit(0);
        }
    }));

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
        rgba.extend_from_slice(&[0x2e, 0xa0, 0x43, 0xff]); // 實心綠方塊（與 Master 藍色區分）
    }
    tray_icon::Icon::from_rgba(rgba, w, h).expect("產生圖示失敗")
}

/// egui 預設字型沒有中日韓字，載入系統 CJK 字型，否則中文會變方框。
fn install_cjk_font(ctx: &egui::Context) {
    let candidates = [
        "C:/Windows/Fonts/msjh.ttc",
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simsun.ttc",
        "/System/Library/Fonts/PingFang.ttc",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_present_and_missing() {
        let (t, b, c) = extract(
            r#"{"title":"任務 #12 · 指派","body":"hi","created_at":"2026-06-25 10:00:00"}"#,
        );
        assert_eq!(t, "任務 #12 · 指派");
        assert_eq!(b, "hi");
        assert_eq!(c, "2026-06-25 10:00:00");

        let (t2, b2, c2) = extract("not json");
        assert_eq!((t2.as_str(), b2.as_str(), c2.as_str()), ("", "", ""));
    }

    #[test]
    fn parse_marker_single_and_multi() {
        // 單題
        let r = r#"前言
@@ASK@@ {"questions": [{"question": "用哪個?", "options": ["A", "B"]}]}"#;
        let qs = parse_ask_marker(r).expect("應解析出 questions");
        assert_eq!(qs.len(), 1);
        // 沒標記 → None
        assert!(parse_ask_marker("純文字完成了").is_none());
    }
}
