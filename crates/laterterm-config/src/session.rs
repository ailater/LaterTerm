//! 会话恢复（M4 基础功能）：退出时保存标签列表，重启后还原。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 一个被恢复的标签。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTab {
    /// 用户自定义标题（双击重命名），None = 自动标题。
    #[serde(default)]
    pub custom_title: Option<String>,
    /// SSH 服务器名（对应 ServerEntry.name）；None = 本地终端。
    #[serde(default)]
    pub server_name: Option<String>,
}

/// 上次会话的完整快照。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub tabs: Vec<SessionTab>,
    #[serde(default)]
    pub active_tab: usize,
}

impl SessionSnapshot {
    fn path() -> Option<PathBuf> {
        crate::app_config_dir().map(|dir| dir.join("session.json"))
    }

    /// 读上次会话（文件缺失/损坏回落空 = 全新启动）。
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// 落盘（退出或标签变更时调用；失败静默——恢复是增强不是保证）。
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// 清除（用户关闭所有标签 = 下次全新启动）。
    pub fn clear() {
        if let Some(path) = Self::path() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip() {
        let snapshot = SessionSnapshot {
            tabs: vec![
                SessionTab {
                    custom_title: Some("跳板".into()),
                    server_name: Some("生产入口".into()),
                },
                SessionTab {
                    custom_title: None,
                    server_name: None,
                },
                SessionTab {
                    custom_title: Some("日志机".into()),
                    server_name: Some("日志".into()),
                },
            ],
            active_tab: 1,
        };
        let json = serde_json::to_string(&snapshot).expect("序列化");
        let back: SessionSnapshot = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, snapshot);
        assert_eq!(back.tabs.len(), 3);
        assert_eq!(back.active_tab, 1);
    }

    #[test]
    fn empty_snapshot_is_minimal_json() {
        let snapshot = SessionSnapshot::default();
        let json = serde_json::to_string(&snapshot).expect("序列化");
        let back: SessionSnapshot = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, snapshot);
        assert!(back.tabs.is_empty());
    }
}
