"""最小自我檢查：python test_app.py（需先裝 Flask）。不碰網路（飛書未設定時指令回覆自動略過）。"""
import os
import tempfile

os.environ["ZYOFLOW_DB"] = tempfile.mktemp(suffix=".sqlite")
import app as A  # noqa: E402

A.init_db()
c = A.app.test_client()

# 位址 normalize：http:// 與 /dispatch 會被去掉
assert A.norm_addr("http://192.168.1.20:39219/dispatch") == "192.168.1.20:39219"

# Sub 報到：以 hostname upsert，回傳目前綁定（未綁時為空）
r = c.post("/api/register", json={"hostname": "PC-A", "addr": "127.0.0.1:9"})
assert r.status_code == 200 and r.get_json()["binding"]["zentao_account"] == "", r.data
# 同 hostname 再報到只更新 addr、不新增列
c.post("/api/register", json={"hostname": "PC-A", "addr": "127.0.0.1:39219"})
subs = c.get("/api/subs").get_json()
assert len(subs) == 1 and subs[0]["addr"] == "127.0.0.1:39219", subs
sid = subs[0]["id"]

# 綁定 Zentao + 飛書
c.put(f"/api/subs/{sid}", json={"zentao_account": "alice", "feishu_open_id": "ou_x",
                                "feishu_name": "王小明", "enabled": True})
assert c.get("/api/subs").get_json()[0]["zentao_account"] == "alice"
# 報到會把綁定回給 Sub 顯示
b = c.post("/api/register", json={"hostname": "PC-A", "addr": "127.0.0.1:39219"}).get_json()["binding"]
assert b == {"zentao_account": "alice", "feishu_name": "王小明", "enabled": True}, b

# 路由查詢：帳號 / open_id 都對得到；查無回 None
assert A.sub_for_account("alice") == "127.0.0.1:39219"
assert A.sub_for_open_id("ou_x") == "127.0.0.1:39219"
assert A.sub_for_account("nobody") is None

# webhook：沒對到負責人 Sub 時仍 ok、dispatched=False（不中斷）
j = c.post("/webhook", json={"title": "任務 #1", "text": "hi"}).get_json()
assert j["ok"] and j["dispatched"] is False, j
assert any("任務 #1" in (m["title"] or "") for m in c.get("/api/messages").get_json())

# 指令框架：/help /bindCheck 已註冊；分派不因未設飛書而崩（回覆自動略過）
assert "/help" in A.COMMANDS and "/bindcheck" in A.COMMANDS
A.handle_message("ou_x", "oc_1", "@_user_1 一般聊天")          # 非指令 → 略過
A.handle_message("ou_x", "oc_1", "@_user_1 /bindCheck alice")  # 有結果 → 回覆(略過送出)
A.handle_message("ou_x", "oc_1", "/nope")                      # 未知指令 → 回覆(略過送出)

# 刪除綁定
c.delete(f"/api/subs/{sid}")
assert c.get("/api/subs").get_json() == []

# 怪 payload 也能寬鬆解析、保留原文
t, body = A.parse_zentao({}, "raw-body")
assert t == "Zentao 通知" and body == "raw-body", (t, body)

# Zentao API：未設定回 502、設定好回 200，都不該 crash
assert c.get("/api/users").status_code in (200, 502)

print("OK")
