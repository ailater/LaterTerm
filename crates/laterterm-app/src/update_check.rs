//! 更新检测（手动触发）：查 GitHub 最新 release tag，与当前版本比较，
//! 有新版本则提示并给出跳转 release 页的入口。
//!
//! ## 为什么是手动而不是自动
//!
//! 静默外连对用户是黑盒行为（隐私与离线可用性都不好），也不做后台周期
//! 轮询。放在设置面板做成显式按钮：用户点了才发一次请求。
//!
//! ## 为什么用 ureq 3 + rustls（不用 gpui 内置 http 通道）
//!
//! `cx.http_client()` 默认是 NullHttpClient（vendor/gpui/src/app.rs），
//! main.rs 未接线 with_http_client，gpui_http_client 也不导出真实 reqwest
//! 构造器——内置通道当前不可用。ureq 是同步 API，免 async runtime；TLS 走
//! `features = ["rustls"]`：该 feature = rustls-no-provider + _ring +
//! rustls-webpki-roots，即自带 ring provider 与**内嵌 webpki 根证书**（不走
//! 系统证书库，rustls-native-certs 不参与），ring 0.17 已在依赖树
//! （zed-reqwest 传递引入），无 aws-lc-rs/CMake 增量。不启 json/gzip：
//! 解析走 serde_json 纯函数（见 evaluate_release）；不启用 gzip 时 ureq 不发
//! Accept-Encoding，GitHub 返回 identity，无需解压。
//!
//! ## 失败语义
//!
//! 无网络 / DNS 黑洞 / GitHub 不可达 / 响应不是预期结构，一律「未能确认」，
//! **绝不**呈现为「已是最新」——那会骗过用户。8s 全局超时封顶（DNS 极端
//! 挂死场景下同步 getaddrinfo 无法被超时中断，是本选型的已知边界，
//! LaterScreen 同款口径）。仓库尚无 release 时该端点返回 404，特判为
//! 「仓库还没有发布版」。未认证限额 60 次/小时/IP，手动触发远达不到；
//! 共享出口 IP 撞上 403/429 归入「GitHub 返回 {status}」静默文案。

use serde_json::Value;
use std::time::Duration;

/// GitHub 仓库（owner/repo）；release 页由此拼出。
pub(crate) const REPO: &str = "ailater/LaterTerm";

/// 当前版本单一真源：workspace version（根 Cargo.toml），后续 auto-tag CI
/// 注入版本时比对基准自动跟随。
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Release 页：用户点「去下载」时用系统默认浏览器打开。
pub(crate) fn release_page_url() -> String {
    format!("https://github.com/{REPO}/releases/latest")
}

/// 最新 release 查询接口。只读公开信息，无需 token。
const LATEST_API: &str = "https://api.github.com/repos/ailater/LaterTerm/releases/latest";

/// 单次请求上限。手动触发也要有界：挂住的请求会让按钮一直「检查中…」。
const TIMEOUT: Duration = Duration::from_secs(8);

/// 检查结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// 已是最新（或本地版本比远端还新，如自行构建）
    UpToDate,
    /// 有新版本：远端 tag 名（如 "v0.3.0"）
    Newer(String),
}

/// 更新检查状态机。UI 侧持有一份，按需推进。
///
/// gpui 语义下不需要 LaterScreen 的 Arc<Mutex> 结果槽 + 帧级 poll：后台
/// 结果经实体更新回写（workspace.rs 的 check_for_update），「切走设置面板」
/// 只是 settings_open 布尔开关，WorkspaceView 实体恒活，结果落字段等下次
/// 打开显示。
#[derive(Default)]
pub(crate) struct UpdateCheck {
    /// 后台检查在途（UI 显示「检查中…」）。
    checking: bool,
    /// 最近一次结果（None = 未检查过或已被「知道了」清除）。
    result: Option<Result<Outcome, String>>,
}

impl UpdateCheck {
    /// 是否正在检查（UI 用它决定按钮文案/禁用感）。
    pub(crate) fn checking(&self) -> bool {
        self.checking
    }

    /// 上一次结果（无则 None）。
    pub(crate) fn result(&self) -> Option<&Result<Outcome, String>> {
        self.result.as_ref()
    }

    /// 发起一次检查的幂等门：已在检查中则忽略（连点不会发多次请求），
    /// 返回是否真正启动。真正 spawn 后台任务前调用。
    pub(crate) fn start(&mut self) -> bool {
        if self.checking {
            return false;
        }
        self.checking = true;
        true
    }

    /// 后台结果回写。任何 Err 原样保存——调用方（UI）把 Err 渲染为
    /// 「未能确认」静默文案，本模块从不把失败翻译成 Ok(UpToDate)。
    pub(crate) fn finish(&mut self, result: Result<Outcome, String>) {
        self.checking = false;
        self.result = Some(result);
    }

    /// 清除结果（「知道了」按钮）；不打断在途检查（checking 不动）。
    pub(crate) fn clear(&mut self) {
        self.result = None;
    }
}

// ---------------------------------------------------------------- 网络

/// 查最新 release 的 tag；失败返回可展示的中文原因。
pub(crate) fn query_latest() -> Result<Outcome, String> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            // ureq 默认把非 2xx 直接折成 Err(Error::StatusCode)，关掉后由
            // 本函数自判状态码：404（仓库还没有发布版）与 403/429（限流）
            // 要给不同文案
            .http_status_as_error(false)
            .build(),
    );
    let mut resp = agent
        .get(LATEST_API)
        .header("Accept", "application/vnd.github+json")
        // GitHub API 要求 UA；不给会 403
        .header("User-Agent", format!("LaterTerm/{CURRENT_VERSION}"))
        .call()
        .map_err(|e| format!("请求 GitHub 失败：{e}"))?;
    let status = resp.status();
    if !status.is_success() {
        if status.as_u16() == 404 {
            // /releases/latest 在仓库无任何 release 时返回 404，不是故障
            return Err("仓库还没有发布版".into());
        }
        return Err(format!("GitHub 返回 {status}"));
    }
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读取响应失败：{e}"))?;
    evaluate_release(&body, CURRENT_VERSION)
}

// ---------------------------------------------------------------- 纯函数

/// 解析 release 响应体并与当前版本比较；失败返回可展示的中文原因。
/// 网络与解析在此分离：本函数零 IO，离线可测（query_latest 只补网络一跳）。
fn evaluate_release(body: &str, current: &str) -> Result<Outcome, String> {
    let tag = parse_release_tag(body)?;
    match compare_versions(current, &tag) {
        Some(outcome) => Ok(outcome),
        None => Err(compare_failure_message(current, &tag)),
    }
}

/// 从 GitHub /releases/latest 响应体取 `tag_name`：只取这一个字段，其余
/// 字段（html_url/assets/prerelease…）容错忽略——GitHub 加字段不该让我们崩。
fn parse_release_tag(body: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("响应不是有效 JSON：{e}"))?;
    value["tag_name"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "响应缺 tag_name".into())
}

/// 远端 tag 与当前版本比较。任一侧无法解析返回 None（调用方转错误文案）。
///
/// tag 形如 `v0.2.0`；也容忍不带 `v` 前缀。比较按「点分数字段逐段比」，
/// 不做 semver 完整语义（预发布/构建元数据用不上：/releases/latest 只返回
/// 非 pre-release、非 draft 的正式版）。段数不同按缺失补 0
/// （`0.2` == `0.2.0`）。current 参数化而非内嵌 env!，便于注入测试。
pub(crate) fn compare_versions(current: &str, tag: &str) -> Option<Outcome> {
    let latest = parse_version(tag)?;
    let current = parse_version(current)?;
    // 段数不同先补 0 对齐，再逐段比：Vec 的默认字典序会把 [0,2,0,0]
    // 判成大于 [0,2,0]，那不是我们要的「0.2.0.0 == 0.2.0」
    let n = latest.len().max(current.len());
    let pad = |v: &Vec<u64>| -> Vec<u64> {
        let mut v = v.clone();
        while v.len() < n {
            v.push(0);
        }
        v
    };
    Some(if pad(&latest) > pad(&current) {
        Outcome::Newer(tag.trim().to_string())
    } else {
        Outcome::UpToDate
    })
}

/// 比较失败时区分是哪一边的版本号不可理解：本地 CARGO_PKG_VERSION 恒可解析，
/// 但未来 auto-tag CI 可能注入带后缀的版本（如 "0.3.0-rc1"）——那时不能把
/// 本地解析失败误报成远端版本号问题（评审意见6）。本地优先判定。
fn compare_failure_message(current: &str, tag: &str) -> String {
    if parse_version(current).is_none() {
        format!("无法理解本地版本号「{current}」")
    } else {
        format!("无法理解远端版本号「{tag}」")
    }
}

/// "v0.2.0" / "0.2" → [0, 2, 0]
fn parse_version(s: &str) -> Option<Vec<u64>> {
    let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
    if s.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for part in s.split('.') {
        // 只认纯数字段：`0.2.0-rc1` 这类预发布标识宁可判「无法理解」也不猜
        //（/releases/latest 只返回正式版，真出现时属异常路径）
        let n: u64 = part.parse().ok()?;
        out.push(n);
    }
    Some(out)
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_table() {
        assert_eq!(parse_version("v0.2.0"), Some(vec![0, 2, 0]));
        assert_eq!(parse_version("0.2.0"), Some(vec![0, 2, 0]));
        assert_eq!(parse_version("1.0"), Some(vec![1, 0]));
        assert_eq!(parse_version(" v0.2.0 "), Some(vec![0, 2, 0]));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("v"), None);
        // 预发布后缀不猜（/releases/latest 只返回正式版，此为纯防御）
        assert_eq!(parse_version("0.2.0-rc1"), None);
    }

    #[test]
    fn compare_rules_with_injected_current() {
        // 远端更新（主/次/补丁任一段更大都要报）
        assert_eq!(
            compare_versions("0.2.0", "v99.0.0"),
            Some(Outcome::Newer("v99.0.0".into()))
        );
        assert_eq!(
            compare_versions("0.2.0", "0.3.0"),
            Some(Outcome::Newer("0.3.0".into()))
        );
        assert_eq!(
            compare_versions("0.2.0", "v0.2.1"),
            Some(Outcome::Newer("v0.2.1".into()))
        );
        // 当前版本自身 / v 前缀形态 = 已是最新
        assert_eq!(compare_versions("0.2.0", "0.2.0"), Some(Outcome::UpToDate));
        assert_eq!(compare_versions("0.2.0", "v0.2.0"), Some(Outcome::UpToDate));
        // 更旧 → 已是最新（本地自构建常见于远端刚删 tag），绝不误报
        assert_eq!(compare_versions("0.2.0", "v0.0.1"), Some(Outcome::UpToDate));
        // 段数不同按补 0 处理：0.2.0 与 0.2.0.0 相等
        assert_eq!(
            compare_versions("0.2.0", "v0.2.0.0"),
            Some(Outcome::UpToDate)
        );
        assert_eq!(compare_versions("0.2", "v0.2.0"), Some(Outcome::UpToDate));
        // 无法理解 → None（调用方转错误文案，绝不能说成「已是最新」）
        assert_eq!(compare_versions("0.2.0", "vnext"), None);
        assert_eq!(compare_versions("0.2.0", ""), None);
        // 本地版本号坏了同样 None
        assert_eq!(compare_versions("0.3.0-rc1", "v0.2.0"), None);
    }

    #[test]
    fn failure_message_distinguishes_local_and_remote() {
        // 本地解析失败不得被扣到远端头上（评审意见6）
        assert_eq!(
            compare_failure_message("0.3.0-rc1", "v0.2.0"),
            "无法理解本地版本号「0.3.0-rc1」"
        );
        assert_eq!(
            compare_failure_message("0.2.0", "vnext"),
            "无法理解远端版本号「vnext」"
        );
    }

    /// GitHub /releases/latest 真实响应形态的内联 fixture：含 html_url /
    /// assets / prerelease 等冗余字段，验证只取 tag_name、其余容错忽略。
    const FIXTURE_RELEASE_JSON: &str = r###"{
  "url": "https://api.github.com/repos/ailater/LaterTerm/releases/1",
  "html_url": "https://github.com/ailater/LaterTerm/releases/tag/v0.2.0",
  "id": 1,
  "tag_name": "v0.2.0",
  "target_commitish": "main",
  "name": "v0.2.0",
  "draft": false,
  "prerelease": false,
  "created_at": "2026-11-03T00:00:00Z",
  "published_at": "2026-11-03T00:00:00Z",
  "assets": [
    {
      "id": 11,
      "name": "LaterTerm_0.2.0_amd64.AppImage",
      "browser_download_url": "https://github.com/ailater/LaterTerm/releases/download/v0.2.0/LaterTerm_0.2.0_amd64.AppImage",
      "size": 13000000
    }
  ],
  "body": "## 更新内容"
}"###;

    #[test]
    fn parse_release_tag_tolerates_extra_fields() {
        assert_eq!(
            parse_release_tag(FIXTURE_RELEASE_JSON),
            Ok("v0.2.0".to_string())
        );
    }

    #[test]
    fn parse_release_tag_reports_missing_field() {
        // 缺 tag_name：明确报缺字段，不猜、不当作已是最新
        let missing =
            r#"{"url":"https://api.github.com/repos/ailater/LaterTerm/releases/1","name":"oops"}"#;
        assert_eq!(parse_release_tag(missing), Err("响应缺 tag_name".into()));
        // tag_name 不是字符串同样算缺
        let numeric = r#"{"tag_name": 42}"#;
        assert_eq!(parse_release_tag(numeric), Err("响应缺 tag_name".into()));
    }

    #[test]
    fn parse_release_tag_rejects_bad_json() {
        let err = parse_release_tag("<html>502 Bad Gateway</html>").unwrap_err();
        assert!(err.starts_with("响应不是有效 JSON"), "实际：{err}");
        // 空串同走 JSON 解析失败路径（serde 的具体报错细节不作精确断言）
        let empty = parse_release_tag("").unwrap_err();
        assert!(empty.starts_with("响应不是有效 JSON"), "实际：{empty}");
    }

    #[test]
    fn evaluate_release_pipeline_offline() {
        // 完整管线（解析 + 比较）离线可测：更新 / 已是最新 / 坏响应
        assert_eq!(
            evaluate_release(FIXTURE_RELEASE_JSON, "0.1.0"),
            Ok(Outcome::Newer("v0.2.0".into()))
        );
        assert_eq!(
            evaluate_release(FIXTURE_RELEASE_JSON, "0.2.0"),
            Ok(Outcome::UpToDate)
        );
        // 失败一律 Err，绝不呈现为 Ok(UpToDate)——LaterScreen 失败语义口径
        assert!(evaluate_release("gateway timeout text", "0.2.0").is_err());
        assert!(evaluate_release(r#"{"name":"no tag"}"#, "0.2.0").is_err());
    }

    #[test]
    fn state_machine_start_is_idempotent_and_clear_keeps_inflight() {
        let mut chk = UpdateCheck::default();
        assert!(!chk.checking());
        assert!(chk.result().is_none());
        // 双击：第一次真正启动，第二次被幂等门挡下
        assert!(chk.start());
        assert!(chk.checking());
        assert!(!chk.start(), "检查中再次 start 应被忽略");
        // 结果未回时 clear 只清结果（本来就空），不打断在途
        chk.clear();
        assert!(chk.checking());
        // 结果回写：checking 翻落，结果可读
        chk.finish(Ok(Outcome::Newer("v99.0.0".into())));
        assert!(!chk.checking());
        assert_eq!(chk.result(), Some(&Ok(Outcome::Newer("v99.0.0".into()))));
        // 「知道了」清除结果；再点可发起新一轮
        chk.clear();
        assert!(chk.result().is_none());
        assert!(!chk.checking());
        assert!(chk.start());
    }

    #[test]
    fn error_result_is_never_up_to_date() {
        // Err 原样保存：状态机不做任何「失败→已是最新」的翻译
        let mut chk = UpdateCheck::default();
        chk.finish(Err("GitHub 返回 403".into()));
        assert_eq!(chk.result(), Some(&Err("GitHub 返回 403".into())));
        assert_ne!(chk.result(), Some(&Ok(Outcome::UpToDate)));
    }

    #[test]
    fn urls_point_to_the_repo() {
        assert_eq!(REPO, "ailater/LaterTerm");
        assert_eq!(
            release_page_url(),
            "https://github.com/ailater/LaterTerm/releases/latest"
        );
        assert_eq!(
            LATEST_API,
            "https://api.github.com/repos/ailater/LaterTerm/releases/latest"
        );
    }
}
