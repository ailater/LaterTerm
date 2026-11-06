//! Quake 下拉模式：系统级全局热键 ↔ 主窗口呼出/收起。
//!
//! 分两层：
//! - 纯逻辑状态机（跨平台，可单测）：把「portal 触发 / 焦点变化 / 开关」翻译成
//!   「呼出 / 最小化 / 不动」；
//! - Linux portal worker（`cfg(target_os = "linux")`）：独立线程 `async_io::block_on`
//!   跑 xdg-desktop-portal GlobalShortcuts 的注册与监听，事件经 flume 回主线程。
//!
//! portal 侧遵循 docs/architecture.md「系统级」四坑：先订阅 Activated 再 bind（坑4）、
//! 注册阶段套 8s deadline（坑2）、失败隔 1s 重试一次后降级（坑3）、授权等待期不算失败
//! 而是发 AwaitingConsent 供 UI 引导（坑1）。

use std::time::Duration;

/// 失焦收起宽限期。portal 触发呼出后合成器置焦有延迟（约 3 倍 200ms 失焦轮询
/// 周期），宽限期内读到的「失焦」不收起；同时吸收用户Alt-Tab 折返的抖动。
pub(crate) const FOCUS_GRACE: Duration = Duration::from_millis(600);

/// 建议（非强制）的键位串。portal 规范为 "CTRL+ALT+T" 样式，实际键位以桌面端
/// bind 返回的 `trigger_description` 与 ShortcutsChanged 流为准——应用永不持有
/// 键位真值，避免用户在桌面端改绑后此处漂移。
#[cfg(target_os = "linux")]
const QUAKE_PREFERRED_TRIGGER: &str = "F12";

/// portal 侧的快捷键 id（应用自定义命名空间）。
#[cfg(target_os = "linux")]
const QUAKE_SHORTCUT_ID: &str = "laterterm.quake-toggle";

/// 窗口可见性。进程存活期内窗口恒存在（主窗口关闭即退出），故初始为 Shown，
/// 没有 unmap 形态——「收起」一律是最小化（gpui 无公开 hide/unmap 口）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuakeVisibility {
    Shown,
    Hidden,
}

/// 喂给状态机的输入。`now` 由调用方注入（运行期取单调时钟，测试注入假时钟）。
#[derive(Clone, Copy, Debug)]
pub(crate) enum QuakeInput {
    /// portal 全局热键触发：幂等翻转呼出/收起。
    PortalToggle { now: Duration },
    /// 失焦轮询读到窗口非活动。
    FocusLost { now: Duration },
    /// 失焦轮询读到窗口活动（用于记录「shown 后见过焦点」）。
    FocusGained,
    /// 设置开关变化；false 时清空全部瞬态。
    SetEnabled(bool),
}

/// 状态机产出的窗口动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuakeAction {
    /// 呼出（activate）。
    Activate,
    /// 收起（最小化）。
    Minimize,
    /// 不动窗口。
    None,
}

/// Quake 状态机。规则：
/// - 未启用时一切输入不动窗口；
/// - Toggle 幂等翻转，Hidden→Shown 记 `shown_at` 并清「见过焦点」标记；
/// - FocusLost 仅在「Shown + auto_hide + seen_focused + 过宽限期」时收起。
///   seen_focused 是 Wayland 死循环防线：activate 走 xdg-activation，portal 热键
///   触发时无用户手势 token 大概率被合成器拒绝——若不要求先见过焦点，失焦轮询
///   会在宽限期一过立即把刚呼出的窗口收回去（2026-11-06 评审硬伤2）。
#[derive(Debug)]
pub(crate) struct QuakeMachine {
    enabled: bool,
    auto_hide_on_blur: bool,
    visibility: QuakeVisibility,
    shown_at: Option<Duration>,
    seen_focused: bool,
}

impl QuakeMachine {
    pub(crate) fn new(auto_hide_on_blur: bool) -> Self {
        Self {
            enabled: false,
            auto_hide_on_blur,
            visibility: QuakeVisibility::Shown,
            shown_at: None,
            seen_focused: false,
        }
    }

    pub(crate) fn set_auto_hide(&mut self, auto_hide_on_blur: bool) {
        self.auto_hide_on_blur = auto_hide_on_blur;
    }

    /// 状态机启用门。机器构造时恒为禁用，必须经 `SetEnabled(true)` 才响应
    /// Toggle/失焦输入——workspace 侧在配置启用时负责喂入（见
    /// `sync_quake_worker`），漏喂则全局热键静默失效（2026-11-06 复核修复）。
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 关闭开关前窗口是否停在收起态（调用方据此决定是否呼回，防止窗口
    /// 「消失」只剩任务栏）。
    pub(crate) fn is_hidden(&self) -> bool {
        self.visibility == QuakeVisibility::Hidden
    }

    pub(crate) fn handle(&mut self, input: QuakeInput) -> QuakeAction {
        match input {
            QuakeInput::SetEnabled(false) => {
                self.enabled = false;
                self.reset();
                QuakeAction::None
            }
            QuakeInput::SetEnabled(true) => {
                self.enabled = true;
                self.reset();
                QuakeAction::None
            }
            QuakeInput::PortalToggle { now } if self.enabled => match self.visibility {
                QuakeVisibility::Hidden => {
                    self.visibility = QuakeVisibility::Shown;
                    self.shown_at = Some(now);
                    self.seen_focused = false;
                    QuakeAction::Activate
                }
                QuakeVisibility::Shown => {
                    self.visibility = QuakeVisibility::Hidden;
                    self.shown_at = None;
                    self.seen_focused = false;
                    QuakeAction::Minimize
                }
            },
            QuakeInput::FocusGained if self.enabled => {
                // 只有 Shown 态的焦点才算「呼出后见过焦点」；最小化后任务栏点回
                // 的焦点不记入，避免陈旧标记影响下一个呼出周期。
                if self.visibility == QuakeVisibility::Shown {
                    self.seen_focused = true;
                }
                QuakeAction::None
            }
            QuakeInput::FocusLost { now } if self.enabled => {
                let past_grace = self
                    .shown_at
                    .is_some_and(|shown_at| now.saturating_sub(shown_at) >= FOCUS_GRACE);
                if self.visibility == QuakeVisibility::Shown
                    && self.auto_hide_on_blur
                    && self.seen_focused
                    && past_grace
                {
                    self.visibility = QuakeVisibility::Hidden;
                    self.shown_at = None;
                    self.seen_focused = false;
                    QuakeAction::Minimize
                } else {
                    QuakeAction::None
                }
            }
            _ => QuakeAction::None,
        }
    }

    fn reset(&mut self) {
        self.visibility = QuakeVisibility::Shown;
        self.shown_at = None;
        self.seen_focused = false;
    }
}

/// 进程内单调时钟读数（OnceLock 记启动时刻，elapsed 单调递增），
/// 供状态机比较宽限期；测试不走这里，直接注入假时钟。
pub(crate) fn monotonic_now() -> Duration {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// portal worker → 主线程事件。跨平台定义（非 Linux 只有 Disabled 状态，
/// 事件不会产生），让 workspace 侧代码免 cfg 分叉。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QuakePortalEvent {
    /// portal 已连通，绑定请求已发出，等待桌面端授权确认（坑1：期间无 Activated
    /// 属正常，UI 必须呈现「等待授权」而非失败）。
    AwaitingConsent,
    /// 绑定成功，携带桌面端回报的实际键位说明（trigger_description）。
    Ready { trigger: String },
    /// 用户在桌面端改绑，键位说明更新（ShortcutsChanged）。
    TriggerUpdated { trigger: String },
    /// 全局热键触发（呼出/收起切换）。
    Toggled,
    /// 注册失败（portal 缺失/拒绝/超时，重试耗尽）：降级为仅应用内快捷键。
    Failed { reason: String },
    /// 请求把窗口从最小化呼回（关闭 Quake 且窗口停在收起态时，经泵在窗口
    /// 上下文外执行，避免实体更新内嵌套窗口更新的重入）。
    RestoreWindow,
}

/// 设置面板状态行文案来源。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum QuakePortalStatus {
    #[default]
    Disabled,
    AwaitingConsent,
    Ready(String),
    Failed(String),
}

impl QuakePortalStatus {
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Disabled => "未启用（仅应用内快捷键）".into(),
            Self::AwaitingConsent => {
                "正在连接 portal，等待桌面端授权确认……（若长时间无反应请检查 xdg-desktop-portal）"
                    .into()
            }
            Self::Ready(trigger) if trigger.is_empty() => {
                "已绑定（桌面端未回报键位说明，可在桌面快捷键设置中查看）".into()
            }
            Self::Ready(trigger) => format!("已绑定：{trigger}"),
            Self::Failed(reason) => format!("portal 不可用，已降级为仅应用内快捷键：{reason}"),
        }
    }

    /// 状态行配色：绿=就绪、黄=等待、红=异常。
    pub(crate) fn text_color(&self) -> u32 {
        match self {
            Self::Disabled => 0x565A78,
            Self::AwaitingConsent => 0xE0AF68,
            Self::Ready(_) => 0x9ECE6A,
            Self::Failed(_) => 0xF7768E,
        }
    }
}

// ---------------------------------------------------------------------------
// Linux portal worker
// ---------------------------------------------------------------------------

/// 注册阶段统一 deadline（LaterScreen 坑2 口径）：portal 在登录后偶发卡住会让
/// 热键永久静默失效，8s 内连不上/建不了会话按失败重试。只包连接/建会话/
/// 订阅——bind 等用户桌面端确认不设超时（坑1）。
#[cfg(target_os = "linux")]
const REGISTRATION_DEADLINE: Duration = Duration::from_secs(8);

/// worker 停止句柄：Drop 时发停止消息，线程在事件循环的赛跑分支里自然退出。
/// JoinHandle 创建后即丢弃（detach）——worker 可能长时间阻塞在授权确认上，
/// 无需也无法及时 join；进程退出时随进程回收。
#[cfg(target_os = "linux")]
pub(crate) struct QuakeWorkerHandle {
    stop_tx: flume::Sender<()>,
}

#[cfg(target_os = "linux")]
impl Drop for QuakeWorkerHandle {
    fn drop(&mut self) {
        // 接收端已丢弃（线程退出）时发送失败，无害。
        let _ = self.stop_tx.send(());
    }
}

/// 启动 portal worker 线程。事件经 `event_tx` 回主线程泵。
#[cfg(target_os = "linux")]
pub(crate) fn spawn_portal_worker(event_tx: flume::Sender<QuakePortalEvent>) -> QuakeWorkerHandle {
    let (stop_tx, stop_rx) = flume::bounded::<()>(1);
    let error_tx = event_tx.clone();
    let spawned = std::thread::Builder::new()
        .name("quake-portal".into())
        .spawn(move || portal_worker_main(event_tx, stop_rx));
    if let Err(error) = spawned {
        let _ = error_tx.send(QuakePortalEvent::Failed {
            reason: format!("portal 线程启动失败: {error}"),
        });
    }
    QuakeWorkerHandle { stop_tx }
}

/// worker 事件循环退出原因：区分用户主动停止与 portal 会话断开。
#[cfg(target_os = "linux")]
enum WorkerExit {
    /// 停止令牌到位（用户关闭开关/热加载禁用），正常退出。
    NormalStop,
    /// 信号流意外结束（portal 会话拆除/连接断开），需向状态行发 Failed。
    PortalDisconnected,
}

/// 运行期断连提示文案：经 `QuakePortalEvent::Failed` 由状态行展示
/// （enabled 门内可见）。措辞避开「portal」开头——Failed 状态行已有
/// 「portal 不可用，已降级为仅应用内快捷键：」前缀，避免双 portal 冗余。
#[cfg(target_os = "linux")]
const PORTAL_DISCONNECT_REASON: &str =
    "热键会话已断开（信号流结束）：请关闭后重新开启 Quake 开关恢复";

/// 归类事件循环的流结束退出：停止未被请求 = portal 断连；已请求 = 正常退出。
///
/// 竞态窗口说明：停止令牌可能已被事件循环里 racing 的 `recv_async` 消费，
/// 此处 `stop_requested` 落空会被误判为断连——由 workspace 侧 `Failed`
/// 事件的 enabled 门兜底（禁用流程中的迟到 Failed 落入禁用分支被忽略）。
#[cfg(target_os = "linux")]
fn classify_stream_exit(stop_requested: bool) -> WorkerExit {
    if stop_requested {
        WorkerExit::NormalStop
    } else {
        WorkerExit::PortalDisconnected
    }
}

#[cfg(target_os = "linux")]
fn portal_worker_main(event_tx: flume::Sender<QuakePortalEvent>, stop_rx: flume::Receiver<()>) {
    async_io::block_on(async move {
        // 坑3：注册失败隔 1s 重试一次，耗尽发 Failed 降级为仅应用内快捷键。
        let mut last_error = "未知错误".to_string();
        for attempt in 0..2 {
            if attempt > 0 && !stop_requested(&stop_rx) {
                async_io::Timer::after(Duration::from_secs(1)).await;
            }
            if stop_requested(&stop_rx) {
                return;
            }
            match register_and_run(&event_tx, &stop_rx).await {
                Ok(()) => return, // 正常结束：停止信号或主线程已弃用
                Err(error) => last_error = error,
            }
        }
        let _ = event_tx.send(QuakePortalEvent::Failed { reason: last_error });
    });
}

/// 注册（带 deadline）+ 授权等待（无 deadline）+ 事件循环。
/// 返回 Err 表示本轮注册失败可重试；Ok 表示正常退出（停止/通道关闭）。
#[cfg(target_os = "linux")]
async fn register_and_run(
    event_tx: &flume::Sender<QuakePortalEvent>,
    stop_rx: &flume::Receiver<()>,
) -> Result<(), String> {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_lite::StreamExt;

    // 坑4：先订阅 Activated 再 bind，防 bind 到订阅之间的触发丢失。
    let shortcuts = with_deadline(GlobalShortcuts::new())
        .await
        .ok_or_else(|| "连接 portal 超时（8s）".to_string())?
        .map_err(|error| describe_error(&error))?;
    let session = with_deadline(shortcuts.create_session())
        .await
        .ok_or_else(|| "创建 portal 会话超时（8s）".to_string())?
        .map_err(|error| describe_error(&error))?;
    let activated = with_deadline(shortcuts.receive_activated())
        .await
        .ok_or_else(|| "订阅热键事件超时（8s）".to_string())?
        .map_err(|error| describe_error(&error))?;
    // ShortcutsChanged 订阅失败可容忍（改绑提醒是增强，不是主链路）。
    let changed = with_deadline(shortcuts.receive_shortcuts_changed())
        .await
        .and_then(|result| result.ok());
    // 信号流可能非 Unpin，箱钉一次省去跨循环的手动 pin 管理。
    let mut activated = Box::pin(activated);
    let mut changed: Option<std::pin::Pin<Box<_>>> = changed.map(Box::pin);

    // bind 等待用户在桌面端确认授权，不设超时（坑1）；等待前先报状态供 UI 引导。
    let _ = event_tx.send(QuakePortalEvent::AwaitingConsent);
    let request = shortcuts
        .bind_shortcuts(
            &session,
            &[
                NewShortcut::new(QUAKE_SHORTCUT_ID, "呼出/收起 LaterTerm 主窗口（Quake）")
                    .preferred_trigger(QUAKE_PREFERRED_TRIGGER),
            ],
            None,
        )
        .await
        .map_err(|error| describe_error(&error))?;
    // 用户拒绝授权时 response 返回 Response 错误，同样按注册失败走重试/降级。
    let bind = request.response().map_err(|error| describe_error(&error))?;
    let trigger = bind
        .shortcuts()
        .iter()
        .find(|shortcut| shortcut.id() == QUAKE_SHORTCUT_ID)
        .map(|shortcut| shortcut.trigger_description().to_string())
        .unwrap_or_default();
    let _ = event_tx.send(QuakePortalEvent::Ready { trigger });

    // 事件循环：Activated / ShortcutsChanged / 停止 三路赛跑，按设计长活。
    // 流结束按 classify_stream_exit 分流：用户停止静默退出，portal 断连发
    // Failed 让状态行可见（旧实现统一归 Stop 静默退出，热键无声失效）。
    enum WorkerStep {
        Activated,
        ShortcutsChanged(ashpd::desktop::global_shortcuts::ShortcutsChanged),
        Exit(WorkerExit),
    }
    loop {
        let step = futures_lite::future::race(
            futures_lite::future::race(
                async {
                    match activated.as_mut().next().await {
                        Some(_) => WorkerStep::Activated,
                        // 信号流结束 = 会话/连接断开或停止令牌已到位
                        None => WorkerStep::Exit(classify_stream_exit(stop_requested(stop_rx))),
                    }
                },
                async {
                    let _ = stop_rx.recv_async().await;
                    WorkerStep::Exit(WorkerExit::NormalStop)
                },
            ),
            async {
                match changed.as_mut() {
                    Some(stream) => match stream.as_mut().next().await {
                        Some(event) => WorkerStep::ShortcutsChanged(event),
                        None => WorkerStep::Exit(classify_stream_exit(stop_requested(stop_rx))),
                    },
                    None => std::future::pending().await,
                }
            },
        )
        .await;
        match step {
            WorkerStep::Activated => {
                if event_tx.send(QuakePortalEvent::Toggled).is_err() {
                    return Ok(()); // 主线程已弃用（workspace 关闭），自然退出
                }
            }
            WorkerStep::ShortcutsChanged(event) => {
                if let Some(shortcut) = event
                    .shortcuts()
                    .iter()
                    .find(|shortcut| shortcut.id() == QUAKE_SHORTCUT_ID)
                {
                    let _ = event_tx.send(QuakePortalEvent::TriggerUpdated {
                        trigger: shortcut.trigger_description().to_string(),
                    });
                }
            }
            WorkerStep::Exit(WorkerExit::NormalStop) => return Ok(()),
            WorkerStep::Exit(WorkerExit::PortalDisconnected) => {
                // 运行期断连不自动重试（注册期 1s 重试语义不适用于已建会话）；
                // 发 Failed 让用户可见，恢复走既有开关重开路径——关：Drop 停止
                // 句柄复位 Disabled；开：workspace 重新 spawn worker。
                let _ = event_tx.send(QuakePortalEvent::Failed {
                    reason: PORTAL_DISCONNECT_REASON.to_string(),
                });
                return Ok(());
            }
        }
    }
}

/// 给 future 套统一 deadline；超时返回 None。
#[cfg(target_os = "linux")]
async fn with_deadline<F: std::future::Future>(future: F) -> Option<F::Output> {
    futures_lite::future::race(async { Some(future.await) }, async {
        async_io::Timer::after(REGISTRATION_DEADLINE).await;
        None
    })
    .await
}

#[cfg(target_os = "linux")]
fn stop_requested(stop_rx: &flume::Receiver<()>) -> bool {
    match stop_rx.try_recv() {
        Ok(()) => true,
        Err(flume::TryRecvError::Disconnected) => true,
        Err(flume::TryRecvError::Empty) => false,
    }
}

/// ashpd 错误 → 状态行可读文案。
#[cfg(target_os = "linux")]
fn describe_error(error: &ashpd::Error) -> String {
    match error {
        ashpd::Error::PortalNotFound(_) => {
            "系统未提供 GlobalShortcuts portal（需 xdg-desktop-portal 前端支持）".to_string()
        }
        ashpd::Error::Response(_) => "桌面端拒绝或取消了授权".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn machine(auto_hide: bool) -> QuakeMachine {
        QuakeMachine::new(auto_hide)
    }

    fn enabled(auto_hide: bool) -> QuakeMachine {
        let mut machine = machine(auto_hide);
        assert_eq!(
            machine.handle(QuakeInput::SetEnabled(true)),
            QuakeAction::None
        );
        machine
    }

    /// 从初始态走到「Shown 且刚呼出」：首次 Toggle 是收起（初始 Shown，
    /// 窗口进程存活期恒存在），第二次才是呼出。
    fn summoned(auto_hide: bool) -> QuakeMachine {
        let mut quake = enabled(auto_hide);
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(0) }),
            QuakeAction::Minimize
        );
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(10) }),
            QuakeAction::Activate
        );
        quake
    }

    #[test]
    fn first_toggle_hides_then_toggles_summon_and_hide() {
        // 初始 Shown（窗口恒存在）：首次热键收起，之后呼出/收起幂等翻转。
        let mut quake = enabled(true);
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(0) }),
            QuakeAction::Minimize
        );
        assert!(quake.is_hidden());
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(100) }),
            QuakeAction::Activate
        );
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(200) }),
            QuakeAction::Minimize
        );
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(300) }),
            QuakeAction::Activate
        );
        assert!(!quake.is_hidden());
    }

    #[test]
    fn machine_starts_disabled_until_explicitly_enabled() {
        // 回归钉子（2026-11-06 复核）：机器出厂即禁用，生产接线必须显式喂
        // SetEnabled(true)——漏喂时 Toggled/失焦全部被禁用兜底吞掉，portal
        // 授权成功、状态行「已绑定」但热键整体 inert。
        let mut quake = machine(true);
        assert!(!quake.is_enabled());
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(0) }),
            QuakeAction::None
        );
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(10_000) }),
            QuakeAction::None
        );
        // set_auto_hide 不触碰启用门
        quake.set_auto_hide(false);
        assert!(!quake.is_enabled());
        // 显式启用后立即响应，且重复启用不产生动作
        assert_eq!(
            quake.handle(QuakeInput::SetEnabled(true)),
            QuakeAction::None
        );
        assert!(quake.is_enabled());
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(10) }),
            QuakeAction::Minimize
        );
        assert_eq!(
            quake.handle(QuakeInput::SetEnabled(true)),
            QuakeAction::None,
            "重复 SetEnabled(true) 不得产生窗口动作"
        );
    }

    #[test]
    fn disabled_ignores_all_inputs() {
        let mut quake = machine(true);
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(0) }),
            QuakeAction::None
        );
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(1000) }),
            QuakeAction::None
        );
        assert_eq!(quake.handle(QuakeInput::FocusGained), QuakeAction::None);
    }

    #[test]
    fn focus_lost_within_grace_is_absorbed() {
        let mut quake = summoned(true);
        quake.handle(QuakeInput::FocusGained);
        // 宽限期内（合成器置焦延迟/Alt-Tab 折返）不收起
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(310) }),
            QuakeAction::None
        );
        // 宽限期一过照常收起
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(710) }),
            QuakeAction::Minimize
        );
    }

    #[test]
    fn focus_lost_without_seen_focused_never_minimizes() {
        // Wayland activate 被合成器拒绝的场景：呼出后从未观测到焦点，
        // 失焦轮询读到的恒为「非活动」——绝不能把刚呼出的窗口收回去。
        let mut quake = summoned(true);
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(5_000) }),
            QuakeAction::None
        );
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(60_000) }),
            QuakeAction::None
        );
        // 显式 Toggle 仍可用（用户主动收起不受影响）
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(60_100) }),
            QuakeAction::Minimize
        );
    }

    #[test]
    fn auto_hide_off_ignores_focus_lost() {
        let mut quake = summoned(false);
        quake.handle(QuakeInput::FocusGained);
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(10_000) }),
            QuakeAction::None
        );
    }

    #[test]
    fn focus_lost_while_hidden_is_ignored() {
        let mut quake = enabled(true);
        quake.handle(QuakeInput::PortalToggle { now: ms(0) }); // 初始 Shown → 收起
        assert!(quake.is_hidden());
        quake.handle(QuakeInput::FocusGained);
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(10_000) }),
            QuakeAction::None
        );
    }

    #[test]
    fn set_enabled_false_resets_state() {
        let mut quake = summoned(true);
        quake.handle(QuakeInput::FocusGained);
        assert_eq!(
            quake.handle(QuakeInput::SetEnabled(false)),
            QuakeAction::None
        );
        assert!(!quake.is_hidden());
        // 重新启用后是全新周期：seen_focused/shown_at 均已清空
        assert_eq!(
            quake.handle(QuakeInput::SetEnabled(true)),
            QuakeAction::None
        );
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(10_000) }),
            QuakeAction::None
        );
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(10_100) }),
            QuakeAction::Minimize
        );
        assert_eq!(
            quake.handle(QuakeInput::PortalToggle { now: ms(10_200) }),
            QuakeAction::Activate
        );
    }

    #[test]
    fn focus_gained_only_counts_while_shown() {
        let mut quake = enabled(true);
        // 初始 Shown（窗口可见且聚焦）时的焦点计入
        quake.handle(QuakeInput::FocusGained);
        quake.handle(QuakeInput::PortalToggle { now: ms(0) }); // 收起，标记清除
        // 最小化后的焦点不计入（任务栏点回）
        quake.handle(QuakeInput::FocusGained);
        quake.handle(QuakeInput::PortalToggle { now: ms(10) }); // 呼出
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(10_010) }),
            QuakeAction::None,
            "Hidden 期观测到的焦点不得带入新呼出周期"
        );
        quake.handle(QuakeInput::FocusGained);
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(10_620) }),
            QuakeAction::Minimize
        );
    }

    #[test]
    fn re_shown_requires_fresh_focus_before_blur_minimize() {
        let mut quake = summoned(true);
        quake.handle(QuakeInput::FocusGained);
        quake.handle(QuakeInput::PortalToggle { now: ms(20) }); // 收起
        quake.handle(QuakeInput::PortalToggle { now: ms(30) }); // 再呼出
        // 上一周期的焦点标记已被清除，未重新聚焦前失焦不得收起
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(1_030) }),
            QuakeAction::None
        );
        quake.handle(QuakeInput::FocusGained);
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(2_030) }),
            QuakeAction::Minimize
        );
    }

    #[test]
    fn set_auto_hide_takes_effect_immediately() {
        let mut quake = summoned(true);
        quake.handle(QuakeInput::FocusGained);
        quake.set_auto_hide(false);
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(1_010) }),
            QuakeAction::None
        );
        quake.set_auto_hide(true);
        assert_eq!(
            quake.handle(QuakeInput::FocusLost { now: ms(2_010) }),
            QuakeAction::Minimize
        );
    }

    #[test]
    fn portal_status_text_covers_all_states() {
        assert!(QuakePortalStatus::Disabled.text().contains("未启用"));
        assert!(QuakePortalStatus::AwaitingConsent.text().contains("授权"));
        assert!(
            QuakePortalStatus::Ready("F12".into())
                .text()
                .contains("F12")
        );
        assert!(
            QuakePortalStatus::Ready(String::new())
                .text()
                .contains("未回报")
        );
        assert!(
            QuakePortalStatus::Failed("x".into())
                .text()
                .contains("降级")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stream_exit_classifies_disconnect_vs_user_stop() {
        // 回归背景（2026-11-06）：信号流结束曾统一归入通用 Stop 静默退出，
        // portal 会话拆除后热键无声失效且状态行停留「已绑定」。现在按停止
        // 令牌是否被请求分流：未被请求 = 断连（发 Failed），已请求 = 正常退出。
        assert!(matches!(
            classify_stream_exit(false),
            WorkerExit::PortalDisconnected
        ));
        assert!(matches!(classify_stream_exit(true), WorkerExit::NormalStop));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portal_disconnect_reason_feeds_failed_status_text() {
        // 钉住断连文案能经既有 Failed 状态行（workspace enabled 门内）可见；
        // 前缀已含「portal 不可用」，reason 本身不再重复 portal 字样。
        let text = QuakePortalStatus::Failed(PORTAL_DISCONNECT_REASON.into()).text();
        assert!(text.contains("断开"), "{text}");
        assert!(text.contains("重新开启"), "{text}");
        // 前缀已有「portal 不可用」，reason 不重复 portal 字样（全文恰 1 次）
        assert_eq!(
            text.matches("portal").count(),
            1,
            "仅前缀含 portal，reason 不得叠加: {text}"
        );
    }
}
