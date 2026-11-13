//! Keyboard input handling for the terminal emulator.
//!
//! This module provides [`keystroke_to_bytes`], which converts GPUI keyboard
//! events into terminal escape sequences that can be written to the PTY.
//!
//! # Key Mappings
//!
//! ## Special Keys
//!
//! | Key | Sequence | Notes |
//! |-----|----------|-------|
//! | Enter | `\r` (0x0D) | Carriage return |
//! | Escape | `\x1b` (0x1B) | ESC |
//! | Backspace | `\x7f` (0x7F) | DEL |
//! | Tab | `\t` (0x09) | Horizontal tab |
//! | Shift+Tab | `\x1b[Z` | Backtab |
//! | Space | ` ` (0x20) | Space |
//! | Ctrl+Space | `\x00` | NUL |
//!
//! ## Arrow Keys
//!
//! Arrow key sequences depend on application cursor mode:
//!
//! | Key | Normal Mode | App Cursor Mode |
//! |-----|-------------|-----------------|
//! | Up | `\x1b[A` | `\x1bOA` |
//! | Down | `\x1b[B` | `\x1bOB` |
//! | Right | `\x1b[C` | `\x1bOC` |
//! | Left | `\x1b[D` | `\x1bOD` |
//!
//! ## Navigation Keys
//!
//! | Key | Sequence |
//! |-----|----------|
//! | Home | `\x1b[H` |
//! | End | `\x1b[F` |
//! | PageUp | `\x1b[5~` |
//! | PageDown | `\x1b[6~` |
//! | Insert | `\x1b[2~` |
//! | Delete | `\x1b[3~` |
//!
//! ## Function Keys
//!
//! | Key | Sequence |
//! |-----|----------|
//! | F1-F4 | `\x1bOP` - `\x1bOS` |
//! | F5-F12 | `\x1b[15~` - `\x1b[24~` |
//!
//! ## Control Combinations
//!
//! Ctrl+A through Ctrl+Z map to ASCII control characters 0x01-0x1A:
//!
//! | Combination | Byte |
//! |-------------|------|
//! | Ctrl+A | 0x01 |
//! | Ctrl+C | 0x03 (interrupt) |
//! | Ctrl+D | 0x04 (EOF) |
//! | Ctrl+Z | 0x1A (suspend) |
//!
//! ## Alt Combinations
//!
//! Alt+key sends ESC followed by the key: `\x1b` + key
//!
//! # Terminal Mode Effects
//!
//! The [`TermMode`] flags affect key sequences:
//!
//! - **APP_CURSOR**: Changes arrow key sequences from CSI to SS3 format
//!
//! # Example
//!
//! ```
//! use gpui::Keystroke;
//! use alacritty_terminal::term::TermMode;
//! use gpui_terminal::input::keystroke_to_bytes;
//!
//! // Enter key
//! let keystroke = Keystroke::parse("enter").unwrap();
//! assert_eq!(keystroke_to_bytes(&keystroke, TermMode::empty()), Some(b"\r".to_vec()));
//!
//! // Ctrl+C (interrupt)
//! let keystroke = Keystroke::parse("ctrl-c").unwrap();
//! assert_eq!(keystroke_to_bytes(&keystroke, TermMode::empty()), Some(vec![0x03]));
//! ```

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// Convert a GPUI keystroke to terminal escape sequence bytes.
///
/// This function translates GPUI keyboard events into the appropriate byte sequences
/// expected by terminal applications. It handles special keys, control characters,
/// and application cursor mode.
///
/// # Arguments
///
/// * `keystroke` - The GPUI keystroke to convert
/// * `mode` - The current terminal mode (affects arrow key sequences)
///
/// # Returns
///
/// An optional vector of bytes representing the terminal escape sequence.
/// Returns `None` if the keystroke should not produce any output.
///
/// # Examples
///
/// ```
/// use gpui::Keystroke;
/// use alacritty_terminal::term::TermMode;
/// use gpui_terminal::input::keystroke_to_bytes;
///
/// let keystroke = Keystroke::parse("enter").unwrap();
/// let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
/// assert_eq!(bytes, Some(b"\r".to_vec()));
/// ```
pub fn keystroke_to_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    // kitty keyboard protocol disambiguate 层：应用经 `CSI > 1u` 协商启用后
    // （TermMode::DISAMBIGUATE_ESC_CODES 置位）优先走本端编码；未覆盖的键
    // 返回 None 落回 legacy 路径。规范：https://sw.kovidgoyal.net/kitty/keyboard-protocol/
    //
    // 已知不完整支持（代码与 roadmap 双处标注）：应用置更高 flags
    // （REPORT_ALL_KEYS_AS_ESC / REPORT_EVENT_TYPES / REPORT_ASSOCIATED_TEXT /
    // REPORT_ALTERNATE_KEYS）时，Term 端照收置位，但本端编码仍停留在
    // disambiguate 层语义——全键上报 / 事件类型 / 关联文本 / 备选键不做；
    // 不因更高 flags 而回退纯 legacy（disambiguate 形态对更高 flags 的应用
    // 而言仍比丢弃修饰的 legacy 形态更接近预期）。
    if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES)
        && let Some(bytes) = kitty_disambiguate_bytes(keystroke)
    {
        return Some(bytes);
    }

    // Handle special keys first. 功能键臂不带修饰守卫：协议未协商（默认）
    // 时带 ctrl/alt 也送去修饰的 legacy 序列（与协议引入前逐字节一致——
    // tmux/vim 的 alt+方向依赖它）；kitty 已协商时上面的 disambiguate 层
    // 已按协议编码返回，到不了这里。
    match keystroke.key.as_str() {
        // Basic control characters
        "space" => {
            if keystroke.modifiers.control {
                return Some(b"\x00".to_vec()); // Ctrl+Space = NUL
            }
            return Some(b" ".to_vec());
        }
        "enter" => return Some(b"\r".to_vec()),
        "escape" => return Some(b"\x1b".to_vec()),
        "backspace" => return Some(b"\x7f".to_vec()),
        "tab" => {
            // Shift+Tab sends a different sequence
            if keystroke.modifiers.shift {
                return Some(b"\x1b[Z".to_vec());
            }
            return Some(b"\t".to_vec());
        }

        // Arrow keys - check APP_CURSOR mode
        "up" => {
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOA".to_vec());
            }
            return Some(b"\x1b[A".to_vec());
        }
        "down" => {
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOB".to_vec());
            }
            return Some(b"\x1b[B".to_vec());
        }
        "right" => {
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOC".to_vec());
            }
            return Some(b"\x1b[C".to_vec());
        }
        "left" => {
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOD".to_vec());
            }
            return Some(b"\x1b[D".to_vec());
        }

        // Navigation keys
        "home" => return Some(b"\x1b[H".to_vec()),
        "end" => return Some(b"\x1b[F".to_vec()),
        "pageup" => return Some(b"\x1b[5~".to_vec()),
        "pagedown" => return Some(b"\x1b[6~".to_vec()),
        "insert" => return Some(b"\x1b[2~".to_vec()),
        "delete" => return Some(b"\x1b[3~".to_vec()),

        // Function keys
        "f1" => return Some(b"\x1bOP".to_vec()),
        "f2" => return Some(b"\x1bOQ".to_vec()),
        "f3" => return Some(b"\x1bOR".to_vec()),
        "f4" => return Some(b"\x1bOS".to_vec()),
        "f5" => return Some(b"\x1b[15~".to_vec()),
        "f6" => return Some(b"\x1b[17~".to_vec()),
        "f7" => return Some(b"\x1b[18~".to_vec()),
        "f8" => return Some(b"\x1b[19~".to_vec()),
        "f9" => return Some(b"\x1b[20~".to_vec()),
        "f10" => return Some(b"\x1b[21~".to_vec()),
        "f11" => return Some(b"\x1b[23~".to_vec()),
        "f12" => return Some(b"\x1b[24~".to_vec()),

        _ => {}
    }

    // Handle Ctrl+key combinations
    if keystroke.modifiers.control {
        let key = keystroke.key.as_str();

        // Ctrl+A through Ctrl+Z map to 0x01 through 0x1a
        if key.len() == 1 {
            let ch = key.chars().next().unwrap();
            if ch.is_ascii_alphabetic() {
                // Convert to uppercase and then to control character
                let upper = ch.to_ascii_uppercase();
                let ctrl_char = (upper as u8) - b'@';
                return Some(vec![ctrl_char]);
            }

            // Special Ctrl combinations
            match ch {
                '[' => return Some(b"\x1b".to_vec()),  // Ctrl+[
                '\\' => return Some(b"\x1c".to_vec()), // Ctrl+\
                ']' => return Some(b"\x1d".to_vec()),  // Ctrl+]
                '^' => return Some(b"\x1e".to_vec()),  // Ctrl+^
                '_' => return Some(b"\x1f".to_vec()),  // Ctrl+_
                '?' => return Some(b"\x7f".to_vec()),  // Ctrl+?
                _ => {}
            }
        }
    }

    // Handle Alt+key combinations
    if keystroke.modifiers.alt {
        let key = keystroke.key.as_str();
        if key.len() == 1 {
            // Alt+key sends ESC followed by the key
            let ch = key.chars().next().unwrap();
            if ch.is_ascii() {
                let mut bytes = vec![b'\x1b'];
                bytes.push(ch as u8);
                return Some(bytes);
            }
        }
    }

    // Handle regular printable characters
    // Use key_char if available (contains the actual typed character with modifiers like Shift)
    if let Some(key_char) = &keystroke.key_char
        && !keystroke.modifiers.control
        && !keystroke.modifiers.alt
    {
        return Some(key_char.as_bytes().to_vec());
    }

    // Fallback to key for single characters
    let key = keystroke.key.as_str();
    if key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii() && !keystroke.modifiers.control {
            // Handle shift modifier for uppercase
            let ch = if keystroke.modifiers.shift {
                ch.to_ascii_uppercase()
            } else {
                ch
            };
            return Some(vec![ch as u8]);
        }
        // For non-ASCII characters, encode as UTF-8
        if !keystroke.modifiers.control && !keystroke.modifiers.alt {
            return Some(key.as_bytes().to_vec());
        }
    }

    // If we get here, the keystroke doesn't produce any output
    None
}

/// kitty keyboard protocol **disambiguate 层**编码（协议 v1 子集）。
///
/// 范围严格按规范钉死（修订 1，依据规范原文）：
///
/// - **(a) 安全阀例外**：Enter/Tab/Backspace 恒走 legacy 字节 `\r`/`\t`/`\x7f`，
///   任何修饰组合都不转 CSI u——规范原文 "The only exceptions are the
///   Enter, Tab and Backspace keys which still generate the same bytes as
///   in legacy mode"（目的：应用崩溃后仍可敲 `reset` 自救）。返回 `None`
///   落回 legacy 路径（含 Shift+Tab 的 `\x1b[Z`）。
/// - **(b) 文本键**：仅规范 disambiguate 列举的四组合（alt+key / ctrl+key /
///   ctrl+alt+key / shift+alt+key）转 `CSI <codepoint>;<mods>u`——纯 alt
///   去 ESC 前缀歧义是本层存在的首要动机。mods 位 shift=1/alt=2/ctrl=4，
///   参数值 = 位掩码 + 1（kitty/xterm 同式），无修饰省略参数。Esc 视为
///   codepoint 27 的文本键且不受组合限定（disambiguate 的本义即消歧孤
///   ESC：`CSI 27u`）。ctrl+shift 与三修饰组合不在规范列举内 → legacy。
///   无修饰文本键不走本函数（维持 key_char/UTF-8 现路径）。
/// - **(c) 功能键**：带 ctrl/alt 走 **legacy 修饰形式**而非 CSI u——
///   方向/Home/End/F1-F4 → `CSI 1;<mods><A|B|C|D|H|F|P|Q|R|S>`，
///   PgUp/PgDn/Insert/Delete/F5-F12 → `CSI <n>;<mods>~`（规范原文：功能键
///   带修饰走 "CSI 1; modifier [~ABCDEFHPQS]" legacy 形式）。属能力补全
///   （现状对这些键丢弃修饰）而非 CSI u 化。无修饰或仅 shift → `None`
///   （维持现有 legacy 序列，含 APP_CURSOR 的 SS3 形态；带修饰按 xterm
///   惯例恒为 CSI 形态）。
///
/// 返回 `None` = 本层无编码，由 legacy 路径处理。
fn kitty_disambiguate_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = &keystroke.modifiers;
    let key = keystroke.key.as_str();

    // (a) 安全阀：Enter/Tab/Backspace 任何模式下不转 CSI u
    if matches!(key, "enter" | "tab" | "backspace") {
        return None;
    }

    // kitty 修饰位掩码：shift=1 / alt=2 / ctrl=4；CSI 参数值 = 掩码 + 1
    let mods = u8::from(modifiers.shift)
        | (u8::from(modifiers.alt) << 1)
        | (u8::from(modifiers.control) << 2);

    // (c) 功能键带 ctrl/alt → legacy 修饰形式（非功能键落空，继续走文本键）
    if modifiers.control || modifiers.alt {
        // 参数值 = 掩码 + 1，恒 ≥ 3（ctrl=5 / alt=3 / ctrl+alt=7 / …）
        let modifier = format!(";{}", mods + 1);
        let sequence: Option<String> = match key {
            "up" => Some(format!("\x1b[1{modifier}A")),
            "down" => Some(format!("\x1b[1{modifier}B")),
            "right" => Some(format!("\x1b[1{modifier}C")),
            "left" => Some(format!("\x1b[1{modifier}D")),
            "home" => Some(format!("\x1b[1{modifier}H")),
            "end" => Some(format!("\x1b[1{modifier}F")),
            "f1" => Some(format!("\x1b[1{modifier}P")),
            "f2" => Some(format!("\x1b[1{modifier}Q")),
            "f3" => Some(format!("\x1b[1{modifier}R")),
            "f4" => Some(format!("\x1b[1{modifier}S")),
            "pageup" => Some(format!("\x1b[5{modifier}~")),
            "pagedown" => Some(format!("\x1b[6{modifier}~")),
            "insert" => Some(format!("\x1b[2{modifier}~")),
            "delete" => Some(format!("\x1b[3{modifier}~")),
            "f5" => Some(format!("\x1b[15{modifier}~")),
            "f6" => Some(format!("\x1b[17{modifier}~")),
            "f7" => Some(format!("\x1b[18{modifier}~")),
            "f8" => Some(format!("\x1b[19{modifier}~")),
            "f9" => Some(format!("\x1b[20{modifier}~")),
            "f10" => Some(format!("\x1b[21{modifier}~")),
            "f11" => Some(format!("\x1b[23{modifier}~")),
            "f12" => Some(format!("\x1b[24{modifier}~")),
            _ => None, // 非功能键：继续走 (b) 文本键判定
        };
        if let Some(sequence) = sequence {
            return Some(sequence.into_bytes());
        }
    }

    // (b) 文本键：规范列举的四组合 → CSI u（Esc 不受组合限定，恒 CSI 27u）
    let listed_combo = matches!(
        (modifiers.shift, modifiers.alt, modifiers.control),
        (false, true, false) | (false, false, true) | (false, true, true) | (true, true, false)
    );
    if !listed_combo && key != "escape" {
        return None;
    }
    let codepoint: u32 = match key {
        "escape" => 27,
        "space" => 32,
        _ => {
            // 仅接受单字符键名（字母/数字/标点）；GPUI 的 key 对文本键
            // 恒为布局层小写字符，shift 进 mods 不进码点（shift+alt+a →
            // CSI 97;4u 而非 65）
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => u32::from(ch),
                _ => return None, // 多字符键名不在文本键编码范围
            }
        }
    };
    let sequence = if mods == 0 {
        format!("\x1b[{codepoint}u")
    } else {
        format!("\x1b[{codepoint};{}u", mods + 1)
    };
    Some(sequence.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enter_key() {
        let keystroke = Keystroke::parse("enter").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn test_escape_key() {
        let keystroke = Keystroke::parse("escape").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b".to_vec()));
    }

    #[test]
    fn test_backspace_key() {
        let keystroke = Keystroke::parse("backspace").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x7f".to_vec()));
    }

    #[test]
    fn test_tab_key() {
        let keystroke = Keystroke::parse("tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\t".to_vec()));
    }

    #[test]
    fn test_shift_tab() {
        let keystroke = Keystroke::parse("shift-tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn test_arrow_keys_normal_mode() {
        let mode = TermMode::empty();

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1b[A".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1b[B".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1b[C".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn test_arrow_keys_app_cursor_mode() {
        let mode = TermMode::APP_CURSOR;

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1bOA".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1bOB".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1bOC".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1bOD".to_vec()));
    }

    #[test]
    fn test_navigation_keys() {
        let mode = TermMode::empty();

        let home = Keystroke::parse("home").unwrap();
        assert_eq!(keystroke_to_bytes(&home, mode), Some(b"\x1b[H".to_vec()));

        let end = Keystroke::parse("end").unwrap();
        assert_eq!(keystroke_to_bytes(&end, mode), Some(b"\x1b[F".to_vec()));

        let pageup = Keystroke::parse("pageup").unwrap();
        assert_eq!(keystroke_to_bytes(&pageup, mode), Some(b"\x1b[5~".to_vec()));

        let pagedown = Keystroke::parse("pagedown").unwrap();
        assert_eq!(
            keystroke_to_bytes(&pagedown, mode),
            Some(b"\x1b[6~".to_vec())
        );

        let insert = Keystroke::parse("insert").unwrap();
        assert_eq!(keystroke_to_bytes(&insert, mode), Some(b"\x1b[2~".to_vec()));

        let delete = Keystroke::parse("delete").unwrap();
        assert_eq!(keystroke_to_bytes(&delete, mode), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn test_function_keys() {
        let mode = TermMode::empty();

        let f1 = Keystroke::parse("f1").unwrap();
        assert_eq!(keystroke_to_bytes(&f1, mode), Some(b"\x1bOP".to_vec()));

        let f2 = Keystroke::parse("f2").unwrap();
        assert_eq!(keystroke_to_bytes(&f2, mode), Some(b"\x1bOQ".to_vec()));

        let f5 = Keystroke::parse("f5").unwrap();
        assert_eq!(keystroke_to_bytes(&f5, mode), Some(b"\x1b[15~".to_vec()));

        let f12 = Keystroke::parse("f12").unwrap();
        assert_eq!(keystroke_to_bytes(&f12, mode), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn test_ctrl_combinations() {
        let mode = TermMode::empty();

        // Ctrl+A = 0x01
        let ctrl_a = Keystroke::parse("ctrl-a").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_a, mode), Some(vec![0x01]));

        // Ctrl+C = 0x03
        let ctrl_c = Keystroke::parse("ctrl-c").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_c, mode), Some(vec![0x03]));

        // Ctrl+Z = 0x1a
        let ctrl_z = Keystroke::parse("ctrl-z").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_z, mode), Some(vec![0x1a]));

        // Ctrl+Space = 0x00
        let ctrl_space = Keystroke::parse("ctrl-space").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_space, mode), Some(vec![0x00]));
    }

    #[test]
    fn test_alt_combinations() {
        let mode = TermMode::empty();

        // Alt+a sends ESC followed by 'a'
        let alt_a = Keystroke::parse("alt-a").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_a, mode), Some(b"\x1ba".to_vec()));

        // Alt+x sends ESC followed by 'x'
        let alt_x = Keystroke::parse("alt-x").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_x, mode), Some(b"\x1bx".to_vec()));
    }

    #[test]
    fn test_regular_characters() {
        let mode = TermMode::empty();

        let a = Keystroke::parse("a").unwrap();
        assert_eq!(keystroke_to_bytes(&a, mode), Some(b"a".to_vec()));

        let z = Keystroke::parse("z").unwrap();
        assert_eq!(keystroke_to_bytes(&z, mode), Some(b"z".to_vec()));

        let zero = Keystroke::parse("0").unwrap();
        assert_eq!(keystroke_to_bytes(&zero, mode), Some(b"0".to_vec()));
    }

    #[test]
    fn test_space_key() {
        let mode = TermMode::empty();

        let space = Keystroke::parse("space").unwrap();
        assert_eq!(keystroke_to_bytes(&space, mode), Some(b" ".to_vec()));
    }

    // ───────── kitty keyboard protocol（disambiguate 层） ─────────
    //
    // 断言一律精确等值：等值断言即「反向断言」——把 legacy 字节钉死，
    // 防止 CSI u 化的过度实现（Enter=CSI 13u / 方向=CSI u）被钉进测试。

    #[test]
    fn kitty_enter_tab_backspace_stay_legacy_in_any_mode() {
        // 规范安全阀：三种键在任何修饰组合下都保持 legacy 字节
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;

        let enter = Keystroke::parse("enter").unwrap();
        let bytes = keystroke_to_bytes(&enter, mode);
        assert_eq!(bytes, Some(b"\r".to_vec()));
        // 反 CSI u 断言：绝不是 CSI 13;u 形态
        assert_ne!(bytes, Some(b"\x1b[13u".to_vec()));

        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("tab").unwrap(), mode),
            Some(b"\t".to_vec())
        );
        assert_ne!(
            keystroke_to_bytes(&Keystroke::parse("tab").unwrap(), mode),
            Some(b"\x1b[9u".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("backspace").unwrap(), mode),
            Some(b"\x7f".to_vec())
        );
        assert_ne!(
            keystroke_to_bytes(&Keystroke::parse("backspace").unwrap(), mode),
            Some(b"\x1b[127u".to_vec())
        );

        // 带修饰同样不转：ctrl+enter / shift+tab 都维持 legacy 形态
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("ctrl-enter").unwrap(), mode),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("shift-tab").unwrap(), mode),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_esc_and_modified_text_keys() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;

        // Esc：孤 ESC 的消歧是本层本义（无修饰也转 CSI 27u）
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("escape").unwrap(), mode),
            Some(b"\x1b[27u".to_vec())
        );
        // 规范列举的四组合（mods 位 shift=1/alt=2/ctrl=4，参数 = 掩码+1）
        let cases = [
            ("alt-a", &b"\x1b[97;3u"[..]),
            ("ctrl-a", &b"\x1b[97;5u"[..]),
            ("ctrl-alt-a", &b"\x1b[97;7u"[..]),
            ("shift-alt-a", &b"\x1b[97;4u"[..]),
            ("ctrl-1", &b"\x1b[49;5u"[..]),
            // space 是文本键（codepoint 32）：ctrl+space 不再是 legacy NUL
            ("ctrl-space", &b"\x1b[32;5u"[..]),
            ("alt-space", &b"\x1b[32;3u"[..]),
        ];
        for (key, expected) in cases {
            let keystroke = Keystroke::parse(key).unwrap();
            assert_eq!(
                keystroke_to_bytes(&keystroke, mode),
                Some(expected.to_vec()),
                "{key} 应按 disambiguate 编码"
            );
        }
        // 码点取布局层小写键（shift 进 mods 不进码点），钉死不过度 CSI u 化
        assert_ne!(
            keystroke_to_bytes(&Keystroke::parse("shift-alt-a").unwrap(), mode),
            Some(b"\x1b[65;4u".to_vec())
        );
    }

    #[test]
    fn kitty_function_keys_use_legacy_modifier_form() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;

        // 带 ctrl/alt 的功能键走 legacy 修饰形式（非 CSI u 化）
        let cases = [
            ("ctrl-left", &b"\x1b[1;5D"[..]),
            ("ctrl-shift-left", &b"\x1b[1;6D"[..]),
            ("alt-up", &b"\x1b[1;3A"[..]),
            ("ctrl-right", &b"\x1b[1;5C"[..]),
            ("ctrl-home", &b"\x1b[1;5H"[..]),
            ("alt-end", &b"\x1b[1;3F"[..]),
            ("ctrl-delete", &b"\x1b[3;5~"[..]),
            ("ctrl-pageup", &b"\x1b[5;5~"[..]),
            ("ctrl-f5", &b"\x1b[15;5~"[..]),
            ("alt-f1", &b"\x1b[1;3P"[..]),
            ("ctrl-f12", &b"\x1b[24;5~"[..]),
        ];
        for (key, expected) in cases {
            let keystroke = Keystroke::parse(key).unwrap();
            assert_eq!(
                keystroke_to_bytes(&keystroke, mode),
                Some(expected.to_vec()),
                "{key} 应走 legacy 修饰形式"
            );
        }
        // 反 CSI u 断言：ctrl+Left 绝不是「左方向码点 CSI u」形态
        let ctrl_left = keystroke_to_bytes(&Keystroke::parse("ctrl-left").unwrap(), mode);
        assert_eq!(ctrl_left, Some(b"\x1b[1;5D".to_vec()));
        assert_ne!(ctrl_left, Some(b"\x1b[109;5D".to_vec()));
        assert_ne!(ctrl_left, Some(b"\x1b[109;5u".to_vec()));
    }

    #[test]
    fn kitty_disambiguate_unlisted_combos_stay_legacy() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;

        // 无修饰键维持现路径（legacy / key_char）
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("a").unwrap(), mode),
            Some(b"a".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("up").unwrap(), mode),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("f5").unwrap(), mode),
            Some(b"\x1b[15~".to_vec())
        );
        // APP_CURSOR 的 SS3 形态不被 kitty 层劫持
        let app_mode = mode | TermMode::APP_CURSOR;
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("up").unwrap(), app_mode),
            Some(b"\x1bOA".to_vec())
        );
        // ctrl+shift 与三修饰组合不在规范列举内 → legacy 字节
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("ctrl-shift-a").unwrap(), mode),
            Some(vec![0x01])
        );
        // 仅 shift 不触发功能键修饰形式（口径：带 ctrl/alt 才走）
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("shift-up").unwrap(), mode),
            Some(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn kitty_off_keeps_zero_behavior_change() {
        // 协议未协商（默认）：一切走 legacy——alt/ctrl+方向 送去修饰序列
        // （协议引入前的既有行为，tmux/vim 依赖），alt+a 仍是 ESC 前缀形态；
        // 与 input.rs 历史行为逐字节一致
        let mode = TermMode::empty();
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("alt-left").unwrap(), mode),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("alt-a").unwrap(), mode),
            Some(b"\x1ba".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("escape").unwrap(), mode),
            Some(b"\x1b".to_vec())
        );
        // 更高 flags 置位：本端仍按 disambiguate 层语义编码（已知不完整
        // 支持，见 kitty_disambiguate_bytes 注记），不回退纯 legacy
        let higher = TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_ALL_KEYS_AS_ESC
            | TermMode::REPORT_EVENT_TYPES;
        assert_eq!(
            keystroke_to_bytes(&Keystroke::parse("ctrl-left").unwrap(), higher),
            Some(b"\x1b[1;5D".to_vec())
        );
    }
}
