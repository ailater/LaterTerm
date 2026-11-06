//! 首启引导（M4）：welcome 覆盖层的探测与暂选逻辑。
//!
//! 职责边界：本模块只做「探测 + 纯解析 + 暂选状态」，不写配置、不碰 UI——
//! 解析全部是纯函数（IO 结果经参数注入），生产 IO 收敛在底部的 cfg 隔离薄壳，
//! 由 workspace 在首启时于后台线程调用 [`probe`] 并经 flume 泵回主线程。
//! 跳过口径沿用 workspace 的现行判定（settings.toml 存在即跳过，与
//! `welcome_visible` 同源），本模块不重复判定。

use std::path::Path;

/// 常用等宽字体预设（fontconfig 解析不到的名称会自然回退系统默认）。
/// 设置面板与首启引导共用这一份清单（单一真值来源）。
pub const FONT_PRESETS: [&str; 6] = [
    "JetBrainsMono Nerd Font",
    "CaskaydiaCove Nerd Font",
    "FiraCode Nerd Font",
    "Sarasa Mono SC",
    "Noto Sans Mono CJK SC",
    "monospace",
];

/// 后台探测产物：shell 候选 + 等宽字体候选。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResults {
    pub shells: Vec<ShellCandidate>,
    pub fonts: Vec<FontCandidate>,
}

/// 引导层暂选状态：探测结果 + 用户在芯片上点选的 shell。
///
/// `selected_shell` 的 `None` 语义是「跟随默认（动态解析）」——选中
/// `is_current` 芯片（当前 shell）时保持 None，避免把动态默认固化成具体
/// 路径（用户日后改登录 shell 应用应继续跟随）；探测未到就点完成时也为
/// None，同样不写。
#[derive(Default)]
pub struct OnboardingState {
    /// 后台探测结果（None = 未到达，UI 显示「探测中」占位）。
    pub probe: Option<ProbeResults>,
    /// 用户暂选的 shell；None = 跟随默认。完成时与 settings.shell 比对落盘。
    pub selected_shell: Option<String>,
}

/// shell 候选来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellSource {
    /// 当前环境 shell（$SHELL，unix）。
    Current,
    /// /etc/shells 枚举出的系统合法 shell。
    Listed,
    /// 平台默认兜底（`laterterm_local::default_shell()`），仅在枚举为空时出现。
    Default,
}

/// shell 候选项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellCandidate {
    pub path: String,
    /// 是否为当前实际使用的 shell（芯片上标「当前」）。
    pub is_current: bool,
    pub source: ShellSource,
}

/// 等宽字体候选。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontCandidate {
    pub name: String,
    /// FONT_PRESETS 命中的推荐项（芯片上标「推荐」并置前）。
    pub recommended: bool,
}

/// 解析 /etc/shells 文本为 shell 候选列表（纯函数，IO 经参数注入）。
///
/// 规则：
/// - 跳过 `#` 注释行与空行；
/// - `env_shell`（$SHELL）合法且存在时置顶并标 `is_current`，与列表去重；
/// - 每个 path 都过 `exists` 闭包过滤（生产传 `Path::exists`，测试传 mock）；
/// - 路径合法性谓词与 laterterm-local / laterterm-config 的
///   `is_valid_shell_path` 同规则（非空、无控制字符/引号、绝对路径）；
/// - 候选为空时退回单条 `fallback`（`source=Default`，标 is_current——它就是
///   应用实际会用的 shell）；候选非空但无 is_current 时（如 $SHELL 未设置但
///   /etc/shells 可枚举），给与 fallback 相等的条目补标 is_current。
pub fn parse_shells(
    env_shell: Option<&str>,
    etc_shells: &str,
    fallback: &str,
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<ShellCandidate> {
    let mut candidates: Vec<ShellCandidate> = Vec::new();
    let env_valid = env_shell.filter(|value| is_valid_shell_path(value));
    if let Some(path) = env_valid.filter(|path| exists(Path::new(path))) {
        candidates.push(ShellCandidate {
            path: path.to_owned(),
            is_current: true,
            source: ShellSource::Current,
        });
    }
    for line in etc_shells.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !is_valid_shell_path(line) {
            continue;
        }
        if !exists(Path::new(line)) || candidates.iter().any(|item| item.path == line) {
            continue;
        }
        candidates.push(ShellCandidate {
            path: line.to_owned(),
            is_current: false,
            source: ShellSource::Listed,
        });
    }
    if candidates.is_empty() {
        candidates.push(ShellCandidate {
            path: fallback.to_owned(),
            is_current: true,
            source: ShellSource::Default,
        });
    } else if !candidates.iter().any(|item| item.is_current)
        && let Some(matched) = candidates.iter_mut().find(|item| item.path == fallback)
    {
        matched.is_current = true;
    }
    candidates
}

/// 解析 `fc-list :mono family` 输出为字体候选列表（纯函数）。
///
/// 实测两坑（本机 fontconfig）：
/// - 逗号别名行「JetBrainsMono Nerd Font,JetBrainsMono NF」须取首个规范名
///   （与 gpui cosmic-text 的 `families.first()` 口径一致）；
/// - `:mono` 会混入 Noto Color Emoji / Noto Sans SignWriting 等非文本字体，
///   按名称关键字过滤，否则「推荐字体」里出现 emoji 字体。
///
/// FONT_PRESETS 命中者置前并标 `recommended`（按预设顺序），其余按字母序。
pub fn parse_mono_fonts(fc_list_output: &str, presets: &[&str]) -> Vec<FontCandidate> {
    let mut names: Vec<String> = Vec::new();
    for line in fc_list_output.lines() {
        // 逗号别名行取首个规范名
        let name = line.split(',').next().unwrap_or("").trim();
        if name.is_empty() || is_symbolic_font_name(name) {
            continue;
        }
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
        }
    }
    let mut candidates: Vec<FontCandidate> = Vec::new();
    for preset in presets {
        if let Some(position) = names.iter().position(|name| name == preset) {
            candidates.push(FontCandidate {
                name: names.remove(position),
                recommended: true,
            });
        }
    }
    names.sort();
    candidates.extend(names.into_iter().map(|name| FontCandidate {
        name,
        recommended: false,
    }));
    candidates
}

/// fc-list 不可用时的兜底：直接把静态预设当候选（全部标推荐）。
pub fn preset_fonts(presets: &[&str]) -> Vec<FontCandidate> {
    presets
        .iter()
        .map(|name| FontCandidate {
            name: (*name).to_owned(),
            recommended: true,
        })
        .collect()
}

/// 名称含 Emoji/Symbol/Icon/SignWriting（不区分大小写）视为非文本字体。
/// 实测 `fc-list :mono` 会混入 Noto Color Emoji、Noto Sans SignWriting 等。
fn is_symbolic_font_name(name: &str) -> bool {
    let lowered = name.to_lowercase();
    ["emoji", "symbol", "icon", "signwriting"]
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// shell 路径结构性校验：非空、无控制字符/引号、绝对路径。
/// 与 laterterm-local / laterterm-config 的同名谓词同规则，修改须三处同步。
fn is_valid_shell_path(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\'' | '"'))
        && Path::new(value).is_absolute()
}

// ----------------------------------------------------------------------
// IO 薄壳（cfg 隔离；仅 probe 在后台线程调用）
// ----------------------------------------------------------------------

/// 一次完整探测：读 /etc/shells + 跑 fc-list，纯解析在上方。
/// 仅生产构建使用（测试统一走 deliver_probe 注入，IO 薄壳一并排除，
/// 避免测试构建 dead_code 告警）。
#[cfg(not(test))]
pub fn probe() -> ProbeResults {
    let fallback = laterterm_local::default_shell();
    let shells = parse_shells(
        env_shell().as_deref(),
        &read_etc_shells(),
        &fallback,
        &Path::exists,
    );
    let fonts = match run_fc_list() {
        Some(output) => parse_mono_fonts(&output, FONT_PRESETS.as_slice()),
        None => preset_fonts(FONT_PRESETS.as_slice()),
    };
    ProbeResults { shells, fonts }
}

/// 当前环境的 shell 环境变量（unix 读 $SHELL；其余平台无对应惯例，返回
/// None 走 default_shell 兜底——Windows 沿用 COMSPEC 逻辑，不另做枚举）。
#[cfg(all(not(test), unix))]
fn env_shell() -> Option<String> {
    std::env::var("SHELL").ok()
}

#[cfg(all(not(test), not(unix)))]
fn env_shell() -> Option<String> {
    None
}

/// 读 /etc/shells；文件缺失/不可读返回空串（解析侧自然退化为兜底候选）。
#[cfg(all(not(test), unix))]
fn read_etc_shells() -> String {
    std::fs::read_to_string("/etc/shells").unwrap_or_default()
}

#[cfg(all(not(test), not(unix)))]
fn read_etc_shells() -> String {
    String::new()
}

/// 跑 `fc-list :mono family`；命令缺失/失败/空输出返回 None → 回退预设。
/// Linux 与 macOS（常带自装 fontconfig）同路径；Windows 不做 DirectWrite
/// 原生枚举（范围控制），恒回退 FONT_PRESETS。
#[cfg(all(not(test), any(target_os = "linux", target_os = "macos")))]
fn run_fc_list() -> Option<String> {
    let output = std::process::Command::new("fc-list")
        .arg(":mono")
        .arg("family")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(all(not(test), not(any(target_os = "linux", target_os = "macos"))))]
fn run_fc_list() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注入假 /etc/shells：$SHELL 置顶标当前、注释行剔除、去重、
    /// 不存在路径被 exists 闭包过滤、结果非空时不含兜底项。
    #[test]
    fn parse_shells_prioritizes_env_dedupes_and_filters_missing() {
        let candidates = parse_shells(
            Some("/bin/bash"),
            "# 注释\n/bin/sh\n/bin/bash\n/usr/bin/fish\n",
            "/bin/bash",
            &|path: &Path| path != Path::new("/usr/bin/fish"),
        );
        assert_eq!(
            candidates,
            vec![
                ShellCandidate {
                    path: "/bin/bash".into(),
                    is_current: true,
                    source: ShellSource::Current,
                },
                ShellCandidate {
                    path: "/bin/sh".into(),
                    is_current: false,
                    source: ShellSource::Listed,
                },
            ]
        );
    }

    /// 空 env + 空文件 → 单条平台默认兜底候选（标当前）。
    #[test]
    fn parse_shells_falls_back_to_default_when_nothing_enumerable() {
        let candidates = parse_shells(None, "", "/bin/bash", &|_| true);
        assert_eq!(
            candidates,
            vec![ShellCandidate {
                path: "/bin/bash".into(),
                is_current: true,
                source: ShellSource::Default,
            }]
        );
    }

    /// env 值非法（带参数/相对路径）视同未设置：走列表枚举，fallback 命中项
    /// 补标当前；exists 全假 → 兜底单条。
    #[test]
    fn parse_shells_treats_invalid_env_as_unset_and_marks_fallback_match() {
        let candidates = parse_shells(
            Some("bash -l"),
            "/bin/bash\n/bin/zsh\n",
            "/bin/bash",
            &|_| true,
        );
        assert_eq!(
            candidates,
            vec![
                ShellCandidate {
                    path: "/bin/bash".into(),
                    is_current: true,
                    source: ShellSource::Listed,
                },
                ShellCandidate {
                    path: "/bin/zsh".into(),
                    is_current: false,
                    source: ShellSource::Listed,
                },
            ]
        );
        let all_missing = parse_shells(Some("/bin/bash"), "/bin/sh\n", "/bin/bash", &|_| false);
        assert_eq!(
            all_missing,
            vec![ShellCandidate {
                path: "/bin/bash".into(),
                is_current: true,
                source: ShellSource::Default,
            }]
        );
    }

    /// 本机真实 fc-list 输出片段：别名取首、Emoji/SignWriting 过滤、
    /// 预设命中标推荐并置前、其余字母序。
    #[test]
    fn parse_mono_fonts_takes_first_alias_and_filters_symbolic() {
        let output = "JetBrainsMono Nerd Font,JetBrainsMono NF\nNoto Color Emoji\nNoto Sans SignWriting,Noto Sans SignWrit\nDejaVu Sans Mono\nUbuntu Mono\n";
        let candidates = parse_mono_fonts(output, &["JetBrainsMono Nerd Font", "monospace"]);
        assert_eq!(
            candidates,
            vec![
                FontCandidate {
                    name: "JetBrainsMono Nerd Font".into(),
                    recommended: true,
                },
                FontCandidate {
                    name: "DejaVu Sans Mono".into(),
                    recommended: false,
                },
                FontCandidate {
                    name: "Ubuntu Mono".into(),
                    recommended: false,
                },
            ]
        );
    }

    /// 去重与「Symbols Nerd Font」类符号字体过滤（不区分大小写）；
    /// 非预设的合法 mono 字体（DejaVu）保留并排在推荐项之后。
    #[test]
    fn parse_mono_fonts_dedupes_and_filters_symbols_case_insensitively() {
        let output = "DejaVu Sans Mono\nDejaVu Sans Mono\nSymbols Nerd Font\nSarasa Mono SC,Sarasa Mono SC Slab\n";
        let candidates = parse_mono_fonts(output, &["Sarasa Mono SC"]);
        assert_eq!(
            candidates,
            vec![
                FontCandidate {
                    name: "Sarasa Mono SC".into(),
                    recommended: true,
                },
                FontCandidate {
                    name: "DejaVu Sans Mono".into(),
                    recommended: false,
                },
            ]
        );
    }

    /// fc-list 不可用：静态预设全量兜底（全部标推荐）。
    #[test]
    fn preset_fonts_marks_every_preset_recommended() {
        let candidates = preset_fonts(&FONT_PRESETS);
        assert_eq!(candidates.len(), FONT_PRESETS.len());
        assert!(candidates.iter().all(|candidate| candidate.recommended));
        assert_eq!(candidates[0].name, "JetBrainsMono Nerd Font");
    }

    /// 路径谓词与 laterterm-local 同规则：带参数/引号/控制字符/相对路径一律非法。
    #[test]
    fn shell_path_predicate_rejects_non_plain_paths() {
        assert!(is_valid_shell_path("/bin/bash"));
        assert!(is_valid_shell_path("/opt/My Shell/bin/sh"));
        for invalid in [
            "",
            "bash -l",
            "relative",
            "/bin/sh' -x",
            "/bin/sh\" tail",
            "/bin/sh\nnewline",
        ] {
            assert!(!is_valid_shell_path(invalid), "{invalid:?}");
        }
    }
}
