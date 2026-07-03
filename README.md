# ZyoFlow Agent

> 把禪道（Zentao）任務自動派給開發者本機的 **Claude Code** 開發；過程中 AI 需要決策時，透過**飛書（Lark/Feishu）互動卡片**遠端回答，讓夜間無人值守也能持續推進。

ZyoFlow Agent 是一條協作的自動化開發流水線：

**禪道指派任務 → Stage 站台依綁定路由 → Sub 在開發者本機跑 Claude Code → 遇到需要人拍板的決策，發飛書卡片遠端回答 → Claude 依答案續跑。**

---

## 架構

```
        (內網 / 個人電腦)
 Zentao ──webhook──▶ Stage 站台 ─────派發─────▶ Sub ──▶ Claude Code
(任務系統)          Flask·zyoflow.exe            Rust·egui   (headless)
                      ▲   │  ▲                      │  ▲
                      │   │  └──── Sub 開機報到 IP ──┘  │
       發/更新問題卡片 │   │                            │
        + 群訊息指令   │   └──── 派發任務 /dispatch ─────┘
     飛書群組 ◀────────┘        答案 /answer ◀───────────┘
          │  長連接收回調 / 群訊息
          └──────────────▶ Stage
```

| 元件 | 技術 | 角色 |
|------|------|------|
| **Stage 站台**<br>`app.py` → `zyoflow.exe` | Python / Flask | 唯一中樞：收禪道 webhook、回呼禪道 API 撈任務細節、**依綁定直接派給對應 Sub**；飛書長連接（收卡片回調、群訊息指令、發/更新問題卡、把答案回送 Sub）；控管網頁綁定 Sub↔Zentao↔飛書 |
| **Sub**<br>`sub/` → `zyoflow-sub.exe` | Rust / egui | 開發者本機：開機自動報到 IP 給 Stage、AI 選 repo、建 git worktree、跑 Claude Code、遇決策發飛書問題、收答案續跑 |

> 舊版的 Rust **Master** 中間層已移除 —— 路由改由 Stage 直接掌管，少一個服務、少一跳、少一個資料庫。

---

## 運作流程

1. **取得任務** — 在禪道指派任務，webhook 打到 Stage。Stage 用任務 ID 回呼禪道 API 撈出 `assignee / product / module / 內容`，組成 enriched task。
2. **路由** — Stage 依 `assignee` 查「Zentao 負責人 → Sub」綁定，直接派發給對應的 Sub。
3. **跑 Claude** — Sub 收到後：用 AI 依各 repo 的關鍵字 + 任務內容挑出該動的程式庫 → 建獨立 git worktree → 在裡面以 headless 模式跑 Claude Code。
4. **互動決策** — Claude 遇到需要人拍板的設計選擇時，輸出一行 `@@ASK@@ {questions}` 並停下。Sub 解析後請 Stage 在飛書群組發一張問題卡片。
5. **遠端回答** — 你在飛書點選項；Stage 透過長連接收到回調、就地更新卡片、把答案送回 Sub。
6. **續跑** — Sub 以 `claude --resume` 把答案餵回**同一個 session**，Claude 接著做，直到完成。

> 互動決策不依賴 MCP / Agent SDK，純靠 Claude CLI 的 `--output-format json` + `--resume`，所以 Sub 維持單一執行檔。問題卡片支援一次多題（對齊 Claude 內建 `AskUserQuestion` 結構）。

### 飛書群指令

在群組 **@機器人** 後輸入指令（指令框架可擴充，見 `COMMANDS`）：

| 指令 | 作用 |
|------|------|
| `/help` | 列出可用指令 |
| `/bindCheck <IP 或 Zentao 帳號 或 飛書名稱>` | 查詢 Sub 綁定狀態（留空列全部） |

---

## 需求

- **Stage 站台**：Windows（打包成 `zyoflow.exe` 後免裝 Python）；能連得出去飛書、且能連到各 Sub 的區網 IP 即可，**不需對外公開、不需公網網域**。
- **Sub**（開發者機）：Windows / macOS；已安裝並登入 [Claude Code](https://claude.com/claude-code) CLI；`git`。
- **外部服務**：一個可用 API 的禪道實例；一個飛書自建應用（開啟機器人、卡片回調與訊息事件走長連接）。

---

## 快速開始

### 1. Stage 站台
把 `zentao.example.json`、`feishu.example.json` 複製成 `zentao.json`、`feishu.json` 放在 exe 旁填好，然後：

```bash
pip install -r requirements.txt
python app.py        # 開發；http://0.0.0.0:39217
build.bat            # 打包 → dist/zyoflow.exe（含 lark-oapi 長連接）
```

**飛書開放平台**：開啟機器人 →

- 權限：`im:message`（收發訊息）、`im:message:send_as_bot`（發訊息）、`im:chat`（讀群成員做綁定下拉）
- 事件訂閱：加 `im.message.receive_v1`（接收群訊息指令）；「事件與回調 → 回調」選**使用長連接**
- 發布版本

> 群組預設只有 **@機器人** 的訊息會進來（要收全部群訊息需另外申請高權限）。所以指令用 `@機器人 /bindCheck …`。

**禪道後台**：新增一個 Web webhook 指向 `http://<Stage>:39217/webhook`。

### 2. Sub（每位開發者機）
把 `sub/sub.example.json` 複製成 `sub.json` 放在 exe 旁：

```json
{
  "station": "192.168.x.x:39217",
  "self_addr": "",
  "claude_bin": "claude",
  "worktree_root": "D:/zyoflow/worktrees",
  "repos": [
    { "name": "前端", "path": "D:/work/frontend", "must": "官網專案", "keywords": "前端, React, 畫面, UI" },
    { "name": "API",  "path": "D:/work/api",      "must": "",         "keywords": "API, 後端, REST, 資料庫" }
  ]
}
```

```bash
cd sub && cargo build --release        # 或 build.bat
```

開 `zyoflow-sub.exe` 後會**自動把電腦名稱 + IP 報到給 Stage**，然後出現在 Stage 控管網頁。

> `self_addr` 留空會自動偵測本機 LAN IP。「綁定狀態」分頁可看本機在 Stage 的綁定。挑 repo 分兩層：先用 **`must`（必填關鍵字，硬條件，例如專案名稱）** 篩候選——`must` 命中任務全文才入選，留空代表不限；再由 **AI 依 `keywords`（選填，軟提示）+ 任務內容** 從候選挑一個。所以**不必對照禪道的 product 設定**；若沒有任何 repo 的 `must` 符合，該任務會被略過不派（避免動到錯的專案）。

### 3. 綁定（在 Stage 控管網頁）
開 `http://<Stage>:39217/`，在「Sub 綁定」表把每台報到的 Sub 選好對應的 **Zentao 負責人**（下拉，來自禪道名單）與 **飛書用戶**（下拉，來自群成員），按儲存。之後：

- 禪道任務依 `assignee` 派到對應 Sub
- 飛書指令依發訊人 open_id 對到對應 Sub

---

## 設定檔一覽

| 檔案 / 位置 | 內容 |
|------|------|
| `zentao.json`（Stage exe 旁） | 禪道 `url / account / password` |
| `feishu.json`（Stage exe 旁） | 飛書 `app_id / app_secret / chat_id`（目標群組） |
| `sub.json`（Sub exe 旁） | Stage 位址、worktree 根目錄、各 repo 路徑＋`must`（必填/硬條件）＋`keywords`（選填/軟提示） |
| Stage 控管網頁 | Sub 綁定表（電腦 → Zentao 負責人 + 飛書用戶）；資料存在 Stage 的 `zyoflow.sqlite` |

> 含帳密 / 本機路徑的檔案**已列入 `.gitignore`，請勿 commit**。範例見各 `*.example.json`。

預設連接埠：Stage `39217`、Sub `39219`。

---

## 安全性 / 設計取捨

- 機密（禪道密碼、飛書 App Secret）只存 Stage 本機的設定檔，不進版控。
- 飛書走**長連接**：Stage 主動撥出去連飛書，不需對外可被連入。
- Sub 綁定以**電腦名稱**當鍵，IP 由每次報到更新，DHCP 換 IP 也不掉綁定。
- Sub 跑 Claude 預設 `--permission-mode acceptEdits`（可改檔，破壞性操作仍受限）；產出**留在 worktree，不自動 push、不回寫禪道**，由人工 review。
- 互動決策的可靠度靠 system prompt 強制 `@@ASK@@` 標記格式；萬一某次沒吐合法標記，會退而求其次。

---

## 從原始碼編譯

| 元件 | 指令 | 產物 |
|------|------|------|
| Stage 站台 | `build.bat` | `dist/zyoflow.exe` |
| Sub | `cd sub && cargo build --release` | `sub/target/release/zyoflow-sub.exe` |

需求：Python 3.10+（Stage）、Rust stable（Sub）、PyInstaller（Stage 打包，build.bat 會自動裝）。

---

## 授權

本專案採用 **MIT** 授權，見 [LICENSE](LICENSE)。
