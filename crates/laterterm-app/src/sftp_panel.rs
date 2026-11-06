//! SFTP 双栏面板（M3 任务4）：本机目录 ↔ 远程目录浏览、上传/下载、
//! 传输进度与取消。
//!
//! 形态：窗口级大覆盖层（复用 settings_overlay 的遮罩+卡片骨架，240px
//! 右面板物理放不下双栏）。后台线程做所有阻塞 IO（SftpHandle 的方法
//! 均阻塞，禁上 UI 线程），经 flume 事件通道回 UI pump。
//!
//! 传输执行模型：队列串行（v1 同时 1 条 active），每条传输一个专属
//! `SftpHandle::spawn`（独立 SSH 连接）——sftp.rs 网络线程逐条串行处理
//! 指令是硬约束，浏览通道绝不能被大传输阻塞。取消 = 置位传输的
//! cancel 标志 → 传输循环下一次分块检查以 Interrupted 返回 → 标
//! Cancelled → 本地/远端半成品尽力清理。`close()` 无法打断在途传输
//! （Shutdown 只是排队），仅用于整体关闭通道。
//!
//! 纯逻辑件（节流、状态机、路径拼接、排序、格式化）与 UI 分离，
//! 全部可无 GPUI 运行时单测。

use crate::workspace::WorkspaceView;
use gpui::{
    App, Context, Div, ElementId, FontWeight, InteractiveElement, IntoElement, ListSizingBehavior,
    MouseButton, MouseDownEvent, ParentElement, StatefulInteractiveElement, Styled, Window, div,
    px, relative, rgb, uniform_list,
};
use laterterm_ssh::SshEndpoint;
use laterterm_ssh::sftp::{CancelFlag, ProgressFn, RemoteEntry, SftpHandle};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 列表条目上限：read_dir 一次拉全量，超过即截断并提示（排序在后台线程
/// 与 read_dir 同一次调用完成，UI 线程只消费已排序结果）。
pub(crate) const MAX_LIST_ENTRIES: usize = 5000;

/// 面板覆盖层卡片尺寸（双栏 + 传输队列的最小可用尺寸）。
const CARD_WIDTH: f32 = 880.0;
const CARD_HEIGHT: f32 = 560.0;

// ---------- 双栏条目 ----------

/// 本地目录条目（与 RemoteEntry 同构；本地侧用 std::fs 枚举）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalEntry {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) modified: Option<u64>,
}

/// 列表行的纯数据快照（渲染闭包捕获用，避免跨帧借用面板状态）。
#[derive(Clone, Debug)]
pub(crate) struct ListRow {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) modified: Option<u64>,
}

impl ListRow {
    pub(crate) fn from_local(entry: &LocalEntry) -> Self {
        Self {
            name: entry.name.clone(),
            is_dir: entry.is_dir,
            size: entry.size,
            modified: entry.modified,
        }
    }

    pub(crate) fn from_remote(entry: &RemoteEntry) -> Self {
        Self {
            name: entry.name.clone(),
            is_dir: entry.is_dir,
            size: entry.size,
            modified: entry.modified,
        }
    }
}

// ---------- 纯逻辑：路径 ----------

/// 拼接远端路径（SFTP 语义恒用 `/`）。
///
/// 归一化目录里的重复分隔符与尾分隔符；拒绝空名、含 `/` 的名称与
/// `.`/`..` 穿越——「上级」导航走 [`remote_parent`]，不靠 `..`。
pub(crate) fn remote_join(dir: &str, name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名称为空".into());
    }
    if name.contains('/') {
        return Err(format!("名称不能包含 “/”：{name}"));
    }
    if name == "." || name == ".." {
        return Err(format!("非法名称：{name}"));
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    // 空目录按根处理（面板恒用绝对路径；防御性口径与 realpath 失败退根一致）
    let absolute = dir.starts_with('/') || parts.is_empty();
    parts.push(name);
    let joined = parts.join("/");
    if absolute {
        Ok(format!("/{joined}"))
    } else {
        Ok(joined)
    }
}

/// 远端父目录：根（`/` 或空）没有上级返回 None；其余取前缀，顶层归 `/`。
pub(crate) fn remote_parent(dir: &str) -> Option<String> {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => Some(parent.to_string()),
        // "/xxx" 的父是根；相对路径顶层也归根（面板恒用绝对路径，防御性口径）
        _ => Some("/".to_string()),
    }
}

/// 本地父目录：无上级（根/单段相对路径）返回 None。
pub(crate) fn local_parent(dir: &Path) -> Option<PathBuf> {
    dir.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

// ---------- 纯逻辑：排序与截断 ----------

/// 列表排序键：目录在前，名称不区分大小写升序（中文按码点序）。
fn compare_listing(a_dir: bool, a_name: &str, b_dir: bool, b_name: &str) -> std::cmp::Ordering {
    b_dir
        .cmp(&a_dir)
        .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
}

pub(crate) fn sort_remote(entries: &mut [RemoteEntry]) {
    entries.sort_by(|a, b| compare_listing(a.is_dir, &a.name, b.is_dir, &b.name));
}

pub(crate) fn sort_local(entries: &mut [LocalEntry]) {
    entries.sort_by(|a, b| compare_listing(a.is_dir, &a.name, b.is_dir, &b.name));
}

/// 超限截断（入参应已排序）；返回 (截断后条目, 被截掉条数)。
pub(crate) fn cap_listing<T>(mut entries: Vec<T>) -> (Vec<T>, usize) {
    let total = entries.len();
    entries.truncate(MAX_LIST_ENTRIES);
    let dropped = total - entries.len();
    (entries, dropped)
}

// ---------- 纯逻辑：进度节流 ----------

/// 进度回调节流（工作线程内，原子实现）：首帧必发；距上次发送
/// ≥ `interval_ms` 必发；总量已知时跨过 1% 粒度台阶也发（大文件长间隔
/// 中仍有颗粒感）。总量未知（total == 0）只按时间节流——UI 侧用
/// 「总量未知」降级显示，分母为 0 不参与百分比计算。
pub(crate) struct ProgressThrottle {
    total: u64,
    interval_ms: u64,
    last_emit_ms: AtomicU64,
    last_emit_bytes: AtomicU64,
}

impl ProgressThrottle {
    pub(crate) fn new(total: u64, interval_ms: u64) -> Self {
        Self {
            total,
            interval_ms,
            // u64::MAX 哨兵：首帧必发
            last_emit_ms: AtomicU64::new(u64::MAX),
            last_emit_bytes: AtomicU64::new(0),
        }
    }

    /// 判定并记录：返回 true 时调用方才发送进度事件（判定与记录合一，
    /// 避免并发重复发送）。
    pub(crate) fn should_emit(&self, transferred: u64, now_ms: u64) -> bool {
        let last = self.last_emit_ms.load(Ordering::Relaxed);
        let first = last == u64::MAX;
        let due_time = !first && now_ms.saturating_sub(last) >= self.interval_ms;
        let step = if self.total > 0 {
            (self.total / 100).max(1)
        } else {
            0
        };
        let due_step =
            step > 0 && transferred / step != self.last_emit_bytes.load(Ordering::Relaxed) / step;
        if !(first || due_time || due_step) {
            return false;
        }
        self.last_emit_ms.store(now_ms, Ordering::Relaxed);
        self.last_emit_bytes.store(transferred, Ordering::Relaxed);
        true
    }
}

// ---------- 纯逻辑：传输状态机 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransferStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl TransferStatus {
    /// 幂等状态机：Queued→Running/Done/Cancelled，Running→Done/Failed/
    /// Cancelled；终态不再接受任何转移（迟到/重复事件静默丢弃）。
    /// 返回是否实际发生了转移。
    pub(crate) fn advance(&mut self, next: TransferStatus) -> bool {
        let allowed = matches!(
            (*self, next),
            (TransferStatus::Queued, TransferStatus::Running)
                | (TransferStatus::Queued, TransferStatus::Done)
                | (TransferStatus::Queued, TransferStatus::Cancelled)
                | (TransferStatus::Running, TransferStatus::Done)
                | (TransferStatus::Running, TransferStatus::Failed)
                | (TransferStatus::Running, TransferStatus::Cancelled)
        );
        if allowed {
            *self = next;
        }
        allowed
    }

    /// 状态的中文展示（进度条行尾）。
    pub(crate) fn label(self) -> &'static str {
        match self {
            TransferStatus::Queued => "排队中",
            TransferStatus::Running => "传输中",
            TransferStatus::Done => "完成",
            TransferStatus::Failed => "失败",
            TransferStatus::Cancelled => "已取消",
        }
    }
}

// ---------- 纯逻辑：格式化 ----------

/// 字节数 → 紧凑展示（B/KB/MB/GB，一位小数）。
pub(crate) fn format_size(size: u64) -> String {
    const UNIT: f64 = 1024.0;
    if size < 1024 {
        return format!("{size} B");
    }
    let kb = size as f64 / UNIT;
    if kb < UNIT {
        return format!("{kb:.1} KB");
    }
    let mb = kb / UNIT;
    if mb < UNIT {
        return format!("{mb:.1} MB");
    }
    format!("{:.1} GB", mb / UNIT)
}

/// Unix 秒 → 「MM-DD HH:mm·YYYY」（远端时间戳无时区语义，统一按 UTC
/// 展示；年份缀在行尾避免与小时混淆）。服务器未提供则「—」。
pub(crate) fn format_mtime(mtime: Option<u64>) -> String {
    let Some(secs) = mtime else {
        return "—".into();
    };
    let (year, month, day) = civil_from_days((secs / 86400) as i64);
    let hour = (secs % 86400) / 3600;
    let minute = (secs % 3600) / 60;
    format!("{month:02}-{day:02} {hour:02}:{minute:02}·{year}")
}

/// Howard Hinnant 的 civil_from_days：天数（自 1970-01-01）→ (年, 月, 日)。
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { y + 1 } else { y }, month, day)
}

// ---------- 传输记录与工作线程 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransferKind {
    Upload,
    Download,
}

impl TransferKind {
    /// 方向标识（传输队列行首）。
    pub(crate) fn arrow(self) -> &'static str {
        match self {
            TransferKind::Upload => "▶ 上传",
            TransferKind::Download => "◀ 下载",
        }
    }
}

/// 一条传输的 UI 侧记录。`cancel` 与工作线程共享；`total == 0` 表示
/// 总量未知（stat 失败或服务器未返回 size）。
#[derive(Clone)]
pub(crate) struct SftpTransfer {
    pub(crate) id: u64,
    pub(crate) kind: TransferKind,
    pub(crate) file_name: String,
    pub(crate) local_path: PathBuf,
    pub(crate) remote_path: String,
    pub(crate) total: u64,
    pub(crate) transferred: u64,
    pub(crate) status: TransferStatus,
    pub(crate) error: Option<String>,
    pub(crate) cancel: CancelFlag,
}

impl SftpTransfer {
    /// 进度条填充比例；总量未知返回 0（UI 降级为文本展示）。
    pub(crate) fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.transferred as f32 / self.total as f32).min(1.0)
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        matches!(
            self.status,
            TransferStatus::Queued | TransferStatus::Running
        )
    }
}

/// 栏位（本机 / 远端）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneSide {
    Local,
    Remote,
}

/// 栏内新建目录 / 重命名的输入弹窗类别。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SftpPromptKind {
    Mkdir,
    Rename { from: String },
}

impl SftpPromptKind {
    pub(crate) fn title(&self) -> &'static str {
        match self {
            SftpPromptKind::Mkdir => "新建目录",
            SftpPromptKind::Rename { .. } => "重命名",
        }
    }

    /// 输入框预填值（重命名预填原名）。
    pub(crate) fn initial(&self) -> String {
        match self {
            SftpPromptKind::Mkdir => String::new(),
            SftpPromptKind::Rename { from } => from.clone(),
        }
    }
}

/// 后台事件（工作线程/列表线程 → UI pump）。`requested` 用于丢弃过期
/// 结果（连续导航时旧列表不得覆盖新目录）。
pub(crate) enum SftpEvent {
    RemoteListed {
        requested: String,
        resolved: Option<String>,
        entries: Vec<RemoteEntry>,
        truncated: usize,
    },
    LocalListed {
        requested: PathBuf,
        entries: Vec<LocalEntry>,
        truncated: usize,
    },
    /// 列目录失败（通道仍活着，展示错误即可）。
    ListingFailed {
        remote: bool,
        message: String,
    },
    /// 浏览通道已死（连接断开/网络线程退出），面板展示断开状态。
    BrowseLost {
        message: String,
    },
    /// 一次性提示（目录传输不支持、操作失败等）。
    Notice {
        error: bool,
        message: String,
    },
    TransferStarted {
        id: u64,
    },
    TransferTotal {
        id: u64,
        total: u64,
    },
    TransferProgress {
        id: u64,
        transferred: u64,
    },
    TransferFinished {
        id: u64,
        result: std::io::Result<u64>,
        cancelled: bool,
    },
}

/// SFTP 指令通道失效的错误判定（close() 之后的排队指令 / 网络线程退出）。
fn is_channel_dead(error: &std::io::Error) -> bool {
    let message = error.to_string();
    message.contains("SFTP 通道已关闭") || message.contains("SFTP 网络线程已退出")
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// ---------- 双栏列表状态（与浏览通道解耦，纯逻辑可单测） ----------

/// 初始远端列目录的请求标签。
///
/// 契约：`realpath(".")` 的结果恒为规范绝对路径、**永不为 "."**
/// （SFTP realpath 语义 + `SftpHandle::realpath` 文档），因此初始加载
/// 的事件必须以本标签作为 `requested` 回传、把规范化结果放进 `resolved`
/// ——若把 realpath 结果当标签，过期守卫永不匹配，首个列表事件被确定性
/// 丢弃，远端栏空转且无法自愈（独立复核 2026-11-06 确认的回归）。
pub(crate) const INITIAL_REMOTE_REQUEST: &str = ".";

/// 远端栏列表状态：当前目录、加载中标签、条目与选中集。
/// 列表用 `Arc<Vec<_>>`：渲染闭包每帧捕获，Arc 克隆避免逐帧深拷贝。
#[derive(Default)]
pub(crate) struct RemoteListing {
    pub(crate) dir: String,
    /// 正在加载的请求标签（过期列表事件按此丢弃）。
    pub(crate) pending: Option<String>,
    pub(crate) entries: Arc<Vec<RemoteEntry>>,
    pub(crate) truncated: usize,
    pub(crate) error: Option<String>,
    pub(crate) selected: BTreeSet<String>,
}

impl RemoteListing {
    /// 标记加载中。`requested` 是请求标签：导航/刷新为目录本身，
    /// 初始加载为 [`INITIAL_REMOTE_REQUEST`]——事件回传时必须原样携带。
    pub(crate) fn mark_loading(&mut self, requested: String) {
        self.pending = Some(requested);
    }

    /// 应用列目录事件：标签不符即过期丢弃（返回 false，状态不动）；
    /// 命中则落地目录（优先 `resolved`）与条目，并修剪已消失的选中项。
    pub(crate) fn apply_listed(
        &mut self,
        requested: &str,
        resolved: Option<String>,
        entries: Vec<RemoteEntry>,
        truncated: usize,
    ) -> bool {
        if self.pending.as_deref() != Some(requested) {
            return false;
        }
        self.pending = None;
        self.dir = resolved.unwrap_or_else(|| requested.to_string());
        self.entries = Arc::new(entries);
        self.truncated = truncated;
        self.error = None;
        let names: BTreeSet<String> = self.entries.iter().map(|e| e.name.clone()).collect();
        self.selected.retain(|name| names.contains(name));
        true
    }

    /// 列目录失败/连接断开：错误可见、loading 解除，既有目录与条目不动。
    pub(crate) fn apply_failed(&mut self, message: String) {
        self.error = Some(message);
        self.pending = None;
    }
}

/// 本地栏列表状态（与远端对称；无 `resolved`——本地请求即目录本身）。
#[derive(Default)]
pub(crate) struct LocalListing {
    pub(crate) dir: PathBuf,
    pub(crate) pending: Option<PathBuf>,
    pub(crate) entries: Arc<Vec<LocalEntry>>,
    pub(crate) truncated: usize,
    pub(crate) error: Option<String>,
    pub(crate) selected: BTreeSet<String>,
}

impl LocalListing {
    pub(crate) fn mark_loading(&mut self, requested: PathBuf) {
        self.pending = Some(requested);
    }

    pub(crate) fn apply_listed(
        &mut self,
        requested: PathBuf,
        entries: Vec<LocalEntry>,
        truncated: usize,
    ) -> bool {
        if self.pending.as_deref() != Some(requested.as_path()) {
            return false;
        }
        self.pending = None;
        self.dir = requested;
        self.entries = Arc::new(entries);
        self.truncated = truncated;
        self.error = None;
        let names: BTreeSet<String> = self.entries.iter().map(|e| e.name.clone()).collect();
        self.selected.retain(|name| names.contains(name));
        true
    }

    pub(crate) fn apply_failed(&mut self, message: String) {
        self.error = Some(message);
        self.pending = None;
    }
}

/// 面板状态：浏览通道 + 双栏列表 + 选中集 + 传输队列。
pub(crate) struct SftpPanelState {
    /// 头部标题：目标机 `user@host`，经跳板链时后缀 ` ⇢ 跳1 ⇢ 跳2`
    /// （与 SshOptions::title 同款拼接）。
    pub(crate) title: String,
    pub(crate) browse: Arc<SftpHandle>,
    /// 连接快照：每条传输用同一链路重建专属连接。
    pub(crate) endpoint: SshEndpoint,
    pub(crate) hops: Vec<SshEndpoint>,
    pub(crate) remote: RemoteListing,
    pub(crate) local: LocalListing,
    pub(crate) transfers: Vec<SftpTransfer>,
    pub(crate) error: Option<String>,
    pub(crate) notice: Option<String>,
    /// 待二次确认的删除操作（对齐 sidebar 删除确认口径）。
    pub(crate) delete_confirm: Option<String>,
}

impl SftpPanelState {
    pub(crate) fn new(
        title: String,
        browse: Arc<SftpHandle>,
        endpoint: SshEndpoint,
        hops: Vec<SshEndpoint>,
        local_dir: PathBuf,
    ) -> Self {
        Self {
            title,
            browse,
            endpoint,
            hops,
            remote: RemoteListing::default(),
            local: LocalListing {
                dir: local_dir,
                ..LocalListing::default()
            },
            transfers: Vec::new(),
            error: None,
            notice: None,
            delete_confirm: None,
        }
    }

    /// 初始加载：后台线程 realpath(".") 规范化起始目录（失败退根），
    /// 随后以 [`INITIAL_REMOTE_REQUEST`] 标签回传事件；本地栏并行加载
    /// 起始目录。
    pub(crate) fn initial_load(&mut self, events: &flume::Sender<SftpEvent>) {
        let browse = Arc::clone(&self.browse);
        let tx = events.clone();
        self.remote.mark_loading(INITIAL_REMOTE_REQUEST.to_string());
        let spawned = std::thread::Builder::new()
            .name("laterterm-sftp-list".into())
            .spawn(move || {
                let dir = browse.realpath(".").unwrap_or_else(|_| "/".to_string());
                // 标签（INITIAL_REMOTE_REQUEST）与读取路径（realpath 结果）
                // 分离：守卫按标签匹配，规范化目录走 resolved
                load_remote_into(&browse, &dir, INITIAL_REMOTE_REQUEST, Some(&dir), &tx);
            });
        if spawned.is_err() {
            self.remote.apply_failed("启动列表线程失败".into());
        }
        let local_dir = self.local.dir.clone();
        self.refresh_local(&local_dir, events);
    }

    /// 导航/刷新远端目录（后台线程；标签 = 目录本身，供过期结果丢弃）。
    pub(crate) fn refresh_remote(&mut self, dir: &str, events: &flume::Sender<SftpEvent>) {
        let browse = Arc::clone(&self.browse);
        let tx = events.clone();
        let requested = dir.to_string();
        self.remote.mark_loading(requested.clone());
        let spawned = std::thread::Builder::new()
            .name("laterterm-sftp-list".into())
            .spawn(move || load_remote_into(&browse, &requested, &requested, None, &tx));
        if spawned.is_err() {
            self.remote.apply_failed("启动列表线程失败".into());
        }
    }

    /// 枚举本地目录（后台线程：IO 不上 UI 线程）。
    pub(crate) fn refresh_local(&mut self, dir: &Path, events: &flume::Sender<SftpEvent>) {
        let tx = events.clone();
        let requested = dir.to_path_buf();
        self.local.mark_loading(requested.clone());
        let spawned = std::thread::Builder::new()
            .name("laterterm-sftp-local".into())
            .spawn(move || load_local_into(requested, &tx));
        if spawned.is_err() {
            self.local.apply_failed("启动列表线程失败".into());
        }
    }

    /// 应用面板事件（UI pump 调用）；返回 true 表示有传输终结，
    /// 调用方需要推进队列并对取消的传输做半成品清理。
    pub(crate) fn apply_event(&mut self, event: SftpEvent) -> bool {
        match event {
            SftpEvent::RemoteListed {
                requested,
                resolved,
                entries,
                truncated,
            } => self
                .remote
                .apply_listed(&requested, resolved, entries, truncated),
            SftpEvent::LocalListed {
                requested,
                entries,
                truncated,
            } => self.local.apply_listed(requested, entries, truncated),
            SftpEvent::ListingFailed { remote, message } => {
                if remote {
                    self.remote.apply_failed(message);
                } else {
                    self.local.apply_failed(message);
                }
                false
            }
            SftpEvent::BrowseLost { message } => {
                self.remote
                    .apply_failed(format!("SFTP 连接已断开：{message}"));
                false
            }
            SftpEvent::Notice { error, message } => {
                if error {
                    self.error = Some(message);
                } else {
                    self.notice = Some(message);
                }
                false
            }
            SftpEvent::TransferStarted { id } => self
                .transfer_mut(id)
                .map(|transfer| transfer.status.advance(TransferStatus::Running))
                .unwrap_or(false),
            SftpEvent::TransferTotal { id, total } => {
                if let Some(transfer) = self.transfer_mut(id) {
                    transfer.total = total;
                }
                false
            }
            SftpEvent::TransferProgress { id, transferred } => {
                if let Some(transfer) = self.transfer_mut(id) {
                    transfer.transferred = transferred;
                }
                false
            }
            SftpEvent::TransferFinished {
                id,
                result,
                cancelled,
            } => {
                let Some(transfer) = self.transfer_mut(id) else {
                    return false;
                };
                let next = if cancelled {
                    TransferStatus::Cancelled
                } else {
                    match &result {
                        // 节流可能吞掉最后一帧进度：终值以返回字节数为准
                        Ok(total) => {
                            transfer.transferred = *total;
                            TransferStatus::Done
                        }
                        Err(error) => {
                            transfer.error = Some(error.to_string());
                            TransferStatus::Failed
                        }
                    }
                };
                transfer.status.advance(next)
            }
        }
    }

    fn transfer_mut(&mut self, id: u64) -> Option<&mut SftpTransfer> {
        self.transfers.iter_mut().find(|transfer| transfer.id == id)
    }

    /// 取消一条传输：置位标志；排队中的直接标 Cancelled（工作线程可能
    /// 尚未启动，启动后看到标志也会立即以取消收场）。返回是否命中
    /// Running（调用方据此推进队列/清理半成品）。
    pub(crate) fn cancel_transfer(&mut self, id: u64) -> bool {
        let Some(transfer) = self.transfer_mut(id) else {
            return false;
        };
        transfer.cancel.store(true, Ordering::Relaxed);
        if transfer.status == TransferStatus::Queued {
            transfer.status = TransferStatus::Cancelled;
            return false;
        }
        transfer.status == TransferStatus::Running
    }

    /// 全部活动传输置取消标志（面板关闭/退出时调用）；返回取消前处于
    /// Running 的传输快照（这些可能留有半成品，调用方按需清理）。
    pub(crate) fn cancel_all(&mut self) -> Vec<SftpTransfer> {
        let mut running = Vec::new();
        for transfer in &mut self.transfers {
            if transfer.is_active() {
                transfer.cancel.store(true, Ordering::Relaxed);
                if transfer.status == TransferStatus::Running {
                    running.push(transfer.clone());
                } else {
                    transfer.status = TransferStatus::Cancelled;
                }
            }
        }
        running
    }

    /// 推进串行队列：无活动传输时把第一条 Queued 交给专属工作线程。
    /// 就绪即标 Running（防同帧双重推进；工作线程的 Started 事件幂等）。
    pub(crate) fn promote_next(&mut self, events: &flume::Sender<SftpEvent>) {
        if self
            .transfers
            .iter()
            .any(|transfer| transfer.status == TransferStatus::Running)
        {
            return;
        }
        let Some(index) = self
            .transfers
            .iter()
            .position(|transfer| transfer.status == TransferStatus::Queued)
        else {
            return;
        };
        let transfer = &mut self.transfers[index];
        transfer.status = TransferStatus::Running;
        spawn_transfer_worker(
            transfer.id,
            transfer.kind,
            transfer.local_path.clone(),
            transfer.remote_path.clone(),
            self.endpoint.clone(),
            self.hops.clone(),
            Arc::clone(&transfer.cancel),
            events.clone(),
        );
    }
}

/// 列本地目录 → 排序 → 截断 → 事件（后台线程内执行；供刷新与
/// 删除/改名等操作完成后同线程续列复用）。
pub(crate) fn load_local_into(requested: PathBuf, tx: &flume::Sender<SftpEvent>) {
    let read = || -> std::io::Result<Vec<LocalEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&requested)? {
            let Ok(entry) = entry else { continue };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            entries.push(LocalEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                is_dir: metadata.is_dir(),
                size: metadata.len(),
                modified: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs()),
            });
        }
        Ok(entries)
    };
    match read() {
        Ok(mut entries) => {
            sort_local(&mut entries);
            let (entries, truncated) = cap_listing(entries);
            let _ = tx.send(SftpEvent::LocalListed {
                requested,
                entries,
                truncated,
            });
        }
        Err(error) => {
            let _ = tx.send(SftpEvent::ListingFailed {
                remote: false,
                message: format!("读取目录失败: {error}"),
            });
        }
    }
}

/// 列远端目录 → 排序 → 截断 → 事件（后台线程内执行）。
///
/// `read_path` 是实际读取的远端路径；`tag` 是请求标签（过期守卫按它
/// 匹配）；`resolved` 是规范路径（初始加载时为 realpath 结果，随事件
/// 回传供 UI 更新当前目录）。三者分离是初始加载正确性的关键。
pub(crate) fn load_remote_into(
    browse: &Arc<SftpHandle>,
    read_path: &str,
    tag: &str,
    resolved: Option<&str>,
    tx: &flume::Sender<SftpEvent>,
) {
    match browse.read_dir(read_path) {
        Ok(mut entries) => {
            sort_remote(&mut entries);
            let (entries, truncated) = cap_listing(entries);
            let _ = tx.send(SftpEvent::RemoteListed {
                requested: tag.to_string(),
                resolved: resolved.map(str::to_string),
                entries,
                truncated,
            });
        }
        Err(error) => {
            let message = error.to_string();
            let _ = tx.send(if is_channel_dead(&error) {
                SftpEvent::BrowseLost { message }
            } else {
                SftpEvent::ListingFailed {
                    remote: true,
                    message: format!("列目录失败: {message}"),
                }
            });
        }
    }
}

/// 启动一条传输的专属工作线程：独立 SSH 连接（浏览通道永不被大传输
/// 阻塞）；先取总量（下载 stat 远端 / 上传读本地元数据），再带进度
/// 节流与取消标志传输，收场必 close() 连接。
/// 参数多是刻意的：一次把「传什么、往哪传、经哪条链路、怎么取消、
/// 事件回哪」全部显式交给工作线程，线程内不再回查任何共享状态。
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_transfer_worker(
    id: u64,
    kind: TransferKind,
    local_path: PathBuf,
    remote_path: String,
    endpoint: SshEndpoint,
    hops: Vec<SshEndpoint>,
    cancel: CancelFlag,
    events: flume::Sender<SftpEvent>,
) {
    let failure_events = events.clone();
    let spawned = std::thread::Builder::new()
        .name("laterterm-sftp-xfer".into())
        .spawn(move || {
            // 启动前已取消（排队中被点掉）：直接收场，不建连接
            if cancel.load(Ordering::Relaxed) {
                let _ = events.send(SftpEvent::TransferFinished {
                    id,
                    result: Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "传输已取消",
                    )),
                    cancelled: true,
                });
                return;
            }
            let finish_events = events.clone();
            let _ = events.send(SftpEvent::TransferStarted { id });
            let handle = match SftpHandle::spawn(endpoint, hops) {
                Ok(handle) => Arc::new(handle),
                Err(error) => {
                    let cancelled = cancel.load(Ordering::Relaxed);
                    let _ = events.send(SftpEvent::TransferFinished {
                        id,
                        result: Err(error),
                        cancelled,
                    });
                    return;
                }
            };
            let total = match kind {
                TransferKind::Upload => std::fs::metadata(&local_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0),
                TransferKind::Download => handle
                    .stat(&remote_path)
                    .map(|entry| entry.size)
                    .unwrap_or(0),
            };
            let _ = events.send(SftpEvent::TransferTotal { id, total });
            let throttle = Arc::new(ProgressThrottle::new(total, 100));
            let progress_events = events.clone();
            let progress: ProgressFn = Arc::new(move |transferred| {
                if throttle.should_emit(transferred, now_millis()) {
                    let _ = progress_events.send(SftpEvent::TransferProgress { id, transferred });
                }
            });
            let result = match kind {
                TransferKind::Upload => handle.upload(
                    &local_path,
                    &remote_path,
                    Some(progress),
                    Some(Arc::clone(&cancel)),
                ),
                TransferKind::Download => handle.download(
                    &remote_path,
                    &local_path,
                    Some(progress),
                    Some(Arc::clone(&cancel)),
                ),
            };
            let cancelled = cancel.load(Ordering::Relaxed);
            // 取消半成品清理（尽力而为）：下载删本地半成品；上传用专属连接
            // 删远端半成品（此刻连接仍活着，比绕道浏览通道更可靠——浏览通道
            // 可能已被面板关闭）。
            if cancelled {
                match kind {
                    TransferKind::Upload => {
                        let _ = handle.remove_file(&remote_path);
                    }
                    TransferKind::Download => {
                        let _ = std::fs::remove_file(&local_path);
                    }
                }
            }
            // 无论成败/取消，专属连接即用即关（每条传输一条独立 SSH 连接
            // 是既定口径：浏览通道不被传输阻塞；v1 串行队列下重建成本可接受）
            handle.close();
            let _ = finish_events.send(SftpEvent::TransferFinished {
                id,
                result,
                cancelled,
            });
        });
    if let Err(error) = spawned {
        // 线程创建失败极端罕见：回写失败事件，让 UI 不悬挂
        let _ = failure_events.send(SftpEvent::TransferFinished {
            id,
            result: Err(std::io::Error::other(format!("启动传输线程失败: {error}"))),
            cancelled: false,
        });
    }
}

/// 面板标题：与 `SshOptions::title` 同款拼接（目标机 + 跳板链标注）。
pub(crate) fn panel_title(endpoint: &SshEndpoint, hops: &[SshEndpoint]) -> String {
    let mut title = endpoint.title();
    for jump in hops {
        title = format!("{title} ⇢ {}", jump.title());
    }
    title
}

// ---------- UI：SFTP 覆盖层 ----------

/// 覆盖层关闭监听（遮罩与标题栏 × 复用；对齐 dismiss_listener 模式）。
fn close_listener(
    cx: &Context<WorkspaceView>,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + Clone + 'static {
    let view = cx.weak_entity();
    move |_event, window, cx| {
        cx.stop_propagation();
        let _ = view.update(cx, |workspace, cx| workspace.close_sftp_panel(window, cx));
    }
}

/// 覆盖层按钮（对齐 setting_button 观感；本文件内独立实现避免跨模块耦合）。
fn chip(
    label: &str,
    enabled: bool,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let base = div()
        .px_2()
        .py_1()
        .rounded_sm()
        .text_size(px(12.0))
        .cursor_pointer();
    if enabled {
        base.text_color(rgb(0xC0CAF5))
            .bg(rgb(0x303548))
            .hover(|style| style.bg(rgb(0x3B4261)))
            .on_mouse_down(MouseButton::Left, listener)
    } else {
        base.text_color(rgb(0x565B79)).bg(rgb(0x24273A))
    }
    .child(label.to_string())
}

/// 面板行内低对比文字按钮（重命名/删除，对齐 sidebar 口径）。
fn text_chip(
    label: &str,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Div {
    div()
        .text_size(px(11.0))
        .text_color(rgb(0xA9B1D6))
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(0xF7768E)))
        .on_mouse_down(MouseButton::Left, listener)
        .child(label.to_string())
}

/// SFTP 双栏面板覆盖层。面板未打开时返回 None（render 期 when_some 消费）。
pub(crate) fn sftp_overlay(
    workspace: &WorkspaceView,
    cx: &mut Context<WorkspaceView>,
) -> Option<gpui::Div> {
    let panel = workspace.sftp_panel.as_ref()?;
    let dismiss = close_listener(cx);
    let weak = cx.weak_entity();

    let local_rows: Arc<Vec<ListRow>> = Arc::new(
        panel
            .local
            .entries
            .iter()
            .map(ListRow::from_local)
            .collect(),
    );
    let remote_rows: Arc<Vec<ListRow>> = Arc::new(
        panel
            .remote
            .entries
            .iter()
            .map(ListRow::from_remote)
            .collect(),
    );
    let local_selected = panel.local.selected.clone();
    let remote_selected = panel.remote.selected.clone();
    let local_dir_text = panel.local.dir.display().to_string();
    let remote_dir_text = panel.remote.dir.clone();
    let title = panel.title.clone();
    let transfers = panel.transfers.clone();

    let card = div()
        .w(px(CARD_WIDTH))
        .h(px(CARD_HEIGHT))
        .max_w(relative(0.95))
        .max_h(relative(0.95))
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded_md()
        .bg(rgb(0x1C1E2C))
        .border_1()
        .border_color(rgb(0x343A50))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        // 头部：主机名（含跳板标注）+ 关闭
        .child(
            div()
                .h(px(44.0))
                .flex_shrink_0()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .px_4()
                .border_b_1()
                .border_color(rgb(0x343750))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(0xC0CAF5))
                        .child(format!("SFTP · {title}")),
                )
                .child(
                    div()
                        .h(px(26.0))
                        .w(px(26.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(0x7D829F))
                        .hover(|style| style.text_color(rgb(0xF7768E)))
                        .on_mouse_down(MouseButton::Left, dismiss.clone())
                        .child("×"),
                ),
        )
        // 主体：双栏 + 中间传输按钮
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .gap_2()
                .p_3()
                .child(file_pane(
                    weak.clone(),
                    local_rows,
                    local_selected,
                    local_dir_text,
                    PaneSide::Local,
                    panel.local.pending.is_some(),
                    panel.local.truncated,
                ))
                .child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(chip("▶ 上传", true, {
                            let view = weak.clone();
                            move |_event, _window, cx| {
                                cx.stop_propagation();
                                let _ = view.update(cx, |workspace, cx| {
                                    workspace.sftp_enqueue(TransferKind::Upload, cx)
                                });
                            }
                        }))
                        .child(chip("◀ 下载", true, {
                            let view = weak.clone();
                            move |_event, _window, cx| {
                                cx.stop_propagation();
                                let _ = view.update(cx, |workspace, cx| {
                                    workspace.sftp_enqueue(TransferKind::Download, cx)
                                });
                            }
                        })),
                )
                .child(file_pane(
                    weak.clone(),
                    remote_rows,
                    remote_selected,
                    remote_dir_text,
                    PaneSide::Remote,
                    panel.remote.pending.is_some(),
                    panel.remote.truncated,
                )),
        )
        // 底部：传输队列
        .child(
            div()
                .border_t_1()
                .border_color(rgb(0x343750))
                .h(px(150.0))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .px_3()
                .py_2()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(0x7D829F))
                        .child("传输队列（串行执行，每条传输独立连接，可取消）"),
                )
                .child(if transfers.is_empty() {
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.0))
                        .text_color(rgb(0x565B79))
                        .child("暂无传输任务")
                        .into_any_element()
                } else {
                    div()
                        .id("sftp-transfer-queue")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(
                            transfers
                                .into_iter()
                                .map(|transfer| transfer_row(transfer, cx).into_any_element()),
                        )
                        .into_any_element()
                }),
        )
        // 状态行：错误（红）/提示（绿）/本地错误（紫）
        .children(panel.error.clone().map(|error| {
            div()
                .px_3()
                .pb_2()
                .text_size(px(12.0))
                .text_color(rgb(0xF7768E))
                .child(error)
        }))
        .children(panel.notice.clone().map(|notice| {
            div()
                .px_3()
                .pb_2()
                .text_size(px(12.0))
                .text_color(rgb(0x9ECE6A))
                .child(notice)
        }))
        .children(panel.local.error.clone().map(|error| {
            div()
                .px_3()
                .pb_2()
                .text_size(px(12.0))
                .text_color(rgb(0xBB9AF7))
                .child(error)
        }));

    Some(
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.6))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Left, dismiss)
            .child(card),
    )
}

/// 单栏文件列表：路径行（上级/刷新/浏览）+ uniform_list + 底部操作行。
#[allow(clippy::too_many_arguments)]
fn file_pane(
    view: gpui::WeakEntity<WorkspaceView>,
    rows: Arc<Vec<ListRow>>,
    selected: BTreeSet<String>,
    path_text: String,
    side: PaneSide,
    loading: bool,
    truncated: usize,
) -> Div {
    let is_local = side == PaneSide::Local;
    let list_id: ElementId = if is_local {
        "sftp-local-list".into()
    } else {
        "sftp-remote-list".into()
    };
    let parent_ok = if is_local {
        local_parent(Path::new(&path_text)).is_some()
    } else {
        remote_parent(&path_text).is_some()
    };

    div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .rounded_sm()
        .bg(rgb(0x161824))
        .border_1()
        .border_color(rgb(0x292D40))
        // 路径行：上级 / 刷新 /（本机）浏览 + 当前路径
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(rgb(0x292D40))
                .child(chip("上级", parent_ok, {
                    let view = view.clone();
                    let path = path_text.clone();
                    move |_event, _window, cx| {
                        cx.stop_propagation();
                        let _ =
                            view.update(cx, |workspace, cx| workspace.sftp_go_up(side, &path, cx));
                    }
                }))
                .child(chip("刷新", true, {
                    let view = view.clone();
                    let path = path_text.clone();
                    move |_event, _window, cx| {
                        cx.stop_propagation();
                        let _ = view
                            .update(cx, |workspace, cx| workspace.sftp_refresh(side, &path, cx));
                    }
                }))
                .children(is_local.then(|| {
                    chip("浏览…", true, {
                        let view = view.clone();
                        move |_event, _window, cx| {
                            cx.stop_propagation();
                            let _ =
                                view.update(cx, |workspace, cx| workspace.sftp_browse_local(cx));
                        }
                    })
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(12.0))
                        .italic()
                        .text_color(rgb(0x7D829F))
                        .child(path_text.clone()),
                ),
        )
        // 文件列表（uniform_list 虚拟化；空态/加载态占位）
        .child(div().flex_1().min_h(px(0.0)).child(if rows.is_empty() {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(rgb(0x565B79))
                .child(if loading {
                    "正在读取…"
                } else {
                    "空目录"
                })
                .into_any_element()
        } else {
            uniform_list(list_id, rows.len(), {
                let rows = Arc::clone(&rows);
                let selected = selected.clone();
                let view = view.clone();
                move |range, _window, _cx| {
                    range
                        .filter_map(|index| {
                            let row = rows.get(index)?;
                            Some(file_row(
                                view.clone(),
                                is_local,
                                row.clone(),
                                selected.contains(&row.name),
                            ))
                        })
                        .collect::<Vec<_>>()
                }
            })
            .with_sizing_behavior(ListSizingBehavior::Auto)
            .into_any_element()
        }))
        // 截断提示（read_dir 超上限时后台已截断并回传省略数）
        .children((truncated > 0).then(|| {
            div()
                .px_2()
                .py_1()
                .text_size(px(11.0))
                .text_color(rgb(0xE0AF68))
                .child(format!(
                    "条目过多，仅显示前 {MAX_LIST_ENTRIES} 项（省略 {truncated} 项）"
                ))
        }))
        // 底部操作行：新建目录 / 重命名 / 删除（删除二次确认在 workspace 状态）
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(rgb(0x292D40))
                .child(chip("新建目录", true, {
                    let view = view.clone();
                    move |_event, window, cx| {
                        cx.stop_propagation();
                        let _ = view.update(cx, |workspace, cx| {
                            workspace.sftp_open_prompt(side, SftpPromptKind::Mkdir, window, cx)
                        });
                    }
                }))
                .child(text_chip("重命名", {
                    let view = view.clone();
                    move |_event, window, cx| {
                        cx.stop_propagation();
                        let _ = view.update(cx, |workspace, cx| {
                            workspace.sftp_rename_selected(side, window, cx)
                        });
                    }
                }))
                .child(text_chip("删除", {
                    let view = view.clone();
                    move |_event, _window, cx| {
                        cx.stop_propagation();
                        let _ = view
                            .update(cx, |workspace, cx| workspace.sftp_delete_selected(side, cx));
                    }
                })),
        )
}

/// 单行文件条目：单击切换选中，双击进入目录。
fn file_row(
    view: gpui::WeakEntity<WorkspaceView>,
    is_local: bool,
    row: ListRow,
    selected: bool,
) -> Div {
    let ListRow {
        name,
        is_dir,
        size,
        modified,
    } = row;
    let click_name = name.clone();
    div()
        .h(px(26.0))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .cursor_pointer()
        .bg(rgb(if selected { 0x33467C } else { 0x161824 }))
        .hover(|style| style.bg(rgb(if selected { 0x3D5390 } else { 0x1E2030 })))
        .on_mouse_down(
            MouseButton::Left,
            move |event: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
                let double = event.click_count >= 2;
                let name = click_name.clone();
                let _ = view.update(cx, |workspace, cx| {
                    workspace.sftp_row_click(is_local, double, name, is_dir, cx)
                });
            },
        )
        .child(
            div()
                .w(px(16.0))
                .text_size(px(12.0))
                .text_color(rgb(if is_dir { 0x7DCFFF } else { 0x7D829F }))
                .child(if is_dir { "📁" } else { "📄" }),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(12.0))
                .text_color(rgb(if selected { 0xE8EEFF } else { 0xC0CAF5 }))
                .child(name),
        )
        .child(
            div()
                .w(px(64.0))
                .text_size(px(11.0))
                .text_color(rgb(0x7D829F))
                .child(if is_dir {
                    String::new()
                } else {
                    format_size(size)
                }),
        )
        .child(
            div()
                .w(px(96.0))
                .text_size(px(11.0))
                .text_color(rgb(0x565B79))
                .child(format_mtime(modified)),
        )
}

/// 传输队列单行：方向 + 文件名 + 进度条 + 状态 + 取消。
fn transfer_row(transfer: SftpTransfer, cx: &mut Context<WorkspaceView>) -> Div {
    let id = transfer.id;
    let active = transfer.is_active();
    let unknown_total = transfer.total == 0;
    let progress_line = if unknown_total {
        format!("已传 {} · 总量未知", format_size(transfer.transferred))
    } else {
        format!(
            "{} / {}（{}%）",
            format_size(transfer.transferred),
            format_size(transfer.total),
            (transfer.fraction() * 100.0) as u64
        )
    };
    let cancel_view = cx.weak_entity();
    let status_color = match transfer.status {
        TransferStatus::Done => 0x9ECE6A,
        TransferStatus::Failed => 0xF7768E,
        TransferStatus::Cancelled => 0xE0AF68,
        _ => 0x7D829F,
    };
    let bar_color = match transfer.status {
        TransferStatus::Done => 0x9ECE6A,
        TransferStatus::Failed | TransferStatus::Cancelled => 0xE0AF68,
        _ => 0x7AA2F7,
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(rgb(0x161824))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(match transfer.kind {
                            TransferKind::Upload => 0x9ECE6A,
                            TransferKind::Download => 0x7DCFFF,
                        }))
                        .child(transfer.kind.arrow()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(rgb(0xC0CAF5))
                        .child(transfer.file_name.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(status_color))
                        .child(transfer.status.label()),
                )
                .children((!active).then(|| {
                    div().text_size(px(11.0)).text_color(rgb(0x565B79)).child(
                        match (&transfer.error, transfer.status) {
                            (Some(error), TransferStatus::Failed) => error.clone(),
                            _ => progress_line.clone(),
                        },
                    )
                }))
                .children(active.then(|| {
                    chip("取消", true, move |_event, _window, cx| {
                        cx.stop_propagation();
                        let _ = cancel_view
                            .update(cx, |workspace, cx| workspace.sftp_cancel_transfer(id, cx));
                    })
                })),
        )
        .child(
            div()
                .w_full()
                .h(px(5.0))
                .rounded_sm()
                .bg(rgb(0x24273A))
                .child(
                    div()
                        .h_full()
                        .rounded_sm()
                        .w(relative(transfer.fraction()))
                        .bg(rgb(bar_color)),
                ),
        )
        .child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(0x565B79))
                .child(progress_line),
        )
}

// ---------- 纯逻辑单测 ----------

#[cfg(test)]
mod tests {
    use super::*;
    use laterterm_ssh::SshOptions;

    /// 回归（独立复核 2026-11-06，severity=high）：初始远端列表事件必须以
    /// 请求标签 [`INITIAL_REMOTE_REQUEST`]（"."）回传、realpath 规范化结果
    /// 走 `resolved`。此前实现把 realpath 结果当标签（"." 只存在
    /// pending 里），守卫 `Some(".") != Some(规范路径)` 永不相等，首个
    /// RemoteListed 被确定性丢弃——面板打开即远端栏空转、无法自愈。
    #[test]
    fn initial_remote_listing_matches_request_tag_with_resolved_dir() {
        let mut listing = RemoteListing::default();
        listing.mark_loading(INITIAL_REMOTE_REQUEST.to_string());
        // 复现原 bug 的形状：事件标签若是 realpath 结果（"/"）而非请求标签，
        // 守卫必须不命中——这正是修复前确定性丢弃的路径
        assert!(
            !listing.apply_listed("/", Some("/".to_string()), vec![], 0),
            "realpath 结果不得作为事件标签（否则守卫永不匹配）"
        );
        assert_eq!(
            listing.pending.as_deref(),
            Some("."),
            "丢弃时 loading 不解除"
        );
        // 修复后的事件形态：标签 = INITIAL_REMOTE_REQUEST，规范目录在 resolved
        let applied = listing.apply_listed(
            INITIAL_REMOTE_REQUEST,
            Some("/".to_string()),
            vec![RemoteEntry {
                name: "etc".into(),
                is_dir: true,
                size: 0,
                modified: None,
            }],
            0,
        );
        assert!(applied, "初始列表事件必须命中守卫");
        assert_eq!(
            listing.dir, "/",
            "remote_dir 取 realpath 规范化结果而非标签"
        );
        assert!(listing.pending.is_none(), "loading 态解除");
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.error.is_none());
        assert!(listing.selected.is_empty());
    }

    /// 过期守卫语义：标签不符（用户已导航到别的目录）必须整体丢弃旧事件，
    /// 目录/条目/loading 一概不动。
    #[test]
    fn stale_remote_listing_event_is_dropped_without_side_effects() {
        let mut listing = RemoteListing::default();
        listing.mark_loading("/new".to_string());
        let applied = listing.apply_listed(
            "/old",
            None,
            vec![RemoteEntry {
                name: "x".into(),
                is_dir: false,
                size: 1,
                modified: None,
            }],
            0,
        );
        assert!(!applied);
        assert_eq!(listing.pending.as_deref(), Some("/new"), "loading 保持");
        assert_eq!(listing.dir, "", "目录不得被过期事件改写");
        assert!(listing.entries.is_empty());
    }

    /// 导航刷新（标签 = 目录本身）：命中后落地并解除 loading。
    #[test]
    fn remote_navigation_listing_applies_when_tag_matches() {
        let mut listing = RemoteListing::default();
        listing.mark_loading("/var/log".to_string());
        let applied = listing.apply_listed(
            "/var/log",
            None,
            vec![RemoteEntry {
                name: "syslog".into(),
                is_dir: false,
                size: 9,
                modified: None,
            }],
            0,
        );
        assert!(applied);
        assert_eq!(listing.dir, "/var/log", "无 resolved 时目录取标签本身");
        assert!(listing.pending.is_none());
    }

    /// 本地栏同契约：requested 即目录本身；命中后落地并修剪已消失的选中项。
    #[test]
    fn local_listing_applies_requested_dir_and_prunes_selection() {
        let mut listing = LocalListing::default();
        listing.mark_loading(PathBuf::from("/home/u"));
        listing.selected.insert("gone.txt".into());
        let applied = listing.apply_listed(
            PathBuf::from("/home/u"),
            vec![LocalEntry {
                name: "keep.txt".into(),
                is_dir: false,
                size: 2,
                modified: None,
            }],
            0,
        );
        assert!(applied);
        assert_eq!(listing.dir, PathBuf::from("/home/u"));
        assert!(listing.pending.is_none());
        assert!(listing.selected.is_empty(), "刷新后已消失的选中项应被修剪");
        // 过期事件不落地
        assert!(
            !listing.apply_listed(PathBuf::from("/other"), vec![], 0),
            "标签不符必须丢弃"
        );
        assert_eq!(listing.dir, PathBuf::from("/home/u"));
    }

    /// 列目录失败/断连：错误可见、loading 解除，既有目录与条目保持不变
    /// （失败时 UI 仍呈现上一份真实状态）。
    #[test]
    fn remote_listing_failure_clears_loading_and_keeps_previous_state() {
        let mut listing = RemoteListing {
            dir: "/data".into(),
            entries: Arc::new(vec![RemoteEntry {
                name: "keep".into(),
                is_dir: true,
                size: 0,
                modified: None,
            }]),
            ..RemoteListing::default()
        };
        listing.mark_loading("/data".to_string());
        listing.apply_failed("read_dir 失败".into());
        assert_eq!(listing.dir, "/data");
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.pending.is_none(), "loading 必须解除（不能永亮）");
        assert_eq!(listing.error.as_deref(), Some("read_dir 失败"));
    }

    #[test]
    fn remote_join_normalizes_and_rejects_traversal() {
        assert_eq!(remote_join("/", "a.txt").unwrap(), "/a.txt");
        assert_eq!(
            remote_join("/var//log/", "a.txt").unwrap(),
            "/var/log/a.txt"
        );
        assert_eq!(remote_join("", "x").unwrap(), "/x");
        assert_eq!(
            remote_join("/home/u", "中文.zip").unwrap(),
            "/home/u/中文.zip"
        );
        assert!(remote_join("/tmp", "a/b").is_err(), "名称不得含 /");
        assert!(remote_join("/tmp", "..").is_err());
        assert!(remote_join("/tmp", ".").is_err());
        assert!(remote_join("/tmp", "").is_err());
        assert!(remote_join("/tmp", "  ").is_err());
    }

    #[test]
    fn remote_parent_derives_prefix_and_stops_at_root() {
        assert_eq!(remote_parent("/a/b").as_deref(), Some("/a"));
        assert_eq!(remote_parent("/a").as_deref(), Some("/"));
        assert_eq!(remote_parent("/a/b/").as_deref(), Some("/a"));
        assert_eq!(remote_parent("/"), None, "根没有上级");
        assert_eq!(remote_parent(""), None);
    }

    #[test]
    fn local_parent_walks_to_root_then_stops() {
        assert_eq!(
            local_parent(Path::new("/home/u/docs")),
            Some(PathBuf::from("/home/u"))
        );
        assert_eq!(local_parent(Path::new("/")), None);
        assert_eq!(local_parent(Path::new("rel")), None, "相对单段无父");
    }

    #[test]
    fn listing_sort_puts_dirs_first_then_case_insensitive_names() {
        let mut remote = vec![
            RemoteEntry {
                name: "zeta.txt".into(),
                is_dir: false,
                size: 1,
                modified: None,
            },
            RemoteEntry {
                name: "Apple".into(),
                is_dir: false,
                size: 2,
                modified: None,
            },
            RemoteEntry {
                name: "中文目录".into(),
                is_dir: true,
                size: 0,
                modified: None,
            },
            RemoteEntry {
                name: "banana".into(),
                is_dir: false,
                size: 3,
                modified: None,
            },
            RemoteEntry {
                name: "Archive".into(),
                is_dir: true,
                size: 0,
                modified: None,
            },
        ];
        sort_remote(&mut remote);
        let names: Vec<&str> = remote.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Archive", "中文目录", "Apple", "banana", "zeta.txt"]
        );

        let mut local = vec![
            LocalEntry {
                name: "b".into(),
                is_dir: false,
                size: 1,
                modified: None,
            },
            LocalEntry {
                name: "A".into(),
                is_dir: false,
                size: 1,
                modified: None,
            },
        ];
        sort_local(&mut local);
        let names: Vec<&str> = local.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["A", "b"]);
    }

    #[test]
    fn cap_listing_truncates_and_counts_dropped() {
        let entries: Vec<u32> = (0..(MAX_LIST_ENTRIES + 7) as u32).collect();
        let (capped, dropped) = cap_listing(entries);
        assert_eq!(capped.len(), MAX_LIST_ENTRIES);
        assert_eq!(dropped, 7);
        let (capped, dropped) = cap_listing(vec![1, 2, 3]);
        assert_eq!(capped.len(), 3);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn progress_throttle_emits_first_time_windows_and_one_percent_steps() {
        let throttle = ProgressThrottle::new(10_000, 100);
        // 首帧必发
        assert!(throttle.should_emit(0, 1_000));
        // 同一时刻附近、未跨 1% 台阶（步长 100 字节）不发
        assert!(!throttle.should_emit(50, 1_010));
        // 跨过 1% 台阶即发（即使时间未到）
        assert!(throttle.should_emit(200, 1_020));
        // 同台阶内且时间未到：吞掉
        assert!(!throttle.should_emit(250, 1_090));
        // 时间窗到（≥100ms）也发
        assert!(throttle.should_emit(250, 1_121));
        // 终点必发
        assert!(throttle.should_emit(10_000, 1_200));
    }

    #[test]
    fn progress_throttle_with_unknown_total_only_respects_time() {
        let throttle = ProgressThrottle::new(0, 100);
        assert!(throttle.should_emit(0, 10));
        assert!(!throttle.should_emit(9_999, 50), "总量未知时只按时间节流");
        assert!(throttle.should_emit(9_999, 111));
    }

    #[test]
    fn transfer_status_machine_is_idempotent_and_terminal_states_absorb() {
        let mut status = TransferStatus::Queued;
        assert!(status.advance(TransferStatus::Running));
        assert!(status.advance(TransferStatus::Done));
        // 终态吸收一切（含自赋值与回退尝试）
        assert!(!status.advance(TransferStatus::Done));
        assert!(!status.advance(TransferStatus::Running));
        assert!(!status.advance(TransferStatus::Failed));
        assert_eq!(status, TransferStatus::Done);

        let mut status = TransferStatus::Running;
        assert!(status.advance(TransferStatus::Cancelled));
        assert!(!status.advance(TransferStatus::Running), "终态吸收一切");

        // 零字节文件可能 Queued→Done（工作线程瞬时完成）
        let mut status = TransferStatus::Queued;
        assert!(status.advance(TransferStatus::Done));
        assert!(!status.advance(TransferStatus::Cancelled));
    }

    #[test]
    fn size_and_mtime_formatting() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");

        // Unix 纪元 / 闰日 / 任务日期（统一 UTC）
        assert_eq!(format_mtime(None), "—");
        assert_eq!(format_mtime(Some(0)), "01-01 00:00·1970");
        assert_eq!(format_mtime(Some(951_782_400)), "02-29 00:00·2000");
        assert_eq!(format_mtime(Some(1_793_836_800)), "11-05 00:00·2026");
    }

    #[test]
    fn panel_title_marks_jump_chain_like_ssh_options() {
        let endpoint = SshEndpoint {
            host: "intranet.internal".into(),
            port: Some(2200),
            username: "root".into(),
            auth: laterterm_ssh::SshAuth::Password("x".into()),
            host_key_policy: laterterm_ssh::HostKeyPolicy::AcceptAll,
            encoding: laterterm_ssh::SessionEncoding::Utf8,
        };
        let jump = SshEndpoint {
            host: "bastion.example.com".into(),
            port: None,
            username: "ops".into(),
            auth: laterterm_ssh::SshAuth::Password("x".into()),
            host_key_policy: laterterm_ssh::HostKeyPolicy::AcceptAll,
            encoding: laterterm_ssh::SessionEncoding::Utf8,
        };
        assert_eq!(panel_title(&endpoint, &[]), "root@intranet.internal:2200");
        assert_eq!(
            panel_title(&endpoint, std::slice::from_ref(&jump)),
            "root@intranet.internal:2200 ⇢ ops@bastion.example.com"
        );
        // 与 SshOptions::title 口径一致
        let options = SshOptions {
            endpoint: endpoint.clone(),
            jump: vec![jump],
            cols: 1,
            rows: 1,
        };
        assert_eq!(
            panel_title(&options.endpoint, &options.jump),
            options.title()
        );
    }

    /// 状态标签齐全（UI 文案不空洞）。
    #[test]
    fn transfer_status_labels_are_present() {
        for status in [
            TransferStatus::Queued,
            TransferStatus::Running,
            TransferStatus::Done,
            TransferStatus::Failed,
            TransferStatus::Cancelled,
        ] {
            assert!(!status.label().is_empty());
        }
        assert_eq!(TransferKind::Upload.arrow(), "▶ 上传");
        assert_eq!(TransferKind::Download.arrow(), "◀ 下载");
    }
}
