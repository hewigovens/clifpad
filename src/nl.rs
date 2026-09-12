//! OpenAI-compatible LLM client plus a tiny offline translator.

use std::env;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::lang;
use crate::state::State;

const SYSTEM_PROMPT: &str = r#"You are the code generator for a Smart REPL.
Translate the user's request into MiniLang — a tiny, safe language that will be
JIT-compiled with Cranelift and executed immediately.

Respond with MiniLang ONLY. No markdown, no commentary, no Rust types.

MiniLang syntax:
  // comments
  let x = <expr>
  x = <expr>
  if <expr> { ... } else { ... }
  while <expr> { ... }
  return <expr>
  fn name(a, b) { ... }
  [1, 2, 3]
  "strings"  true  false  null
  operators: + - * / % == != < <= > >= && || ?? ! 

Built-in functions (these talk to the persistent REPL state):
  get("name")                 -> value or null
  set("name", value)          -> value  (writes REPL state)
  del("name")                 -> bool
  keys()                      -> list of variable names
  print(...)                  -> null   (prints to the console)
  len(x)  push(list, v)  list(...)
  str(x)  int(x)  float(x)  typeof(x)
  error("message")            -> abort with an error

Rules:
  - Do NOT emit Rust types, `fn run`, `State`, `Value`, `Result`, or `Ok`.
  - Persist data the user should remember with set()/get(), not only locals.
  - Use `get("x") ?? 0` when incrementing a possibly missing variable.
  - Prefer a last expression as the result (no semicolon on the final line).
  - Keep programs short. No filesystem, network, or unsafe operations.
  - If the user writes MiniLang already, echo it unchanged (cleaned up).

Examples:
  User: set n to 1
  set("n", 1)

  User: increment n
  set("n", (get("n") ?? 0) + 1)

  User: add 2 and 3
  2 + 3

  User: make a list of 1, 2, 3 called xs
  set("xs", [1, 2, 3])

  User: define add and compute add(10, 32)
  fn add(a, b) { a + b }
  add(10, 32)
"#;

#[derive(Clone, Debug)]
pub struct LlmClient {
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
    pub offline: bool,
}

impl LlmClient {
    pub fn from_env() -> Self {
        let _ = dotenvy::dotenv();
        let api_key = first_env(&["SMART_REPL_API_KEY", "OPENAI_API_KEY"]);
        let base_url = first_env(&["SMART_REPL_BASE_URL", "OPENAI_BASE_URL"])
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let model = first_env(&["SMART_REPL_MODEL", "OPENAI_MODEL"])
            .unwrap_or_else(|| "gpt-4o-mini".to_string());
        let offline_flag = first_env(&["SMART_REPL_OFFLINE"])
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        let offline = offline_flag || api_key.is_none();
        Self {
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            offline,
        }
    }

    pub fn describe(&self) -> String {
        if self.offline {
            format!(
                "offline translator (no LLM)  [set OPENAI_API_KEY to enable {}]",
                self.model
            )
        } else {
            format!("{}  @ {}", self.model, self.base_url)
        }
    }

    pub fn generate(
        &self,
        user: &str,
        state: &State,
        last_error: Option<&str>,
    ) -> Result<Generated> {
        if self.offline {
            let code = offline_generate(user).ok_or_else(|| {
                anyhow!(
                    "offline translator did not understand {user:?}. \
                         Write MiniLang directly (e.g. set(\"n\", 1)) or set OPENAI_API_KEY."
                )
            })?;
            return Ok(Generated {
                code,
                source: CodeSource::Offline,
                raw: None,
            });
        }
        let raw = self.chat(user, state, last_error)?;
        let code = lang::extract_code(&raw);
        Ok(Generated {
            code,
            source: CodeSource::Llm,
            raw: Some(raw),
        })
    }

    fn chat(&self, user: &str, state: &State, last_error: Option<&str>) -> Result<String> {
        let key = self
            .api_key
            .as_deref()
            .ok_or_else(|| anyhow!("missing API key"))?;
        let mut user_content = format!(
            "Current REPL state:\n{}\nUser request:\n{user}",
            state.summary()
        );
        if let Some(err) = last_error {
            user_content.push_str(
                "\n\nThe previous MiniLang failed with this error. \
                 Emit a corrected MiniLang program only:\n",
            );
            user_content.push_str(err);
        }

        let req = ChatRequest {
            model: self.model.clone(),
            temperature: 0.1,
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: SYSTEM_PROMPT.into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: user_content,
                },
            ],
        };

        let url = format!("{}/chat/completions", self.base_url);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("build HTTP client")?;
        let response = client
            .post(&url)
            .bearer_auth(key)
            .json(&req)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if !status.is_success() {
            bail_http(status.as_u16(), &body)?;
        }
        let parsed: ChatResponse =
            serde_json::from_str(&body).with_context(|| format!("decode LLM response: {body}"))?;
        let content = parsed
            .choices
            .into_iter()
            .find_map(|c| c.message.content)
            .ok_or_else(|| anyhow!("LLM returned no content"))?;
        Ok(content)
    }
}

fn bail_http(status: u16, body: &str) -> Result<()> {
    Err(anyhow!("LLM HTTP {status}: {}", truncate(body, 800)))
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| env::var(k).ok())
        .filter(|s| !s.is_empty())
}

#[derive(Debug, Clone)]
pub struct Generated {
    pub code: String,
    pub source: CodeSource,
    pub raw: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeSource {
    Llm,
    Offline,
    Direct,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    temperature: f32,
    messages: Vec<ChatMessage>,
}

#[derive(Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
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

/// Tiny pattern-based fallback so the prototype works without an API key.
pub fn offline_generate(input: &str) -> Option<String> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();

    if let Some(rest) = strip_ci(&lower, raw, "set ") {
        // set NAME to VALUE
        if let Some((name, value)) = split_ci(rest, " to ") {
            let name = ident(name)?;
            let value = expr_or_quote(value.trim());
            return Some(format!("set(\"{name}\", {value})"));
        }
    }
    if let Some(name) = strip_ci(&lower, raw, "increment ") {
        let name = ident(name)?;
        return Some(format!("set(\"{name}\", (get(\"{name}\") ?? 0) + 1)"));
    }
    if let Some(name) = strip_ci(&lower, raw, "decrement ") {
        let name = ident(name)?;
        return Some(format!("set(\"{name}\", (get(\"{name}\") ?? 0) - 1)"));
    }
    if let Some(rest) = strip_ci(&lower, raw, "add ") {
        if let Some((amount, name)) = split_ci(rest, " to ") {
            let name = ident(name)?;
            let amount = expr_or_quote(amount.trim());
            return Some(format!(
                "set(\"{name}\", (get(\"{name}\") ?? 0) + {amount})"
            ));
        }
        if let Some((a, b)) = split_ci(rest, " and ") {
            return Some(format!(
                "{} + {}",
                expr_or_quote(a.trim()),
                expr_or_quote(b.trim())
            ));
        }
    }
    if let Some(name) = strip_prefix_any(raw, &["what is ", "show ", "print ", "get "]) {
        let name = ident(name)?;
        return Some(format!("get(\"{name}\")"));
    }
    if let Some(rest) = strip_ci(&lower, raw, "make a list ") {
        // make a list NAME with 1, 2, 3
        let rest = rest.strip_prefix("called ").unwrap_or(rest);
        if let Some((name, items)) = split_ci(rest, " with ") {
            let name = ident(name)?;
            let items = items.trim();
            return Some(format!("set(\"{name}\", [{items}])"));
        }
    }
    if let Some((name, value)) = raw.split_once('=') {
        if ident(name).is_some() && !name.contains('"') {
            let name = ident(name)?;
            return Some(format!("set(\"{name}\", {})", value.trim()));
        }
    }
    None
}

fn strip_ci<'a>(lower: &str, raw: &'a str, prefix: &str) -> Option<&'a str> {
    if lower.starts_with(prefix) {
        Some(raw[prefix.len()..].trim())
    } else {
        None
    }
}

fn strip_prefix_any<'a>(raw: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    let lower = raw.to_ascii_lowercase();
    for p in prefixes {
        if lower.starts_with(p) {
            return Some(raw[p.len()..].trim());
        }
    }
    None
}

fn split_ci<'a>(s: &'a str, sep: &str) -> Option<(&'a str, &'a str)> {
    let lower = s.to_ascii_lowercase();
    let sep_l = sep.to_ascii_lowercase();
    let idx = lower.find(&sep_l)?;
    Some((&s[..idx], &s[idx + sep.len()..]))
}

fn ident(s: &str) -> Option<String> {
    let s = s.trim().trim_matches(|c| c == '"' || c == '\'' || c == '.');
    if s.is_empty() {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(s.to_string())
    } else {
        None
    }
}

fn expr_or_quote(s: &str) -> String {
    let t = s.trim().trim_end_matches('.');
    if t.parse::<i64>().is_ok() || t.parse::<f64>().is_ok() {
        return t.to_string();
    }
    if t.starts_with('"') || t == "true" || t == "false" || t == "null" || t.starts_with('[') {
        return t.to_string();
    }
    if t.chars()
        .all(|c| c.is_ascii_digit() || "+-*/%(). ".contains(c))
    {
        return t.to_string();
    }
    format!("{t:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_increment() {
        assert_eq!(
            offline_generate("increment n").unwrap(),
            r#"set("n", (get("n") ?? 0) + 1)"#
        );
    }

    #[test]
    fn offline_set_to() {
        assert_eq!(offline_generate("set n to 1").unwrap(), r#"set("n", 1)"#);
    }
}
