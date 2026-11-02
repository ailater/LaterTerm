#[cfg(any(windows, test))]
fn resource_icon_path(path: &str) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!("//{}", unc.replace('\\', "/"))
    } else {
        path.strip_prefix(r"\\?\")
            .unwrap_or(path)
            .replace('\\', "/")
    }
}

fn main() {
    #[cfg(windows)]
    {
        use std::path::PathBuf;

        let manifest =
            PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let icon = manifest
            .join("../../assets/icons/laterterm.ico")
            .canonicalize()
            .expect("应用图标必须存在");
        let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
        let resource = output.join("laterterm.rc");
        // 使用正斜线避免 RC 将 Windows 反斜线路径误作转义；图标固定使用 ID 1。
        let icon_path = icon.to_str().expect("图标路径必须为 UTF-8");
        let icon_path = resource_icon_path(icon_path);
        assert!(
            !icon_path.contains(['"', '\n', '\r']),
            "图标路径包含非法字符"
        );
        std::fs::write(&resource, format!("1 ICON \"{icon_path}\"\n"))
            .expect("生成 Windows 图标资源失败");
        println!("cargo:rerun-if-changed={}", icon.display());
        embed_resource::compile(resource, embed_resource::NONE)
            .manifest_required()
            .expect("嵌入 Windows 图标资源失败");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn resource_paths_keep_drive_and_unc_roots() {
        assert_eq!(
            super::resource_icon_path(r"\\?\C:\work space\logo.ico"),
            "C:/work space/logo.ico"
        );
        assert_eq!(
            super::resource_icon_path(r"\\?\UNC\server\share\logo.ico"),
            "//server/share/logo.ico"
        );
        assert_eq!(
            super::resource_icon_path(r"\\server\share\logo.ico"),
            "//server/share/logo.ico"
        );
    }
}
