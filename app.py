"""ZyoFlow Stage 站台 — 直接接 Zentao webhook + 飛書，路由派發給各 Sub（已移除 Master 中間層）。

  Zentao --(固定 webhook)--> 本服務（Stage 39217）--(依負責人綁定)--> Sub（個人電腦 39219）

單檔 Flask app：
  POST /webhook        接 Zentao webhook：撈細節 + 依 assignee 派給綁定的 Sub
  POST /api/register   Sub 開機/定期回報 {hostname, addr}，Stage 記錄並回綁定
  GET  /               控管網站（綁定 Sub↔Zentao↔飛書、看通知、送測試）
  API  /api/subs       Sub 綁定增刪改查；/api/users 禪道名單；/api/feishu-members 群成員
  飛書長連接           收卡片回調（問題卡）＋ 群訊息指令（/bindCheck…，可擴充）

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
            CREATE TABLE IF NOT EXISTS subs(
              id             INTEGER PRIMARY KEY,
              hostname       TEXT UNIQUE,          -- 綁定的穩定鍵；DHCP 換 IP 也不掉綁定
              addr           TEXT,                 -- ip:port，Sub 每次報到更新
              zentao_account TEXT,                 -- 綁定：禪道負責人帳號（任務路由用）
              feishu_open_id TEXT,                 -- 綁定：飛書 open_id（指令路由用）
              feishu_name    TEXT,                 -- 綁定的飛書顯示名
              enabled        INTEGER NOT NULL DEFAULT 1,
              last_seen      TEXT
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


def norm_addr(s: str) -> str:
    """使用者可能填 http://x:39219 或 x:39219/dispatch，一律 normalize 成 host:port。"""
    s = (s or "").strip().removeprefix("http://").removeprefix("https://").rstrip("/")
    for suffix in ("/dispatch", "/answer", "/webhook", "/push"):
        if s.endswith(suffix):
            s = s[: -len(suffix)].rstrip("/")
    return s


# ------------------------------------------------------ Sub 路由 / 派發
def sub_for_account(account: str):
    """依禪道負責人帳號找出綁定且啟用的 Sub 位址；沒有回 None。"""
    if not (account or "").strip():
        return None
    with db() as c:
        r = c.execute(
            "SELECT addr FROM subs WHERE enabled=1 AND zentao_account=? AND addr<>''", (account,)
        ).fetchone()
    return r["addr"] if r else None


def sub_for_open_id(open_id: str):
    """依飛書 open_id 找出綁定且啟用的 Sub 位址；沒有回 None。"""
    if not open_id:
        return None
    with db() as c:
        r = c.execute(
            "SELECT addr FROM subs WHERE enabled=1 AND feishu_open_id=? AND addr<>''", (open_id,)
        ).fetchone()
    return r["addr"] if r else None


def dispatch_to_sub(addr: str, payload_bytes: bytes) -> bool:
    """把 enriched task 原封 POST 給 Sub 的 /dispatch。"""
    url = f"http://{addr}/dispatch"
    try:
        req = urllib.request.Request(
            url, data=payload_bytes, headers={"Content-Type": "application/json"}, method="POST"
        )
        urllib.request.urlopen(req, timeout=5).close()
        print(f"[dispatch] 已派發 → {url}", flush=True)
        return True
    except Exception as e:  # noqa: BLE001 - 派發失敗不可中斷主流程
        print(f"[dispatch] {url} 失敗: {e}", flush=True)
        return False


def store_message(title, body, raw):
    """通知落庫（控管網頁的「最近通知」）。"""
    with db() as c:
        c.execute("INSERT INTO messages(title, body, raw) VALUES(?,?,?)", (title, body, raw))


def push_to_subs(payload_bytes: bytes) -> int:
    """把顯示用訊息推給所有啟用 Sub 的 /push（測試/通知用），回推送台數。"""
    with db() as c:
        addrs = [r["addr"] for r in c.execute("SELECT addr FROM subs WHERE enabled=1 AND addr<>''")]
    for addr in addrs:
        try:
            req = urllib.request.Request(
                f"http://{addr}/push", data=payload_bytes,
                headers={"Content-Type": "application/json"}, method="POST",
            )
            urllib.request.urlopen(req, timeout=5).close()
        except Exception as e:  # noqa: BLE001
            print(f"[push] {addr} 失敗: {e}", flush=True)
    return len(addrs)


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


def parse_zentao(data, raw: str):
    """Zentao webhook 格式依類型而異，這裡寬鬆抽取、原文一律保留。"""
    if isinstance(data, dict):
        title = data.get("title") or data.get("subject") or data.get("action") or "Zentao 通知"
        body = data.get("text") or data.get("content") or data.get("comment") or raw[:500]
        return str(title), str(body)
    return "Zentao 通知", (raw or "")[:500]


# ----------------------------------------------------- 飛書共用
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


def feishu_reply_text(chat_id, text):
    """發純文字到群組（指令回覆用）。失敗只記 log。"""
    cfg = feishu_cfg()
    app_id, app_secret = cfg.get("app_id"), cfg.get("app_secret")
    if not (app_id and app_secret and chat_id):
        return
    body = json.dumps(
        {"receive_id": chat_id, "msg_type": "text",
         "content": json.dumps({"text": text}, ensure_ascii=False)},
        ensure_ascii=False,
    ).encode()
    req = urllib.request.Request(
        "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id",
        data=body,
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {_feishu_token(app_id, app_secret)}"},
        method="POST",
    )
    try:
        urllib.request.urlopen(req, timeout=10).close()
    except Exception as e:  # noqa: BLE001
        print(f"[feishu] 回訊息失敗: {e}", flush=True)


def feishu_chat_members():
    """列目標群組成員 (open_id, name)，供網頁綁定下拉。需 im:chat 權限、feishu.json 有 chat_id。"""
    cfg = feishu_cfg()
    app_id, app_secret, chat_id = cfg.get("app_id"), cfg.get("app_secret"), cfg.get("chat_id")
    if not (app_id and app_secret and chat_id):
        raise RuntimeError("feishu.json 未設定 app_id/app_secret/chat_id")
    token = _feishu_token(app_id, app_secret)
    url = (f"https://open.feishu.cn/open-apis/im/v1/chats/{chat_id}/members"
           "?member_id_type=open_id&page_size=100")
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"}, method="GET")
    with urllib.request.urlopen(req, timeout=10) as r:
        data = json.loads(r.read())
    items = (data.get("data") or {}).get("items") or []
    # ponytail: 只取第一頁 100 人；一般群足夠，超過再加 page_token 分頁
    return [{"open_id": it.get("member_id", ""), "name": it.get("name", "")}
            for it in items if it.get("member_id")]


# ------------------------------------------------ 飛書問題卡（Sub 決策回答）
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


# ----------------------------------------------------- 飛書指令框架
# 好加新指令：@command("/xxx") 掛一個 handler(ctx)。ctx 有 open_id / chat_id / args。
COMMANDS = {}  # "/name"(lower) -> {"fn":..., "usage":...}


class CmdCtx:
    def __init__(self, open_id, chat_id, args):
        self.open_id = open_id
        self.chat_id = chat_id
        self.args = args


def command(name, usage=""):
    def deco(fn):
        COMMANDS[name.lower()] = {"fn": fn, "usage": usage}
        return fn
    return deco


_MENTION_RE = re.compile(r"@_(?:user_\d+|all)\s*")  # 群訊息 @機器人 會留下 @_user_1 佔位


def handle_message(open_id, chat_id, text):
    """群訊息進來：去掉 @佔位，找 /指令 分派；非指令忽略。"""
    text = _MENTION_RE.sub("", text or "").strip()
    if not text.startswith("/"):
        return
    head, _, rest = text.partition(" ")
    entry = COMMANDS.get(head.lower())
    if not entry:
        feishu_reply_text(chat_id, f"未知指令 {head}。可用：{', '.join(sorted(COMMANDS))}")
        return
    try:
        entry["fn"](CmdCtx(open_id, chat_id, rest.strip()))
    except Exception as e:  # noqa: BLE001 - 單一指令失敗不可影響長連接
        print(f"[cmd] {head} 失敗: {e}", flush=True)
        feishu_reply_text(chat_id, f"指令 {head} 執行失敗：{e}")


@command("/help", "顯示可用指令")
def cmd_help(ctx):
    lines = ["📖 可用指令："]
    for name in sorted(COMMANDS):
        lines.append(f"· {name}　{COMMANDS[name]['usage']}")
    feishu_reply_text(ctx.chat_id, "\n".join(lines))


@command("/bindCheck", "查綁定：/bindCheck <IP 或 Zentao 帳號 或 飛書名稱>（留空列全部）")
def cmd_bind_check(ctx):
    q = ctx.args.strip()
    with db() as c:
        if q:
            like = f"%{q}%"
            rows = c.execute(
                "SELECT * FROM subs WHERE addr LIKE ? OR hostname LIKE ? OR zentao_account LIKE ? "
                "OR feishu_name LIKE ? OR feishu_open_id=? ORDER BY id",
                (like, like, like, like, q),
            ).fetchall()
        else:
            rows = c.execute("SELECT * FROM subs ORDER BY id").fetchall()
    if not rows:
        feishu_reply_text(ctx.chat_id, f"查無綁定：{q or '(全部)'}")
        return
    lines = ["🔗 綁定狀態" + (f"（查詢：{q}）" if q else "")]
    for r in rows:
        state = "✅啟用" if r["enabled"] else "⛔停用"
        lines.append(
            f"· {r['hostname'] or '?'}（{r['addr'] or '未上線'}）{state}\n"
            f"　Zentao：{r['zentao_account'] or '未綁'}　飛書：{r['feishu_name'] or '未綁'}\n"
            f"　最後上線：{r['last_seen'] or '—'}"
        )
    feishu_reply_text(ctx.chat_id, "\n".join(lines))


# ----------------------------------------------------- 飛書長連接
def start_feishu_listener():
    """長連接同時處理：卡片回調(問題卡答案) + 群訊息指令。跑在背景執行緒、阻塞、斷線自動重連。"""
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
        from lark_oapi.api.im.v1 import P2ImMessageReceiveV1
    except ImportError as e:
        print(f"[feishu] 缺 lark-oapi（{e}），不啟動長連接", flush=True)
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
            else:  # 非本系統卡片（例如手動測試），存起來給網頁看
                store_message("飛書回調", json.dumps(value, ensure_ascii=False),
                              json.dumps({"feishu_callback": value, "open_id": open_id}, ensure_ascii=False))
        except Exception as e:  # noqa: BLE001
            print(f"[feishu] 處理回調失敗: {e}", flush=True)
        return P2CardActionTriggerResponse(resp)

    def on_message(data: P2ImMessageReceiveV1) -> None:
        try:
            msg = data.event.message
            open_id = data.event.sender.sender_id.open_id
            if msg.message_type != "text":
                return
            text = (json.loads(msg.content) or {}).get("text", "")
            print(f"[feishu] 收訊息 open_id={open_id} text={text!r}", flush=True)
            handle_message(open_id, msg.chat_id, text)
        except Exception as e:  # noqa: BLE001
            print(f"[feishu] 處理訊息失敗: {e}", flush=True)

    handler = (
        lark.EventDispatcherHandler.builder("", "")
        .register_p2_card_action_trigger(on_card_action)
        .register_p2_im_message_receive_v1(on_message)
        .build()
    )
    # 斷線自動重連：長連接掉了就等 30 秒重來，別讓站台靜默停止收事件
    while True:
        try:
            print("[feishu] 長連接啟動中…（卡片回調 + 群訊息指令）", flush=True)
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
    store_message(enriched["title"], enriched["content"], raw_enriched)
    # 依負責人直接派給綁定的 Sub（Master 中間層已移除）
    addr = sub_for_account(enriched["assignee"])
    dispatched = False
    if addr:
        dispatched = dispatch_to_sub(addr, raw_enriched.encode("utf-8"))
    else:
        print(f"[webhook] 找不到負責 {enriched['assignee']!r} 的 Sub，僅記錄不派發", flush=True)
    return jsonify(ok=True, dispatched=dispatched, enriched=enriched)


@app.post("/api/register")
def api_register():
    """Sub 開機/定期回報 {hostname, addr}。以 hostname upsert、更新 last_seen，回目前綁定給 Sub 顯示。"""
    d = request.get_json(force=True, silent=True) or {}
    hostname = (d.get("hostname") or "").strip()
    addr = norm_addr(d.get("addr") or "")
    if not hostname or not addr:
        return jsonify(error="need hostname / addr"), 400
    with db() as c:
        c.execute(
            """INSERT INTO subs(hostname, addr, last_seen)
                 VALUES(?, ?, datetime('now','localtime'))
               ON CONFLICT(hostname) DO UPDATE SET
                 addr=excluded.addr, last_seen=datetime('now','localtime')""",
            (hostname, addr),
        )
        r = c.execute(
            "SELECT zentao_account, feishu_name, enabled FROM subs WHERE hostname=?", (hostname,)
        ).fetchone()
    return jsonify(ok=True, binding={
        "zentao_account": r["zentao_account"] or "",
        "feishu_name": r["feishu_name"] or "",
        "enabled": bool(r["enabled"]),
    })


@app.post("/api/test-send")
def test_send():
    d = request.get_json(force=True, silent=True) or {}
    title = d.get("title") or "測試通知"
    body = d.get("body") or "這是一則測試訊息"
    raw = json.dumps({"title": title, "body": body}, ensure_ascii=False)
    store_message(title, body, raw)
    n = push_to_subs(raw.encode("utf-8"))
    return jsonify(ok=True, pushed=n)


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
    """給控管網頁撈 Zentao 使用者清單（account + 中文 realname）。"""
    try:
        return jsonify(zentao_users())
    except Exception as e:  # noqa: BLE001
        return jsonify(error=str(e)), 502


@app.get("/api/feishu-members")
def api_feishu_members():
    """給控管網頁撈飛書群成員（open_id + name）做綁定下拉。"""
    try:
        return jsonify(feishu_chat_members())
    except Exception as e:  # noqa: BLE001
        return jsonify(error=str(e)), 502


@app.get("/api/subs")
def list_subs():
    with db() as c:
        return jsonify([dict(r) for r in c.execute("SELECT * FROM subs ORDER BY id")])


@app.put("/api/subs/<int:sid>")
def update_sub(sid):
    """綁定：設定這台 Sub 對應的 Zentao 帳號 / 飛書用戶 / 啟用。"""
    d = request.get_json(force=True, silent=True) or {}
    with db() as c:
        c.execute(
            "UPDATE subs SET zentao_account=?, feishu_open_id=?, feishu_name=?, enabled=? WHERE id=?",
            (
                (d.get("zentao_account") or "").strip(),
                (d.get("feishu_open_id") or "").strip(),
                (d.get("feishu_name") or "").strip(),
                1 if d.get("enabled", True) else 0,
                sid,
            ),
        )
    return jsonify(ok=True)


@app.delete("/api/subs/<int:sid>")
def delete_sub(sid):
    with db() as c:
        c.execute("DELETE FROM subs WHERE id=?", (sid,))
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
