//! # 表情包素材库（出口）
//!
//! [`crate::sticker_memory`] 管的是"她看懂别人发的表情"；这里管的是"**她能发出去**
//! 的表情包"。素材全部来自 `qq_sticker.dir` 里运维自己放的图片：文件名（去掉扩展名）
//! 就是标签，例如 `无语又想笑.gif` 的标签是 `无语又想笑`。她不缓存、不转发聊天里
//! 别人的图，所以这条链路不改变任何隐私口径。
//!
//! 发送形态是 OneBot 的 `image` 段，`file` 用 `base64://`：这样不必要求 NapCat 与
//! 机器人共享同一个文件系统（语音那条链路需要 `staging_dir` / `napcat_staging_dir`
//! 两份路径映射，图片没有这个必要，也少一处部署会配错的地方）。
//!
//! **不发商城表情（`mface`）**：它的 `key` 是服务端下发、绑定具体资源的，重发会被
//! QQ 拒；而 NapCat 收到商城表情时本来就已经转成 `image` 段交给我们了。以图片段发出
//! 去的效果在聊天里与表情包一致，只是不算"商城表情"那条 UI。

use crate::config::QqStickerConfig;
use base64::Engine;
use kovi::Message;
use kovi::bot::message::Segment;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// 认得的图片扩展名。NapCat 会把它们交给 QQ 上传，其余格式一律不收录。
const SUPPORTED_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
/// 标签长度上限（字符）。标签会进提示词，不能由文件名无限拉长。
const MAX_LABEL_CHARS: usize = 64;
/// 目录递归深度上限。素材目录是给运维丢文件的，不该有人把整块盘塞进来。
const MAX_SCAN_DEPTH: usize = 3;

/// 管理员的素材库命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StickerLibraryCommand {
    /// `#表情列表`
    List,
    /// `#发表情 <标签>`
    Send { label: String },
    /// 命令形状对但缺标签，或标签为空。
    Invalid,
}

/// 解析素材库命令；不是这两条命令时返回 `None`。
pub(crate) fn parse_command(message: &str) -> Option<StickerLibraryCommand> {
    let text = message.trim();
    if text == "#表情列表" {
        return Some(StickerLibraryCommand::List);
    }
    let rest = text.strip_prefix("#发表情")?;
    // `#发表情包` 之类的词不该被当成命令。
    if !rest.is_empty() && !rest.starts_with([' ', '\t', '　']) {
        return None;
    }
    let label = rest.trim();
    if label.is_empty() {
        return Some(StickerLibraryCommand::Invalid);
    }
    Some(StickerLibraryCommand::Send {
        label: label.to_string(),
    })
}

/// 解析结果：素材库当前状态的一句话说明，给命令回执用。
pub(crate) fn command_help() -> String {
    "格式：#表情列表 看有哪些表情包；#发表情 标签 让她发一张。".to_string()
}

/// 命令回执：当前可用的标签清单（空库时说明该往哪个目录放素材）。
pub(crate) fn library_listing_reply() -> String {
    let labels = available_labels();
    if labels.is_empty() {
        return format!(
            "表情包素材库现在是空的，往 {} 里放几张图片再试（文件名就是标签）。",
            crate::config::sticker_library_path().display()
        );
    }
    // 命令回执在 QQ 里发，太长会刷屏：只列前若干个，其余给个数。
    const REPLY_LABELS: usize = 24;
    let mut listing = labels
        .iter()
        .take(REPLY_LABELS)
        .cloned()
        .collect::<Vec<_>>()
        .join("、");
    if labels.len() > REPLY_LABELS {
        listing.push_str(&format!(
            "（共 {} 个，这里只列了前 {} 个）",
            labels.len(),
            REPLY_LABELS
        ));
    }
    format!("现在有 {} 张表情：{listing}", labels.len())
}

/// 命令回执：这个标签库里没有。
pub(crate) fn missing_label_reply(label: &str) -> String {
    let labels = available_labels();
    if labels.is_empty() {
        return format!(
            "素材库里还没有表情。往 {} 里放几张图片再试（文件名就是标签）。",
            crate::config::sticker_library_path().display()
        );
    }
    let mut listing = labels
        .iter()
        .take(20)
        .cloned()
        .collect::<Vec<_>>()
        .join("、");
    if labels.len() > 20 {
        listing.push_str("……");
    }
    format!("素材库里没有“{label}”这张表情。可用的有：{listing}")
}

/// 一条只带这张表情的消息，交给 tracked send 直发（管理员命令的兜底路径）。
pub(crate) fn build_sticker_message(raw_label: &str) -> Option<Message> {
    let segment = build_sticker_segment(raw_label)?;
    let mut message = Message::new();
    message.push(segment);
    Some(message)
}

/// 素材目录的准备（幂等）：缺了就补建。
///
/// 与 `admin.annotation_dir` 同一条约定——部署完就该能直接把素材丢进去，不该先让人
/// 手工 `mkdir`（`scp` 到不存在的目录会直接失败）。关闭配置时不碰磁盘。
pub(crate) fn ensure_directory() -> anyhow::Result<PathBuf> {
    let config = crate::config::get();
    let dir = crate::config::sticker_library_path();
    if !config.qq_sticker().enabled() {
        return Ok(dir);
    }
    ensure_directory_exists(&dir).map_err(|error| {
        anyhow::anyhow!(
            "创建表情包素材库目录失败 (目录: {}): {error}",
            dir.display()
        )
    })?;
    Ok(dir)
}

/// `create_dir_all` 的薄包装，返回"这次是不是真的建了"（便于只在该打日志时打）。
fn ensure_directory_exists(dir: &Path) -> std::io::Result<bool> {
    if dir.is_dir() {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    Ok(true)
}

/// 素材目录（无论配置开没开都解析得出来）。
pub(crate) fn directory_path() -> PathBuf {
    crate::config::sticker_library_path()
}

/// 让下一次访问重新扫目录。
///
/// 后台传完/删完素材后必须立刻调用：默认 `rescan_secs` 是 30 秒，不这样的话
/// "刚传完就去 QQ 里试"会撞上还没过期的旧索引，看起来像没传上去。
pub(crate) fn invalidate_index() {
    if let Ok(mut index) = INDEX.lock() {
        index.scanned_at = None;
    }
}

/// 素材库入库/删除时的失败原因。
///
/// 区分"请求本身不合法"（后台回 400，把原因原样告诉人）与"机器这一侧不行"
/// （IO/权限，回 500 或按 `admin.annotation_dir` 的约定解释成 400）。
#[derive(Debug)]
pub(crate) enum StickerStoreError {
    Invalid(String),
    Io(String),
}

/// 素材库里的一张图（后台列表用它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StickerFileEntry {
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) bytes: u64,
    pub(crate) modified_unix_secs: Option<u64>,
}

/// 当前素材清单（按标签、再按文件名排序），带上体积与修改时间。
///
/// 目录不存在时返回空列表——后台据此显示"还没有素材"，而不是报错。
pub(crate) fn listing() -> Vec<StickerFileEntry> {
    let dir = directory_path();
    let mut entries = Vec::new();
    for (label, files) in scan_directory(&dir, crate::config::get().qq_sticker().max_files()) {
        for path in files {
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            let metadata = std::fs::metadata(&path).ok();
            entries.push(StickerFileEntry {
                name: name.to_string(),
                label: label.clone(),
                bytes: metadata.as_ref().map_or(0, std::fs::Metadata::len),
                modified_unix_secs: metadata
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs()),
            });
        }
    }
    entries
}

/// 后台/脚本传来的一张图，落到素材目录里。
///
/// 返回实际写下的文件名（标签一样时自动加编号，不会覆盖已有素材）。落盘是
/// "临时文件 + rename"的原子替换，扫目录的一方永远看不到写了一半的图。
pub(crate) fn store_upload(label: &str, bytes: &[u8]) -> Result<String, StickerStoreError> {
    let config = crate::config::get();
    let sticker = config.qq_sticker();
    let stem = file_stem_for_label(label)?;
    if bytes.is_empty() {
        return Err(StickerStoreError::Invalid("上传内容是空的".to_string()));
    }
    let limit = sticker.max_file_bytes();
    if bytes.len() as u64 > limit {
        return Err(StickerStoreError::Invalid(format!(
            "单张表情不能超过 {} KB（当前 {} KB）",
            limit / 1024,
            bytes.len().div_ceil(1024)
        )));
    }
    let extension = extension_for_image(bytes).ok_or_else(|| {
        StickerStoreError::Invalid(
            "只支持 PNG / JPEG / GIF / WebP / BMP；文件内容看起来不是图片".to_string(),
        )
    })?;

    let dir = directory_path();
    // 上传是管理员的显式动作：即使 `qq_sticker` 还没打开，也允许先把素材备好。
    ensure_directory_exists(&dir).map_err(|error| {
        StickerStoreError::Io(format!("创建素材目录失败 ({}): {error}", dir.display()))
    })?;
    let existing = scan_directory(&dir, sticker.max_files().saturating_add(1));
    let count = existing.values().map(Vec::len).sum::<usize>();
    if count >= sticker.max_files() {
        return Err(StickerStoreError::Invalid(format!(
            "素材数量已达上限（{} 张），先删几张或调大 qq_sticker.max_files",
            sticker.max_files()
        )));
    }

    let name = next_available_name(&dir, &stem, extension);
    let path = dir.join(&name);
    let temp = dir.join(format!(".{name}.upload-{}", std::process::id()));
    std::fs::write(&temp, bytes).map_err(|error| {
        StickerStoreError::Io(format!("写入素材失败 ({}): {error}", temp.display()))
    })?;
    if let Err(error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(StickerStoreError::Io(format!(
            "保存素材失败 ({}): {error}",
            path.display()
        )));
    }
    invalidate_index();
    println!(
        "[INFO] 表情包素材已入库: {} ({} 字节)",
        path.display(),
        bytes.len()
    );
    Ok(name)
}

/// 删除一张素材（只接受素材目录里的裸文件名）。
pub(crate) fn delete_upload(name: &str) -> Result<(), StickerStoreError> {
    let path = validated_file_path(name)?;
    std::fs::remove_file(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => StickerStoreError::Invalid(format!("素材不存在: {name}")),
        _ => StickerStoreError::Io(format!("删除素材失败 ({}): {error}", path.display())),
    })?;
    invalidate_index();
    println!("[INFO] 表情包素材已删除: {}", path.display());
    Ok(())
}

/// 读取一张素材（后台缩略图用）。
pub(crate) fn read_upload(name: &str) -> Result<Vec<u8>, StickerStoreError> {
    let path = validated_file_path(name)?;
    std::fs::read(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => StickerStoreError::Invalid(format!("素材不存在: {name}")),
        _ => StickerStoreError::Io(format!("读取素材失败 ({}): {error}", path.display())),
    })
}

/// 裸文件名 → 目录内的路径。
///
/// 与标注那边的批次名同一条思路：拒绝分隔符、`..`、隐藏文件与超长名字之后，
/// `join` 的结果必然是该目录的直接子项，请求拼不出目录之外的路径。
fn validated_file_path(name: &str) -> Result<PathBuf, StickerStoreError> {
    let name = name.trim();
    let reject =
        || StickerStoreError::Invalid("文件名不合法：只接受素材目录内的图片文件名".to_string());
    if name.is_empty() || name.len() > 255 || name.starts_with('.') {
        return Err(reject());
    }
    if name.contains(['/', '\\']) || name.contains("..") {
        return Err(reject());
    }
    if !is_supported_image_path(Path::new(name)) {
        return Err(reject());
    }
    Ok(directory_path().join(name))
}

/// 标签 → 文件名主干：只留安全字符，任何输入都拼不出越界的路径。
///
/// 首尾的空格与点一律去掉（`.`、`..`、隐藏文件都不是合法主干），标签里的路径
/// 分隔符、控制字符与 Windows 保留字符一并剔除。
fn file_stem_for_label(label: &str) -> Result<String, StickerStoreError> {
    let cleaned: String = label
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.', '\t']);
    if cleaned.is_empty() {
        return Err(StickerStoreError::Invalid(
            "标签不能为空（去掉空格和非法字符之后就什么都不剩了）".to_string(),
        ));
    }
    Ok(cleaned.chars().take(MAX_LABEL_CHARS).collect())
}

/// 不覆盖已有素材：`开心.png` 已存在就写 `开心-2.png`（编号会被标签解析重新归到
/// 「开心」下，所以标签语义不变）。
fn next_available_name(dir: &Path, stem: &str, extension: &str) -> String {
    let first = format!("{stem}.{extension}");
    if !dir.join(&first).exists() {
        return first;
    }
    for index in 2..=9_999_u32 {
        let candidate = format!("{stem}-{index}.{extension}");
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    // 理论上到不了这里（9998 张同名图）；真到了也别覆盖，用进程号兜一个唯一名。
    format!("{stem}-{}.{extension}", std::process::id())
}

/// 图片内容类型（缩略图响应头用）：按文件头判断，不听扩展名。
pub(crate) fn image_content_type(bytes: &[u8]) -> &'static str {
    match extension_for_image(bytes) {
        Some("png") => "image/png",
        Some("jpg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, Default)]
struct StickerIndex {
    scanned_at: Option<Instant>,
    /// 上一次打日志时的 `(文件数, 标签数)`。默认 30 秒重扫一次，按次打日志会把
    /// 日志刷满，所以只在素材库的状态**变了**的时候打一行。
    logged: Option<(usize, usize)>,
    /// 标签 → 该标签下的素材文件（已排序，便于轮换与测试）。
    labels: BTreeMap<String, Vec<PathBuf>>,
}

static INDEX: LazyLock<Mutex<StickerIndex>> = LazyLock::new(|| Mutex::new(StickerIndex::default()));
/// 每个标签已经用过几次，用来轮换同一标签下的多张图。
static USE_COUNTS: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// "只发一张表情、但那张取不到"时宿主补的一句话。
///
/// 绝不整轮沉默：她把一张表情当成了整条回复，取不到就什么都不发的话，群里看到的
/// 是"她掉线了"（线上 02:16:10 那轮就是这样被丢掉的）。这句话是宿主替她说的，
/// 所以刻意写得短、不含承诺。
pub(crate) fn unavailable_sticker_reply(label: &str) -> String {
    let label = label.trim();
    if label.is_empty() {
        "这张表情我这边暂时没有，先这样回你。".to_string()
    } else {
        format!("「{label}」这张表情我这边没有，先这样回你。")
    }
}

/// 素材库现在能不能用：配置打开、且目录里确实有素材。
///
/// 提示词组装只认这一个判据——关掉配置却仍然告诉模型"你可以发表情包"，只会得到
/// 一条永远发不出去的标记。
pub(crate) fn is_available() -> bool {
    is_available_with(crate::config::get().qq_sticker())
}

fn is_available_with(config: &QqStickerConfig) -> bool {
    config.enabled() && !labels_snapshot().is_empty()
}

/// 全部标签（已排序）。命令回执用它。
pub(crate) fn available_labels() -> Vec<String> {
    labels_snapshot().into_keys().collect()
}

/// 表情包协议：**一句短话，不含清单**。
///
/// 清单不进常驻提示词（2026-09-15 用户口径）：素材一多，每轮都带上它就是白花钱，
/// 而且那些标签只在真要发图的那几轮才有用。所以提示词里只说清"要发就先调
/// `sticker.list` 拿标签"，清单由工具在那一刻给——这也是原先的设计，2026-09-15
/// 中途试过改成常驻，被否掉，这里留一句免得后人再改回去。
///
/// 相册语义留在这一句里（她要认得出相册里那张就是自己）：有人要看照片时她该发那张，
/// 而不是说"那不是我真人的样子"——线上 2026-09-15 13:20 就是这么答的。
///
/// "发不出去就别发、别答应"那类禁止句**不在这里**：按用户口径，这里修的是"她不知道
/// 有什么"（工具随时可查），不是靠禁止句把她的回复按住；标签写错时也不再重写她的回合，
/// 交付层去掉那张图、正文照发。
///
/// **名字写 `sticker_list` 而不是 `sticker.list`**：注册名带点是内部 id，发到 provider
/// 的函数名由 [`crate::model::tool_access::wire_tool_name`] 把点换成下划线（DeepSeek 等
/// 网关按 `^[a-zA-Z0-9_-]+$` 校验，带点直接 400）。提示词里必须写模型真正能调的那个名字，
/// 否则她照着抄也调不到。
pub(crate) const STICKER_PROMPT: &str = "素材库就是你自己的相册（图都是你的）：想发哪张就先调 sticker_list 拿标签，把 [[STICKER 标签]] 写在正文最前面（不展示，正文可留空），标签照抄工具给的、别自己起名字。带你自己名字的标签就是你本人的照片：有人要看你的照片，就把那张发出去，不要说那不是你。";

/// `sticker.list` 的**注册名**（内部 id，带点）。
///
/// 两条链路都要在工具调用结果上认这个名字（"刚查完清单就别再查一遍"），所以只能有一份：
/// 各写一份字面量的话，改注册名时漏掉一处就会退化成"查了又查"。
/// 注意它**不是**给模型看的名字——发到 provider 的函数名要经
/// [`crate::model::tool_access::wire_tool_name`] 把点换成下划线（见 [`STICKER_PROMPT`]）。
pub(crate) const TOOL_NAME: &str = "sticker.list";

/// 这一轮要不要给这段协议：素材库关了或空着就不给（不能让她以为自己有一个当下用不了
/// 的出口），给了就一定是上面那一份。
pub(crate) fn prompt_instruction() -> Option<&'static str> {
    is_available().then_some(STICKER_PROMPT)
}

/// `sticker.list` 工具返回给模型的清单：**全部**标签（`A；B；C`）。
///
/// 这是清单唯一的来源：常驻提示词里只有一句"要发就先调它"（见 [`STICKER_PROMPT`]），
/// 所以它必须给全，不能截断——截断了就等于让她去猜没列出来的那些。
/// 素材库关闭或为空时返回 `None`——那种情况下工具本身也不会下发给模型。
pub(crate) fn tool_listing() -> Option<String> {
    let labels = labels_snapshot();
    if labels.is_empty() {
        return None;
    }
    Some(labels.into_keys().collect::<Vec<_>>().join("；"))
}

fn labels_snapshot() -> BTreeMap<String, Vec<PathBuf>> {
    let config = crate::config::get();
    let config = config.qq_sticker();
    if !config.enabled() {
        return BTreeMap::new();
    }
    INDEX
        .lock()
        .map(|mut index| {
            refresh_if_stale(&mut index, config);
            index.labels.clone()
        })
        .unwrap_or_default()
}

fn refresh_if_stale(index: &mut StickerIndex, config: &QqStickerConfig) {
    let ttl = Duration::from_secs(config.rescan_secs());
    if index
        .scanned_at
        .is_some_and(|scanned_at| scanned_at.elapsed() < ttl)
    {
        return;
    }
    let dir = crate::config::sticker_library_path();
    // 目录缺失就补建（幂等）：运维不该先手工 mkdir 才能把素材丢进去；管理后台热开
    // 这个开关、或者目录被外部删掉时，这里也会自己长回来。
    let created = match ensure_directory_exists(&dir) {
        Ok(created) => created,
        Err(error) => {
            eprintln!(
                "[ERROR] 表情包素材库目录创建失败，她这一轮不会发表情包 (目录: {}): {}",
                dir.display(),
                error
            );
            false
        }
    };
    let labels = scan_directory(&dir, config.max_files());
    let files = labels.values().map(Vec::len).sum::<usize>();
    // 只在状态变了的时候打一行：默认 30 秒重扫一次，按次打会把日志刷满。目录刚建出来
    // 也算变化——那一行正是运维最需要看到的"该往哪儿放"。
    if created || index.logged != Some((files, labels.len())) {
        if files == 0 {
            println!(
                "[INFO] 表情包素材库{}，往里面放几张图片就能用 (目录: {})",
                if created {
                    "目录已创建，现在是空的"
                } else {
                    "为空"
                },
                dir.display()
            );
        } else {
            println!(
                "[INFO] 表情包素材库已加载 {} 张图 / {} 个标签{} (目录: {})",
                files,
                labels.len(),
                if created { "，目录已创建" } else { "" },
                dir.display()
            );
        }
    }
    let logged = Some((files, labels.len()));
    *index = StickerIndex {
        scanned_at: Some(Instant::now()),
        logged,
        labels,
    };
}

/// 扫目录建索引。纯函数，便于测试；`max_files` 是收录文件数上限。
fn scan_directory(dir: &Path, max_files: usize) -> BTreeMap<String, Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files(dir, max_files, 0, &mut files);
    files.sort();
    let mut labels: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for path in files {
        let Some(label) = label_for_path(&path) else {
            continue;
        };
        labels.entry(label).or_default().push(path);
    }
    labels
}

fn collect_files(dir: &Path, max_files: usize, depth: usize, files: &mut Vec<PathBuf>) {
    if files.len() >= max_files || depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    children.sort();
    for path in children {
        if files.len() >= max_files {
            return;
        }
        if path.is_dir() {
            collect_files(&path, max_files, depth + 1, files);
            continue;
        }
        if is_supported_image_path(&path) {
            files.push(path);
        }
    }
}

fn is_supported_image_path(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return false;
    };
    SUPPORTED_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
}

/// 文件路径 → 标签：去扩展名、剥掉尾部编号、限长。
///
/// 隐藏文件、取不出名字的文件（`.DS_Store`、`~$x.png` 这类）与不支持的格式直接跳过。
fn label_for_path(path: &Path) -> Option<String> {
    if !is_supported_image_path(path) {
        return None;
    }
    let stem = path.file_stem()?.to_str()?.trim();
    if stem.is_empty() || stem.starts_with('.') || stem.starts_with("~$") {
        return None;
    }
    let label = strip_trailing_ordinal(stem);
    let label: String = label.chars().take(MAX_LABEL_CHARS).collect();
    let label = label.trim().to_string();
    (!label.is_empty()).then_some(label)
}

/// 剥掉同一标签多张图用的尾部编号：`开心-1`、`开心_2`、`开心 3`、`开心(4)`、
/// `开心（5）` 都归到 `开心`。
///
/// 只在编号前面确实有分隔符时才剥：`39度`、`版本2` 这类名字里的数字是名字的一部分，
/// 剥了反而会撞到别的标签上。
fn strip_trailing_ordinal(stem: &str) -> &str {
    let trimmed = stem.trim_end();
    // 先脱掉 `(4)` 这类包起来的写法，再剥数字，最后去掉数字前面的分隔符。
    let unwrapped = trimmed.trim_end_matches([')', '）', ']', '】']);
    let without_digits = unwrapped.trim_end_matches(|character: char| character.is_ascii_digit());
    if without_digits.len() == unwrapped.len() {
        return trimmed;
    }
    let head = without_digits.trim_end_matches([' ', '-', '_', '.', '(', '（', '[', '【']);
    if head.len() == without_digits.len() || head.trim().is_empty() {
        return trimmed;
    }
    head
}

/// 把模型/命令给的标签落到库里真实存在的标签上。
///
/// 先精确匹配（忽略空白与标点、ASCII 不分大小写），再退一步找唯一包含关系。
/// 仍然不唯一时按"标签更短优先、同长按字典序"取一个——确定性的选择好过随机挑一张
/// 发错。找不到返回 `None`，调用方应当放弃发表情而不是发一张不相干的。
pub(crate) fn resolve_label(raw: &str) -> Option<String> {
    let labels = labels_snapshot();
    resolve_in(labels.keys().map(String::as_str), raw)
}

fn resolve_in<'a>(labels: impl Iterator<Item = &'a str>, raw: &str) -> Option<String> {
    let query = normalize_for_match(raw);
    if query.is_empty() {
        return None;
    }
    let candidates: Vec<&str> = labels.collect();
    if let Some(exact) = candidates
        .iter()
        .find(|label| normalize_for_match(label) == query)
    {
        return Some((*exact).to_string());
    }
    let mut contained: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|label| {
            let normalized = normalize_for_match(label);
            normalized.contains(&query) || query.contains(&normalized)
        })
        .collect();
    contained.sort_by(|left, right| {
        left.chars()
            .count()
            .cmp(&right.chars().count())
            .then_with(|| left.cmp(right))
    });
    contained.first().map(|label| (*label).to_string())
}

/// 匹配用的归一化：去掉空白与常见标点，ASCII 转小写。
fn normalize_for_match(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !character.is_whitespace()
                && !matches!(
                    character,
                    '，' | '。'
                        | '！'
                        | '？'
                        | '、'
                        | '~'
                        | '～'
                        | '!'
                        | '?'
                        | '.'
                        | ','
                        | ':'
                        | '：'
                        | '"'
                        | '\''
                        | '“'
                        | '”'
                        | '‘'
                        | '’'
                        | '('
                        | ')'
                        | '（'
                        | '）'
                )
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// 按标签造一个可发送的图片段；标签不存在、文件读不出来或内容不像图片时返回 `None`。
pub(crate) fn build_sticker_segment(raw_label: &str) -> Option<Segment> {
    let config = crate::config::get();
    let config = config.qq_sticker();
    if !config.enabled() {
        return None;
    }
    let label = resolve_label(raw_label)?;
    let path = next_sticker_file(&label)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!(
                "[WARN] 读取表情包素材失败，本轮不发这张 (文件: {}): {}",
                path.display(),
                error
            );
            return None;
        }
    };
    let limit = config.max_file_bytes();
    if bytes.len() as u64 > limit {
        eprintln!(
            "[WARN] 表情包素材超过大小上限，本轮不发这张 (文件: {}, {} > {} 字节)",
            path.display(),
            bytes.len(),
            limit
        );
        return None;
    }
    if !looks_like_supported_image(&bytes) {
        eprintln!(
            "[WARN] 表情包素材内容不是受支持的图片格式，本轮不发这张 (文件: {})",
            path.display()
        );
        return None;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(Segment::new(
        "image",
        json!({
            "file": format!("base64://{encoded}"),
            "summary": label,
        }),
    ))
}

/// 取这个标签下的下一张图（轮换），并记一次使用。
fn next_sticker_file(label: &str) -> Option<PathBuf> {
    let labels = labels_snapshot();
    let files = labels.get(label)?;
    if files.is_empty() {
        return None;
    }
    let mut counters = USE_COUNTS.lock().ok()?;
    let used = counters.entry(label.to_string()).or_insert(0);
    let index = usize::try_from(*used).unwrap_or(usize::MAX) % files.len();
    *used = used.saturating_add(1);
    files.get(index).cloned()
}

/// 只认文件头，不认扩展名：运维把 `.txt` 改名成 `.png` 时应当当场发现，
/// 而不是把一段文本当图片发给 QQ。返回该内容应使用的扩展名。
fn extension_for_image(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G'];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];
    if bytes.starts_with(PNG) {
        return Some("png");
    }
    if bytes.starts_with(JPEG) {
        return Some("jpg");
    }
    if bytes.starts_with(b"GIF8") {
        return Some("gif");
    }
    if bytes.starts_with(b"BM") {
        return Some("bmp");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// 内容像不像一张能发的图。收录与上传共用这一处判断。
fn looks_like_supported_image(bytes: &[u8]) -> bool {
    extension_for_image(bytes).is_some()
}

#[cfg(test)]
mod tests {
    use super::{
        StickerLibraryCommand, ensure_directory_exists, extension_for_image, file_stem_for_label,
        label_for_path, looks_like_supported_image, next_available_name, parse_command, resolve_in,
        scan_directory, strip_trailing_ordinal, validated_file_path,
    };
    use std::path::{Path, PathBuf};

    /// 协议里必须写**模型真正能调到的那个名字**：注册名 `sticker.list` 带点，发到
    /// provider 时会被 `wire_tool_name` 换成下划线（DeepSeek 按 `^[a-zA-Z0-9_-]+$`
    /// 校验函数名，带点直接 400）。写错名字的后果是她照着提示词抄也调不到工具，于是
    /// 又回到"凭印象编标签"——这正是这次要修的东西。
    #[test]
    fn sticker_protocol_names_the_callable_tool() {
        let wire = crate::model::tool_access::wire_tool_name("sticker.list");
        assert!(
            super::STICKER_PROMPT.contains(&wire),
            "协议里要写 {wire}：{}",
            super::STICKER_PROMPT
        );
        assert!(
            !super::STICKER_PROMPT.contains("sticker.list"),
            "带点的注册名不是模型能调的名字：{}",
            super::STICKER_PROMPT
        );
    }

    /// 丢掉进程内的扫描缓存与轮换计数，让下一次访问重新扫一遍目录。
    ///
    /// 只给"改配置/改目录"的用例用：正常路径靠 `rescan_secs` 自己过期，不该依赖它。
    fn reset_library_state() {
        if let Ok(mut index) = super::INDEX.lock() {
            *index = super::StickerIndex::default();
        }
        if let Ok(mut counters) = super::USE_COUNTS.lock() {
            counters.clear();
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kovi-sticker-library-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("应能建临时目录");
        dir
    }

    #[test]
    fn commands_are_recognised_and_bounded() {
        assert_eq!(
            parse_command("#表情列表"),
            Some(StickerLibraryCommand::List)
        );
        assert_eq!(
            parse_command("#发表情 无语又想笑"),
            Some(StickerLibraryCommand::Send {
                label: "无语又想笑".to_string()
            })
        );
        assert_eq!(
            parse_command("   #发表情   开心  "),
            Some(StickerLibraryCommand::Send {
                label: "开心".to_string()
            })
        );
        assert_eq!(
            parse_command("#发表情"),
            Some(StickerLibraryCommand::Invalid)
        );
        assert_eq!(parse_command("#发表情包"), None);
        assert_eq!(parse_command("发表情 开心"), None);
        assert_eq!(parse_command("普通聊天"), None);
    }

    #[test]
    fn labels_come_from_file_names_without_ordinals() {
        assert_eq!(strip_trailing_ordinal("开心"), "开心");
        assert_eq!(strip_trailing_ordinal("开心-1"), "开心");
        assert_eq!(strip_trailing_ordinal("开心_2"), "开心");
        assert_eq!(strip_trailing_ordinal("开心 3"), "开心");
        assert_eq!(strip_trailing_ordinal("开心(4)"), "开心");
        assert_eq!(strip_trailing_ordinal("开心（5）"), "开心");
        assert_eq!(strip_trailing_ordinal("开心4"), "开心4");
        assert_eq!(strip_trailing_ordinal("(4)"), "(4)");
        // 数字是名字的一部分时不剥，否则会撞到别的标签。
        assert_eq!(strip_trailing_ordinal("39度"), "39度");
        assert_eq!(strip_trailing_ordinal("版本2"), "版本2");
        assert_eq!(strip_trailing_ordinal("1"), "1");
    }

    #[test]
    fn only_supported_images_become_labels() {
        assert_eq!(
            label_for_path(Path::new("/tmp/stickers/无语又想笑.gif")),
            Some("无语又想笑".to_string())
        );
        assert_eq!(
            label_for_path(Path::new("/tmp/stickers/开心-1.PNG")),
            Some("开心".to_string())
        );
        assert_eq!(label_for_path(Path::new("/tmp/stickers/.DS_Store")), None);
        assert_eq!(label_for_path(Path::new("/tmp/stickers/notes.txt")), None);
        assert_eq!(label_for_path(Path::new("/tmp/stickers/")), None);
    }

    #[test]
    fn scanning_groups_ordinals_and_skips_noise() {
        let dir = temp_dir("scan");
        std::fs::write(dir.join("开心-1.png"), b"x").expect("应能写文件");
        std::fs::write(dir.join("开心-2.png"), b"x").expect("应能写文件");
        std::fs::write(dir.join("无语又想笑.gif"), b"x").expect("应能写文件");
        std::fs::write(dir.join("说明.txt"), b"x").expect("应能写文件");
        std::fs::create_dir_all(dir.join("nested")).expect("应能建子目录");
        std::fs::write(dir.join("nested/收到.webp"), b"x").expect("应能写文件");

        let labels = scan_directory(&dir, 100);
        // BTreeMap 按码点排序：开(U+5F00) < 收(U+6536) < 无(U+65E0)。
        assert_eq!(
            labels.keys().cloned().collect::<Vec<_>>(),
            vec![
                "开心".to_string(),
                "收到".to_string(),
                "无语又想笑".to_string()
            ]
        );
        assert_eq!(labels.get("开心").map(Vec::len), Some(2));

        let capped = scan_directory(&dir, 2);
        assert_eq!(capped.values().map(Vec::len).sum::<usize>(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录缺失要自动补建（幂等）：运维不该先手工 mkdir 才能把素材丢进去。
    #[test]
    fn missing_directories_are_created_and_creation_is_idempotent() {
        let parent = temp_dir("ensure");
        let nested = parent.join("stickers/nested");

        assert!(
            ensure_directory_exists(&nested).expect("应能补建目录"),
            "第一次调用应当真的建了目录"
        );
        assert!(nested.is_dir());
        assert!(
            !ensure_directory_exists(&nested).expect("已存在的目录应当是空操作"),
            "目录已存在时不该报'这次建了'"
        );

        // 路径被一个同名文件占住时如实报错，而不是当成"建好了"。
        let blocked = parent.join("blocked");
        std::fs::write(&blocked, b"not a directory").expect("应能写占位文件");
        assert!(ensure_directory_exists(&blocked).is_err());

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// 标签会变成文件名：任何输入都不能拼出目录之外的路径，也不能变成隐藏文件。
    #[test]
    fn labels_become_safe_file_stems() {
        assert_eq!(
            file_stem_for_label("无语又想笑").expect("合法"),
            "无语又想笑"
        );
        assert_eq!(file_stem_for_label("  开心  ").expect("合法"), "开心");
        // 路径分隔符、控制字符与 Windows 保留字符都被剔掉。
        assert_eq!(
            file_stem_for_label("../../etc/passwd").expect("合法"),
            "etcpasswd"
        );
        assert_eq!(file_stem_for_label("a/b\\c:d*e?f").expect("合法"), "abcdef");
        assert_eq!(file_stem_for_label("开心\n难过").expect("合法"), "开心难过");
        // 去掉首尾的点：`.`、`..`、隐藏文件都不是合法主干。
        assert!(file_stem_for_label(".").is_err());
        assert!(file_stem_for_label("..").is_err());
        assert!(file_stem_for_label("   ").is_err());
        assert!(
            file_stem_for_label(".hidden")
                .expect("合法")
                .starts_with("hidden")
        );
        // 超长标签按上限截断，不会写出超长文件名。
        let long = file_stem_for_label(&"开".repeat(200)).expect("合法");
        assert_eq!(long.chars().count(), super::MAX_LABEL_CHARS);
    }

    /// 只接受素材目录里的裸图片文件名：分隔符、`..`、隐藏文件、非图片一律拒绝。
    #[test]
    fn stored_file_names_cannot_escape_the_directory() {
        assert!(validated_file_path("开心.png").is_ok());
        assert!(validated_file_path("开心-2.gif").is_ok());
        for bad in [
            "../bot.conf.toml",
            "..",
            ".",
            "a/b.png",
            "a\\b.png",
            ".hidden.png",
            "notes.txt",
            "",
        ] {
            assert!(validated_file_path(bad).is_err(), "{bad} 不该被接受");
        }
    }

    /// 同名不覆盖：第二张自动加编号，而编号会被标签解析重新归到同一个标签下。
    #[test]
    fn uploads_never_overwrite_an_existing_sticker() {
        let dir = temp_dir("collision");
        assert_eq!(next_available_name(&dir, "开心", "png"), "开心.png");
        std::fs::write(dir.join("开心.png"), b"x").expect("应能写文件");
        assert_eq!(next_available_name(&dir, "开心", "png"), "开心-2.png");
        std::fs::write(dir.join("开心-2.png"), b"x").expect("应能写文件");
        assert_eq!(next_available_name(&dir, "开心", "png"), "开心-3.png");
        // 编号仍然是同一个标签：这是"传第二张同表情"不改变语义的前提。
        assert_eq!(
            label_for_path(Path::new("/tmp/stickers/开心-3.png")),
            Some("开心".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 扩展名由文件头决定，不听客户端给的文件名。
    #[test]
    fn upload_extension_comes_from_the_content() {
        assert_eq!(extension_for_image(&[0x89, b'P', b'N', b'G']), Some("png"));
        assert_eq!(extension_for_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(extension_for_image(b"GIF89a"), Some("gif"));
        assert_eq!(extension_for_image(b"BM____"), Some("bmp"));
        assert_eq!(extension_for_image(b"RIFF____WEBPVP8 "), Some("webp"));
        assert_eq!(extension_for_image(b"not an image"), None);
    }

    /// 取不到表情时的兜底话术：带标签、简短、不留承诺。
    #[test]
    fn unavailable_sticker_reply_is_short_and_names_the_label() {
        let reply = super::unavailable_sticker_reply("猫猫歪头");
        assert!(reply.contains("猫猫歪头"));
        assert!(reply.chars().count() <= 40);
        assert!(super::unavailable_sticker_reply("   ").contains("暂时没有"));
    }

    #[test]
    fn missing_directory_scans_as_empty() {
        let labels = scan_directory(Path::new("/tmp/kovi-sticker-library-does-not-exist"), 10);
        assert!(labels.is_empty());
    }

    #[test]
    fn resolution_prefers_exact_then_the_tightest_containment() {
        let labels = ["开心", "开心到飞起", "无语又想笑"];
        let resolve = |query: &str| resolve_in(labels.iter().copied(), query);

        assert_eq!(resolve("开心"), Some("开心".to_string()));
        assert_eq!(resolve(" 开心 "), Some("开心".to_string()));
        assert_eq!(resolve("无语又想笑！"), Some("无语又想笑".to_string()));
        assert_eq!(resolve("开心到飞起"), Some("开心到飞起".to_string()));
        // 只说"开心"是精确命中；说"飞起"才落到更长的那个标签上。
        assert_eq!(resolve("飞起"), Some("开心到飞起".to_string()));
        assert_eq!(resolve("找不到的标签"), None);
        assert_eq!(resolve(""), None);
    }

    #[test]
    fn resolution_ignores_punctuation_and_ascii_case() {
        let labels = ["OK", "好耶"];
        assert_eq!(
            resolve_in(labels.iter().copied(), "ok"),
            Some("OK".to_string())
        );
        assert_eq!(
            resolve_in(labels.iter().copied(), "好耶！！！"),
            Some("好耶".to_string())
        );
    }

    #[test]
    fn image_sniffing_rejects_renamed_text() {
        assert!(looks_like_supported_image(&[0x89, b'P', b'N', b'G', 0x0D]));
        assert!(looks_like_supported_image(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(looks_like_supported_image(b"GIF89a"));
        assert!(looks_like_supported_image(b"RIFF____WEBPVP8 "));
        assert!(!looks_like_supported_image(b"not an image at all"));
        assert!(!looks_like_supported_image(b""));
    }

    /// 端到端：真配置 + 真目录 + 真字节，走的就是线上那条一模一样的路
    /// （扫目录 → 标签解析 → 读文件 → 认格式 → `base64://` 图片段）。
    ///
    /// 打开 `qq_sticker` 需要改进程级配置，所以按仓库既有约定标 `#[ignore]`，由
    /// `ci.yml` 点名单跑（不需要数据库，也不需要网络）。真机上"QQ 里能不能收到"
    /// 要部署后由人手验，但"她到底会发出去哪几个字节"在这里就能钉死。
    #[test]
    #[ignore = "mutates the process-global config; run via --ignored --exact"]
    fn configured_library_renders_a_sendable_image_segment() {
        use base64::Engine;

        // 故意指向一个**还不存在**的目录：补建是这条链路的一部分。
        let root = temp_dir("configured");
        let dir = root.join("stickers");
        assert!(!dir.is_dir(), "前置条件：目录一开始不该存在");

        let previous = crate::config::get();
        let source = format!(
            "[qq_sticker]\nenabled = true\ndir = \"{}\"\nrescan_secs = 1\n",
            dir.display()
        );
        let candidate = crate::config::validate_candidate(&source).expect("候选配置应合法");
        crate::config::install(candidate).expect("应安装测试配置");
        reset_library_state();

        // 第一次扫描：目录被补出来，但还没有素材，所以这个出口仍然不下发。
        assert!(!super::is_available());
        assert!(dir.is_dir(), "扫描时应当补建素材目录");

        // 真图片头即可：这条链路不解码图片，只按文件头认格式。
        let png: Vec<u8> = vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R',
        ];
        // 第二张故意换一个字节：轮换是"换文件"，不是"换同一份字节"。
        let mut png_alt = png.clone();
        png_alt.push(0x01);
        std::fs::write(dir.join("开心-1.png"), &png).expect("应能写素材");
        std::fs::write(dir.join("开心-2.png"), &png_alt).expect("应能写素材");
        std::fs::write(dir.join("说明.txt"), b"not a sticker").expect("应能写素材");
        reset_library_state();

        assert!(super::is_available());
        // `sticker_list` 拿到的就是这份清单：全部标签，且素材库关着时没有清单。
        assert_eq!(
            super::tool_listing().as_deref(),
            Some("开心"),
            "工具清单应当包含素材库里的全部标签"
        );
        // 文件名即标签；带编号的两张图归到同一个标签下。
        assert_eq!(super::available_labels(), vec!["开心".to_string()]);
        assert_eq!(super::resolve_label("开心"), Some("开心".to_string()));
        assert_eq!(super::resolve_label(" 开心 "), Some("开心".to_string()));
        assert_eq!(super::resolve_label("没有这张"), None);
        // 不支持的格式不会变成标签。
        assert_eq!(super::resolve_label("说明"), None);

        let segment = super::build_sticker_segment("开心").expect("应能取到这张图");
        assert_eq!(segment.type_, "image");
        let file = segment.data["file"].as_str().expect("image 段应带 file");
        let encoded = file
            .strip_prefix("base64://")
            .expect("素材应当以 base64:// 交付：不依赖 NapCat 与本机共享文件系统，也不要路径映射");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("交付内容应是合法 base64");
        assert_eq!(decoded, png, "发出去的必须就是素材文件本身的字节");
        assert_eq!(segment.data["summary"], "开心");

        // 同一个标签下的多张图轮换着发：第二张就是目录里的另一个文件。
        let rotated = super::build_sticker_segment("开心").expect("应能取到第二张");
        let rotated_file = rotated.data["file"].as_str().expect("image 段应带 file");
        let rotated_decoded = base64::engine::general_purpose::STANDARD
            .decode(
                rotated_file
                    .strip_prefix("base64://")
                    .expect("同样应是 base64://"),
            )
            .expect("交付内容应是合法 base64");
        assert_eq!(rotated_decoded, png_alt);

        reset_library_state();
        crate::config::install(previous).expect("应还原配置");
        assert!(!super::is_available());
        assert!(super::tool_listing().is_none(), "关掉素材库后不该再有清单");
        let _ = std::fs::remove_dir_all(&root);
    }
}
