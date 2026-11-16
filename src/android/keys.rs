//! Key and button names to Android key codes (`input keyevent`).

/// Text `input text` can type: printable ASCII. Anything else (Korean,
/// emoji, accents, control characters) it drops or garbles. `%s` is its own
/// escape for a space, so a literal `%s` cannot be typed either.
pub fn typeable(text: &str) -> Result<(), String> {
    if let Some(c) = text.chars().find(|c| !(' '..='~').contains(c)) {
        return Err(format!(
            "`input text` can only type printable ASCII; {:?} (U+{:04X}) is not",
            c, c as u32
        ));
    }
    if text.contains("%s") {
        return Err("`input text` reads `%s` as a space, so it cannot type a literal `%s`".into());
    }
    Ok(())
}

/// `text` as the argument of `input text`: spaces as `%s`, the rest quoted
/// for the phone's shell.
pub fn input_text_arg(text: &str) -> String {
    super::adb::sh_quote(&text.replace(' ', "%s"))
}

fn named(key: &str) -> Option<u32> {
    Some(match key {
        "return" | "enter" => 66,
        "esc" | "escape" => 111,
        "backspace" => 67,
        "delete" | "forward_delete" => 112,
        "tab" => 61,
        "space" => 62,
        "up" => 19,
        "down" => 20,
        "left" => 21,
        "right" => 22,
        "home" => 122,
        "end" => 123,
        "pageup" => 92,
        "pagedown" => 93,
        "back" => 4,
        "menu" => 82,
        "search" => 84,
        "-" => 69,
        "=" => 70,
        "[" => 71,
        "]" => 72,
        "\\" => 73,
        ";" => 74,
        "'" => 75,
        "`" => 68,
        "," => 55,
        "." => 56,
        "/" => 76,
        _ => return None,
    })
}

fn char_code(c: char) -> Option<u32> {
    match c {
        'a'..='z' => Some(29 + (c as u32 - 'a' as u32)),
        '0'..='9' => Some(7 + (c as u32 - '0' as u32)),
        _ => None,
    }
}

/// `return`, `ctrl+a`, `shift+tab`, `keycode_back`, `kc:4`. Returns the key
/// code and modifier key codes. `cmd` means Ctrl: Android's text shortcuts
/// (select all, copy, paste) are Ctrl ones.
pub fn parse_combo(input: &str) -> Result<(u32, Vec<u32>), String> {
    let lower = input.trim().to_lowercase();
    if lower.is_empty() {
        return Err("empty key".into());
    }
    let parts: Vec<&str> = if lower == "+" { vec!["+"] } else { lower.split('+').collect() };
    let (key, mods) = parts.split_last().unwrap();
    let mut modifiers = Vec::new();
    for m in mods {
        modifiers.push(match *m {
            "cmd" | "command" | "ctrl" | "control" => 113,
            "opt" | "option" | "alt" => 57,
            "shift" => 59,
            other => return Err(format!("unknown modifier `{other}` (cmd, ctrl, opt, shift)")),
        });
    }
    let code = if let Some(n) = key.strip_prefix("kc:") {
        n.parse::<u32>().map_err(|_| format!("bad key code `{key}`"))?
    } else if let Some(name) = key.strip_prefix("keycode_") {
        return keycode_name(name).map(|c| (c, modifiers)).ok_or_else(|| format!("unknown key `{key}`"));
    } else if let Some(c) = named(key) {
        c
    } else if key.chars().count() == 1 {
        char_code(key.chars().next().unwrap()).ok_or_else(|| format!("unknown key `{key}`"))?
    } else {
        return Err(format!(
            "unknown key `{key}` (return, esc, tab, space, backspace, delete, up, down, left, right, home, end, pageup, pagedown, back, a letter, a digit, keycode_<NAME> or kc:<number>)"
        ));
    };
    if code == 0 || code > 400 {
        return Err(format!("key code {code} is out of range"));
    }
    Ok((code, modifiers))
}

/// `KEYCODE_<NAME>` for the names above and the letters and digits.
fn keycode_name(name: &str) -> Option<u32> {
    let alias = match name {
        "enter" => "return",
        "escape" => "esc",
        "del" => "backspace",
        "forward_del" => "delete",
        "dpad_up" => "up",
        "dpad_down" => "down",
        "dpad_left" => "left",
        "dpad_right" => "right",
        "move_home" => "home",
        "move_end" => "end",
        "page_up" => "pageup",
        "page_down" => "pagedown",
        other => other,
    };
    named(alias).or_else(|| (alias.chars().count() == 1).then(|| char_code(alias.chars().next()?)).flatten())
}

/// Hardware buttons: home, back, lock (power), volume, recents.
pub fn button(name: &str) -> Result<u32, String> {
    Ok(match name.trim().to_lowercase().replace('-', "_").as_str() {
        "home" => 3,
        "back" => 4,
        "lock" | "power" => 26,
        "volume_up" => 24,
        "volume_down" => 25,
        "recents" | "app_switch" => 187,
        other => return Err(format!("unknown button `{other}` (home, back, lock, volume_up, volume_down, recents)")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_printable_ascii_is_typeable() {
        assert!(typeable("hello world 123 !@#$^&*()_+-=[]{};':\",./<>?`~\\|").is_ok());
        assert!(typeable("안녕").unwrap_err().contains("U+C548"));
        assert!(typeable("😀").is_err());
        assert!(typeable("café").is_err());
        assert!(typeable("line\nbreak").is_err());
        assert!(typeable("tab\there").is_err());
        assert!(typeable("100%sure").is_err());
        assert!(typeable("100% sure").is_ok());
    }

    #[test]
    fn text_is_escaped_for_input_and_the_shell() {
        assert_eq!(input_text_arg("wifi"), "wifi");
        assert_eq!(input_text_arg("hello world"), "hello%sworld");
        assert_eq!(input_text_arg("it's"), r"'it'\''s'");
        assert_eq!(input_text_arg("a&b;c|d"), "'a&b;c|d'");
        assert_eq!(input_text_arg("$(id) `id`"), "'$(id)%s`id`'");
        assert_eq!(input_text_arg("\"q\""), "'\"q\"'");
    }

    #[test]
    fn keys_and_combos() {
        assert_eq!(parse_combo("return").unwrap(), (66, vec![]));
        assert_eq!(parse_combo("Cmd+A").unwrap(), (29, vec![113]));
        assert_eq!(parse_combo("shift+tab").unwrap(), (61, vec![59]));
        assert_eq!(parse_combo("0").unwrap(), (7, vec![]));
        assert_eq!(parse_combo("z").unwrap(), (54, vec![]));
        assert_eq!(parse_combo("back").unwrap(), (4, vec![]));
        assert_eq!(parse_combo("KEYCODE_DEL").unwrap(), (67, vec![]));
        assert_eq!(parse_combo("keycode_a").unwrap(), (29, vec![]));
        assert_eq!(parse_combo("kc:187").unwrap(), (187, vec![]));
        assert!(parse_combo("hyper+a").is_err());
        assert!(parse_combo("nope").is_err());
        assert!(parse_combo("").is_err());
        assert!(parse_combo("kc:0").is_err());
    }

    #[test]
    fn buttons() {
        assert_eq!(button("home").unwrap(), 3);
        assert_eq!(button("back").unwrap(), 4);
        assert_eq!(button("lock").unwrap(), 26);
        assert_eq!(button("volume-up").unwrap(), 24);
        assert!(button("siri").is_err());
    }
}
