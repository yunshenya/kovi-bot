use kovi::RuntimeBot;
use kovi::serde_json::Value;
use kovi::tokio::sync::{Mutex, MutexGuard};
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

static MEMORY: LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static IS_BANNED: LazyLock<Mutex<HashMap<i64, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static PRIVATE_MESSAGE_MEMORY: LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Roles {
    System,
    User,
    Assistant,
}

#[derive(Debug, Serialize)]
pub struct BotMemory {
    pub(crate) role: Roles,
    pub(crate) content: String,
}

#[derive(Debug, Serialize)]
struct ModelConf<'a> {
    model: &'a str,
    messages: &'a Vec<BotMemory>,
    stream: bool,
    temperature: f32,
}

pub async fn control_model(
    guard: &mut MutexGuard<'_, HashMap<i64, Vec<BotMemory>>>,
    group_id: i64,
    bot: Arc<RuntimeBot>,
    nickname: String,
    message: &str,
) {
    match guard.get_mut(&group_id) {
        None => {
            guard.insert(
                group_id,
                vec![
                    BotMemory {
                        role: Roles::System,
                        content: crate::config::get().prompt().system_prompt().to_string(),
                    },
                    BotMemory {
                        role: Roles::User,
                        content: format!("{}:{}", nickname, message),
                    },
                ],
            );
            if let Some(vec) = guard.get_mut(&group_id) {
                let model = params_model(vec).await;
                if !model.content.contains("[sp]") {
                    bot.send_group_msg(group_id, &model.content);
                };
                vec.push(BotMemory {
                    role: Roles::Assistant,
                    content: model.content,
                })
            };
        }
        Some(vec) => {
            vec.push(BotMemory {
                role: Roles::User,
                content: format!("{}:{}", nickname, message),
            });
            let resp = params_model(vec).await;
            if !resp.content.contains("[sp]") {
                bot.send_group_msg(group_id, &resp.content);
            };
            vec.push(resp);
        }
    }
}
pub async fn params_model(messages: &mut Vec<BotMemory>) -> BotMemory {
    let server_config = crate::config::get().server_config();
    if messages.len() > 9 {
        messages.drain(1..3);
    };
    let bot_conf = ModelConf {
        model: server_config.model_name(),
        messages,
        stream: false,
        temperature: 0.7,
    };
    let mut header = HeaderMap::new();
    let token = std::env::var("BOT_API_TOKEN").expect("BOT_API_TOKEN must be set");
    header.insert(AUTHORIZATION, format!("Bearer {}", token).parse().unwrap());
    header.insert(CONTENT_TYPE, "application/json".parse().unwrap());
    let client = Client::new();
    let resp = client
        .post(server_config.url())
        .headers(header)
        .json(&bot_conf)
        .send()
        .await
        .unwrap();
    let text = resp.json::<Value>().await.unwrap();
    let bot_content = text
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("余额不足或者文档有更改")
        .trim()
        .replace("芸汐：", "")
        .to_string();
    BotMemory {
        role: Roles::Assistant,
        content: bot_content,
    }
}

pub fn instance_is_ban() -> &'static Mutex<HashMap<i64, bool>> {
    &IS_BANNED
}

pub fn get_memory() -> &'static Mutex<HashMap<i64, Vec<BotMemory>>> {
    &MEMORY
}

pub fn get_private_message_memory() -> &'static Mutex<HashMap<i64, Vec<BotMemory>>> {
    &PRIVATE_MESSAGE_MEMORY
}
