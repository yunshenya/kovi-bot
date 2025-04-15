use std::collections::HashMap;
use kovi::{MsgEvent, PluginBuilder, RuntimeBot};
use std::sync::{Arc, LazyLock};
use kovi::serde_json::Value;
use kovi::tokio::sync::Mutex;
use reqwest::Client;
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;

static MEMORY:LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[kovi::plugin]
async fn main() {
    let bot_shore = PluginBuilder::get_runtime_bot();
    let group_message = {
        let bot = bot_shore.clone();
        move |event|{
            let bot = bot.clone();
            async move{
                group_message_event(event, bot).await;
            }
        }
    };
    PluginBuilder::on_group_msg(group_message);
}

async fn group_message_event(event: Arc<MsgEvent>, bot: Arc<RuntimeBot>){
    let group_id = event.group_id.unwrap();
    if let Some(message) = event.borrow_text() {
        let mut guard = MEMORY.lock().await;
        match guard.get_mut(&group_id) {
            None => {
                guard.insert(group_id, vec![
                    BotMemory{
                        role: Roles::System,
                        content: "你现在在一个群里面，如果不想回复就用[sp],你是一个叫芸汐的女孩子，患有失忆症，记不住任何对话".to_string()
                    },
                    BotMemory{
                        role: Roles::User,
                        content: message.to_string()
                    }
                ]);
                if let Some(vec) = guard.get_mut(&group_id){
                    let model = params_model(&vec).await;
                    if !model.content.contains("[sp]") {
                        bot.send_group_msg(group_id, &model.content);
                    };
                    vec.push(
                        BotMemory{
                            role: Roles::Assistant,
                            content: model.content
                        }
                    )
                };
            }
            Some(vec) => {
                vec.push(BotMemory{
                    role: Roles::User,
                    content:message.to_string()
                });
                let resp = params_model(&vec).await;
                if resp.content.contains("[sp]") {
                    bot.send_group_msg(group_id, &resp.content);
                };
                vec.push(resp);
            }
        }
    }
}


#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Roles{
    System,
    User,
    Assistant
}

#[derive(Debug, Serialize)]
struct BotMemory{
    role: Roles,
    content: String,
}

#[derive(Debug, Serialize)]
struct ModelConf<'a>{
    model: &'a str,
    messages: &'a Vec<BotMemory>,
    stream: bool,
    temperature: f32
}

async fn params_model(messages: &Vec<BotMemory>) -> BotMemory {
    let bot_conf = ModelConf{
        model: "Qwen/QwQ-32B",
        messages,
        stream: false,
        temperature: 0.7
    };
    let url = "https://api.siliconflow.cn/v1/chat/completions";
    let mut header = HeaderMap::new();
    header.insert(AUTHORIZATION, "Bearer sk-pwntkzsdxsutunxzbdwfauvukmufhmfycnrlptzwqhsgujqu".parse().unwrap());
    header.insert(CONTENT_TYPE, "application/json".parse().unwrap());
    let client = Client::new();
    let resp = client.post(url)
        .headers(header)
        .json(&bot_conf).send().await.unwrap();
    let text = resp.json::<Value>().await.unwrap();
    let bot_content = text.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("[Safe: Not found]").to_string();
    BotMemory{
        role: Roles::Assistant,
        content: bot_content,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use kovi::tokio;

    #[tokio::test]
    async fn test(){
        let ps = BotMemory{
            role: Roles::User,
            content: "你会什么".to_string()
        };
        println!("{:?}", params_model(&vec![ps]).await);

    }
}