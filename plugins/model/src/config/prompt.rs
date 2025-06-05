use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Prompt {
    system_prompt: String,
    private_prompt: String,
}

impl Prompt {
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub fn private_prompt(&self) -> &str {
        &self.private_prompt
    }
}
