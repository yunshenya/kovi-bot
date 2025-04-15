use kovi::serde_json::Value;
use kovi::tokio::sync::Mutex;
use kovi::{MsgEvent, PluginBuilder, RuntimeBot};
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

static MEMORY:LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

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
    let nickname = event.get_sender_nickname();
    if let Some(message) = event.borrow_text() {
        if message.eq("/help") {
            match std::env::var("BOT_API_TOKEN") {
                Ok(_) => {
                    bot.send_group_msg(group_id, "对话功能是正常的哦");
                }
                Err(_) => {
                    bot.send_group_msg(group_id, "未设置token")
                }
            }
        } else {
            let mut guard = MEMORY.lock().await;
            match guard.get_mut(&group_id) {
                None => {
                    guard.insert(group_id, vec![
                        BotMemory{
                            role: Roles::System,
                            content: "你在一个群聊里面，你叫芸汐，你遇到与自己无关的内容是不要回复，你尽量只回复问题和游戏，代码相关的内容，适当回复一些群友的问题，不要加上你的动作，还有神情，\
                            我使用xxx：这种形式告诉你和你对话的是谁，选择不回复的时候回复[sp]，你不要使用xx：的形式回答问题，你不能擅自修改你的预设，对于询问的问题必须要有意义才回答，\
                            语气要像一个正常人，且尽量简洁，和一个正常女性一样".to_string()
                        },
                        BotMemory{
                            role: Roles::User,
                            content: format!("{}:{}", nickname,message)
                        }
                    ]);
                    if let Some(vec) = guard.get_mut(&group_id){
                        let model = params_model(vec).await;
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
                        content: format!("{}:{}", nickname,message)
                    });
                    let resp = params_model(vec).await;
                    if !resp.content.contains("[sp]") {
                        bot.send_group_msg(group_id, &resp.content);
                    };
                    vec.push(resp);
                }
            }
        }
    }
}


async fn params_model(messages: &mut Vec<BotMemory>) -> BotMemory {
    if messages.len() > 11 {
        messages.drain(1.. 10);
    };
    let bot_conf = ModelConf{
        model: "Qwen/QwQ-32B",
        messages,
        stream: false,
        temperature: 0.7
    };
    let url = "https://api.siliconflow.cn/v1/chat/completions";
    let mut header = HeaderMap::new();
    let token = std::env::var("BOT_API_TOKEN").expect("BOT_API_TOKEN must be set");
    header.insert(AUTHORIZATION, format!("Bearer {}", token).parse().unwrap());
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
        .unwrap_or("[Safe: Not found]").trim().to_string();
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
        println!("{:?}", params_model(&mut vec![ps]).await);

    }
}