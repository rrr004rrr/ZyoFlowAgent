"""最小自我檢查：python test_app.py（需先裝 Flask）。"""
import os
import tempfile

os.environ["ZYOFLOW_DB"] = tempfile.mktemp(suffix=".sqlite")
import app as A  # noqa: E402

A.init_db()
c = A.app.test_client()

# 名稱、位址都必填
assert c.post("/api/masters", json={"name": "", "addr": "x:1"}).status_code == 400
assert c.post("/api/masters", json={"name": "x", "addr": ""}).status_code == 400

# 位址 normalize：http:// 與 /webhook 會被去掉
assert A.norm_addr("http://192.168.1.20:39218/webhook") == "192.168.1.20:39218"

# 新增 Master（指到不存在的埠，轉發會失敗但不影響流程）
r = c.post("/api/masters", json={"name": "我的電腦", "addr": "127.0.0.1:9"})
assert r.status_code == 200, r.data
mid = r.get_json()["id"]

# 啟用中的 Master 應被算進轉發對象
j = c.post("/webhook", json={"title": "任務 #1234", "text": "alice 指派給你"}).get_json()
assert j["ok"] and j["forwarded"] == 1, j

# 訊息已存
msgs = c.get("/api/messages").get_json()
assert msgs and msgs[0]["title"] == "任務 #1234", msgs

# 怪 payload 也能寬鬆解析、保留原文
t, b = A.parse_zentao({}, "raw-body")
assert t == "Zentao 通知" and b == "raw-body", (t, b)

# 停用後不再轉發
c.put(f"/api/masters/{mid}", json={"name": "我的電腦", "addr": "127.0.0.1:9", "enabled": False})
assert c.post("/webhook", json={"text": "hi"}).get_json()["forwarded"] == 0

# Zentao API：未設定回 502、設定好回 200，都不該 crash
assert c.get("/api/users").status_code in (200, 502)

print("OK")
