//! `uiautomator dump` XML to the same [`Screen`] an iPhone reading gives.
//!
//! The dump is a tree of `<node>` elements whose attributes are the
//! accessibility node's: `text`, `content-desc`, `resource-id`, `class`,
//! `package`, `bounds="[x1,y1][x2,y2]"` in pixels, and boolean flags. Frames
//! become density-independent pixels (dp) so a screen reads at about the size
//! an iPhone's points do; taps are converted back with the same scale.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::ax::tree::{AppInfo, BuildOptions, Element, Rect, Screen};

/// One XML element: its name, attributes in order, and children.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XmlNode {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<XmlNode>,
}

impl XmlNode {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn flag(&self, key: &str) -> bool {
        self.attr(key) == Some("true")
    }
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let ch = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse the small XML subset uiautomator writes: elements with quoted
/// attributes, a declaration, and nothing else of note. Text between tags
/// (uiautomator writes none) is ignored.
pub fn parse_xml(xml: &str) -> Result<XmlNode, String> {
    let bytes = xml.as_bytes();
    let mut i = 0;
    let mut stack: Vec<XmlNode> = vec![XmlNode { name: "#document".into(), ..Default::default() }];
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &xml[i..];
        if rest.starts_with("<?") {
            i += rest.find("?>").ok_or("unterminated declaration")? + 2;
            continue;
        }
        if rest.starts_with("<!--") {
            i += rest.find("-->").ok_or("unterminated comment")? + 3;
            continue;
        }
        if rest.starts_with("<!") {
            i += rest.find('>').ok_or("unterminated markup")? + 1;
            continue;
        }
        if let Some(close) = rest.strip_prefix("</") {
            let end = close.find('>').ok_or("unterminated end tag")?;
            let name = close[..end].trim();
            let node = stack.pop().ok_or("unbalanced end tag")?;
            if node.name != name || stack.is_empty() {
                return Err(format!("end tag </{name}> does not close <{}>", node.name));
            }
            stack.last_mut().unwrap().children.push(node);
            i += 2 + end + 1;
            continue;
        }
        // A start tag.
        let mut j = i + 1;
        while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' && bytes[j] != b'/' {
            j += 1;
        }
        let mut node = XmlNode { name: xml[i + 1..j].to_string(), ..Default::default() };
        if node.name.is_empty() {
            return Err("empty tag name".into());
        }
        let self_closing;
        loop {
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= bytes.len() {
                return Err("unterminated start tag".into());
            }
            if bytes[j] == b'>' {
                self_closing = false;
                j += 1;
                break;
            }
            if bytes[j] == b'/' && bytes.get(j + 1) == Some(&b'>') {
                self_closing = true;
                j += 2;
                break;
            }
            let k0 = j;
            while j < bytes.len() && bytes[j] != b'=' && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' {
                j += 1;
            }
            let key = xml[k0..j].to_string();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) != Some(&b'=') {
                return Err(format!("attribute `{key}` has no value"));
            }
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let quote = *bytes.get(j).ok_or("unterminated attribute")?;
            if quote != b'"' && quote != b'\'' {
                return Err(format!("attribute `{key}` is not quoted"));
            }
            let v0 = j + 1;
            let len = xml[v0..].find(quote as char).ok_or("unterminated attribute value")?;
            node.attrs.push((key, unescape(&xml[v0..v0 + len])));
            j = v0 + len + 1;
        }
        if self_closing {
            stack.last_mut().unwrap().children.push(node);
        } else {
            stack.push(node);
        }
        i = j;
    }
    if stack.len() != 1 {
        return Err(format!("<{}> is not closed", stack.last().map(|n| n.name.as_str()).unwrap_or("?")));
    }
    let doc = stack.pop().unwrap();
    doc.children
        .into_iter()
        .find(|n| n.name == "hierarchy")
        .ok_or_else(|| "no <hierarchy> element".to_string())
}

/// The XML part of what `uiautomator dump /dev/tty` prints: it is followed
/// by "UI hierchary dumped to: /dev/tty" (sic) and may be preceded by
/// warnings.
pub fn extract_xml(out: &str) -> Option<&str> {
    let start = out.find("<?xml").or_else(|| out.find("<hierarchy"))?;
    let end = out.rfind("</hierarchy>")? + "</hierarchy>".len();
    (end > start).then(|| &out[start..end])
}

/// Pixel bounds `[x1,y1][x2,y2]`.
pub fn parse_bounds(s: &str) -> Option<(f64, f64, f64, f64)> {
    let nums: Vec<f64> = s
        .split(['[', ']', ','])
        .filter(|p| !p.is_empty())
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .ok()?;
    match nums.as_slice() {
        [x1, y1, x2, y2] => Some((*x1, *y1, *x2, *y2)),
        _ => None,
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Pixel bounds to a frame in dp (`scale` = density / 160).
pub fn bounds_to_frame(b: (f64, f64, f64, f64), scale: f64) -> Rect {
    let (x1, y1, x2, y2) = b;
    Rect {
        x: round1(x1 / scale),
        y: round1(y1 / scale),
        w: round1((x2 - x1).max(0.0) / scale),
        h: round1((y2 - y1).max(0.0) / scale),
    }
}

/// A point in dp back to whole pixels, kept inside the screen.
pub fn to_pixels(x: f64, y: f64, scale: f64, px: (u32, u32)) -> (i64, i64) {
    let clamp = |v: f64, max: u32| (v * scale).round().clamp(0.0, (max.max(1) - 1) as f64) as i64;
    (clamp(x, px.0), clamp(y, px.1))
}

/// The class's last name: `android.widget.EditText` → `EditText`.
fn short_class(class: &str) -> &str {
    class.rsplit(['.', '$']).next().unwrap_or(class)
}

/// The role a node plays, in the iPhone's role names.
pub fn role_of(node: &XmlNode) -> &'static str {
    let class = node.attr("class").unwrap_or("");
    let short = short_class(class);
    let id = node.attr("resource-id").unwrap_or("").to_lowercase();
    let clickable = node.flag("clickable") || node.flag("long-clickable");
    let has_text = !node.attr("text").unwrap_or("").trim().is_empty()
        || !node.attr("content-desc").unwrap_or("").trim().is_empty();
    if is_editable(node) {
        if node.flag("password") {
            return "secure_text_field";
        }
        if id.contains("search") || short.contains("Search") {
            return "search_field";
        }
        return "text_field";
    }
    if short.contains("Switch") || short.contains("Toggle") || short.contains("CheckBox") || short.contains("RadioButton")
        || short.contains("CheckedTextView") || (node.flag("checkable") && !short.contains("Layout"))
    {
        return "switch";
    }
    if short.contains("SeekBar") || short.contains("RatingBar") || short == "NumberPicker" {
        return "adjustable";
    }
    if short.ends_with("TabView") || short == "TabView" || short.contains("TabItem") {
        return "tab";
    }
    if short.contains("ImageButton") || short.ends_with("Button") || short == "Button" {
        return "button";
    }
    if short.contains("Image") {
        return if clickable { "button" } else { "image" };
    }
    if clickable {
        return "button";
    }
    // An empty text view is a spacer, not text.
    if has_text { "text" } else { "other" }
}

fn is_editable(node: &XmlNode) -> bool {
    let short = short_class(node.attr("class").unwrap_or(""));
    short.contains("EditText") || short.contains("AutoCompleteTextView") || short == "SearchAutoComplete"
}

fn nonempty(v: Option<&str>) -> Option<String> {
    let v = v?.trim();
    (!v.is_empty()).then(|| v.to_string())
}

fn hash_id(s: &str) -> String {
    // FNV-1a, folded to 32 bits: short and stable while the node keeps its
    // place, class and resource id.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", ((h >> 32) ^ h) as u32)
}

/// Screen size and scale, from `wm size` and `wm density`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    /// Pixels.
    pub px: (u32, u32),
    /// Density / 160: pixels per dp.
    pub scale: f64,
}

impl Geometry {
    pub fn dp(&self) -> (f64, f64) {
        (round1(self.px.0 as f64 / self.scale), round1(self.px.1 as f64 / self.scale))
    }
}

/// `wm size`: the override size when one is set, else the physical one.
pub fn parse_wm_size(text: &str) -> Option<(u32, u32)> {
    let pick = |label: &str| {
        text.lines().find_map(|l| {
            let v = l.trim().strip_prefix(label)?.trim();
            let (w, h) = v.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
    };
    pick("Override size:").or_else(|| pick("Physical size:"))
}

/// `wm density`: the override density when one is set, else the physical one.
pub fn parse_wm_density(text: &str) -> Option<u32> {
    let pick = |label: &str| text.lines().find_map(|l| l.trim().strip_prefix(label)?.trim().parse().ok());
    pick("Override density:").or_else(|| pick("Physical density:"))
}

/// The package uiautomator says the dump belongs to (its first node's).
pub fn dump_package(root: &XmlNode) -> Option<String> {
    fn first(n: &XmlNode) -> Option<String> {
        if let Some(p) = nonempty(n.attr("package")) {
            return Some(p);
        }
        n.children.iter().find_map(first)
    }
    first(root)
}

/// Build the screen. `app` names the front app (pid, name, package); its
/// application element frames the whole screen, as an iPhone's does.
pub fn build_screen(root: &XmlNode, geo: Geometry, app: &AppInfo, opts: BuildOptions) -> Screen {
    let (w, h) = geo.dp();
    let bounds = Rect { x: 0.0, y: 0.0, w, h };
    let mut screen = Screen { width: w, height: h, apps: vec![app.clone()], elements: Vec::new() };
    let mut used: HashSet<String> = HashSet::new();
    let pkg = app.bundle_id.clone().unwrap_or_default();
    let app_id = hash_id(&format!("app|{pkg}"));
    used.insert(app_id.clone());
    screen.elements.push(Element {
        id: app_id.clone(),
        role: "application".into(),
        label: app.name.clone().or_else(|| app.bundle_id.clone()),
        value: None,
        identifier: None,
        hint: None,
        frame: Some(bounds),
        traits: Vec::new(),
        enabled: true,
        selected: false,
        focused: false,
        parent: None,
        depth: 0,
        pid: app.pid,
        on_screen: true,
        raw: None,
        key: None,
    });
    // Depth first, in the dump's own order.
    let mut stack: Vec<(&XmlNode, String, String, u32)> = root
        .children
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, n)| n.name == "node")
        .map(|(i, n)| (n, format!("{i}"), app_id.clone(), 1))
        .collect();
    while let Some((node, path, parent, depth)) = stack.pop() {
        let class = node.attr("class").unwrap_or("");
        let rid = nonempty(node.attr("resource-id"));
        let mut id = hash_id(&format!("{path}|{class}|{}", rid.as_deref().unwrap_or("")));
        while !used.insert(id.clone()) {
            id.push('x');
        }
        for (i, child) in node.children.iter().enumerate().rev().filter(|(_, c)| c.name == "node") {
            stack.push((child, format!("{path}/{i}"), id.clone(), depth + 1));
        }
        let frame = node.attr("bounds").and_then(parse_bounds).map(|b| bounds_to_frame(b, geo.scale));
        let on_screen = frame.is_some_and(|f| f.area() > 0.0 && f.intersection(&bounds).is_some());
        if !on_screen && !opts.include_offscreen {
            continue;
        }
        let role = role_of(node);
        let text = nonempty(node.attr("text"));
        let desc = nonempty(node.attr("content-desc"));
        let editable = is_editable(node);
        let (label, value, hint) = if editable {
            // The field's text is its value (uiautomator may report the
            // placeholder there while the field is empty).
            (desc.clone(), Some(node.attr("text").unwrap_or("").to_string()), None)
        } else if role == "switch" {
            let checked = if node.flag("checked") { "1" } else { "0" };
            (text.clone().or(desc.clone()), Some(checked.to_string()), None)
        } else {
            let hint = match (&text, &desc) {
                (Some(t), Some(d)) if t != d => Some(d.clone()),
                _ => None,
            };
            (text.clone().or(desc.clone()), None, hint)
        };
        let mut traits: Vec<String> = Vec::new();
        for (flag, name) in [
            ("clickable", "clickable"),
            ("long-clickable", "long_clickable"),
            ("checkable", "checkable"),
            ("checked", "checked"),
            ("scrollable", "scrollable"),
            ("focusable", "focusable"),
            ("focused", "focused"),
            ("selected", "selected"),
            ("password", "password"),
        ] {
            if node.flag(flag) {
                traits.push(name.to_string());
            }
        }
        if node.attr("enabled") == Some("false") {
            traits.push("not_enabled".into());
        }
        let raw = opts.raw.then(|| {
            let m: Map<String, Value> = node.attrs.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
            Value::Object(m)
        });
        screen.elements.push(Element {
            id,
            role: role.to_string(),
            label,
            value,
            identifier: rid,
            hint,
            frame,
            traits,
            enabled: node.attr("enabled") != Some("false"),
            selected: node.flag("selected"),
            // `focused` means "has the keyboard", as on the iPhone: only an
            // editable field with input focus.
            focused: editable && node.flag("focused"),
            parent: Some(parent),
            depth,
            pid: app.pid,
            on_screen,
            raw,
            key: None,
        });
    }
    screen
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/android-settings.xml");

    fn geo() -> Geometry {
        Geometry { px: (1080, 2400), scale: 3.0 }
    }

    fn app() -> AppInfo {
        AppInfo { pid: 4242, name: Some("Settings".into()), bundle_id: Some("com.android.settings".into()) }
    }

    fn screen(opts: BuildOptions) -> Screen {
        let root = parse_xml(FIXTURE).unwrap();
        build_screen(&root, geo(), &app(), opts)
    }

    #[test]
    fn the_fixture_parses_into_a_tree() {
        let root = parse_xml(FIXTURE).unwrap();
        assert_eq!(root.name, "hierarchy");
        assert_eq!(root.attr("rotation"), Some("0"));
        assert_eq!(dump_package(&root).as_deref(), Some("com.android.settings"));
    }

    #[test]
    fn entities_and_both_quotes_are_understood() {
        let root = parse_xml(
            "<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\"><node text=\"A &amp; B &lt;3 &quot;x&quot; &#44608;&#x1F600;\" a='1'/></hierarchy>",
        )
        .unwrap();
        assert_eq!(root.children[0].attr("text"), Some("A & B <3 \"x\" 김😀"));
        assert_eq!(root.children[0].attr("a"), Some("1"));
        assert!(parse_xml("<hierarchy><node></hierarchy>").is_err());
        assert!(parse_xml("<hierarchy><node a=1/></hierarchy>").is_err());
        assert!(parse_xml("nothing").is_err());
    }

    #[test]
    fn the_xml_is_cut_out_of_what_dump_prints() {
        let out = "<?xml version='1.0' ?><hierarchy rotation=\"0\"></hierarchy>UI hierchary dumped to: /dev/tty\n";
        assert_eq!(extract_xml(out), Some("<?xml version='1.0' ?><hierarchy rotation=\"0\"></hierarchy>"));
        assert_eq!(extract_xml("ERROR: could not get idle state.\n"), None);
    }

    #[test]
    fn bounds_become_dp_and_taps_go_back_to_pixels() {
        assert_eq!(parse_bounds("[0,84][1080,231]"), Some((0.0, 84.0, 1080.0, 231.0)));
        assert_eq!(parse_bounds("[0,84][1080]"), None);
        let f = bounds_to_frame((42.0, 300.0, 1038.0, 461.0), 3.0);
        assert_eq!((f.x, f.y, f.w, f.h), (14.0, 100.0, 332.0, 53.7));
        // 2.625 (420 dpi): fractions round to one decimal.
        let f = bounds_to_frame((0.0, 0.0, 1080.0, 2400.0), 2.625);
        assert_eq!((f.w, f.h), (411.4, 914.3));
        assert_eq!(to_pixels(180.0, 400.0, 3.0, (1080, 2400)), (540, 1200));
        assert_eq!(to_pixels(360.0, 800.0, 3.0, (1080, 2400)), (1079, 2399));
        assert_eq!(Geometry { px: (1080, 2636), scale: 3.0 }.dp(), (360.0, 878.7));
    }

    #[test]
    fn wm_output_prefers_the_override() {
        assert_eq!(parse_wm_size("Physical size: 1080x2636\n"), Some((1080, 2636)));
        assert_eq!(parse_wm_size("Physical size: 1440x3088\nOverride size: 1080x2316\n"), Some((1080, 2316)));
        assert_eq!(parse_wm_density("Physical density: 480\n"), Some(480));
        assert_eq!(parse_wm_density("Physical density: 560\nOverride density: 420\n"), Some(420));
        assert_eq!(parse_wm_size("garbage"), None);
    }

    #[test]
    fn roles_follow_class_and_flags() {
        let n = |class: &str, extra: &[(&str, &str)]| XmlNode {
            name: "node".into(),
            attrs: std::iter::once(("class".to_string(), class.to_string()))
                .chain(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())))
                .collect(),
            children: vec![],
        };
        assert_eq!(role_of(&n("android.widget.EditText", &[])), "text_field");
        assert_eq!(role_of(&n("android.widget.EditText", &[("password", "true")])), "secure_text_field");
        assert_eq!(role_of(&n("android.widget.EditText", &[("resource-id", "com.android.settings:id/search_src_text")])), "search_field");
        assert_eq!(role_of(&n("android.widget.Switch", &[("checkable", "true")])), "switch");
        assert_eq!(role_of(&n("android.widget.CheckBox", &[])), "switch");
        assert_eq!(role_of(&n("android.widget.Button", &[])), "button");
        assert_eq!(role_of(&n("android.widget.ImageButton", &[])), "button");
        assert_eq!(role_of(&n("android.widget.ImageView", &[])), "image");
        assert_eq!(role_of(&n("android.widget.ImageView", &[("clickable", "true")])), "button");
        assert_eq!(role_of(&n("android.widget.TextView", &[("text", "Wi-Fi")])), "text");
        assert_eq!(role_of(&n("android.widget.LinearLayout", &[("clickable", "true")])), "button");
        assert_eq!(role_of(&n("android.widget.FrameLayout", &[])), "other");
        assert_eq!(role_of(&n("android.widget.SeekBar", &[])), "adjustable");
        assert_eq!(role_of(&n("com.google.android.material.tabs.TabLayout$TabView", &[])), "tab");
    }

    #[test]
    fn elements_carry_labels_values_ids_and_dp_frames() {
        let s = screen(BuildOptions::default());
        assert_eq!((s.width, s.height), (360.0, 800.0));
        assert_eq!(s.apps[0].bundle_id.as_deref(), Some("com.android.settings"));
        let app = &s.elements[0];
        assert_eq!(app.role, "application");
        assert_eq!(app.label.as_deref(), Some("Settings"));
        assert!(s.elements[1..].iter().all(|e| e.depth >= 1 && e.parent.is_some() && e.pid == 4242));

        let wifi = s.elements.iter().find(|e| e.label.as_deref() == Some("Network & internet")).unwrap();
        assert_eq!(wifi.role, "text");
        assert_eq!(wifi.identifier.as_deref(), Some("android:id/title"));
        let f = wifi.frame.unwrap();
        assert_eq!((f.x, f.y), (72.0, 152.0));

        let search = s.elements.iter().find(|e| e.role == "search_field").unwrap();
        assert!(search.focused, "the focused EditText has the keyboard");
        assert_eq!(search.value.as_deref(), Some("wifi"));
        assert_eq!(search.label.as_deref(), Some("Search settings"));

        let toggle = s.elements.iter().find(|e| e.role == "switch").unwrap();
        assert_eq!(toggle.value.as_deref(), Some("1"));
        assert!(toggle.traits.iter().any(|t| t == "checked"));

        let disabled = s.elements.iter().find(|e| e.label.as_deref() == Some("Unavailable")).unwrap();
        assert!(!disabled.enabled);

        let back = s.elements.iter().find(|e| e.label.as_deref() == Some("Navigate up")).unwrap();
        assert_eq!(back.role, "button");

        // Text and a different content-desc: text is the label, the
        // description the hint.
        let both = s.elements.iter().find(|e| e.label.as_deref() == Some("Display")).unwrap();
        assert_eq!(both.hint.as_deref(), Some("Display settings"));

        let ids: HashSet<&str> = s.elements.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), s.elements.len(), "ids are unique");
    }

    #[test]
    fn offscreen_and_empty_nodes_stay_out_unless_asked() {
        let s = screen(BuildOptions::default());
        assert!(s.elements.iter().all(|e| e.on_screen));
        assert!(!s.elements.iter().any(|e| e.label.as_deref() == Some("Below the fold")));
        let all = screen(BuildOptions { include_offscreen: true, raw: true });
        let below = all.elements.iter().find(|e| e.label.as_deref() == Some("Below the fold")).unwrap();
        assert!(!below.on_screen);
        assert_eq!(below.raw.as_ref().unwrap()["class"], "android.widget.TextView");
        assert!(all.elements.len() > s.elements.len());
    }

    #[test]
    fn ids_hold_across_readings_of_the_same_tree() {
        let a = screen(BuildOptions::default());
        let b = screen(BuildOptions::default());
        assert_eq!(a.elements.iter().map(|e| &e.id).collect::<Vec<_>>(), b.elements.iter().map(|e| &e.id).collect::<Vec<_>>());
        assert_eq!(a.signature(), b.signature());
    }
}
