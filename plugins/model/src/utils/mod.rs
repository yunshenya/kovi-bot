mod system_info;

pub use crate::utils::system_info::system_info_get;

#[macro_export]
macro_rules! register_chat_function {
    ($(($register_name:ident, $function_name:ident)),* $(,)*) => {
        let bot_shore = kovi::PluginBuilder::get_runtime_bot();
        $(let $register_name = {
            let bot = std::sync::Arc::clone(&bot_shore);
            move |event| {
                let bot = std::sync::Arc::clone(&bot);
                async move {
                    $function_name(event, bot).await;
                }
            }
        };)*
    }
}
