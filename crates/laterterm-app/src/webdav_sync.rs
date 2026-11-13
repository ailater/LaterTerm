//! WebDAV 配置同步（手动触发）：快照上传三步序 + 远程合并编排。
//!
//! 结构对标 update_check.rs：ureq 3 同步 API、失败原文呈现、绝不把失败
//! 翻译成成功。本模块只做「可注入 `WebDav` trait 的编排」，主线程收发
//! （幂等门 + 实体更新回写）在 workspace.rs 的 run_webdav_task。
//!
//! ## 为什么经 ureq::http::Request + RequestExt 发任意方法
//!
//! PROPFIND / MOVE 不是 ureq 的内置便捷方法（agent.rs 只有 get/put/head/
//! post/…）。ureq 3 把 http crate re-export 为 `ureq::http`（ureq src/lib.rs
//! `pub use ureq_proto::http`），并对 `http::Request<S: AsSendBody>` 提供
//! `RequestExt::with_agent(&agent).run()`——因此**不新增 http 直接依赖**，
//! Cargo.lock 归一同为 ureq 生态内的版本。Agent 配置同 update_check.rs：
//! rustls feature 内嵌根证书、`http_status_as_error(false)` 由本模块自判
//! 状态码、全局超时封顶。
//!
//! ## 密码红线
//!
//! 密码只进系统密钥环（laterterm-secrets `webdav/password`），由调用方在
//! **后台线程**经 `run_task` 注入 [`WebDavAuth`]；错误文案与日志绝不携带
//! 密码（[`WebDavAuth`] 手写 Debug 打码；ureq 错误为 IO/URL/协议层，不含
//! 请求头）。
//!
//! ## 上传三步序（主档不空窗）
//!
//! PUT 临时名 → 主档 MOVE 到时间戳保留档（404 = 首次备份，静默跳过）→
//! 临时档 MOVE 转正。任一步失败时主档路径不空窗、不覆盖；另一台机器并发
//! 「从远程合并」不会因主档临时缺位而 404。保留档持续累积（无自动清理——
//! 需要 PROPFIND 枚举 + 207 Multi-Status XML 解析，v1 拒绝该复杂度），
//! UI 提示用户可手动清理。

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use laterterm_config::sync::{
    ConfigSnapshot, compact_timestamp, format_utc_seconds, is_safe_theme_name, merge_settings,
    now_unix_seconds,
};
use laterterm_config::{Settings, themes_dir as default_themes_dir};
use laterterm_secrets::SecretStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
// PROPFIND/MOVE 等任意方法经 http::Request 发送：RequestExt 提供
// with_agent().run()（ureq 对 http::Request<S: AsSendBody> 的扩展）。
use ureq::RequestExt;

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::Mutex;

/// 单次请求上限。上传比更新检查（8s）大，取 15s；DNS 极端挂死场景下同步
/// getaddrinfo 无法被超时中断，是同步选型的已知边界（update_check.rs 同款）。
const TIMEOUT: Duration = Duration::from_secs(15);

/// 远端快照主档名（相对 WebDAV 目录；文件名固定格式纯 ASCII，无需转义）。
pub(crate) const SNAPSHOT_MAIN: &str = "laterterm-config-snapshot.toml";

// ---------------------------------------------------------------------------
// 凭据与错误
// ---------------------------------------------------------------------------

/// Basic auth 凭据。只在后台线程内存中出现，绝不落盘/入日志。
pub(crate) struct WebDavAuth {
    pub(crate) username: String,
    pub(crate) password: String,
}

impl WebDavAuth {
    /// `Authorization` 头值：`Basic base64("user:pass")`（RFC 7617）。
    fn authorization_header(&self) -> String {
        format!(
            "Basic {}",
            BASE64_STANDARD.encode(format!("{}:{}", self.username, self.password))
        )
    }
}

/// 手写 Debug：密码打码。错误路径一旦误打印凭据结构，日志里也不能出明文。
impl std::fmt::Debug for WebDavAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebDavAuth")
            .field("username", &self.username)
            .field("password", &"******")
            .finish()
    }
}

/// WebDAV 操作失败。变体即文案分类：401/403 归 Auth（账号或密码错误），
/// 其余状态码原样透出，网络层错误原文呈现——绝不把失败翻译成成功。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WebDavError {
    /// 401/403：认证失败。
    Auth,
    /// 404：资源不存在（首次备份时主档不存在属正常，调用方特判）。
    NotFound,
    /// 其它状态码。
    Status(u16),
    /// 网络层失败（DNS/连接/超时/协议），原文呈现。
    Network(String),
}

impl WebDavError {
    /// 可展示的中文文案（不含密码）。
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Auth => "认证失败（账号或密码错误）".into(),
            Self::NotFound => "远程资源不存在".into(),
            Self::Status(code) => format!("服务器返回 {code}"),
            Self::Network(detail) => format!("网络错误：{detail}"),
        }
    }
}

/// 状态码归一：凭据问题、缺失、其余三分。
fn status_to_error(status: u16) -> WebDavError {
    match status {
        401 | 403 => WebDavError::Auth,
        404 => WebDavError::NotFound,
        code => WebDavError::Status(code),
    }
}

// ---------------------------------------------------------------------------
// WebDav trait 与 ureq 生产实现
// ---------------------------------------------------------------------------

/// WebDAV 客户端契约。生产实现 [`UreqWebDav`]；测试用内存 Mock
/// （见 tests 模块）。
pub(crate) trait WebDav {
    /// 探测目录可达：PROPFIND Depth:0（207 = Multi-Status 可达），
    /// 服务器不支持 PROPFIND（405/501）时回退 OPTIONS 探活。
    fn probe(&self, auth: &WebDavAuth) -> Result<(), WebDavError>;
    /// 上传/覆盖文件（2xx = 成功）。
    fn put(&self, auth: &WebDavAuth, file: &str, body: &str) -> Result<(), WebDavError>;
    /// 读取文件全文（404 → NotFound）。
    fn get(&self, auth: &WebDavAuth, file: &str) -> Result<String, WebDavError>;
    /// 同目录改名/移动（MOVE + Destination 绝对地址；404 → NotFound）。
    fn move_resource(&self, auth: &WebDavAuth, from: &str, to: &str) -> Result<(), WebDavError>;
}

/// ureq 3 生产实现（同步 API，跑在后台执行器上）。
pub(crate) struct UreqWebDav {
    /// 目录地址（已去尾斜杠）；文件请求 = `{base_url}/{file}`。
    base_url: String,
    agent: ureq::Agent,
}

impl UreqWebDav {
    /// 校验地址并构造。Agent 配置与 update_check.rs 同款（rustls 内嵌根
    /// 证书 + 关状态码转错误 + 全局超时封顶）。
    pub(crate) fn new(base_url: &str) -> Result<Self, String> {
        let mut base_url = base_url.trim().to_string();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        if !base_url.starts_with("https://") && !base_url.starts_with("http://") {
            return Err("服务器地址必须以 https:// 或 http:// 开头".into());
        }
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(TIMEOUT))
                // 状态码由本模块自判：401 → 认证失败、404 → 首次备份特判、
                // 其余原样透出（关掉 ureq 默认的非 2xx 转 Err）
                .http_status_as_error(false)
                .build(),
        );
        Ok(Self { base_url, agent })
    }

    fn url_for(&self, file: &str) -> String {
        format!("{}/{file}", self.base_url)
    }

    /// 发任意方法请求。PROPFIND/MOVE 等 ureq 无内置便捷方法，统一走
    /// `ureq::http::Request` + `RequestExt::with_agent().run()`（re-export
    /// 见模块头注记）；GET/PUT 同路，全客户端一条请求构造路径。
    fn send(
        &self,
        method: &str,
        file: &str,
        auth: &WebDavAuth,
        body: Option<&str>,
        extra_headers: &[(&str, String)],
    ) -> Result<ureq::http::Response<ureq::Body>, WebDavError> {
        let mut builder = ureq::http::Request::builder()
            .method(method)
            .uri(self.url_for(file))
            .header("Authorization", auth.authorization_header())
            // 部分服务器（nginx dav 模块等）对无 UA 请求直接拒绝
            .header(
                "User-Agent",
                format!("LaterTerm/{}", env!("CARGO_PKG_VERSION")),
            );
        for (name, value) in extra_headers {
            builder = builder.header(*name, value);
        }
        let request = builder
            .body(body.unwrap_or(""))
            .map_err(|error| WebDavError::Network(format!("构造请求失败：{error}")))?;
        request
            .with_agent(&self.agent)
            .run()
            .map_err(|error| match error {
                // http_status_as_error(false) 下不该出现，防御性归一
                ureq::Error::StatusCode(code) => status_to_error(code),
                ureq::Error::HostNotFound => WebDavError::Network("域名解析失败".into()),
                ureq::Error::Timeout(_) => WebDavError::Network("请求超时（15 秒）".into()),
                other => WebDavError::Network(other.to_string()),
            })
    }
}

impl WebDav for UreqWebDav {
    fn probe(&self, auth: &WebDavAuth) -> Result<(), WebDavError> {
        let response = self.send("PROPFIND", "", auth, Some(""), &[("Depth", "0".into())])?;
        let status = response.status().as_u16();
        if status == 207 {
            // 207 Multi-Status：目录存在且服务端支持 PROPFIND
            return Ok(());
        }
        match status_to_error(status) {
            // 最小实现服务器可能没有 PROPFIND：回退 OPTIONS 只探连通性
            WebDavError::Status(405) | WebDavError::Status(501) => {
                let response = self.send("OPTIONS", "", auth, None, &[])?;
                let status = response.status().as_u16();
                if (200..300).contains(&status) {
                    Ok(())
                } else {
                    Err(status_to_error(status))
                }
            }
            other => Err(other),
        }
    }

    fn put(&self, auth: &WebDavAuth, file: &str, body: &str) -> Result<(), WebDavError> {
        let response = self.send("PUT", file, auth, Some(body), &[])?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(status_to_error(status))
        }
    }

    fn get(&self, auth: &WebDavAuth, file: &str) -> Result<String, WebDavError> {
        let mut response = self.send("GET", file, auth, None, &[])?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(status_to_error(status));
        }
        response
            .body_mut()
            .read_to_string()
            .map_err(|error| WebDavError::Network(format!("读取响应失败：{error}")))
    }

    fn move_resource(&self, auth: &WebDavAuth, from: &str, to: &str) -> Result<(), WebDavError> {
        // Destination 必须是绝对 URL（RFC 4918 §9.9.3）
        let extra = [
            ("Destination", self.url_for(to)),
            ("Overwrite", "T".to_string()),
        ];
        let response = self.send("MOVE", from, auth, None, &extra)?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(status_to_error(status))
        }
    }
}

// ---------------------------------------------------------------------------
// 任务模型（workspace 状态机/结果行的数据源）
// ---------------------------------------------------------------------------

/// 同步动作种类（设置面板三个按钮一一对应）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebDavTaskKind {
    Test,
    Upload,
    Merge,
}

/// 任务状态机：非 Idle 即在途，兼作按钮幂等门（连点不发第二次请求）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WebDavTaskState {
    #[default]
    Idle,
    Testing,
    Uploading,
    Merging,
}

impl WebDavTaskState {
    /// 在途与否。
    pub(crate) fn busy(self) -> bool {
        self != Self::Idle
    }
    /// 在途按钮的 muted 文案。
    pub(crate) fn busy_label(self) -> &'static str {
        match self {
            Self::Idle => "",
            Self::Testing => "测试中…",
            Self::Uploading => "备份中…",
            Self::Merging => "合并中…",
        }
    }
}

/// 任务产出（workspace 侧 webdav_result 的展示数据源）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SyncOutcome {
    /// 测试连接成功。
    ProbeOk,
    /// 备份完成；`archive` = 时间戳保留档名（首次备份 None）。
    BackupOk { archive: Option<String> },
    /// 合并完成：合并设置交工作区应用链路（落盘 + apply_loaded_settings），
    /// 差异报告进状态行明细。Settings 装箱——合并载荷远大于其余变体
    /// （clippy::large_enum_variant）。
    MergeApplied {
        settings: Box<Settings>,
        report: MergeReport,
    },
}

/// 合并差异报告（UI 差异明细渲染的数据源）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MergeReport {
    /// 远端新增键路径。
    pub added: Vec<String>,
    /// 远程覆盖键路径（服务器/命令等列表整体以远程为准）。
    pub overridden: Vec<String>,
    /// 远端新增主题（已落盘）。
    pub themes_added: Vec<String>,
    /// 远端覆盖主题（本地同名不同内容，远程胜；覆盖前整目录已备份）。
    pub themes_overwritten: Vec<String>,
    /// 本地 settings.toml 备份文件名（None = 本机无既有配置文件，无可备份）。
    pub settings_backup: Option<String>,
    /// 本地 themes 备份目录名（None = 本地无主题目录，无需备份）。
    pub themes_backup_dir: Option<String>,
}

// ---------------------------------------------------------------------------
// 编排（可注入 WebDav，离线可测）
// ---------------------------------------------------------------------------

/// 后台执行一次同步任务（跑在 background_executor 上）：密钥环读/写密码 →
/// 构造客户端 → 执行动作。`password_input` 非空 = 先写密钥环再用；空 = 取
/// 已存（UI「留空保持已存密码」）。返回可展示中文错误。
pub(crate) fn run_task(
    kind: WebDavTaskKind,
    url: &str,
    username: &str,
    password_input: &str,
    store: Arc<dyn SecretStore>,
    settings_path: &Path,
    themes_path: &Path,
) -> Result<SyncOutcome, String> {
    // 密码读写只发生在本函数（后台线程）内，绝不回传主线程/日志
    let account = laterterm_secrets::webdav_account().map_err(|error| error.to_string())?;
    let password = if password_input.is_empty() {
        store
            .get(&account)
            .map_err(|error| format!("读取已存密码失败：{error:#}"))?
            .ok_or("密钥环中没有已存密码，请先输入密码")?
    } else {
        store
            .set(&account, password_input)
            .map_err(|error| format!("保存密码到密钥环失败：{error:#}"))?;
        password_input.to_string()
    };
    let auth = WebDavAuth {
        username: username.to_string(),
        password,
    };
    let dav = UreqWebDav::new(url)?;
    match kind {
        WebDavTaskKind::Test => {
            dav.probe(&auth).map_err(|error| error.message())?;
            Ok(SyncOutcome::ProbeOk)
        }
        WebDavTaskKind::Upload => {
            let snapshot = build_upload_snapshot(settings_path, themes_path)?;
            let archive = backup_to_remote(
                &dav,
                &auth,
                &snapshot,
                &compact_timestamp(&format_utc_seconds(now_unix_seconds())),
            )
            .map_err(|error| error.message())?;
            Ok(SyncOutcome::BackupOk { archive })
        }
        WebDavTaskKind::Merge => {
            let (settings, report) = merge_from_remote(
                &dav,
                &auth,
                settings_path,
                themes_path,
                &compact_timestamp(&format_utc_seconds(now_unix_seconds())),
            )?;
            Ok(SyncOutcome::MergeApplied {
                settings: Box::new(settings),
                report,
            })
        }
    }
}

/// 组装上传快照：读本地 settings.toml 原文 + themes/ 逐文件内嵌（IO，
/// 由 run_task 在后台线程调用）。
pub(crate) fn build_upload_snapshot(
    settings_path: &Path,
    themes_path: &Path,
) -> Result<String, String> {
    let settings_raw = std::fs::read_to_string(settings_path)
        .map_err(|error| format!("读取本地设置失败：{error}"))?;
    let themes = read_local_themes(themes_path);
    let snapshot = ConfigSnapshot::build(
        settings_raw,
        themes,
        env!("CARGO_PKG_VERSION"),
        now_unix_seconds(),
    );
    snapshot.to_toml_string()
}

/// 上传三步序：PUT 临时名 → 主档挪时间戳保留档（404 = 首次备份静默跳过）
/// → 临时档转正。任一步失败主档路径不空窗、不覆盖；成功返回保留档名
/// （首次备份 None）。
pub(crate) fn backup_to_remote(
    dav: &dyn WebDav,
    auth: &WebDavAuth,
    snapshot_toml: &str,
    ts: &str,
) -> Result<Option<String>, WebDavError> {
    let tmp = format!("{SNAPSHOT_MAIN}.tmp-{ts}");
    let archive = format!("laterterm-config-snapshot-{ts}.toml");
    dav.put(auth, &tmp, snapshot_toml)?;
    // 主档挪保留档；首次备份主档不存在（404）属正常，静默跳过。
    // 此步失败时主档仍在原位未动（临时档残留无害，下次覆盖）。
    let archived = match dav.move_resource(auth, SNAPSHOT_MAIN, &archive) {
        Ok(()) => true,
        Err(WebDavError::NotFound) => false,
        Err(error) => return Err(error),
    };
    // PUT 刚成功，临时档必然存在；转正失败如实上报（主档仍是旧内容，未损坏）
    dav.move_resource(auth, &tmp, SNAPSHOT_MAIN)?;
    Ok(archived.then_some(archive))
}

/// 从远程合并编排：GET 快照 → 解析（版本门禁）→ 本地 settings 与 themes
/// 双备份 → merge_settings → 远端主题补齐型写入 → 返回合并设置 + 报告。
/// `ts` 为备份文件名的时间戳（文件名安全形态，见 [`compact_timestamp`]），
/// 由调用方生成——注入确定值让测试可断言备份名。
///
/// 主题是**补齐型合并非镜像**：本地无同名 → 落盘记新增；同名同字节 → 跳过；
/// 同名不同内容 → 远程胜覆盖（备份已在备份步整目录完成，备份目录不可用时
/// 宁可跳过覆盖也不静默丢失用户手写主题）；本地独有主题一律不动（A 机删
/// 主题不得误伤 B 机）；快照无 `[[themes]]` 或列表为空 → 完全不动本地。
pub(crate) fn merge_from_remote(
    dav: &dyn WebDav,
    auth: &WebDavAuth,
    settings_path: &Path,
    themes_path: &Path,
    ts: &str,
) -> Result<(Settings, MergeReport), String> {
    let raw = dav.get(auth, SNAPSHOT_MAIN).map_err(|error| match error {
        WebDavError::NotFound => "远程还没有快照（先执行「备份到远程」）".to_string(),
        other => other.message(),
    })?;
    let snapshot = ConfigSnapshot::parse(&raw)?;
    // 本地原文；缺文件（全新安装首合并）用默认设置的序列化产物作底
    let local_raw = match std::fs::read_to_string(settings_path) {
        Ok(raw) => raw,
        Err(_) => toml::to_string_pretty(&Settings::default())
            .map_err(|error| format!("序列化默认设置失败：{error}"))?,
    };
    // 双备份先于任何写盘动作；settings 备份失败 = 整次合并放弃（绝不无备份覆盖）
    let settings_backup = backup_settings_file(settings_path, ts)?;
    // 主题备份只在快照确实带主题时做（空列表根本不会写 themes/，不造空目录）
    let themes_backup_dir = if snapshot.themes.is_empty() {
        None
    } else {
        backup_themes_dir(themes_path, ts)?
    };
    let outcome = merge_settings(&local_raw, &snapshot.settings.body)?;
    let mut report = MergeReport {
        added: outcome.added,
        overridden: outcome.overridden,
        settings_backup,
        themes_backup_dir: themes_backup_dir.clone(),
        ..MergeReport::default()
    };
    // 主题补齐型写入（见函数头注记）
    if !snapshot.themes.is_empty() {
        let local_themes = read_local_themes(themes_path);
        std::fs::create_dir_all(themes_path)
            .map_err(|error| format!("创建主题目录失败：{error}"))?;
        for theme in &snapshot.themes {
            // 快照来自远程服务器，主题名要拼本地路径：不可信输入先过安全校验
            if !is_safe_theme_name(&theme.name) {
                eprintln!("快照中的主题名不安全，已跳过：{}", theme.name);
                continue;
            }
            let file = themes_path.join(format!("{}.toml", theme.name));
            match local_themes.iter().find(|(name, _)| *name == theme.name) {
                // 本地无同名：远端新增
                None => {
                    std::fs::write(&file, &theme.body)
                        .map_err(|error| format!("写入远端主题 {} 失败：{error}", theme.name))?;
                    report.themes_added.push(theme.name.clone());
                }
                // 同名同字节：跳过
                Some((_, body)) if body == &theme.body => {}
                // 同名不同内容：远程胜覆盖（对齐「自定义主题同名覆盖内置」
                // 约定下用户手写主题不可静默丢失——备份不可用时宁可跳过）
                Some(_) => {
                    if themes_backup_dir.is_none() {
                        eprintln!(
                            "主题 {} 与远程不一致，但本地备份目录不可用，已跳过覆盖",
                            theme.name
                        );
                        continue;
                    }
                    std::fs::write(&file, &theme.body)
                        .map_err(|error| format!("覆盖本地主题 {} 失败：{error}", theme.name))?;
                    report.themes_overwritten.push(theme.name.clone());
                }
            }
        }
    }
    Ok((outcome.settings, report))
}

/// 备份本地 settings.toml → 同目录 `settings.toml.bak-<ts>`；失败整次合并
/// 放弃（无备份不覆盖）。本地文件不存在（全新安装首合并）= 无物可备份（None）。
fn backup_settings_file(settings_path: &Path, ts: &str) -> Result<Option<String>, String> {
    if !settings_path.exists() {
        return Ok(None);
    }
    let backup = settings_path.with_file_name(format!(
        "{}.bak-{ts}",
        settings_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("settings.toml")
    ));
    std::fs::copy(settings_path, &backup).map_err(|error| format!("备份本地设置失败：{error}"))?;
    Ok(Some(
        backup
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string(),
    ))
}

/// 备份本地 themes/ 整目录 → 同父目录 `themes.bak-<ts>/`（逐文件
/// std::fs::copy）。本地无主题目录 = 无需备份（None）。单文件复制失败即
/// 报错放弃——备份目录残缺就不能作为覆盖回退的凭据。
fn backup_themes_dir(themes_path: &Path, ts: &str) -> Result<Option<String>, String> {
    if !themes_path.is_dir() {
        return Ok(None);
    }
    let parent = themes_path.parent().ok_or("无法定位主题目录父目录")?;
    let backup_dir = parent.join(format!("themes.bak-{ts}"));
    std::fs::create_dir_all(&backup_dir)
        .map_err(|error| format!("创建主题备份目录失败：{error}"))?;
    let entries =
        std::fs::read_dir(themes_path).map_err(|error| format!("读取主题目录失败：{error}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        std::fs::copy(&path, backup_dir.join(name))
            .map_err(|error| format!("备份主题 {name} 失败：{error}"))?;
    }
    Ok(Some(format!("themes.bak-{ts}")))
}

/// 读本地主题目录为 (文件名主干, 原文) 列表（仅 `.toml`；目录不存在 = 空；
/// 坏文件 eprintln 跳过）。排序保证快照与对比的确定性。
pub(crate) fn read_local_themes(themes_path: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(themes_path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(body) => out.push((stem.to_string(), body)),
            Err(error) => eprintln!("读取主题 {} 失败，已跳过：{error}", path.display()),
        }
    }
    out.sort();
    out
}

/// 快照/合并使用的主题目录：测试注入 settings_save_path 时取同父目录
/// themes/（落盘断言全在临时目录内），生产恒为用户默认主题目录。
pub(crate) fn themes_dir_for(settings_path: &Path) -> PathBuf {
    settings_path
        .parent()
        .map(|dir| dir.join("themes"))
        .unwrap_or_else(|| default_themes_dir().unwrap_or_else(|| PathBuf::from("themes")))
}

// ---------------------------------------------------------------------------
// 测试（内存 Mock，离线全流程）
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) struct MockWebDav {
    /// 路径 → 内容表（Mutex：trait 方法只拿 &self）。
    pub(crate) files: Mutex<BTreeMap<String, String>>,
    /// 操作序列（"PUT <path>" / "MOVE <from>-><to>"），供三步序断言。
    pub(crate) ops: Mutex<Vec<String>>,
    /// 最近一次 Authorization 头值，供 Basic 头断言。
    pub(crate) last_auth: Mutex<Option<String>>,
    /// 注入错误：键 = "<METHOD> <file>"（MOVE 按 from 记），命中即返回。
    pub(crate) errors: BTreeMap<String, WebDavError>,
}

#[cfg(test)]
impl MockWebDav {
    pub(crate) fn new() -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
            ops: Mutex::new(Vec::new()),
            last_auth: Mutex::new(None),
            errors: BTreeMap::new(),
        }
    }

    /// 预置远端文件。
    pub(crate) fn with_file(self, path: &str, body: &str) -> Self {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), body.to_string());
        self
    }

    /// 注入指定操作（如 "MOVE laterterm-config-snapshot.toml"）的错误。
    pub(crate) fn with_error(self, op: &str, error: WebDavError) -> Self {
        let mut this = self;
        this.errors.insert(op.to_string(), error);
        this
    }

    fn error_for(&self, op: &str) -> Option<WebDavError> {
        self.errors.get(op).cloned()
    }
}

#[cfg(test)]
impl WebDav for MockWebDav {
    fn probe(&self, auth: &WebDavAuth) -> Result<(), WebDavError> {
        *self.last_auth.lock().unwrap() = Some(auth.authorization_header());
        self.ops.lock().unwrap().push("PROBE".into());
        Ok(())
    }

    fn put(&self, auth: &WebDavAuth, file: &str, body: &str) -> Result<(), WebDavError> {
        *self.last_auth.lock().unwrap() = Some(auth.authorization_header());
        if let Some(error) = self.error_for(&format!("PUT {file}")) {
            return Err(error);
        }
        self.ops.lock().unwrap().push(format!("PUT {file}"));
        self.files
            .lock()
            .unwrap()
            .insert(file.to_string(), body.to_string());
        Ok(())
    }

    fn get(&self, auth: &WebDavAuth, file: &str) -> Result<String, WebDavError> {
        *self.last_auth.lock().unwrap() = Some(auth.authorization_header());
        if let Some(error) = self.error_for(&format!("GET {file}")) {
            return Err(error);
        }
        self.files
            .lock()
            .unwrap()
            .get(file)
            .cloned()
            .ok_or(WebDavError::NotFound)
    }

    fn move_resource(&self, auth: &WebDavAuth, from: &str, to: &str) -> Result<(), WebDavError> {
        *self.last_auth.lock().unwrap() = Some(auth.authorization_header());
        if let Some(error) = self.error_for(&format!("MOVE {from}")) {
            return Err(error);
        }
        let mut files = self.files.lock().unwrap();
        let Some(body) = files.remove(from) else {
            return Err(WebDavError::NotFound);
        };
        files.insert(to.to_string(), body);
        drop(files);
        self.ops.lock().unwrap().push(format!("MOVE {from}->{to}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use laterterm_config::sync::SNAPSHOT_VERSION;

    const AUTH: fn() -> WebDavAuth = || WebDavAuth {
        username: "alice".into(),
        password: "_SECRET_pw_42".into(),
    };

    /// 临时目录（进程号隔离，测试尾清理交给调用方断言后 remove_dir_all）。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "laterterm-webdav-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Basic 头解码：base64("alice:_SECRET_pw_42")。
    #[test]
    fn authorization_header_is_basic_base64_of_user_pass() {
        let dav = MockWebDav::new();
        dav.probe(&AUTH()).unwrap();
        let header = dav.last_auth.lock().unwrap().clone().unwrap();
        let encoded = header.strip_prefix("Basic ").unwrap();
        use base64::Engine as _;
        assert_eq!(
            BASE64_STANDARD.decode(encoded).unwrap(),
            b"alice:_SECRET_pw_42"
        );
        // Debug 打码：凭据结构打印不出密码
        let rendered = format!("{:?}", AUTH());
        assert!(!rendered.contains("_SECRET_pw_42"), "{rendered}");
        assert!(rendered.contains("******"));
    }

    /// 上传三步序：PUT 临时名 → 主档挪保留档 → 临时档转正；保留档存旧内容。
    #[test]
    fn upload_runs_three_steps_and_keeps_archive() {
        let dav = MockWebDav::new().with_file(SNAPSHOT_MAIN, "旧快照");
        let archive = backup_to_remote(&dav, &AUTH(), "新快照", "20231114T221320Z").unwrap();
        assert_eq!(
            archive.as_deref(),
            Some("laterterm-config-snapshot-20231114T221320Z.toml")
        );
        assert_eq!(
            *dav.ops.lock().unwrap(),
            vec![
                format!("PUT {SNAPSHOT_MAIN}.tmp-20231114T221320Z"),
                format!("MOVE {SNAPSHOT_MAIN}->laterterm-config-snapshot-20231114T221320Z.toml"),
                format!("MOVE {SNAPSHOT_MAIN}.tmp-20231114T221320Z->{SNAPSHOT_MAIN}"),
            ]
        );
        let files = dav.files.lock().unwrap();
        assert_eq!(files[SNAPSHOT_MAIN].as_str(), "新快照", "主档已是新内容");
        assert_eq!(
            files["laterterm-config-snapshot-20231114T221320Z.toml"].as_str(),
            "旧快照",
            "保留档存挪走前的旧内容"
        );
        assert!(
            !files.contains_key(&format!("{SNAPSHOT_MAIN}.tmp-20231114T221320Z")),
            "临时档已转正，不残留"
        );
    }

    /// 首次备份：主档不存在（404）静默跳过，不产生保留档。
    #[test]
    fn first_backup_skips_missing_main_archive() {
        let dav = MockWebDav::new();
        let archive = backup_to_remote(&dav, &AUTH(), "新快照", "ts1").unwrap();
        assert_eq!(archive, None, "首次备份无保留档");
        let files = dav.files.lock().unwrap();
        assert_eq!(files[SNAPSHOT_MAIN].as_str(), "新快照");
        assert!(!files.contains_key("laterterm-config-snapshot-ts1.toml"));
    }

    /// 失败中断：保留档步骤失败时主档原样保留（不空窗、不覆盖）。
    #[test]
    fn upload_failure_keeps_main_intact() {
        let dav = MockWebDav::new()
            .with_file(SNAPSHOT_MAIN, "旧快照")
            .with_error(
                &format!("MOVE {SNAPSHOT_MAIN}"),
                WebDavError::Network("连接被重置".into()),
            );
        let error = backup_to_remote(&dav, &AUTH(), "新快照", "ts2").unwrap_err();
        assert_eq!(error, WebDavError::Network("连接被重置".into()));
        let files = dav.files.lock().unwrap();
        assert_eq!(files[SNAPSHOT_MAIN].as_str(), "旧快照", "主档未受影响");
    }

    /// 401 归一为 Auth 且文案不含密码。
    #[test]
    fn auth_error_message_carries_no_password() {
        let message = WebDavError::Auth.message();
        assert_eq!(message, "认证失败（账号或密码错误）");
        assert!(!message.contains("_SECRET_pw_42"));
    }

    /// 合并全流程：远端快照（改字号 + 远端新键 + 换 [sync]）+ 远端主题
    /// （一个新增、一个同名不同内容）→ 双备份落位、主题补齐、报告齐全。
    #[test]
    fn merge_full_flow_with_backups_and_theme_merge() {
        let dir = temp_dir("merge-full");
        let settings_path = dir.join("settings.toml");
        let themes_path = dir.join("themes");
        std::fs::create_dir_all(&themes_path).unwrap();
        // TOML 语法：顶层键（shell）必须写在任何 [表] 头之前
        let local_settings = "shell = \"/usr/bin/fish\"\n\n[appearance]\nfont_size = 13.0\n\n[sync]\nurl = \"https://dav-a.example.com/dav\"\nusername = \"alice\"\n";
        std::fs::write(&settings_path, local_settings).unwrap();
        std::fs::write(
            themes_path.join("solarized.toml"),
            "[colors]\nbackground = \"#002B36\"\n",
        )
        .unwrap();
        std::fs::write(
            themes_path.join("local-only.toml"),
            "[colors]\nbackground = \"#111111\"\n",
        )
        .unwrap();

        let remote_theme_new = "[colors]\nbackground = \"#ABCDEF\"\n";
        let remote_theme_override = "[colors]\nbackground = \"#654321\"\n";
        let snapshot = ConfigSnapshot::build(
            // TOML 语法：顶层键（restore_session）必须写在任何 [表] 头之前
            "restore_session = true\n\n[appearance]\nfont_size = 15.5\n\n[sync]\nurl = \"https://dav-b.example.com/dav\"\nusername = \"bob\"\n"
                .into(),
            vec![
                ("new-theme".into(), remote_theme_new.into()),
                ("solarized".into(), remote_theme_override.into()),
            ],
            "0.3.0",
            1_700_000_000,
        );
        let dav = MockWebDav::new().with_file(SNAPSHOT_MAIN, &snapshot.to_toml_string().unwrap());
        let (merged, report) = merge_from_remote(
            &dav,
            &AUTH(),
            &settings_path,
            &themes_path,
            "20231114T221320Z",
        )
        .unwrap();

        // 设置合并：远程胜 + 远端新增 + local-only 恒本地
        assert_eq!(merged.appearance.font_size, 15.5);
        assert!(merged.restore_session);
        assert_eq!(merged.sync.url, "https://dav-a.example.com/dav");
        assert_eq!(merged.shell.as_deref(), Some("/usr/bin/fish"));
        assert_eq!(report.added, vec!["restore_session".to_string()]);
        assert_eq!(report.overridden, vec!["appearance.font_size".to_string()]);

        // 主题补齐：新增落盘、同名覆盖、本地独有不动
        assert_eq!(report.themes_added, vec!["new-theme".to_string()]);
        assert_eq!(report.themes_overwritten, vec!["solarized".to_string()]);
        assert_eq!(
            std::fs::read_to_string(themes_path.join("new-theme.toml")).unwrap(),
            remote_theme_new
        );
        assert_eq!(
            std::fs::read_to_string(themes_path.join("solarized.toml")).unwrap(),
            remote_theme_override
        );
        assert_eq!(
            std::fs::read_to_string(themes_path.join("local-only.toml")).unwrap(),
            "[colors]\nbackground = \"#111111\"\n",
            "本地独有主题一律不动"
        );

        // 双备份：settings.toml.bak-<ts> 与 themes.bak-<ts>/ 均存在且为原内容
        assert_eq!(
            report.settings_backup.as_deref(),
            Some(format!(
                "settings.toml.bak-{}",
                compact_timestamp("2023-11-14T22:13:20Z")
            ))
            .as_deref()
        );
        assert_eq!(
            report.themes_backup_dir.as_deref(),
            Some("themes.bak-20231114T221320Z")
        );
        let backup_dir = dir.join("themes.bak-20231114T221320Z");
        assert!(backup_dir.is_dir());
        assert_eq!(
            std::fs::read_to_string(backup_dir.join("solarized.toml")).unwrap(),
            "[colors]\nbackground = \"#002B36\"\n",
            "备份保存被覆盖前的原主题"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.toml.bak-20231114T221320Z")).unwrap(),
            local_settings
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 快照无主题列表：本地 themes/ 完全不动，也不产生主题备份目录。
    #[test]
    fn merge_without_themes_leaves_local_themes_alone() {
        let dir = temp_dir("merge-no-themes");
        let settings_path = dir.join("settings.toml");
        let themes_path = dir.join("themes");
        std::fs::create_dir_all(&themes_path).unwrap();
        std::fs::write(&settings_path, "[appearance]\nfont_size = 13.0\n").unwrap();
        std::fs::write(
            themes_path.join("mine.toml"),
            "[colors]\nbackground = \"#222222\"\n",
        )
        .unwrap();
        let snapshot = ConfigSnapshot::build(
            "[appearance]\nfont_size = 14.0\n".into(),
            Vec::new(),
            "0.3.0",
            0,
        );
        let dav = MockWebDav::new().with_file(SNAPSHOT_MAIN, &snapshot.to_toml_string().unwrap());
        let (merged, report) = merge_from_remote(
            &dav,
            &AUTH(),
            &settings_path,
            &themes_path,
            "20231114T221320Z",
        )
        .unwrap();
        assert_eq!(merged.appearance.font_size, 14.0);
        assert!(report.themes_added.is_empty() && report.themes_overwritten.is_empty());
        assert!(report.themes_backup_dir.is_none(), "无主题不造备份目录");
        assert_eq!(
            std::fs::read_to_string(themes_path.join("mine.toml")).unwrap(),
            "[colors]\nbackground = \"#222222\"\n"
        );
        assert_eq!(themes_path.read_dir().unwrap().count(), 1, "无新增文件");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// GET 401 → 合并报「认证失败」；GET 网络错误 → 原文呈现。
    #[test]
    fn merge_surfaces_auth_and_network_errors() {
        let dir = temp_dir("merge-errors");
        let settings_path = dir.join("settings.toml");
        let themes_path = dir.join("themes");
        std::fs::write(&settings_path, "").unwrap();

        let dav = MockWebDav::new().with_error(&format!("GET {SNAPSHOT_MAIN}"), WebDavError::Auth);
        let error = merge_from_remote(
            &dav,
            &AUTH(),
            &settings_path,
            &themes_path,
            "20231114T221320Z",
        )
        .unwrap_err();
        assert_eq!(error, "认证失败（账号或密码错误）");

        let dav = MockWebDav::new().with_error(
            &format!("GET {SNAPSHOT_MAIN}"),
            WebDavError::Network("DNS 解析失败".into()),
        );
        let error = merge_from_remote(
            &dav,
            &AUTH(),
            &settings_path,
            &themes_path,
            "20231114T221320Z",
        )
        .unwrap_err();
        assert_eq!(error, "网络错误：DNS 解析失败");

        // 主档不存在 → 提示先备份，而不是含糊的 404
        let dav = MockWebDav::new();
        let error = merge_from_remote(
            &dav,
            &AUTH(),
            &settings_path,
            &themes_path,
            "20231114T221320Z",
        )
        .unwrap_err();
        assert!(error.contains("先执行「备份到远程」"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 本地 settings.toml 缺失（全新安装首合并）以默认设置为底，流程不炸。
    #[test]
    fn merge_with_missing_local_settings_falls_back_to_defaults() {
        let dir = temp_dir("merge-no-local");
        let settings_path = dir.join("settings.toml");
        let themes_path = dir.join("themes");
        let snapshot = ConfigSnapshot::build(
            "[appearance]\nfont_size = 14.0\n".into(),
            Vec::new(),
            "0.3.0",
            0,
        );
        let dav = MockWebDav::new().with_file(SNAPSHOT_MAIN, &snapshot.to_toml_string().unwrap());
        let (merged, _report) = merge_from_remote(
            &dav,
            &AUTH(),
            &settings_path,
            &themes_path,
            "20231114T221320Z",
        )
        .unwrap();
        assert_eq!(merged.appearance.font_size, 14.0);
        // 快照体为空 settings：合并结果 = 本地默认
        assert_eq!(merged.sync, laterterm_config::WebdavSyncSettings::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 快照版本门禁在编排层生效（过新拒绝）。
    #[test]
    fn merge_rejects_snapshot_newer_than_supported() {
        let dir = temp_dir("merge-new-version");
        let settings_path = dir.join("settings.toml");
        let themes_path = dir.join("themes");
        std::fs::write(&settings_path, "").unwrap();
        let raw = format!(
            "[meta]\nversion = {}\napp_version = \"9.0.0\"\nsaved_at = \"2100-01-01T00:00:00Z\"\n[settings]\nbody = \"\"\n",
            SNAPSHOT_VERSION + 1
        );
        let dav = MockWebDav::new().with_file(SNAPSHOT_MAIN, &raw);
        let error = merge_from_remote(
            &dav,
            &AUTH(),
            &settings_path,
            &themes_path,
            "20231114T221320Z",
        )
        .unwrap_err();
        assert!(error.contains("过新"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 主题名不安全（路径穿越）跳过且不落盘。
    #[test]
    fn merge_skips_unsafe_theme_names() {
        let dir = temp_dir("merge-unsafe-name");
        let settings_path = dir.join("settings.toml");
        let themes_path = dir.join("themes");
        std::fs::write(&settings_path, "").unwrap();
        let snapshot = ConfigSnapshot::build(
            "".into(),
            vec![(
                "../escape".into(),
                "[colors]\nbackground = \"#000000\"\n".into(),
            )],
            "0.3.0",
            0,
        );
        let dav = MockWebDav::new().with_file(SNAPSHOT_MAIN, &snapshot.to_toml_string().unwrap());
        let (merged, report) = merge_from_remote(
            &dav,
            &AUTH(),
            &settings_path,
            &themes_path,
            "20231114T221320Z",
        )
        .unwrap();
        assert_eq!(merged.appearance.font_size, 13.0, "默认字号（空快照体）");
        assert!(report.themes_added.is_empty());
        assert!(
            !dir.join("escape.toml").exists()
                && !dir.parent().unwrap().join("escape.toml").exists(),
            "穿越名不得落盘"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 上传快照组装：内嵌 settings 原文与主题，且不含密钥环里的任何密钥。
    #[test]
    fn upload_snapshot_embeds_settings_and_themes_without_secrets() {
        let dir = temp_dir("upload-snapshot");
        let settings_path = dir.join("settings.toml");
        let themes_path = dir.join("themes");
        std::fs::create_dir_all(&themes_path).unwrap();
        std::fs::write(
            &settings_path,
            "[sync]\nurl = \"https://dav.example.com/dav\"\n",
        )
        .unwrap();
        std::fs::write(
            themes_path.join("mine.toml"),
            "[colors]\nbackground = \"#333333\"\n",
        )
        .unwrap();
        let snapshot = build_upload_snapshot(&settings_path, &themes_path).unwrap();
        let parsed = ConfigSnapshot::parse(&snapshot).unwrap();
        assert!(parsed.settings.body.contains("[sync]"));
        assert_eq!(parsed.themes.len(), 1);
        assert_eq!(parsed.themes[0].name, "mine");
        // 密钥环样例密钥（WebDAV 密码 + TOTP 种子）都不得出现在上传体
        for secret in ["_SECRET_pw_42", "JBSWY3DPEHPK3PXP"] {
            assert!(!snapshot.contains(secret), "上传体泄漏样例密钥 {secret}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 地址校验：非 http(s) 前缀拒绝（不发请求）。
    #[test]
    fn ureq_client_rejects_non_http_base_url() {
        assert!(UreqWebDav::new("ftp://dav.example.com").is_err());
        assert!(UreqWebDav::new("dav.example.com/dav").is_err());
        // 尾斜杠归一
        let dav = UreqWebDav::new("https://dav.example.com/dav/").unwrap();
        assert_eq!(
            dav.url_for("file.toml"),
            "https://dav.example.com/dav/file.toml"
        );
    }
}
