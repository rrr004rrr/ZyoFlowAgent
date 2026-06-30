# ZyoFlow Agent

> 把禪道（Zentao）任務自動派給開發者本機的 **Claude Code** 開發；過程中 AI 需要決策時，透過**飛書（Lark/Feishu）互動卡片**遠端回答，讓夜間無人值守也能持續推進。

ZyoFlow Agent 是一條三端協作的自動化開發流水線：

**禪道指派任務 → 站台路由 → Master 派發 → Sub 在開發者本機跑 Claude Code → 遇到需要人拍板的決策，發飛書卡片遠端回答 → Claude 依答案續跑。**

---

## 架構

```
        (公網)                          (內網 / 個人電腦)
 Zentao ──webhook──▶ 站台 Gateway ──轉發──▶ Master ──派發──▶ Sub ──▶ Claude Code
(任務系統)          Flask·zyoflow.exe      Rust·egui       Rust·egui    (headless)
                       ▲   │                                   │
                       │   └─── 發/更新問題卡片 ───▶ 飛書群組    │
                       └──────── 長連接收回調 ◀──── (你點選項) ◀─┘
```

| 元件 | 技術 | 角色 |
|------|------|------|
| **站台 Gateway**<br>`app.py` → `zyoflow.exe` | Python / Flask | 唯一對外接點：收禪道 webhook、回呼禪道 API 撈任務細節、轉發給 Master；同時是飛書中樞（長連接收卡片回調、發/更新問題卡、把答案回送 Sub） |
| **Master**<br>`master/` → `zyoflow-master.exe` | Rust / egui | 依任務負責人（assignee）把任務路由派發給對應的 Sub |
| **Sub**<br>`sub/` → `zyoflow-sub.exe` | Rust / egui | 開發者本機：AI 選 repo、建 git worktree、跑 Claude Code、遇決策發飛書問題、收答案續跑 |

---

## 運作流程

1. **取得任務** — 在禪道指派任務，webhook 打到站台。站台用任務 ID 回呼禪道 API 撈出 `assignee / product / module / 內容`，組成 enriched task 轉發給 Master。
2. **路由** — Master 依 `assignee` 比對「負責人 → Sub」設定，派發給對應的 Sub。
3. **跑 Claude** — Sub 收到後：用 AI 依各 repo 的關鍵字 + 任務內容挑出該動的程式庫 → 建獨立 git worktree → 在裡面以 headless 模式跑 Claude Code。
4. **互動決策** — Claude 遇到需要人拍板的設計選擇時，輸出一行 `@@ASK@@ {questions}` 並停下。Sub 解析後請站台在飛書群組發一張問題卡片。
5. **遠端回答** — 你在飛書點選項；站台透過長連接收到回調、就地更新卡片、把答案送回 Sub。
6. **續跑** — Sub 以 `claude --resume` 把答案餵回**同一個 session**，Claude 接著做，直到完成。

> 互動決策不依賴 MCP / Agent SDK，純靠 Claude CLI 的 `--output-format json` + `--resume`，所以 Sub 維持單一執行檔。問題卡片支援一次多題（對齊 Claude 內建 `AskUserQuestion` 結構）。

---

## 需求

- **站台**：Windows（打包成 `zyoflow.exe` 後免裝 Python）；能連得出去飛書即可，**不需對外公開、不需公網網域**。
- **Master**：Windows / macOS。
- **Sub**（開發者機）：Windows / macOS；已安裝並登入 [Claude Code](https://claude.com/claude-code) CLI；`git`。
- **外部服務**：一個可用 API 的禪道實例；一個飛書自建應用（開啟機器人、卡片回調走長連接）。

---

## 快速開始

### 1. 站台（Gateway）
把 `zentao.example.json`、`feishu.example.json` 複製成 `zentao.json`、`feishu.json` 放在 exe 旁填好，然後：

```bash
pip install -r requirements.txt
python app.py        # 開發；http://0.0.0.0:39217
build.bat            # 打包 → dist/zyoflow.exe（含 lark-oapi 長連接）
```

飛書開放平台：開啟機器人、加權限 `im:message` / `im:message:send_as_bot` / `im:chat:readonly`、「事件與回調 → 回調」選**使用長連接**、發布版本。
禪道後台：新增一個 Web webhook 指向 `http://<站台>:39217/webhook`。

### 2. Master

```bash
cd master && cargo build --release     # 或 build.bat
```

開 `zyoflow-master.exe` →「Sub 路由」分頁加每個 Sub（名稱 / IP / 負責人禪道帳號）。
到站台的配置網頁（`http://<站台>:39217/`）把這台 Master 的位址加進去。

### 3. Sub（每位開發者機）
把 `sub/sub.example.json` 複製成 `sub.json` 放在 exe 旁：

```json
{
  "station": "192.168.x.x:39217",
  "self_addr": "",
  "claude_bin": "claude",
  "worktree_root": "D:/zyoflow/worktrees",
  "repos": [
    { "name": "前端", "path": "D:/work/frontend", "keywords": "前端, React, 畫面, UI" },
    { "name": "API",  "path": "D:/work/api",      "keywords": "API, 後端, REST, 資料庫" }
  ]
}
```

```bash
cd sub && cargo build --release        # 或 build.bat
```

> `self_addr` 留空會自動偵測本機 LAN IP。`repos` 用 AI 依 `keywords` + 任務內容挑一個，所以**不必對照禪道的 product 設定**，一個 product 對前端/後台/API 多 repo 也分得出來。

---

## 設定檔一覽

| 檔案 | 位置 | 內容 |
|------|------|------|
| `zentao.json` | 站台 exe 旁 | 禪道 `url / account / password` |
| `feishu.json` | 站台 exe 旁 | 飛書 `app_id / app_secret / chat_id`（目標群組） |
| `sub.json` | Sub exe 旁 | 站台位址、worktree 根目錄、各 repo 路徑＋關鍵字 |
| Master GUI | — | Sub 路由表（負責人 → Sub IP） |

> 這些檔含帳密 / 本機路徑，**已列入 `.gitignore`，請勿 commit**。範例見各 `*.example.json`。

預設連接埠：站台 `39217`、Master `39218`、Sub `39219`。

---

## 安全性 / 設計取捨

- 機密（禪道密碼、飛書 App Secret）只存各機器本機的設定檔，不進版控。
- 飛書回調走**長連接**：站台主動撥出去連飛書，不需對外可被連入。
- Sub 跑 Claude 預設 `--permission-mode acceptEdits`（可改檔，破壞性操作仍受限）；產出**留在 worktree，不自動 push、不回寫禪道**，由人工 review。
- 互動決策的可靠度靠 system prompt 強制 `@@ASK@@` 標記格式；萬一某次沒吐合法標記，會退而求其次。

---

## 從原始碼編譯

| 元件 | 指令 | 產物 |
|------|------|------|
| 站台 | `build.bat` | `dist/zyoflow.exe` |
| Master | `cd master && cargo build --release` | `master/target/release/zyoflow-master.exe` |
| Sub | `cd sub && cargo build --release` | `sub/target/release/zyoflow-sub.exe` |

需求：Python 3.10+（站台）、Rust stable（Master/Sub）、PyInstaller（站台打包，build.bat 會自動裝）。

---

## 授權

本專案採用 **MIT** 授權，見 [LICENSE](LICENSE)。
