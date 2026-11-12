//! SSH 端口转发（隧道）：本地转发（L，`ssh -L`）与远程转发（R，`ssh -R`）。
//!
//! 结构：[`ForwardHub`] 在会话建立后按 [`TunnelSpec`] 建隧道——
//! - L：本机 `TcpListener` accept → 目标机 `channel_open_direct_tcpip` →
//!   `copy_bidirectional` 双向泵（经跳板链时同样打在**目标机** Handle 上，
//!   跳板穿透自动成立）；
//! - R：目标机 `tcpip_forward` 全局请求 → 服务端连入时主动开
//!   forwarded-tcpip 通道 → [`RemoteForwardRouter`] 按
//!   `(connected_address, connected_port)` 二元组路由到本机目标。
//!
//! **并发纪律（写死，勿回归成内联）**：R 转发回调（lib.rs 的
//! `ClientHandler::server_channel_open_forwarded_tcpip`）在 russh 会话消息
//! 循环内联 await（russh 0.65 encrypted.rs:925-936 `.await?` 实锤）——回调
//! 只做 accept + `tokio::spawn` 泵任务 + 立即返回，绝不在回调内连接本地
//! 目标或等待泵；`SetTunnels` 的 diff 是多次全局请求往返，同样 spawn 出去
//! 执行（16 条隧道 × 高 RTT 内联展开必卡 Shutdown）。
//!
//! 通道终结口径：泵随任务结束 drop，`ChannelStream` 内的
//! `ChannelCloseOnDrop` 发 CHANNEL_CLOSE（本地信号终结，绝不等待远端回包，
//! 与 shell/SFTP 泵同口径，见 docs/roadmap.md russh 0.65 Close 缺口注记）。

use crate::ClientHandler;
use russh::client::{Handle, Msg};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::TcpStream;

/// 隧道事件缓冲（SshSession 持有，`take_port_forward_events` 一次性取走）。
pub(crate) type EventSink = Arc<Mutex<Vec<PortForwardEvent>>>;

/// 转发方向（与配置层 `laterterm_config::TunnelKind` 同形，序列化边界在本
/// crate 之外——app 层负责转换，本类型只做连接期语义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunnelKind {
    /// 本地转发：本机监听 bind，经目标机连 target。
    Local,
    /// 远程转发：目标机侧监听 bind，回连本机侧 target。
    Remote,
}

/// 一条隧道连接期规格（enabled 项已在 app 层过滤）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelSpec {
    pub kind: TunnelKind,
    pub bind_host: String,
    pub bind_port: u16,
    pub target_host: String,
    pub target_port: u16,
}

impl TunnelSpec {
    /// 展示/日志/错误文案用标签：`L 127.0.0.1:8080→db:5432`。
    pub fn label(&self) -> String {
        let kind = match self.kind {
            TunnelKind::Local => "L",
            TunnelKind::Remote => "R",
        };
        format!(
            "{kind} {}:{}→{}:{}",
            self.bind_host, self.bind_port, self.target_host, self.target_port
        )
    }
}

/// 单条隧道的运行态（窗格 watcher 收敛进 UI chip）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TunnelState {
    /// 建立（或重估）成功；`connections` 为当前经隧道泵着的连接数。
    Active { connections: usize },
    /// 运行期故障：只发事件+UI 红态，不杀会话（连接期失败走整体错误路径）。
    Failed(String),
    /// 已停止（shutdown / diff 撤销）。
    Stopped,
}

/// 端口转发事件：hub 投递 → 窗格 watcher 每秒 drain。
#[derive(Clone, Debug)]
pub enum PortForwardEvent {
    /// 隧道进入/刷新活跃态（连接数变化也经此刷新）。
    Active {
        spec: TunnelSpec,
        connections: usize,
    },
    /// 隧道失败（运行期软失败）。
    Failed { spec: TunnelSpec, message: String },
    /// 隧道停止。
    Stopped { spec: TunnelSpec },
}

fn push_event(events: &EventSink, event: PortForwardEvent) {
    events.lock().unwrap().push(event);
}

/// R 转发路由器：把服务端主动开来的 forwarded-tcpip 通道接到对应隧道的
/// 本机目标。按 `(connected_address, connected_port)` 二元组建键——仅按端口
/// 单键会把 `127.0.0.1:8080` 与 `0.0.0.0:8080` 两条合法共存的隧道路由串线。
pub(crate) struct RemoteForwardRouter {
    events: EventSink,
    routes: Mutex<HashMap<RouteKey, RouteEntry>>,
}

/// 路由键：服务器回传的 connected_address 是其记录的请求串（建议配置用
/// IP 字面量，主机名形式可能不逐字回传 → 未命中 = 通道被拒，不串线）。
type RouteKey = (String, u16);

struct RouteEntry {
    spec: TunnelSpec,
    /// 当前经该隧道泵着的连入连接数（回调路径计数）。
    connections: Arc<AtomicUsize>,
}

impl RemoteForwardRouter {
    pub(crate) fn new(events: EventSink) -> Self {
        Self {
            events,
            routes: Mutex::new(HashMap::new()),
        }
    }

    /// 登记一条 R 路由（`port` 为服务器回执的实占端口，0 请求时可能是
    /// 服务器自选值）。
    pub(crate) fn register(&self, address: &str, port: u32, spec: TunnelSpec) {
        self.routes.lock().unwrap().insert(
            (address.to_string(), port as u16),
            RouteEntry {
                spec,
                connections: Arc::new(AtomicUsize::new(0)),
            },
        );
    }

    pub(crate) fn unregister(&self, address: &str, port: u32) {
        self.routes
            .lock()
            .unwrap()
            .remove(&(address.to_string(), port as u16));
    }

    pub(crate) fn clear(&self) {
        self.routes.lock().unwrap().clear();
    }

    /// 回调路径：按 (address, port) 查路由并计一次连入；`None` = 未命中，
    /// 调用方丢弃 reply 拒绝通道（可预期，不串线）。
    pub(crate) fn accept_incoming(&self, address: &str, port: u32) -> Option<TunnelSpec> {
        let (spec, connections) = {
            let mut routes = self.routes.lock().unwrap();
            let entry = routes.get_mut(&(address.to_string(), port as u16))?;
            let connections = entry.connections.fetch_add(1, Ordering::Relaxed) + 1;
            (entry.spec.clone(), connections)
        };
        push_event(
            &self.events,
            PortForwardEvent::Active {
                spec: spec.clone(),
                connections,
            },
        );
        Some(spec)
    }

    /// 泵结束路径：连接数回落并刷新事件。
    pub(crate) fn connection_closed(&self, address: &str, port: u32) {
        let mut routes = self.routes.lock().unwrap();
        let Some(entry) = routes.get_mut(&(address.to_string(), port as u16)) else {
            return;
        };
        let remaining = entry
            .connections
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        let spec = entry.spec.clone();
        drop(routes);
        push_event(
            &self.events,
            PortForwardEvent::Active {
                spec,
                connections: remaining,
            },
        );
    }

    /// 当前在泵连接数（仅测试断言用，生产路径走事件流）。
    #[cfg(test)]
    fn live_count(&self, address: &str, port: u32) -> Option<usize> {
        self.routes
            .lock()
            .unwrap()
            .get(&(address.to_string(), port as u16))
            .map(|entry| entry.connections.load(Ordering::Relaxed))
    }
}

/// 端口转发集线器：会话生命周期内持有，负责 L/R 隧道的建立、运行期事件与
/// 退出序（cancel R → 停 L）。全部方法 `&self`，内部 `Mutex` 收敛——
/// `SetTunnels` diff 与 shutdown 可并发。
pub(crate) struct ForwardHub {
    handle: Arc<Handle<ClientHandler>>,
    events: EventSink,
    router: Arc<RemoteForwardRouter>,
    inner: Mutex<HubInner>,
    /// 关闭标记：shutdown 后 apply_diff 直接放弃（防退出序与 diff 竞态复活隧道）。
    closed: AtomicBool,
}

#[derive(Default)]
struct HubInner {
    /// L 隧道 accept 循环任务（abort 即停监听；泵任务是独立 spawn，随流自然死）。
    local_tasks: Vec<(TunnelSpec, tokio::task::JoinHandle<()>)>,
    /// R 隧道活跃表：(spec, 服务器回执的实占端口)。
    remote_active: Vec<(TunnelSpec, u32)>,
}

impl ForwardHub {
    pub(crate) fn new(
        handle: Arc<Handle<ClientHandler>>,
        events: EventSink,
        router: Arc<RemoteForwardRouter>,
    ) -> Self {
        Self {
            handle,
            events,
            router,
            inner: Mutex::new(HubInner::default()),
            closed: AtomicBool::new(false),
        }
    }

    /// 建立全部隧道（顺序执行；任一失败 = 连接期硬失败，先撤已完成部分再
    /// 返回错误——L 的 accept 任务持有 Handle 克隆，不撤会留僵尸连接）。
    pub(crate) async fn establish(&self, specs: Vec<TunnelSpec>) -> std::io::Result<()> {
        for spec in specs {
            if let Err(error) = self.start_tunnel(&spec).await {
                self.shutdown().await;
                return Err(error);
            }
        }
        Ok(())
    }

    /// 运行期启停（`SessionCmd::SetTunnels` 的 diff，软失败口径）。
    pub(crate) async fn apply_diff(&self, specs: Vec<TunnelSpec>) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let current: Vec<TunnelSpec> = {
            let inner = self.inner.lock().unwrap();
            inner
                .local_tasks
                .iter()
                .map(|(spec, _)| spec.clone())
                .chain(inner.remote_active.iter().map(|(spec, _)| spec.clone()))
                .collect()
        };
        for spec in &specs {
            if current.iter().any(|active| active == spec) {
                continue;
            }
            if let Err(error) = self.start_tunnel(spec).await {
                push_event(
                    &self.events,
                    PortForwardEvent::Failed {
                        spec: spec.clone(),
                        message: error.to_string(),
                    },
                );
            }
        }
        for active in &current {
            if !specs.contains(active) {
                self.stop_tunnel(active).await;
            }
        }
    }

    /// 建单条隧道：L = bind + accept 循环；R = tcpip_forward + 路由登记。
    async fn start_tunnel(&self, spec: &TunnelSpec) -> std::io::Result<()> {
        match spec.kind {
            TunnelKind::Local => self.start_local(spec).await,
            TunnelKind::Remote => self.start_remote(spec).await,
        }
    }

    async fn start_local(&self, spec: &TunnelSpec) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::bind((spec.bind_host.as_str(), spec.bind_port))
            .await
            .map_err(|error| {
                let reason = if error.kind() == std::io::ErrorKind::AddrInUse {
                    "端口被占用"
                } else {
                    "监听失败"
                };
                std::io::Error::other(format!("隧道 {} {reason}: {error}", spec.label()))
            })?;
        let handle = Arc::clone(&self.handle);
        let events = Arc::clone(&self.events);
        let task_spec = spec.clone();
        let connections = Arc::new(AtomicUsize::new(0));
        // accept 循环独立成任务：accept + 每连接 spawn 泵，绝不阻塞调用方
        let task = tokio::spawn(async move {
            let spec = task_spec;
            loop {
                match listener.accept().await {
                    Ok((tcp, peer)) => {
                        let handle = Arc::clone(&handle);
                        let spec = spec.clone();
                        let events = Arc::clone(&events);
                        let connections = Arc::clone(&connections);
                        let live = connections.fetch_add(1, Ordering::Relaxed) + 1;
                        push_event(
                            &events,
                            PortForwardEvent::Active {
                                spec: spec.clone(),
                                connections: live,
                            },
                        );
                        // 泵任务：direct-tcpip 打在目标机 Handle 上（经跳板链
                        // 时穿透自动成立）；失败回落计数并继续 accept
                        tokio::spawn(async move {
                            let opened = handle
                                .channel_open_direct_tcpip(
                                    spec.target_host.as_str(),
                                    spec.target_port as u32,
                                    peer.ip().to_string(),
                                    peer.port() as u32,
                                )
                                .await;
                            match opened {
                                Ok(channel) => pump(channel.into_stream(), tcp).await,
                                Err(error) => {
                                    eprintln!("隧道 {} 通道打开失败: {error}", spec.label());
                                }
                            }
                            let remaining = connections
                                .fetch_sub(1, Ordering::Relaxed)
                                .saturating_sub(1);
                            push_event(
                                &events,
                                PortForwardEvent::Active {
                                    spec,
                                    connections: remaining,
                                },
                            );
                        });
                    }
                    Err(error) => {
                        // listener 死了（进程 fd 异常等）：软失败退出循环
                        push_event(
                            &events,
                            PortForwardEvent::Failed {
                                spec,
                                message: format!("接受连接失败: {error}"),
                            },
                        );
                        break;
                    }
                }
            }
        });
        self.inner
            .lock()
            .unwrap()
            .local_tasks
            .push((spec.clone(), task));
        push_event(
            &self.events,
            PortForwardEvent::Active {
                spec: spec.clone(),
                connections: 0,
            },
        );
        Ok(())
    }

    async fn start_remote(&self, spec: &TunnelSpec) -> std::io::Result<()> {
        // tcpip_forward 每条 1 次全局请求往返；拒绝 = 连接期硬失败
        let assigned = self
            .handle
            .tcpip_forward(&spec.bind_host, spec.bind_port as u32)
            .await
            .map_err(|error| {
                std::io::Error::other(format!("隧道 {} 远端拒绝转发: {error}", spec.label()))
            })?;
        let bound_port = if assigned == 0 {
            spec.bind_port as u32
        } else {
            assigned
        };
        self.router
            .register(&spec.bind_host, bound_port, spec.clone());
        self.inner
            .lock()
            .unwrap()
            .remote_active
            .push((spec.clone(), bound_port));
        push_event(
            &self.events,
            PortForwardEvent::Active {
                spec: spec.clone(),
                connections: 0,
            },
        );
        Ok(())
    }

    /// 停单条隧道（diff 撤销路径，尽力而为）。
    async fn stop_tunnel(&self, spec: &TunnelSpec) {
        match spec.kind {
            TunnelKind::Local => {
                // 锁纪律：position 与 remove 必须同住一把锁的作用域——
                // ① 写成 `lock().unwrap().iter().position(..).map(|i|
                // self.inner.lock()...)` 时，第一个 MutexGuard 是语句级
                // 临时值（存活到分号），闭包内第二次 lock() 即自死锁
                // （网络线程冻结，shell/resize/shutdown 全堵）；② 即便拆
                // 成两条语句，apply_diff 与 shutdown 可并发，两次加锁之间
                // 索引会被 mem::take/remove 移位——remove 错任务甚至越界
                // panic。abort 放锁外（JoinHandle::abort 不需要锁）。
                let task = {
                    let mut inner = self.inner.lock().unwrap();
                    inner
                        .local_tasks
                        .iter()
                        .position(|(active, _)| active == spec)
                        .map(|index| inner.local_tasks.remove(index).1)
                };
                if let Some(task) = task {
                    task.abort();
                }
            }
            TunnelKind::Remote => {
                let port = self
                    .inner
                    .lock()
                    .unwrap()
                    .remote_active
                    .iter()
                    .find(|(active, _)| active == spec)
                    .map(|(_, port)| *port);
                if let Some(port) = port {
                    let _ = self
                        .handle
                        .cancel_tcpip_forward(&spec.bind_host, port)
                        .await;
                    self.router.unregister(&spec.bind_host, port);
                    self.inner
                        .lock()
                        .unwrap()
                        .remote_active
                        .retain(|(active, _)| active != spec);
                }
            }
        }
        push_event(
            &self.events,
            PortForwardEvent::Stopped { spec: spec.clone() },
        );
    }

    /// 退出序第一步（之后才 disconnect 目标机 → 跳板逆序）：cancel R（逆序）
    /// → abort L 监听。幂等；已 close 后再调是空操作。
    pub(crate) async fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let (local_tasks, remote_active) = {
            let mut inner = self.inner.lock().unwrap();
            (
                std::mem::take(&mut inner.local_tasks),
                std::mem::take(&mut inner.remote_active),
            )
        };
        // 先撤 R（全局请求往返），再停 L 监听（abort 毫秒级）——撤 R 时 L
        // 还活着可减小「撤一半被掐」窗口
        for (spec, port) in remote_active.iter().rev() {
            let _ = self
                .handle
                .cancel_tcpip_forward(&spec.bind_host, *port)
                .await;
            self.router.unregister(&spec.bind_host, *port);
            push_event(
                &self.events,
                PortForwardEvent::Stopped { spec: spec.clone() },
            );
        }
        for (spec, task) in local_tasks {
            task.abort();
            push_event(&self.events, PortForwardEvent::Stopped { spec });
        }
        self.router.clear();
    }
}

/// 双向泵：`ChannelStream ↔ TcpStream`。泵结束（任一侧半关闭/断开）时任务
/// drop，通道经 `ChannelCloseOnDrop` 发 CHANNEL_CLOSE——本地信号终结，
/// 绝不等待远端回包（russh 0.65 Close 缺口口径，见 roadmap）。
pub(crate) async fn pump(mut channel: russh::ChannelStream<Msg>, mut tcp: TcpStream) {
    let _ = tokio::io::copy_bidirectional(&mut channel, &mut tcp).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: TunnelKind, bind: &str, port: u16) -> TunnelSpec {
        TunnelSpec {
            kind,
            bind_host: bind.into(),
            bind_port: port,
            target_host: "db.internal".into(),
            target_port: 5432,
        }
    }

    #[test]
    fn tunnel_spec_label_marks_kind_and_endpoints() {
        assert_eq!(
            spec(TunnelKind::Local, "127.0.0.1", 8080).label(),
            "L 127.0.0.1:8080→db.internal:5432"
        );
        assert_eq!(
            spec(TunnelKind::Remote, "0.0.0.0", 8443).label(),
            "R 0.0.0.0:8443→db.internal:5432"
        );
    }

    #[test]
    fn router_keys_by_address_and_port_pair_not_port_alone() {
        // 评审意见 4：127.0.0.1:8080 与 0.0.0.0:8080 两条合法共存，仅按端口
        // 单键会串线；未命中 = None（调用方拒绝通道）
        let events: EventSink = Arc::default();
        let router = RemoteForwardRouter::new(Arc::clone(&events));
        router.register(
            "127.0.0.1",
            8080,
            spec(TunnelKind::Remote, "127.0.0.1", 8080),
        );
        router.register("0.0.0.0", 8080, spec(TunnelKind::Remote, "0.0.0.0", 8080));

        let hit = router
            .accept_incoming("127.0.0.1", 8080)
            .expect("按地址+端口应命中");
        assert_eq!(hit.bind_host, "127.0.0.1");
        assert!(
            router.accept_incoming("missing", 8080).is_none(),
            "未命中拒绝"
        );
        assert!(router.accept_incoming("127.0.0.1", 9999).is_none());

        // 连入/结束计数：accept +1 → close 回 0，Active 事件随之刷新
        assert_eq!(router.live_count("127.0.0.1", 8080), Some(1));
        router.connection_closed("127.0.0.1", 8080);
        assert_eq!(router.live_count("127.0.0.1", 8080), Some(0));
        let active = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, PortForwardEvent::Active { .. }))
            .count();
        assert_eq!(active, 2, "accept 与 close 各刷一次 Active: {active}");
    }
}
