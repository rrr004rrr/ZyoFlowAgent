//! 飛書（Feishu）REST 出站：tenant_access_token + 發訊息（text/post/互動卡片）+ 就地更新卡片。
//! 只在 Master、只走出站（Master 主動連 open.feishu.cn）；收使用者回覆/按鈕回呼屬入站(Stage 2)，不在此檔。
//! ponytail: 用既有的 ureq(同步)，不引入 reqwest；網路呼叫在 GUI 的背景 std::thread 跑。
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const HOST: &str = "https://open.feishu.cn"; // 中國版飛書；國際版 Lark 改 open.larksuite.com

/// token 快取：(token, 到期時間, 取得時用的 app_secret)。secret 換了就重取。
pub type TokenCache = Mutex<Option<(String, Instant, String)>>;

/// 取 tenant_access_token，提前 5 分鐘刷新；同 secret 在有效期內走快取。
pub fn get_token(app_id: &str, app_secret: &str, cache: &TokenCache) -> Result<String, String> {
    if let Some((t, exp, sec)) = &*cache.lock().unwrap() {
        if Instant::now() < *exp && sec == app_secret {
            return Ok(t.clone());
        }
    }
    let url = format!("{HOST}/open-apis/auth/v3/tenant_access_token/internal");
    let body = json!({ "app_id": app_id, "app_secret": app_secret }).to_string();
    let v = post_json(&url, None, &body)?;
    if v.get("code").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(format!("取得 token 失敗：{v}"));
    }
    let token = v
        .get("tenant_access_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let expire = v.get("expire").and_then(Value::as_u64).unwrap_or(7200);
    let exp = Instant::now() + Duration::from_secs(expire.saturating_sub(300).max(60));
    *cache.lock().unwrap() = Some((token.clone(), exp, app_secret.to_string()));
    Ok(token)
}

/// 發訊息，回傳 message_id。content 必須是「字串化的 JSON」（飛書規定）。
pub fn send_message(
    app_id: &str,
    app_secret: &str,
    cache: &TokenCache,
    receive_id_type: &str,
    receive_id: &str,
    msg_type: &str,
    content: &str,
) -> Result<String, String> {
    let token = get_token(app_id, app_secret, cache)?;
    let url = format!("{HOST}/open-apis/im/v1/messages?receive_id_type={receive_id_type}");
    let body = json!({
        "receive_id": receive_id,
        "msg_type": msg_type,
        "content": content,
    })
    .to_string();
    let v = post_json(&url, Some(&token), &body)?;
    if v.get("code").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(format!("發送失敗：{v}"));
    }
    Ok(v.pointer("/data/message_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

/// 就地更新互動卡片（同一 message_id，不洗版）。卡片需 config.update_multi=true。
pub fn update_card(
    app_id: &str,
    app_secret: &str,
    cache: &TokenCache,
    message_id: &str,
    content: &str,
) -> Result<(), String> {
    let token = get_token(app_id, app_secret, cache)?;
    let url = format!("{HOST}/open-apis/im/v1/messages/{message_id}");
    let body = json!({ "content": content }).to_string();
    let v = patch_json(&url, &token, &body)?;
    if v.get("code").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(format!("更新失敗：{v}"));
    }
    Ok(())
}

/// 列出機器人所在的群組，回傳 (name, chat_id)。需 scope im:chat:readonly（或 im:chat）。
/// 群組發訊一定要 chat_id；UI 上不好挖，靠這支撈出來貼到收件人。
pub fn list_chats(
    app_id: &str,
    app_secret: &str,
    cache: &TokenCache,
) -> Result<Vec<(String, String)>, String> {
    let token = get_token(app_id, app_secret, cache)?;
    let url = format!("{HOST}/open-apis/im/v1/chats?page_size=100");
    let v = get_json(&url, &token)?;
    if v.get("code").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(format!("列出群組失敗：{v}"));
    }
    let items = v
        .pointer("/data/items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(items
        .iter()
        .map(|it| {
            let name = it
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("(無名群組)")
                .to_string();
            let chat_id = it
                .get("chat_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            (name, chat_id)
        })
        .collect())
}

// ----------------------------------------- 內容組裝（content 一律回字串化 JSON）

pub fn text_content(text: &str) -> String {
    json!({ "text": text }).to_string()
}

/// 富文本範例（固定樣本，夠驗證渲染）。
pub fn post_content() -> String {
    json!({
        "zh_cn": {
            "title": "富文本測試",
            "content": [
                [ {"tag":"text","text":"這是富文本，含超連結："},
                  {"tag":"a","text":"飛書開放平台","href":"https://open.feishu.cn"} ],
                [ {"tag":"text","text":"第二段純文字。"} ]
            ]
        }
    })
    .to_string()
}

/// 互動卡片：含一顆 open_url 按鈕(點了開網頁，免 inbound)＋一顆回呼按鈕(需 Stage 2 才收得到點擊)。
pub fn card_content(title: &str, body_md: &str) -> String {
    json!({
        "config": { "wide_screen_mode": true, "update_multi": true },
        "header": { "template": "blue", "title": { "tag": "plain_text", "content": title } },
        "elements": [
            { "tag": "div", "text": { "tag": "lark_md", "content": body_md } },
            { "tag": "action", "actions": [
                { "tag":"button", "text":{"tag":"plain_text","content":"開啟網頁"},
                  "type":"default", "url":"https://open.feishu.cn" }
            ] }
        ]
    })
    .to_string()
}

/// 用來示範就地更新的「更新後」卡片（換顏色與內容，message_id 不變）。
pub fn updated_card_content(title: &str) -> String {
    json!({
        "config": { "wide_screen_mode": true, "update_multi": true },
        "header": { "template": "green", "title": { "tag":"plain_text", "content": format!("{title}（已更新）") } },
        "elements": [ { "tag":"div", "text":{"tag":"lark_md","content":"✅ 這張卡片剛被 **就地更新**，message_id 沒變。"} } ]
    })
    .to_string()
}

// ------------------------- ureq 共用：回 JSON，HTTP 4xx/5xx 也把 body 帶出來方便看錯誤

fn post_json(url: &str, bearer: Option<&str>, body: &str) -> Result<Value, String> {
    let mut req = ureq::post(url)
        .timeout(Duration::from_secs(10))
        .set("Content-Type", "application/json");
    if let Some(t) = bearer {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    finish(req.send_string(body))
}

fn patch_json(url: &str, bearer: &str, body: &str) -> Result<Value, String> {
    let req = ureq::request("PATCH", url)
        .timeout(Duration::from_secs(10))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {bearer}"));
    finish(req.send_string(body))
}

fn get_json(url: &str, bearer: &str) -> Result<Value, String> {
    let req = ureq::get(url)
        .timeout(Duration::from_secs(10))
        .set("Authorization", &format!("Bearer {bearer}"));
    finish(req.call())
}

fn finish(resp: Result<ureq::Response, ureq::Error>) -> Result<Value, String> {
    match resp {
        Ok(r) => {
            let s = r.into_string().map_err(|e| format!("讀取回應失敗：{e}"))?;
            serde_json::from_str(&s).map_err(|e| format!("回應非 JSON：{e}；原文：{s}"))
        }
        Err(ureq::Error::Status(code, r)) => {
            let s = r.into_string().unwrap_or_default();
            Err(format!("HTTP {code}：{s}"))
        }
        Err(e) => Err(format!("連線失敗：{e}")),
    }
}

// ----------------------------------------- 憑證/設定持久化（%APPDATA%\ZyoFlow\lark.json）

#[derive(Default, Clone)]
pub struct LarkConfig {
    pub app_id: String,
    pub app_secret: String,
    pub chat_id: String,
    pub chat_name: String,
}

/// 設定檔位置：Windows 用 %APPDATA%\ZyoFlow\lark.json；mac/Linux 退回 $HOME\ZyoFlow。
fn config_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("APPDATA").or_else(|_| std::env::var("HOME")).ok()?;
    let dir = std::path::Path::new(&base).join("ZyoFlow");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("lark.json"))
}

/// 讀設定（含 App Secret）；讀不到回空值。
pub fn load_config() -> LarkConfig {
    let mut c = LarkConfig::default();
    if let Some(p) = config_path() {
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                let g = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                c.app_id = g("app_id");
                c.app_secret = g("app_secret");
                c.chat_id = g("chat_id");
                c.chat_name = g("chat_name");
            }
        }
    }
    c
}

/// 寫設定到 appdata。ponytail: 明文 JSON；要更嚴改 Windows DPAPI/憑證管理員。
pub fn save_config(c: &LarkConfig) -> Result<(), String> {
    let p = config_path().ok_or("找不到設定目錄（APPDATA/HOME）")?;
    let v = json!({
        "app_id": c.app_id,
        "app_secret": c.app_secret,
        "chat_id": c.chat_id,
        "chat_name": c.chat_name,
    });
    std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap_or_default())
        .map_err(|e| format!("寫入設定失敗：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_stringified_and_card_updatable() {
        // content 必須是字串化 JSON（常見錯誤是傳物件）
        assert_eq!(text_content("hi"), r#"{"text":"hi"}"#);
        // 卡片要帶 update_multi=true，否則 update_card 會失敗
        let c = card_content("標題", "內文");
        let v: Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v.pointer("/config/update_multi"), Some(&Value::Bool(true)));
    }
}
