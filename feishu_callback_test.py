"""飛書卡片回調最小測試 —— 證明「按鈕點擊收得回來」。

做兩件事：
  1. 發一張帶兩顆選項按鈕的互動卡片到群組（chat_id 讀 Master 存的 lark.json）
  2. 啟動長連接，印出你在飛書點的按鈕內容（value + 點的人 open_id）

跑法：
  pip install -U lark-oapi
  python feishu_callback_test.py

憑證與群組讀 Master 存的 %APPDATA%\\ZyoFlow\\lark.json（app_id / app_secret / chat_id）。
站台對不對外網都無所謂——是這支程式「撥出去」連飛書，不是飛書打進來。
"""
import json
import os
import urllib.error
import urllib.request
from pathlib import Path

try:
    import lark_oapi as lark
    from lark_oapi.event.callback.model.p2_card_action_trigger import (
        P2CardActionTrigger,
        P2CardActionTriggerResponse,
    )
except ImportError:
    raise SystemExit("缺 lark-oapi，先跑：pip install -U lark-oapi")

HOST = "https://open.feishu.cn"  # 中國版飛書


def load_creds():
    """讀 Master 存的設定（App ID/Secret/固定群組 chat_id）。"""
    p = Path(os.environ.get("APPDATA", str(Path.home()))) / "ZyoFlow" / "lark.json"
    cfg = json.loads(p.read_text(encoding="utf-8-sig"))
    return cfg["app_id"], cfg["app_secret"], cfg.get("chat_id", "")


APP_ID, APP_SECRET, CHAT_ID = load_creds()


def tenant_token():
    body = json.dumps({"app_id": APP_ID, "app_secret": APP_SECRET}).encode()
    req = urllib.request.Request(
        f"{HOST}/open-apis/auth/v3/tenant_access_token/internal",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())["tenant_access_token"]


def send_test_card():
    """發一張帶 value 按鈕的卡片；value 之後會原樣出現在回調裡。"""
    card = {
        "config": {"wide_screen_mode": True, "update_multi": True},
        "header": {"template": "blue", "title": {"tag": "plain_text", "content": "回調測試 · 請點一個選項"}},
        "elements": [
            {"tag": "div", "text": {"tag": "lark_md", "content": "點下面任一顆，這支程式應印出你點的內容。"}},
            {"tag": "action", "actions": [
                {"tag": "button", "text": {"tag": "plain_text", "content": "選項 A"},
                 "type": "primary", "value": {"action": "choose", "option": "A"}},
                {"tag": "button", "text": {"tag": "plain_text", "content": "選項 B"},
                 "type": "default", "value": {"action": "choose", "option": "B"}},
            ]},
        ],
    }
    body = json.dumps(
        {"receive_id": CHAT_ID, "msg_type": "interactive",
         "content": json.dumps(card, ensure_ascii=False)},
        ensure_ascii=False,
    ).encode()
    req = urllib.request.Request(
        f"{HOST}/open-apis/im/v1/messages?receive_id_type=chat_id",
        data=body,
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {tenant_token()}"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            print("發卡片回應:", r.read().decode())
    except urllib.error.HTTPError as e:
        print("發卡片失敗:", e.code, e.read().decode())
    except Exception as e:  # noqa: BLE001
        print("發卡片失敗:", e)


def on_card_action(data: P2CardActionTrigger) -> P2CardActionTriggerResponse:
    """收到按鈕點擊。必須 3 秒內回應（這裡回個 toast 給點的人看）。"""
    open_id = data.event.operator.open_id
    value = data.event.action.value
    print(f"\n🎉 收到回調！ open_id={open_id}  value={value}\n", flush=True)
    return P2CardActionTriggerResponse({
        "toast": {"type": "info", "content": "已收到 ✅",
                  "i18n": {"zh_cn": "已收到 ✅", "en_us": "Got it"}},
    })


if __name__ == "__main__":
    if CHAT_ID:
        send_test_card()
        print(f"卡片已發到群組 {CHAT_ID}，去飛書點按鈕；長連接啟動中…", flush=True)
    else:
        print("lark.json 沒有 chat_id：先在 Master 選好群組。仍會啟動長連接等回調。", flush=True)

    handler = (
        lark.EventDispatcherHandler.builder("", "")  # (encrypt_key, verification_token) 都空：長連接免加密
        .register_p2_card_action_trigger(on_card_action)
        .build()
    )
    ws = lark.ws.Client(APP_ID, APP_SECRET, event_handler=handler, log_level=lark.LogLevel.INFO)
    ws.start()  # 阻塞，持續接收（Ctrl+C 結束）
