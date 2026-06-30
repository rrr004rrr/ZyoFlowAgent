"""ZyoFlow 站台閘道 — 固定接 Zentao webhook，轉發給各 Master。

  Zentao --(固定 webhook，設定一次)--> 本服務（站台 39217）--(轉發)--> Master（個人電腦 39218）

單檔 Flask app：
  POST /webhook        接 Zentao webhook：存檔 + 轉發給所有啟用的 Master
  GET  /               配置網站（管理要轉發的 Master、看最近通知、送測試）
  API  /api/masters    Master 端點增刪改查；/api/messages 最近通知；/api/test-send 測試送

跑在站台主機，預設 port 39217（ZYOFLOW_PORT 可覆寫）。
"""
import json
import os
import re
import sqlite3
import sys
import threading
import time
import urllib.request
from contextlib import contextmanager
from pathlib import Path

from flask import Flask, jsonify, request, send_from_directory

if getattr(sys, "frozen", False):           # PyInstaller 打包後
    RES_DIR = Path(sys._MEIPASS)             # 唯讀資源（web/ 解壓於此）
    DATA_DIR = Path(sys.executable).parent   # 可寫資料放 exe 旁邊
else:
    RES_DIR = DATA_DIR = Path(__file__).parent

DB_PATH = Path(os.environ.get("ZYOFLOW_DB", DATA_DIR / "zyoflow.sqlite"))
PORT = int(os.environ.get("ZYOFLOW_PORT", "39217"))

app = Flask(__name__, static_folder=None)


# ---------------------------------------------------------------- DB
@contextmanager
def db():
    c = sqlite3.connect(DB_PATH)
    c.row_factory = sqlite3.Row
    try:
        yield c
        c.commit()
    finally:
        c.close()


def init_db():
    with db() as c:
        c.executescript(
            """
            CREATE TABLE IF NOT EXISTS masters(
              id         INTEGER PRIMARY KEY,
              name       TEXT NOT NULL,
              addr       TEXT NOT NULL,           -- host:port，例如 192.168.x.x:39218
              enabled    INTEGER NOT NULL DEFAULT 1,
              created_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
            );
            CREATE TABLE IF NOT EXISTS messages(
              id         INTEGER PRIMARY KEY,
              title      TEXT,
              body       TEXT,
              raw        TEXT,
              created_at TEXT NOT NULL DEFAULT (datetime('now','localtime'))
            );
            """
        )


# ----------------------------------------------------- 轉發給 Master
def forward_to_masters(raw_bytes: bytes, addrs: list):
    """把原始 payload 原封不動 POST 給每個 Master 的 /webhook；背景執行，失敗只記 log。"""
    for addr in addrs:
        url = f"http://{addr}/webhook"
        try:
            req = urllib.request.Request(
                url, data=raw_bytes, headers={"Content-Type": "application/json"}, method="POST"
            )
            urllib.request.urlopen(req, timeout=5).close()
        except Exception as e:  # noqa: BLE001 - 轉發失敗不可中斷主流程
            print(f"[forward] {url} 失敗: {e}", flush=True)


def norm_addr(s: str) -> str:
    """使用者可能填 http://x:39218 或 x:39218/webhook，一律 normalize 成 host:port。"""
    s = s.strip().removeprefix("http://").removeprefix("https://").rstrip("/")
    if s.endswith("/webhook"):
        s = s[: -len("/webhook")].rstrip("/")
    return s


# ----------------------------------------------------- Zentao API
ZENTAO_CFG = DATA_DIR / "zentao.json"


def zentao_cfg():
    """讀 exe 旁的 zentao.json：{"url","account","password"}。utf-8-sig 容忍記事本 BOM。"""
    try:
        return json.loads(ZENTAO_CFG.read_text(encoding="utf-8-sig"))
    except Exception:
        return {}


def _zentao_token(url, account, password):
    body = json.dumps({"account": account, "password": password}).encode("utf-8")
    req = urllib.request.Request(
        url.rstrip("/") + "/api.php/v1/tokens",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read().decode("utf-8"))["token"]


def zentao_users():
    """回傳 [{account, realname}]（含中文姓名）；未設定或失敗丟例外。"""
    cfg = zentao_cfg()
    url, account, password = cfg.get("url") or "", cfg.get("account") or "", cfg.get("password") or ""
    if not (url and account and password):
        raise RuntimeError(f"找不到或未填妥：{ZENTAO_CFG}（需有 url / account / password）")
    token = _zentao_token(url, account, password)
    req = urllib.request.Request(
        url.rstrip("/") + "/api.php/v1/users?limit=1000",
        headers={"Token": token},
        method="GET",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        data = json.loads(r.read().decode("utf-8"))
    users = data.get("users", data) if isinstance(data, dict) else data
    out = []
    for u in users:
        acc = (u.get("account") or "").strip()
        if acc:
            out.append({"account": acc, "realname": (u.get("realname") or "").strip() or acc})
    return out


def zentao_object_detail(otype, oid):
    """撈單一物件細節（task/bug/story），回正規化 dict；失敗回 {}。
    ponytail: 欄位名各 Zentao 版本可能不同，防禦性抽取；撈不到就讓上層用 webhook 既有欄位 fallback。"""
    cfg = zentao_cfg()
    url = cfg.get("url") or ""
    if not (url and oid):
        return {}
    ep = {"task": "tasks", "bug": "bugs", "story": "stories"}.get(str(otype), "tasks")
    try:
        token = _zentao_token(url, cfg.get("account", ""), cfg.get("password", ""))
        req = urllib.request.Request(
            url.rstrip("/") + f"/api.php/v1/{ep}/{oid}", headers={"Token": token}, method="GET"
        )
        with urllib.request.urlopen(req, timeout=10) as r:
            o = json.loads(r.read().decode("utf-8"))
    except Exception as e:  # noqa: BLE001
        print(f"[zentao] 撈 {ep}/{oid} 失敗: {e}", flush=True)
        return {}
    g = lambda *ks: next((str(o[k]) for k in ks if o.get(k) not in (None, "", "0", 0)), "")
    # assignedTo 可能是 'alice'，也可能是物件 {'account':'alice',...} → 一律取 account
    av = o.get("assignedTo")
    if av in (None, "", 0):
        av = o.get("assigned_to")
    if isinstance(av, dict):
        assignee = str(av.get("account") or "")
    else:
        assignee = str(av) if av not in (None, "", 0) else ""
    return {
        "assignee": assignee,
        "product": g("product", "productName", "project"),
        "module": g("module", "moduleName"),
        "title": g("name", "title", "subject"),
        "content": _strip_html(g("desc", "steps", "spec", "content")),
    }


def _strip_html(s):
    """去掉 HTML 標籤，禪道的 desc 常是 <p><span>…，清成純文字給 claude。"""
    return re.sub(r"<[^>]+>", "", s or "").strip()


# --------------------------------------------------------- Zentao 解析
def parse_zentao(data, raw: str):
    """Zentao webhook 格式依類型而異，這裡寬鬆抽取、原文一律保留。
    看到真實 payload 後再依欄位精修。"""
    if isinstance(data, dict):
        title = data.get("title") or data.get("subject") or data.get("action") or "Zentao 通知"
        body = data.get("text") or data.get("content") or data.get("comment") or raw[:500]
        return str(title), str(body)
    return "Zentao 通知", (raw or "")[:500]


def _deliver(title, body, raw, raw_bytes):
    """存檔 → 背景轉發給啟用的 Master。回傳轉發的 Master 數。"""
    with db() as c:
        c.execute("INSERT INTO messages(title, body, raw) VALUES(?,?,?)", (title, body, raw))
        addrs = [r["addr"] for r in c.execute("SELECT addr FROM masters WHERE enabled=1")]
    if addrs:
        threading.Thread(target=forward_to_masters, args=(raw_bytes, addrs), daemon=True).start()
    return len(addrs)


# ----------------------------------------------------- 飛書長連接（卡片回調）
# 站台「撥出去」連 Feishu 收按鈕點擊（card.action.trigger）；不需對外公開，只需連得出去。
FEISHU_CFG = DATA_DIR / "feishu.json"


def feishu_cfg():
    """讀 exe 旁的 feishu.json：{"app_id","app_secret","chat_id"}。utf-8-sig 容忍 BOM。"""
    try:
        return json.loads(FEISHU_CFG.read_text(encoding="utf-8-sig"))
    except Exception:
        return {}


def _feishu_token(app_id, app_secret):
    body = json.dumps({"app_id": app_id, "app_secret": app_secret}).encode()
    req = urllib.request.Request(
        "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal",
        data=body, headers={"Content-Type": "application/json"}, method="POST",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())["tenant_access_token"]


def feishu_send_card(app_id, app_secret, chat_id, card):
    """發互動卡片到群組，回傳 message_id。"""
    body = json.dumps(
        {"receive_id": chat_id, "msg_type": "interactive",
         "content": json.dumps(card, ensure_ascii=False)},
        ensure_ascii=False,
    ).encode()
    req = urllib.request.Request(
        "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id",
        data=body,
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {_feishu_token(app_id, app_secret)}"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())["data"]["message_id"]


def feishu_update_card(app_id, app_secret, message_id, card):
    """就地更新卡片（需 update_multi=true）。"""
    body = json.dumps({"content": json.dumps(card, ensure_ascii=False)}, ensure_ascii=False).encode()
    req = urllib.request.Request(
        f"https://open.feishu.cn/open-apis/im/v1/messages/{message_id}",
        data=body,
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {_feishu_token(app_id, app_secret)}"},
        method="PATCH",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read()


def build_question_card(title, questions, cid, answers, done=False):
    """已答的問題只顯示文字(無按鈕)、未答的才有按鈕；done=True → 綠底完成、整張無按鈕。"""
    elems = []
    for qi, q in enumerate(questions):
        picked = answers.get(str(qi))
        qtext = q.get("question", "?")
        if picked is not None:
            elems.append({"tag": "div", "text": {"tag": "lark_md",
                          "content": f"**Q{qi + 1}. {qtext}**\n　✅ 已選：{picked}"}})
        else:
            elems.append({"tag": "div", "text": {"tag": "lark_md", "content": f"**Q{qi + 1}. {qtext}**"}})
            btns = [
                {"tag": "button", "text": {"tag": "plain_text", "content": opt}, "type": "primary",
                 "value": {"zyo": "ask", "cid": cid, "qi": str(qi), "label": opt}}
                for opt in q.get("options", [])
            ]
            if btns:
                elems.append({"tag": "action", "actions": btns})
    if done:
        elems.append({"tag": "div", "text": {"tag": "lark_md",
                      "content": "**✅ 已收到全部回答，任務繼續中…**"}})
    return {
        "config": {"wide_screen_mode": True, "update_multi": True},
        "header": {"template": "green" if done else "orange",
                   "title": {"tag": "plain_text", "content": title}},
        "elements": elems,
    }


# 進行中的提問：cid -> {reply_to, message_id, questions, answers:{qi:label}, title}
ASK_STORE = {}
ASK_LOCK = threading.Lock()


def post_answer_to_sub(reply_to, cid, answers):
    """全部問題答完後，把答案 POST 回 Sub 的 /answer。"""
    url = f"http://{reply_to}/answer"
    body = json.dumps({"correlation_id": cid, "answers": answers}, ensure_ascii=False).encode("utf-8")
    try:
        req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"}, method="POST")
        urllib.request.urlopen(req, timeout=5).close()
        print(f"[feishu] 答案已回送 Sub {url}", flush=True)
    except Exception as e:  # noqa: BLE001
        print(f"[feishu] 回送 Sub 失敗 {url}: {e}", flush=True)


def handle_ask_click(value):
    """記錄答案 → 回傳要更新的卡片(由回調回應帶回，最即時)；全答完則回送 Sub。
    回 None 表示找不到該提問(站台重啟過)。"""
    cid, qi, label = value.get("cid"), value.get("qi"), value.get("label")
    with ASK_LOCK:
        st = ASK_STORE.get(cid)
        if not st:
            print(f"[feishu] 找不到 ask cid={cid}（站台可能重啟過）", flush=True)
            return None
        st["answers"][qi] = label
        snap = {**st, "answers": dict(st["answers"])}
    done = len(snap["answers"]) >= len(snap["questions"])
    if done:
        answers_by_q = {snap["questions"][int(i)]["question"]: lab for i, lab in snap["answers"].items()}
        post_answer_to_sub(snap["reply_to"], cid, answers_by_q)
        with ASK_LOCK:
            ASK_STORE.pop(cid, None)
    return build_question_card(snap["title"], snap["questions"], cid, snap["answers"], done=done)


def start_feishu_listener():
    """長連接接卡片回調：屬本系統提問(zyo=ask)就路由答案，其餘轉發給 Master。跑在背景執行緒、阻塞。"""
    cfg = feishu_cfg()
    app_id, app_secret = cfg.get("app_id"), cfg.get("app_secret")
    if not (app_id and app_secret):
        print(f"[feishu] 未設定 {FEISHU_CFG}（需 app_id/app_secret），不啟動長連接", flush=True)
        return
    try:
        import lark_oapi as lark
        from lark_oapi.event.callback.model.p2_card_action_trigger import (
            P2CardActionTrigger, P2CardActionTriggerResponse,
        )
    except ImportError:
        print("[feishu] 缺 lark-oapi，不啟動長連接", flush=True)
        return

    def on_card_action(data: P2CardActionTrigger) -> P2CardActionTriggerResponse:
        value = data.event.action.value or {}
        open_id = data.event.operator.open_id
        print(f"[feishu] 回調 open_id={open_id} value={value}", flush=True)
        resp = {"toast": {"type": "info", "content": "已收到 ✅",
                          "i18n": {"zh_cn": "已收到 ✅", "en_us": "Got it"}}}
        try:
            if value.get("zyo") == "ask":
                card = handle_ask_click(value)
                if card is not None:
                    resp["card"] = {"type": "raw", "data": card}  # 回應直接帶新卡片 → 即時換掉、不卡
            else:  # 非本系統卡片（例如手動測試），轉發給 Master 顯示
                raw = json.dumps({"title": "飛書回調", "text": json.dumps(value, ensure_ascii=False),
                                  "feishu_callback": value, "open_id": open_id}, ensure_ascii=False)
                _deliver("飛書回調", raw, raw, raw.encode("utf-8"))
        except Exception as e:  # noqa: BLE001
            print(f"[feishu] 處理回調失敗: {e}", flush=True)
        return P2CardActionTriggerResponse(resp)

    handler = lark.EventDispatcherHandler.builder("", "").register_p2_card_action_trigger(on_card_action).build()
    # 斷線自動重連：長連接掉了就等 30 秒重來，別讓站台靜默停止收回調
    while True:
        try:
            print("[feishu] 長連接啟動中…（card.action.trigger）", flush=True)
            lark.ws.Client(app_id, app_secret, event_handler=handler, log_level=lark.LogLevel.INFO).start()
        except Exception as e:  # noqa: BLE001
            print(f"[feishu] 長連接中斷：{e}；30 秒後重連", flush=True)
        time.sleep(30)


# -------------------------------------------------------------- routes
@app.get("/health")
def health():
    return jsonify(ok=True)


@app.post("/webhook")
def webhook():
    raw_bytes = request.get_data()
    raw = raw_bytes.decode("utf-8", "replace")
    try:
        data = json.loads(raw)
    except Exception:
        data = {}
    otype = str(data.get("objectType") or "")
    oid = data.get("objectID") or data.get("objectId") or ""
    action = str(data.get("action") or "")
    detail = zentao_object_detail(otype, oid) if oid else {}
    ptitle, pbody = parse_zentao(data, raw)
    enriched = {
        "kind": "task",
        "task_id": str(oid),
        "object_type": otype,
        "action": action,
        "type": {"bug": "bugfix"}.get(otype, "feature"),  # bug→bugfix，其餘當 feature
        "assignee": detail.get("assignee", ""),
        "product": detail.get("product", ""),
        "module": detail.get("module", ""),
        "title": detail.get("title", "") or ptitle,
        "content": detail.get("content", "") or pbody,
    }
    raw_enriched = json.dumps(enriched, ensure_ascii=False)
    n = _deliver(enriched["title"], enriched["content"], raw_enriched, raw_enriched.encode("utf-8"))
    return jsonify(ok=True, forwarded=n, enriched=enriched)


@app.post("/api/test-send")
def test_send():
    d = request.get_json(force=True, silent=True) or {}
    title = d.get("title") or "測試通知"
    body = d.get("body") or "這是一則測試訊息"
    # 轉發的 payload 要帶內容，Master 才解析得出東西（否則收到空的 {}）
    raw = json.dumps({"title": title, "text": body}, ensure_ascii=False)
    n = _deliver(title, body, raw, raw.encode("utf-8"))
    return jsonify(ok=True, forwarded=n)


@app.post("/lark/ask")
def lark_ask():
    """Sub 叫站台發飛書問題卡片。body: {reply_to, correlation_id, title, questions:[{question,options}]}。"""
    d = request.get_json(force=True, silent=True) or {}
    cid = d.get("correlation_id") or ""
    questions = d.get("questions") or []
    reply_to = norm_addr(d.get("reply_to") or "")
    title = d.get("title") or "任務需要你決定"
    if not (cid and questions and reply_to):
        return jsonify(error="need correlation_id / questions / reply_to"), 400
    cfg = feishu_cfg()
    app_id, app_secret, chat_id = cfg.get("app_id"), cfg.get("app_secret"), cfg.get("chat_id")
    if not (app_id and app_secret and chat_id):
        return jsonify(error="feishu.json 未設定 app_id/app_secret/chat_id"), 500
    try:
        mid = feishu_send_card(app_id, app_secret, chat_id, build_question_card(title, questions, cid, {}))
    except Exception as e:  # noqa: BLE001
        return jsonify(error=f"發卡片失敗: {e}"), 502
    with ASK_LOCK:
        ASK_STORE[cid] = {"reply_to": reply_to, "message_id": mid,
                          "questions": questions, "answers": {}, "title": title}
    print(f"[feishu] 已發問題卡 cid={cid} mid={mid} → 等使用者點", flush=True)
    return jsonify(ok=True, message_id=mid)


@app.get("/api/users")
def api_users():
    """給 Master 撈 Zentao 使用者清單（account + 中文 realname）。"""
    try:
        return jsonify(zentao_users())
    except Exception as e:  # noqa: BLE001
        return jsonify(error=str(e)), 502


@app.get("/api/masters")
def list_masters():
    with db() as c:
        return jsonify([dict(r) for r in c.execute("SELECT * FROM masters ORDER BY id")])


@app.post("/api/masters")
def add_master():
    d = request.get_json(force=True, silent=True) or {}
    name = (d.get("name") or "").strip()
    addr = norm_addr(d.get("addr") or "")
    if not name or not addr:
        return jsonify(error="名稱和位址都要填"), 400
    with db() as c:
        cur = c.execute(
            "INSERT INTO masters(name, addr, enabled) VALUES(?,?,?)",
            (name, addr, 1 if d.get("enabled", True) else 0),
        )
        return jsonify(id=cur.lastrowid)


@app.put("/api/masters/<int:mid>")
def update_master(mid):
    d = request.get_json(force=True, silent=True) or {}
    name = (d.get("name") or "").strip()
    addr = norm_addr(d.get("addr") or "")
    if not name or not addr:
        return jsonify(error="名稱和位址都要填"), 400
    with db() as c:
        c.execute(
            "UPDATE masters SET name=?, addr=?, enabled=? WHERE id=?",
            (name, addr, 1 if d.get("enabled", True) else 0, mid),
        )
    return jsonify(ok=True)


@app.delete("/api/masters/<int:mid>")
def delete_master(mid):
    with db() as c:
        c.execute("DELETE FROM masters WHERE id=?", (mid,))
    return jsonify(ok=True)


@app.get("/api/messages")
def list_messages():
    with db() as c:
        rows = c.execute("SELECT id,title,body,created_at FROM messages ORDER BY id DESC LIMIT 50")
        return jsonify([dict(r) for r in rows])


@app.get("/")
def index():
    return send_from_directory(str(RES_DIR / "web"), "index.html")


if __name__ == "__main__":
    init_db()
    threading.Thread(target=start_feishu_listener, daemon=True).start()  # 飛書長連接（背景）
    app.run(host="0.0.0.0", port=PORT, threaded=True)
