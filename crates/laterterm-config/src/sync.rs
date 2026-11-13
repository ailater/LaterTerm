//! 配置同步纯函数层：WebDAV 账户元数据、双机设置合并、单文件快照编解码。
//!
//! 网络与文件 IO 在 app 层（laterterm-app/webdav_sync.rs）；本模块零 IO、
//! 离线可测。三块职责：
//!
//! - [`WebdavSyncSettings`]：settings.toml `[sync]` 段。红线：**永不含密码
//!   字段**——settings.toml 整份会上传到 WebDAV 服务器，密码只进系统密钥环。
//! - [`merge_settings`]：`toml::Value` 递归 diff，远程胜策略 + local-only
//!   特判集合（见 [`LOCAL_ONLY_PATHS`]）。
//! - [`ConfigSnapshot`]：单文件快照（settings.toml 原文 + themes/ 逐文件
//!   内嵌）。拒绝 tar（新增依赖 + 非 Unix 解包）与多文件直传（需要
//!   MKCOL/PROPFIND 枚举/逐文件 MOVE），单文件 PUT+MOVE 最省事也最好核对。
//!
//! 时间格式化手写 days-from-civil（仓库无 chrono/time 直接依赖，不为一个
//! 时间戳引入）。

use crate::Settings;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// WebDAV 同步账户配置（settings.toml `[sync]` 段）。
///
/// 红线：本结构体**永远不得**加入密码/令牌字段。settings.toml 会整份上传到
/// WebDAV 服务器并被另一台机器下载，任何凭据进这里等于公开；密码只进系统
/// 密钥环（laterterm-secrets `webdav/password` 单条目）。测试
/// `snapshot_body_never_contains_password` 钉住此约束。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebdavSyncSettings {
    /// WebDAV 目录地址（如 `https://dav.example.com/dav/laterterm`）。
    pub url: String,
    /// Basic auth 账号。
    pub username: String,
}

impl WebdavSyncSettings {
    /// 去首尾空白与地址尾斜杠归一（连接前与落盘前调用，保存归一值防
    /// `url/` 与 `url` 在合并 local-only 特判下产生假差异）。
    pub fn normalize(&mut self) {
        self.url = self.url.trim().trim_end_matches('/').to_string();
        self.username = self.username.trim().to_string();
    }

    /// 是否已配置到可以发起同步（地址 + 账号齐备；密码看密钥环）。
    pub fn is_configured(&self) -> bool {
        !self.url.is_empty() && !self.username.is_empty()
    }
}

/// 本机专属路径集合：合并前先把本地值**预覆盖进远端表**（本地缺键则从远端
/// 删去同名键），使通用 diff 自然跳过这些路径——恒取本地且不产生差异条目。
///
/// 逐项理由：
/// - `legacy_servers_migrated`：旧 servers.toml 一次性迁移标记，回退 false
///   会让已删除条目在下次启动复活；
/// - `sync`（顶层整段）：被远程覆盖会让 A 机拿 A 的密钥环密码把整份配置
///   发往 B 的服务器（错传风险）；
/// - `shell` / `appearance.downloads_dir`：本机属性（lib.rs 注记），远端值
///   合法但不适配，且合并后无人再改它、不会自愈。
pub const LOCAL_ONLY_PATHS: &[&str] = &[
    "legacy_servers_migrated",
    "sync",
    "shell",
    "appearance.downloads_dir",
];

/// 合并结果：还原后的完整设置 + 差异清单（UI 差异明细直接渲染这两张表）。
#[derive(Clone, Debug, PartialEq)]
pub struct MergeOutcome {
    /// 合并后的完整设置。已过与 [`Settings::load`] 同一条载入期修正链
    /// （sanitize 外观/ quake/重连/TOTP/服务器/shell），远端值同样不带病生效。
    pub settings: Settings,
    /// 远端新增的键路径（本地缺失、整值取远程）。
    pub added: Vec<String>,
    /// 远程覆盖的键路径（两侧都有但不等，远程胜；列表整体视为一个值，
    /// 服务器/命令列表冲突记单条路径、整体以远程为准）。
    pub overridden: Vec<String>,
}

/// 双机设置合并：本地原文 × 远端原文 → 合并设置 + 差异清单。
///
/// 语义（修订版口径）：
/// - remote 缺键：留 local，不产生条目；
/// - local 缺键：取 remote，记「远端新增」；
/// - 两侧都有但不等：**远程胜**，记冲突路径；表递归逐键，数组（commands/
///   servers/totp/triggers）整体视为一个值，不做条目级 diff（过度设计）；
/// - [`LOCAL_ONLY_PATHS`] 预覆盖后恒取本地，不产生条目。
///
/// 合并结果无法还原为 [`Settings`] 时整体报错（"未应用任何变更"）——
/// 绝不部分应用：半份配置比没有更危险。
pub fn merge_settings(local_raw: &str, remote_raw: &str) -> Result<MergeOutcome, String> {
    let local: toml::Value =
        toml::from_str(local_raw).map_err(|e| format!("本地设置解析失败：{e}"))?;
    let mut remote: toml::Value =
        toml::from_str(remote_raw).map_err(|e| format!("远端设置解析失败：{e}"))?;
    if !local.is_table() || !remote.is_table() {
        return Err("设置文件顶层必须是表".into());
    }
    // 借用拆装：is_table 已判定，unwrap 安全
    let (local_table, remote_table) = (local.as_table().unwrap(), remote.as_table_mut().unwrap());
    apply_local_only_overrides(local_table, remote_table);
    let mut added = Vec::new();
    let mut overridden = Vec::new();
    let merged = toml::Value::Table(merge_tables(
        local_table,
        remote_table,
        "",
        &mut added,
        &mut overridden,
    ));
    let mut settings: Settings = merged
        .try_into()
        .map_err(|e| format!("合并结果无法还原为设置（未应用任何变更）：{e}"))?;
    // 与 Settings::load 同一条修正链（根模块私有方法，同 crate 可达）：
    // 另一台机器的值（或两台机器合并出的值）同样不该带病生效。
    settings.appearance.sanitize();
    settings.quake.sanitize();
    settings.reconnect.sanitize();
    settings.sanitize_totp();
    let cleared = settings.sanitize_servers();
    if !cleared.is_empty() {
        eprintln!(
            "合并后的无效跳板引用已清除 {}（{}）",
            cleared.len(),
            cleared.join("、")
        );
    }
    settings.sanitize_shell();
    Ok(MergeOutcome {
        settings,
        added,
        overridden,
    })
}

/// local-only 预覆盖：把本地值写进远端表（嵌套键只动叶子），本地缺键则把
/// 远端同名键删掉——两种方向都让合并结果在该路径上恒等于本地。
fn apply_local_only_overrides(
    local: &toml::map::Map<String, toml::Value>,
    remote: &mut toml::map::Map<String, toml::Value>,
) {
    for path in LOCAL_ONLY_PATHS {
        match path.split_once('.') {
            // 顶层键：legacy_servers_migrated / sync / shell
            None => match local.get(*path) {
                Some(value) => {
                    remote.insert((*path).to_string(), value.clone());
                }
                None => {
                    remote.remove(*path);
                }
            },
            // 嵌套键：appearance.downloads_dir
            Some((parent, leaf)) => {
                let Some(local_parent) = local.get(parent).and_then(|value| value.as_table())
                else {
                    // 本地连父表都没有：合并期该段整体取本地，无需特判
                    continue;
                };
                let Some(remote_parent) = remote
                    .get_mut(parent)
                    .and_then(|value| value.as_table_mut())
                else {
                    // 远端没有父表：合并期该段整体取本地（本地新增语义不触发），
                    // 天然恒本地
                    continue;
                };
                match local_parent.get(leaf) {
                    Some(value) => {
                        remote_parent.insert(leaf.to_string(), value.clone());
                    }
                    None => {
                        remote_parent.remove(leaf);
                    }
                }
            }
        }
    }
}

/// 递归合并：以本地表为底，逐键吸收远端（新增/覆盖），本地独有键原样保留。
fn merge_tables(
    local: &toml::map::Map<String, toml::Value>,
    remote: &toml::map::Map<String, toml::Value>,
    path: &str,
    added: &mut Vec<String>,
    overridden: &mut Vec<String>,
) -> toml::map::Map<String, toml::Value> {
    let mut merged = local.clone();
    for (key, remote_value) in remote {
        let child_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match local.get(key) {
            // 本地缺失：远端新增，整值取远程
            None => {
                merged.insert(key.clone(), remote_value.clone());
                added.push(child_path);
            }
            // 两侧都是表：递归逐键；数组不是表，走下面的整值分支
            Some(local_value) if local_value.is_table() && remote_value.is_table() => {
                let child = merge_tables(
                    local_value.as_table().unwrap(),
                    remote_value.as_table().unwrap(),
                    &child_path,
                    added,
                    overridden,
                );
                merged.insert(key.clone(), toml::Value::Table(child));
            }
            // 相等：保持本地（值相同，写谁都一样）
            Some(local_value) if local_value == remote_value => {}
            // 不等：远程胜（列表整体远程胜，差异记单条路径）
            Some(_) => {
                merged.insert(key.clone(), remote_value.clone());
                overridden.push(child_path);
            }
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// 单文件快照
// ---------------------------------------------------------------------------

/// 快照格式版本。解析期门禁：meta.version 高于本端时拒绝（报「快照版本过
/// 新」，不猜兼容；Settings 无 deny_unknown_fields，未知字段本可容忍，但
/// 版本门禁先行更诚实）。
pub const SNAPSHOT_VERSION: u64 = 1;

/// 单文件快照：`[meta]` + `[settings]`（settings.toml 原文内嵌）+ 逐文件
/// `[[themes]]`。序列化产物即上传体，**不含任何密钥**（settings 无密码字段、
/// 主题只有色板，测试钉住）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    /// 快照元信息。
    pub meta: SnapshotMeta,
    /// settings.toml 原文（多行字符串内嵌；解析推迟到合并期，快照自身不碰语义）。
    pub settings: SnapshotSettings,
    /// 自定义主题逐文件内嵌；空 = 远端没有任何自定义主题。
    #[serde(default)]
    pub themes: Vec<SnapshotTheme>,
}

/// 快照元信息（[meta]）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    /// 快照格式版本（见 [`SNAPSHOT_VERSION`]）。
    pub version: u64,
    /// 产出快照的应用版本（诊断用，不做门禁）。
    pub app_version: String,
    /// 产出时刻（UTC，[`format_utc_seconds`] 格式）。
    pub saved_at: String,
}

/// settings.toml 原文载体（[settings] 段）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSettings {
    /// settings.toml 文件原文。
    pub body: String,
}

/// 内嵌主题文件（[[themes]]）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotTheme {
    /// 主题文件名主干（`<name>.toml` 的 `<name>`；须过 [`is_safe_theme_name`]）。
    pub name: String,
    /// 主题 TOML 文件原文。
    pub body: String,
}

impl ConfigSnapshot {
    /// 组装快照。`themes` 为 (文件名主干, 文件原文) 列表。
    pub fn build(
        settings_body: String,
        themes: Vec<(String, String)>,
        app_version: &str,
        saved_at_unix: u64,
    ) -> Self {
        Self {
            meta: SnapshotMeta {
                version: SNAPSHOT_VERSION,
                app_version: app_version.to_string(),
                saved_at: format_utc_seconds(saved_at_unix),
            },
            settings: SnapshotSettings {
                body: settings_body,
            },
            themes: themes
                .into_iter()
                .map(|(name, body)| SnapshotTheme { name, body })
                .collect(),
        }
    }

    /// 序列化为上传体（TOML 文本）。
    pub fn to_toml_string(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| format!("快照序列化失败：{e}"))
    }

    /// 解析上传体；版本门禁先行（过新拒绝，不猜兼容）。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let snapshot: Self =
            toml::from_str(raw).map_err(|e| format!("快照不是有效的 TOML：{e}"))?;
        if snapshot.meta.version > SNAPSHOT_VERSION {
            return Err(format!(
                "快照版本过新（v{}，本机支持到 v{SNAPSHOT_VERSION}），请先升级 LaterTerm",
                snapshot.meta.version
            ));
        }
        Ok(snapshot)
    }
}

/// 主题文件名安全校验：拒绝路径分隔符、`..`、隐藏/控制字符——快照来自
/// 远程服务器，主题名要拼成本地落盘路径，必须先当不可信输入处理。
pub fn is_safe_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

// ---------------------------------------------------------------------------
// UTC 时间戳（手写 days-from-civil，仓库无 chrono/time 直接依赖）
// ---------------------------------------------------------------------------

/// Unix 秒 → `YYYY-MM-DDTHH:MM:SSZ`（UTC，秒级）。
///
///civil-from-days 取 Howard Hinnant 公有领域算法；u64 入参天然非负，
/// 负数分支（1970 前）不需要。
pub fn format_utc_seconds(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// 当前 Unix 秒（SystemTime；失败回落 0——时钟倒挂时快照 saved_at 退化为
/// 纪元，不影响正确性）。
pub fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 文件名安全时间戳：ISO 串去 `-`/`:`（冒号在部分 WebDAV 服务器与 Windows
/// 文件系统上不合法）→ `20231114T221320Z`。
pub fn compact_timestamp(iso: &str) -> String {
    iso.replace(['-', ':'], "")
}

/// civil-from-days：1970-01-01 起的天数 → (年, 月, 日)。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 两机合并主场景：A 机（本地）与 B 机（远端）的 [sync]/shell/
    /// downloads_dir 各不相同——合并后三者恒取本地，且不产生差异条目；
    /// 其余差异按远程胜/远端新增记账。
    #[test]
    fn merge_keeps_local_only_paths_from_both_machines() {
        // 注意 TOML 语法：顶层键（shell 等）必须写在任何 [表] 头之前
        let local = r#"
shell = "/usr/bin/fish"
legacy_servers_migrated = true

[appearance]
font_size = 13.0
downloads_dir = "/home/a/Downloads"

[sync]
url = "https://dav-a.example.com/dav"
username = "alice"
"#;
        let remote = r#"
shell = "/bin/zsh"
legacy_servers_migrated = false
restore_session = true

[appearance]
font_size = 15.0
downloads_dir = "/home/b/下载"

[sync]
url = "https://dav-b.example.com/dav"
username = "bob"
"#;
        let outcome = merge_settings(local, remote).unwrap();
        // local-only 四类恒本地
        assert_eq!(outcome.settings.sync.url, "https://dav-a.example.com/dav");
        assert_eq!(outcome.settings.sync.username, "alice");
        assert_eq!(outcome.settings.shell.as_deref(), Some("/usr/bin/fish"));
        assert_eq!(
            outcome.settings.appearance.downloads_dir,
            "/home/a/Downloads"
        );
        assert!(outcome.settings.legacy_servers_migrated);
        // 远程胜 + 远端新增照常记账
        assert_eq!(outcome.settings.appearance.font_size, 15.0);
        assert!(outcome.settings.restore_session);
        assert_eq!(outcome.added, vec!["restore_session".to_string()]);
        assert_eq!(outcome.overridden, vec!["appearance.font_size".to_string()]);
        // 四类 local-only 路径绝不出现在差异清单里
        for path in outcome.added.iter().chain(outcome.overridden.iter()) {
            for only in LOCAL_ONLY_PATHS {
                assert_ne!(path, *only, "{path} 不应记为差异");
                assert!(
                    !path.starts_with(&format!("{only}.")),
                    "{path} 不应记为差异"
                );
            }
        }
    }

    /// 本地缺键时 local-only 预覆盖走删除方向：远端 shell 键被删掉，
    /// 合并结果 shell=None（跟随 $SHELL），不报「远端新增」。
    #[test]
    fn merge_local_only_absent_locally_removes_remote_key() {
        let local = "[appearance]\nfont_size = 13.0\n";
        let remote = "shell = \"/bin/zsh\"\n";
        let outcome = merge_settings(local, remote).unwrap();
        assert_eq!(outcome.settings.shell, None);
        assert!(outcome.added.is_empty(), "local-only 不产生差异条目");
    }

    /// 远端缺键：留本地且不产生条目；嵌套表递归合并只动分歧键。
    #[test]
    fn merge_remote_missing_keys_silently_keep_local() {
        let local = "[appearance]\nfont_size = 13.0\nscrollback = 50_000\n";
        let remote = "[appearance]\nfont_size = 14.0\n";
        let outcome = merge_settings(local, remote).unwrap();
        assert_eq!(outcome.settings.appearance.font_size, 14.0);
        assert_eq!(outcome.settings.appearance.scrollback, 50_000);
        assert!(outcome.added.is_empty());
        assert_eq!(outcome.overridden, vec!["appearance.font_size".to_string()]);
    }

    /// 列表（服务器/命令）整体远程胜，差异记单条路径（不做条目级 diff）。
    #[test]
    fn merge_lists_are_whole_values_remote_wins() {
        let local = r#"
[[commands]]
name = "本机命令"
template = "pwd"
"#;
        let remote = r#"
[[commands]]
name = "远端命令"
template = "ls"
"#;
        let outcome = merge_settings(local, remote).unwrap();
        assert_eq!(outcome.settings.commands.len(), 1);
        assert_eq!(outcome.settings.commands[0].name, "远端命令");
        assert_eq!(outcome.overridden, vec!["commands".to_string()]);
    }

    /// 合并结果无法还原为 Settings 时整体报错，绝不部分应用：
    /// 远端 font_size 是字符串，类型不符必须让整次合并失败。
    #[test]
    fn merge_failure_is_whole_not_partial() {
        let local = "[appearance]\nfont_size = 13.0\n";
        let remote = "[appearance]\nfont_size = \"big\"\n";
        let err = merge_settings(local, remote).unwrap_err();
        assert!(
            err.contains("未应用任何变更"),
            "错误应说明整体未应用：{err}"
        );
    }

    /// 远端原文本身不是合法 TOML：报解析失败而非 panic。
    #[test]
    fn merge_rejects_broken_remote_body() {
        let err = merge_settings("[appearance]\nfont_size = 13.0\n", "]]not toml").unwrap_err();
        assert!(err.starts_with("远端设置解析失败"), "{err}");
    }

    /// 快照编解码往返：settings 原文与主题文件逐字节还原。
    #[test]
    fn snapshot_roundtrip_preserves_bodies() {
        let settings_body = "[appearance]\nfont_size = 13.7\n\n[sync]\nurl = \"https://dav.example.com/dav\"\nusername = \"alice\"\n";
        let themes = vec![
            (
                "solarized".to_string(),
                "[colors]\nbackground = \"#002B36\"\n".to_string(),
            ),
            (
                "我的主题".to_string(),
                "[colors]\nbackground = \"#123456\"\n".to_string(),
            ),
        ];
        let snapshot = ConfigSnapshot::build(
            settings_body.to_string(),
            themes.clone(),
            "0.3.0",
            1_700_000_000,
        );
        assert_eq!(snapshot.meta.version, SNAPSHOT_VERSION);
        assert_eq!(snapshot.meta.saved_at, "2023-11-14T22:13:20Z");
        let raw = snapshot.to_toml_string().unwrap();
        let parsed = ConfigSnapshot::parse(&raw).unwrap();
        assert_eq!(parsed.settings.body, settings_body);
        assert_eq!(parsed.themes, snapshot.themes);
        // 多行 body 起始/结尾换行也不丢（TOML 多行字符串裁首换行的坑）
        let edge = ConfigSnapshot::build(
            "\n[appearance]\nfont_size = 13.0\n".to_string(),
            Vec::new(),
            "0.3.0",
            0,
        );
        let parsed = ConfigSnapshot::parse(&edge.to_toml_string().unwrap()).unwrap();
        assert_eq!(parsed.settings.body, "\n[appearance]\nfont_size = 13.0\n");
    }

    /// 红线钉住：快照序列化产物（即上传体）不含 password 键——[sync] 段
    /// 只有 url/username，密钥只进密钥环。
    #[test]
    fn snapshot_body_never_contains_password() {
        let settings_body = "[sync]\nurl = \"https://dav.example.com/dav\"\nusername = \"alice\"\n";
        let snapshot = ConfigSnapshot::build(
            settings_body.to_string(),
            vec![(
                "solarized".into(),
                "[colors]\nbackground = \"#002B36\"\n".into(),
            )],
            "0.3.0",
            0,
        );
        let raw = snapshot.to_toml_string().unwrap();
        assert!(!raw.to_ascii_lowercase().contains("password"), "{raw}");
    }

    /// 版本门禁：meta.version 高于本端支持值时拒绝解析。
    #[test]
    fn snapshot_rejects_newer_version() {
        let raw = "[meta]\nversion = 99\napp_version = \"9.0.0\"\nsaved_at = \"2100-01-01T00:00:00Z\"\n[settings]\nbody = \"\"\n";
        let err = ConfigSnapshot::parse(raw).unwrap_err();
        assert!(err.contains("过新"), "{err}");
        // 未来版本新增未知字段但版本号不变：容忍（Settings 同口径）
        let raw = "[meta]\nversion = 1\napp_version = \"0.3.0\"\nsaved_at = \"2026-11-14T00:00:00Z\"\nfuture_field = true\n[settings]\nbody = \"\"\n";
        assert!(ConfigSnapshot::parse(raw).is_ok());
    }

    /// 主题名安全校验：路径分隔符 / 上跳目录 / 隐藏与控制字符全拒。
    #[test]
    fn theme_name_safety_rejects_traversal() {
        for name in ["solarized", "我的主题", "a-b_c1"] {
            assert!(is_safe_theme_name(name), "{name}");
        }
        for name in ["", ".", "..", ".hidden", "a/b", "a\\b", "a\nb", "a/b/../c"] {
            assert!(!is_safe_theme_name(name), "{name}");
        }
    }

    /// UTC 秒级时间戳：纪元、闰日、已知整点三例（civil_from_days 对位）。
    #[test]
    fn utc_timestamp_known_instants() {
        assert_eq!(format_utc_seconds(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc_seconds(86_400), "1970-01-02T00:00:00Z");
        // 2000-02-29（闰日）：era/闰表分支
        assert_eq!(format_utc_seconds(951_782_400), "2000-02-29T00:00:00Z");
        // 已知整点：2023-11-14T22:13:20Z
        assert_eq!(format_utc_seconds(1_700_000_000), "2023-11-14T22:13:20Z");
        // 尾部秒/分/时不为零
        assert_eq!(
            format_utc_seconds(1_700_000_000 + 3_723),
            "2023-11-14T23:15:23Z"
        );
    }

    /// 文件名安全时间戳：去 `-`/`:`。
    #[test]
    fn compact_timestamp_strips_separators() {
        assert_eq!(
            compact_timestamp("2023-11-14T22:13:20Z"),
            "20231114T221320Z"
        );
    }

    /// Settings 序列化产物含 [sync] 段且不含密码字段（落盘口径红线）。
    #[test]
    fn settings_serializes_sync_section_without_password() {
        let mut settings = Settings::default();
        settings.sync.url = "https://dav.example.com/dav".into();
        settings.sync.username = "alice".into();
        let raw = toml::to_string_pretty(&settings).unwrap();
        assert!(raw.contains("[sync]"), "{raw}");
        assert!(raw.contains("username = \"alice\""), "{raw}");
        assert!(!raw.to_ascii_lowercase().contains("password"), "{raw}");
        // 旧配置缺 [sync] 段 → 容器级 serde(default) 补默认空值
        let legacy: Settings = toml::from_str("[appearance]\nfont_size = 14.0\n").unwrap();
        assert_eq!(legacy.sync, WebdavSyncSettings::default());
        // roundtrip
        let parsed: Settings = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.sync, settings.sync);
    }

    /// normalize：去空白与地址尾斜杠；is_configured 双门槛。
    #[test]
    fn sync_settings_normalize_and_configure_gate() {
        let mut sync = WebdavSyncSettings {
            url: "  https://dav.example.com/dav/  ".into(),
            username: " alice ".into(),
        };
        sync.normalize();
        assert_eq!(sync.url, "https://dav.example.com/dav");
        assert_eq!(sync.username, "alice");
        assert!(sync.is_configured());
        sync.url = String::new();
        assert!(!sync.is_configured());
    }
}
