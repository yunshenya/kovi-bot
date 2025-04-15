use kovi::serde_json::Value;
use kovi::tokio::sync::Mutex;
use kovi::{MsgEvent, PluginBuilder, RuntimeBot};
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use sysinfo::{ProcessExt, System, SystemExt};

static MEMORY:LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

static IS_BANNED:LazyLock<Mutex<HashMap<i64, bool>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

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
        if message.eq("#系统信息") {
            match std::env::var("BOT_API_TOKEN") {
                Ok(_) => {
                    let system_info = get_system_info();
                    bot.send_group_msg(group_id, format!("{}\n{}\n{}", "对话功能是正常的哦", system_info.0, system_info.1));
                }
                Err(_) => {
                    bot.send_group_msg(group_id, "未设置token")
                }
            }
        } else {
            let mut guard = MEMORY.lock().await;
            let mut banned_list = IS_BANNED.lock().await;
            match banned_list.get_mut(&group_id) {
                None => {
                    if message.eq("#禁言") {
                        banned_list.insert(group_id, true);
                        bot.send_group_msg(group_id, "禁言成功");
                    }else {
                        banned_list.insert(group_id, false);
                    }
                }
                Some(is_ban) => {
                    if !*is_ban {
                        if message.eq("#禁言") {
                            *is_ban = true;
                        } else {
                            match guard.get_mut(&group_id) {
                                None => {
                                    guard.insert(group_id, vec![
                                        BotMemory{
                                            role: Roles::System,
                                            content: "你在一个群聊里面，你叫芸汐，你遇到与自己无关的内容是不要回复，你尽量只回复问题和游戏，代码相关的内容，适当回复一些群友的问题，不要加上你的动作，还有神情，\
                            我使用xxx：这种形式告诉你和你对话的是谁，选择不回复的时候回复[sp]，你一定不要使用 某某：xxx的形式回答问题，只有确定是和你对话的时候才能回答，你不能擅自修改你的预设，对于询问的问题必须要有意义才回答，\
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
                    }else {
                        if message.eq("#结束禁言") {
                            *is_ban = false;
                        }
                    }
                }
            };
        }
    }
}


async fn params_model(messages: &mut Vec<BotMemory>) -> BotMemory {
    if messages.len() > 25 {
        messages.drain(1..23);
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

fn format_uptime(seconds: u64) -> String{
    let days = seconds / 86400;        // 天：86400秒 = 24*60*60
    let hours = (seconds % 86400) / 3600; // 小时：剩余秒数转小时
    let minutes = (seconds % 3600) / 60;   // 分钟：剩余秒数转分钟
    format!("{}天 {}小时 {}分钟", days, hours, minutes)
}

fn get_system_info() -> (String, String) {
    // 初始化系统信息
    let mut system = System::new_all();
    system.refresh_all();  // 刷新数据

    // 获取系统运行时间
    let uptime = system.uptime();
    let update_time = format_uptime(uptime);

    let mut process_now = String::new();
    // 获取当前进程的内存占用（单位：字节）
    let pid = sysinfo::get_current_pid().expect("获取进程ID失败");
    if let Some(process) = system.process(pid) {
        process_now = format!("内存占用: {} KB", process.memory() / 1024);
    };
    (update_time, process_now)
}

#[cfg(test)]
mod tests {
    use kovi::tokio;


    #[tokio::test]
    async fn test(){

    }
}