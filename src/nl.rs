//! Natural-language layer of the CLI: an OpenAI-compatible chat client that
//! turns a request into CLIF text. See DESIGN.md §6.
//!
//! Binary-only module; the library, the MCP server and the harness never call
//! a model. The retry loop lives in `main.rs` because it needs the `Session`.

use std::cell::OnceCell;
use std::env;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// The CLIF reference the model writes against (DESIGN.md §6, §10).
const CHEATSHEET: &str = include_str!("../prompts/cheatsheet.md");

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434/v1";
pub const DEFAULT_MODEL: &str = "qwen3";
const TIMEOUT: Duration = Duration::from_secs(180);
const MAX_TOKENS: u32 = 4096;
/// Base temperature; each retry adds this much (DESIGN.md §11.1).
pub const BASE_TEMPERATURE: f32 = 0.1;
pub const TEMPERATURE_STEP: f32 = 0.3;

/// One chat message. Also the unit of the CLI's conversation history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Msg {
    pub role: String,
    pub content: String,
}

impl Msg {
    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", content)
    }

    fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: content.into(),
        }
    }
}

/// An OpenAI-compatible endpoint, configured from the environment.
pub struct Client {
    pub base_url: String,
    pub model: String,
    api_key: Option<String>,
    enabled: bool,
    http: OnceCell<reqwest::blocking::Client>,
}

/// True when `base_url` points at OpenAI itself (`https://api.openai.com/...`).
fn is_openai_host(base_url: &str) -> bool {
    let rest = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    let host = rest.split(['/', ':']).next().unwrap_or("");
    host.eq_ignore_ascii_case("api.openai.com")
}

impl Client {
    /// Reads `.env` and the environment. `offline_flag` is `--offline`; the
    /// API key is optional because most local servers do not want one.
    pub fn from_env(offline_flag: bool) -> Self {
        let _ = dotenvy::dotenv();
        let base_url = first_env(&["CLIFPAD_BASE_URL", "OPENAI_BASE_URL"])
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let model =
            first_env(&["CLIFPAD_MODEL", "OPENAI_MODEL"]).unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let base_url = base_url.trim_end_matches('/').to_string();
        // CLIFPAD_API_KEY is explicit and always sent. OPENAI_API_KEY is
        // scoped to OpenAI: sending it as a bearer token to a LAN server or a
        // mistyped host would leak it, so it is only used for api.openai.com.
        let api_key = first_env(&["CLIFPAD_API_KEY"]).or_else(|| {
            if is_openai_host(&base_url) {
                first_env(&["OPENAI_API_KEY"])
            } else {
                None
            }
        });
        let env_offline = first_env(&["CLIFPAD_OFFLINE"])
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        Self {
            base_url,
            model,
            api_key,
            enabled: !offline_flag && !env_offline,
            http: OnceCell::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// One line for the banner and for `:online` / `:offline`.
    pub fn describe(&self) -> String {
        if self.enabled {
            format!("{} @ {}", self.model, self.base_url)
        } else {
            format!("offline (model disabled; {} @ {})", self.model, self.base_url)
        }
    }

    /// System prompt: the cheat sheet plus the session's current symbols.
    pub fn system_prompt(&self, state: &str) -> String {
        let state = state.trim();
        let state = if state.is_empty() { "(empty session)" } else { state };
        format!("{CHEATSHEET}\n\n## Session state\n\n{state}\n")
    }

    /// POST `/chat/completions` and return the assistant's raw content.
    pub fn chat(&self, messages: &[Msg], temperature: f32) -> Result<String> {
        if !self.enabled {
            bail!("the model is disabled; use :online or drop --offline");
        }
        let req = ChatRequest {
            model: &self.model,
            temperature,
            max_tokens: MAX_TOKENS,
            messages,
        };
        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self.http()?.post(&url).json(&req);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().with_context(|| format!("POST {url}"))?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if !status.is_success() {
            bail!("model HTTP {}: {}", status.as_u16(), truncate(&body, 800));
        }
        let parsed: ChatResponse = serde_json::from_str(&body)
            .with_context(|| format!("decode model response: {}", truncate(&body, 800)))?;
        let content = parsed
            .choices
            .into_iter()
            .find_map(|c| c.message.content)
            .ok_or_else(|| anyhow!("model returned no content"))?;
        if content.trim().is_empty() {
            bail!("model returned an empty reply");
        }
        Ok(content)
    }

    fn http(&self) -> Result<&reqwest::blocking::Client> {
        if let Some(c) = self.http.get() {
            return Ok(c);
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .context("build HTTP client")?;
        let _ = self.http.set(client);
        Ok(self.http.get().expect("just set"))
    }
}

/// Pull CLIF out of a reply: drop reasoning, prefer the first fenced block,
/// otherwise take everything from the first `function` onward.
pub fn extract_clif(reply: &str) -> String {
    let text = strip_think(reply);
    if let Some(block) = fenced_block(&text) {
        return block.trim().to_string();
    }
    match text.find("function") {
        Some(i) => text[i..].trim().to_string(),
        None => text.trim().to_string(),
    }
}

/// The retry message fed back to the model after a parse/verifier/run error.
pub fn retry_message(error: &str) -> Msg {
    Msg::user(format!(
        "The scratchpad rejected that. Error:\n{}\n\nReply with corrected CLIF only.",
        truncate(error, 2000)
    ))
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = n;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

fn strip_think(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("<think>") {
        out.push_str(&rest[..i]);
        match rest[i..].find("</think>") {
            Some(j) => rest = &rest[i + j + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    // Some servers emit the closing tag only.
    if let Some(k) = out.rfind("</think>") {
        out = out[k + "</think>".len()..].to_string();
    }
    out
}

/// Body of the first ```…``` block, if there is one.
fn fenced_block(s: &str) -> Option<&str> {
    let start = s.find("```")?;
    let after = &s[start + 3..];
    let nl = after.find('\n')?;
    let body = &after[nl + 1..];
    let end = body.find("```").unwrap_or(body.len());
    Some(&body[..end])
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| env::var(k).ok())
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    temperature: f32,
    max_tokens: u32,
    messages: &'a [Msg],
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_prefers_the_fenced_block() {
        let reply = "Sure.\n```clif\nfunction %main() -> i64 {\nblock0:\n    return\n}\n```\nDone.";
        assert!(extract_clif(reply).starts_with("function %main()"));
        assert!(!extract_clif(reply).contains("Done."));
    }

    #[test]
    fn extract_drops_reasoning_and_prose() {
        let reply = "<think>I should use iconst\nfunction %nope() {}</think>\nHere:\nfunction %main() -> i64 {\nblock0:\n    v0 = iconst.i64 1\n    return v0\n}";
        let code = extract_clif(reply);
        assert!(code.starts_with("function %main()"), "{code}");
        assert!(!code.contains("%nope"));
    }

    #[test]
    fn extract_handles_a_dangling_close_tag() {
        let reply = "thinking out loud</think>function %main() {\nblock0:\n    return\n}";
        assert!(extract_clif(reply).starts_with("function %main()"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 8), "abc");
        assert_eq!(truncate("µµµµ", 3), "µ…");
    }

    #[test]
    fn openai_key_only_goes_to_openai() {
        assert!(is_openai_host("https://api.openai.com/v1"));
        assert!(is_openai_host("https://API.openai.com:443/v1/"));
        assert!(!is_openai_host("http://192.168.1.10:8080/v1"));
        assert!(!is_openai_host("https://api.openai.com.evil.example/v1"));
        assert!(!is_openai_host("http://127.0.0.1:11434/v1"));
    }
}
