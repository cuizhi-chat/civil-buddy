use crate::config::{llm_api_key, llm_config, llm_uses_thinking};
use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct LlmError(pub String);

pub fn has_key() -> bool {
    crate::config::has_key()
}

fn headers() -> Result<reqwest::header::HeaderMap, LlmError> {
    let key = llm_api_key();
    if key.is_empty() {
        return Err(LlmError(
            "未配置 API Key。在 demo/.env 写入 CIVIL_API_KEY / OPENAI_API_KEY / DEEPSEEK_API_KEY 后重启。".into(),
        ));
    }
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {key}")
            .parse()
            .map_err(|e: reqwest::header::InvalidHeaderValue| LlmError(e.to_string()))?,
    );
    h.insert(
        reqwest::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    h.insert(
        reqwest::header::ACCEPT_ENCODING,
        "identity".parse().unwrap(),
    );
    Ok(h)
}

fn thinking_on(for_tools: bool) -> bool {
    match std::env::var("DEEPSEEK_THINKING")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "0" | "off" | "disabled" | "false" => false,
        "1" | "on" | "enabled" | "true" => true,
        _ => for_tools,
    }
}

/// Max silence between two upstream chunks (streaming) / total body wait (non-streaming).
fn read_timeout() -> std::time::Duration {
    let secs = std::env::var("CIVIL_LLM_READ_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(180);
    std::time::Duration::from_secs(secs)
}

fn http_client() -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(read_timeout())
        .http1_only()
        .build()
        .map_err(|e| LlmError(format!("http client: {e}")))
}

fn stream_client() -> Result<reqwest::Client, LlmError> {
    // no total timeout: a long draft may stream for minutes; only silence is a failure
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(read_timeout())
        .http1_only()
        .build()
        .map_err(|e| LlmError(format!("http client: {e}")))
}

pub const STOPPED: &str = "__stopped__";

impl LlmError {
    pub fn stopped() -> Self {
        LlmError(STOPPED.into())
    }
    pub fn is_stopped(&self) -> bool {
        self.0 == STOPPED
    }
}

fn http_err(status: reqwest::StatusCode, body: &str) -> LlmError {
    if status.as_u16() == 401 {
        return LlmError(
            "API Key 401：无效。请更新 demo/.env 后重启工作台。".into(),
        );
    }
    let cut: String = body.chars().take(400).collect();
    LlmError(format!("LLM {status}: {cut}"))
}

pub async fn chat(messages: &[Value], tools: Option<&[Value]>, temperature: f32) -> Result<Value, LlmError> {
    let cfg = llm_config();
    let thinking = llm_uses_thinking(&cfg.base_url) && thinking_on(tools.is_some());
    let mut payload = json!({
        "model": cfg.model,
        "messages": messages,
    });
    if thinking {
        payload["thinking"] = json!({ "type": "enabled" });
    } else {
        payload["temperature"] = json!(temperature);
    }
    if let Some(tools) = tools {
        payload["tools"] = json!(tools);
        payload["tool_choice"] = json!("auto");
    }
    let url = format!("{}/chat/completions", cfg.base_url);
    let r = http_client()?
        .post(url)
        .headers(headers()?)
        .json(&payload)
        .send()
        .await
        .map_err(|e| LlmError(format!("chat send: {e}")))?;
    let status = r.status();
    let raw = r
        .bytes()
        .await
        .map_err(|e| LlmError(format!("chat body: {e}")))?;
    let body = String::from_utf8_lossy(&raw).into_owned();
    if !status.is_success() {
        return Err(http_err(status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| LlmError(e.to_string()))?;
    v.pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| LlmError("LLM 响应缺少 message".into()))
}

/// Streaming completion that also assembles tool calls from deltas.
///
/// `on_piece` receives content as it arrives; the returned value is shaped like a
/// non-streaming `choices[0].message` (content + tool_calls) so callers can append it
/// to history unchanged. `should_stop` is polled per upstream line; when it returns
/// true the upstream connection is dropped and `LlmError::stopped()` is returned.
pub async fn stream_chat<F, S>(
    messages: &[Value],
    tools: Option<&[Value]>,
    temperature: f32,
    mut on_piece: F,
    should_stop: S,
) -> Result<Value, LlmError>
where
    F: FnMut(&str),
    S: Fn() -> bool,
{
    let cfg = llm_config();
    let thinking = llm_uses_thinking(&cfg.base_url) && thinking_on(tools.is_some());
    let mut payload = json!({
        "model": cfg.model,
        "messages": messages,
        "stream": true,
    });
    if thinking {
        payload["thinking"] = json!({ "type": "enabled" });
    } else {
        payload["temperature"] = json!(temperature);
    }
    if let Some(tools) = tools {
        payload["tools"] = json!(tools);
        payload["tool_choice"] = json!("auto");
    }
    let url = format!("{}/chat/completions", cfg.base_url);
    let mut r = stream_client()?
        .post(url)
        .headers(headers()?)
        .json(&payload)
        .send()
        .await
        .map_err(|e| LlmError(format!("stream send: {e}")))?;
    if !r.status().is_success() {
        let status = r.status();
        let raw = r.bytes().await.unwrap_or_default();
        let body = String::from_utf8_lossy(&raw).into_owned();
        return Err(http_err(status, &body));
    }
    let mut content = String::new();
    let mut calls: std::collections::BTreeMap<usize, Value> = std::collections::BTreeMap::new();
    let mut finish = String::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut done = false;
    while !done {
        let Some(chunk) = r
            .chunk()
            .await
            .map_err(|e| LlmError(format!("stream chunk: {e}")))?
        else {
            break;
        };
        if should_stop() {
            return Err(LlmError::stopped());
        }
        buf.extend_from_slice(&chunk);
        // SSE lines can straddle network chunks: only consume complete lines
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                done = true;
                break;
            }
            let Ok(ev) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let Some(choice) = ev.pointer("/choices/0") else {
                continue;
            };
            if let Some(f) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                finish = f.to_string();
            }
            let delta = choice.get("delta").cloned().unwrap_or(json!({}));
            if let Some(piece) = delta.get("content").and_then(|v| v.as_str()) {
                if !piece.is_empty() {
                    content.push_str(piece);
                    on_piece(piece);
                }
            }
            for tc in delta.get("tool_calls").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let slot = calls.entry(idx).or_insert_with(|| {
                    json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}})
                });
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    slot["id"] = json!(id);
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(|v| v.as_str()) {
                        let cur = slot["function"]["name"].as_str().unwrap_or("").to_string();
                        slot["function"]["name"] = json!(format!("{cur}{n}"));
                    }
                    if let Some(a) = f.get("arguments").and_then(|v| v.as_str()) {
                        let cur = slot["function"]["arguments"].as_str().unwrap_or("").to_string();
                        slot["function"]["arguments"] = json!(format!("{cur}{a}"));
                    }
                }
            }
        }
    }
    let mut msg = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        msg["tool_calls"] = json!(calls.into_values().collect::<Vec<_>>());
    }
    if !finish.is_empty() {
        msg["finish_reason"] = json!(finish);
    }
    Ok(msg)
}

pub async fn stream_plain<F>(messages: &[Value], temperature: f32, on_piece: F) -> Result<(), LlmError>
where
    F: FnMut(&str),
{
    stream_chat(messages, None, temperature, on_piece, || false).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthorized_does_not_echo_key_suffix() {
        let msg = http_err(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"Authentication Fails, Your api key: ****715b is invalid"}}"#,
        );
        let text = msg.to_string();
        assert!(!text.contains("715b"), "{text}");
        assert!(text.contains("401"), "{text}");
        assert!(text.contains("demo/.env"), "{text}");
    }
}
