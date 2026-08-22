//! 私聊近期图片的短期索引。
//!
//! 只保存 OneBot 图片引用和少量消息上下文，不保存图片文件，也不进入长期记忆。

use crate::vision::ImageAttachment;
use kovi::tokio::sync::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const RECENT_IMAGE_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_IMAGES_PER_USER: usize = 8;
const MAX_IMAGE_SESSIONS: usize = 512;
const MAX_CAPTION_CHARS: usize = 180;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentPrivateImage {
    pub(crate) message_id: i32,
    pub(crate) ordinal: usize,
    pub(crate) caption: String,
    pub(crate) attachment: ImageAttachment,
    stored_at: Instant,
}

#[derive(Default)]
struct RecentImageSession {
    images: VecDeque<RecentPrivateImage>,
    last_access: Option<Instant>,
}

static RECENT_PRIVATE_IMAGES: LazyLock<Mutex<HashMap<i64, RecentImageSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) async fn remember_private_images(
    user_id: i64,
    message_id: i32,
    images: &[ImageAttachment],
    caption: &str,
) {
    if message_id <= 0 || images.is_empty() {
        return;
    }

    let now = Instant::now();
    let mut sessions = RECENT_PRIVATE_IMAGES.lock().await;
    prune_sessions(&mut sessions, now);
    if !sessions.contains_key(&user_id) && sessions.len() >= MAX_IMAGE_SESSIONS {
        remove_oldest_session(&mut sessions);
    }
    let session = sessions.entry(user_id).or_default();
    session.last_access = Some(now);
    let caption = truncate_chars(caption.trim(), MAX_CAPTION_CHARS);

    // 逆序压入队首，使同一条消息中的图片仍保持原始顺序。
    for (index, attachment) in images.iter().enumerate().rev() {
        session
            .images
            .retain(|image| image.attachment.key != attachment.key);
        session.images.push_front(RecentPrivateImage {
            message_id,
            ordinal: index + 1,
            caption: caption.clone(),
            attachment: attachment.clone(),
            stored_at: now,
        });
    }
    session.images.truncate(MAX_IMAGES_PER_USER);
}

pub(crate) async fn recent_private_images(
    user_id: i64,
    excluded_message_ids: &[i32],
) -> Vec<RecentPrivateImage> {
    let now = Instant::now();
    let mut sessions = RECENT_PRIVATE_IMAGES.lock().await;
    prune_sessions(&mut sessions, now);
    let Some(session) = sessions.get_mut(&user_id) else {
        return Vec::new();
    };
    session.last_access = Some(now);
    session
        .images
        .iter()
        .filter(|image| !excluded_message_ids.contains(&image.message_id))
        .cloned()
        .collect()
}

pub(crate) async fn forget_private_message_images(user_id: i64, message_id: i32) {
    let mut sessions = RECENT_PRIVATE_IMAGES.lock().await;
    let Some(session) = sessions.get_mut(&user_id) else {
        return;
    };
    session
        .images
        .retain(|image| image.message_id != message_id);
    if session.images.is_empty() {
        sessions.remove(&user_id);
    }
}

pub(crate) async fn forget_private_user_images(user_id: i64) {
    RECENT_PRIVATE_IMAGES.lock().await.remove(&user_id);
}

fn prune_sessions(sessions: &mut HashMap<i64, RecentImageSession>, now: Instant) {
    sessions.retain(|_, session| {
        session
            .images
            .retain(|image| now.duration_since(image.stored_at) < RECENT_IMAGE_TTL);
        !session.images.is_empty()
            && session
                .last_access
                .is_some_and(|last_access| now.duration_since(last_access) < RECENT_IMAGE_TTL)
    });

    while sessions.len() > MAX_IMAGE_SESSIONS {
        remove_oldest_session(sessions);
    }
}

fn remove_oldest_session(sessions: &mut HashMap<i64, RecentImageSession>) {
    let Some(oldest_user) = sessions
        .iter()
        .min_by_key(|(_, session)| session.last_access)
        .map(|(user_id, _)| *user_id)
    else {
        return;
    };
    sessions.remove(&oldest_user);
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::{forget_private_message_images, recent_private_images, remember_private_images};
    use crate::vision::ImageAttachment;

    fn image(key: &str) -> ImageAttachment {
        ImageAttachment {
            key: key.to_string(),
            file: Some(format!("{key}.png")),
            url: None,
        }
    }

    #[test]
    fn remembers_images_newest_first_and_can_exclude_current_batch() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let user_id = 8_600_001;
                remember_private_images(user_id, 11, &[image("first"), image("second")], "两张图")
                    .await;
                remember_private_images(user_id, 12, &[image("latest")], "最后一张").await;

                let recent = recent_private_images(user_id, &[]).await;
                assert_eq!(recent[0].message_id, 12);
                assert_eq!(recent[1].attachment.key, "first");
                assert_eq!(recent[2].attachment.key, "second");

                let previous = recent_private_images(user_id, &[12]).await;
                assert_eq!(previous.len(), 2);
                assert!(previous.iter().all(|image| image.message_id == 11));
            });
    }

    #[test]
    fn resending_the_same_image_moves_it_to_the_latest_message() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let user_id = 8_600_002;
                remember_private_images(user_id, 21, &[image("same")], "旧说明").await;
                remember_private_images(user_id, 22, &[image("same")], "新说明").await;

                let recent = recent_private_images(user_id, &[]).await;
                assert_eq!(recent.len(), 1);
                assert_eq!(recent[0].message_id, 22);
                assert_eq!(recent[0].caption, "新说明");
            });
    }

    #[test]
    fn recalled_message_is_removed_from_recent_images() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let user_id = 8_600_003;
                remember_private_images(user_id, 31, &[image("recalled")], "会撤回").await;
                forget_private_message_images(user_id, 31).await;
                assert!(recent_private_images(user_id, &[]).await.is_empty());
            });
    }
}
