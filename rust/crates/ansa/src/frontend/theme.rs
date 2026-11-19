//! UI preferences and palettes are independent of model configuration and epochs.
use crate::auth::Cancel;
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub type Result<T> = std::result::Result<T, &'static str>;
const FILE_LIMIT: usize = 16 * 1024;
const THEME_LIMIT: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Color(u8, u8, u8);
impl Color {
    fn parse(text: &str) -> Result<Self> {
        if text.len() != 7
            || !text.starts_with('#')
            || !text[1..].bytes().all(|v| v.is_ascii_hexdigit())
        {
            return Err("theme_color");
        }
        let byte = |at| u8::from_str_radix(&text[at..at + 2], 16).map_err(|_| "theme_color");
        Ok(Self(byte(1)?, byte(3)?, byte(5)?))
    }
    fn fg(self) -> String {
        format!("\x1b[38;2;{};{};{}m", self.0, self.1, self.2)
    }
    fn bg(self) -> String {
        format!("\x1b[48;2;{};{};{}m", self.0, self.1, self.2)
    }
    fn blend(self, other: Self, weight: u16) -> Self {
        let channel =
            |a: u8, b: u8| ((u16::from(a) * (100 - weight) + u16::from(b) * weight) / 100) as u8;
        Self(
            channel(self.0, other.0),
            channel(self.1, other.1),
            channel(self.2, other.2),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Palette {
    accent: Color,
    foreground: Color,
    muted: Color,
    background: Color,
    input: Color,
    selection: Color,
    success: Color,
    warning: Color,
    error: Color,
    code: Color,
    rule: Color,
}
impl Default for Palette {
    fn default() -> Self {
        Self {
            accent: Color(133, 220, 177),
            foreground: Color(215, 223, 235),
            muted: Color(145, 157, 177),
            background: Color(18, 23, 27),
            input: Color(29, 35, 39),
            selection: Color(40, 61, 51),
            success: Color(133, 220, 177),
            warning: Color(232, 189, 117),
            error: Color(241, 142, 150),
            code: Color(232, 189, 117),
            rule: Color(62, 74, 73),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    id: String,
    name: String,
    palette: Palette,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            id: "carbon".into(),
            name: "Carbon".into(),
            palette: Palette::default(),
        }
    }
}
impl Theme {
    pub fn builtin(id: &str) -> Option<Self> {
        builtin().into_iter().find(|theme| theme.id() == id)
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Trusted, exact SGR tokens. The renderer applies this mapping once and
    /// simultaneously, including its split-pane metadata.
    pub fn replacements(&self) -> Vec<(String, String)> {
        let p = &self.palette;
        let fg = [
            (Color(133, 220, 177), p.success),
            (Color(215, 223, 235), p.foreground),
            (Color(145, 157, 177), p.muted),
            (Color(125, 183, 245), p.accent.blend(p.foreground, 20)),
            (Color(188, 166, 242), p.accent),
            (Color(232, 189, 117), p.warning),
            (Color(229, 193, 137), p.code),
            (Color(241, 142, 150), p.error),
            (Color(62, 74, 93), p.rule),
            (Color(232, 240, 255), p.foreground),
        ];
        let bg = [
            (Color(29, 35, 39), p.input),
            (Color(46, 63, 88), p.selection),
            (Color(24, 58, 39), p.background.blend(p.success, 18)),
            (Color(68, 31, 36), p.background.blend(p.error, 18)),
        ];
        fg.into_iter()
            .map(|(from, to)| (from.fg(), to.fg()))
            .chain(bg.into_iter().map(|(from, to)| (from.bg(), to.bg())))
            .collect()
    }
    pub fn background_sgr(&self) -> String {
        "\x1b[49m".into()
    }
    pub fn foreground_sgr(&self) -> String {
        self.palette.foreground.fg()
    }
    pub fn is_dynamic(&self) -> bool {
        self.id == "dms"
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Colors {
    accent: Option<String>,
    foreground: Option<String>,
    muted: Option<String>,
    background: Option<String>,
    input: Option<String>,
    selection: Option<String>,
    success: Option<String>,
    warning: Option<String>,
    error: Option<String>,
    code: Option<String>,
    rule: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Custom {
    name: String,
    colors: Colors,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    schema_version: usize,
    theme: String,
}

fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_'))
}
fn selector(text: &str) -> bool {
    identifier(text.strip_prefix("custom:").unwrap_or(text))
}
fn display_name(text: &str) -> bool {
    !text.trim().is_empty()
        && text.len() <= 100
        && text.chars().count() <= 40
        && !text
            .chars()
            .any(|v| v.is_control() || matches!(v,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}'))
}
fn custom(id: &str, bytes: &[u8]) -> Result<Theme> {
    if bytes.len() > FILE_LIMIT || !identifier(id) {
        return Err("theme_size_limit");
    }
    json_guard(bytes)?;
    let value: Custom = serde_json::from_slice(bytes).map_err(|_| "theme_json")?;
    if !display_name(&value.name) {
        return Err("theme_name");
    }
    let mut p = Palette::default();
    for (value, target) in [
        (value.colors.accent, &mut p.accent),
        (value.colors.foreground, &mut p.foreground),
        (value.colors.muted, &mut p.muted),
        (value.colors.background, &mut p.background),
        (value.colors.input, &mut p.input),
        (value.colors.selection, &mut p.selection),
        (value.colors.success, &mut p.success),
        (value.colors.warning, &mut p.warning),
        (value.colors.error, &mut p.error),
        (value.colors.code, &mut p.code),
        (value.colors.rule, &mut p.rule),
    ] {
        if let Some(value) = value {
            *target = Color::parse(&value)?;
        }
    }
    Ok(Theme {
        id: format!("custom:{id}"),
        name: value.name,
        palette: p,
    })
}
fn builtin() -> Vec<Theme> {
    let mut themes = vec![Theme::default()];
    for (id, name, accent) in [
        ("pink", "Pink", Color(232, 155, 192)),
        ("red", "Red", Color(235, 137, 137)),
        ("blue", "Blue", Color(142, 188, 238)),
        ("amber", "Amber", Color(231, 189, 117)),
    ] {
        let mut palette = Palette {
            accent,
            ..Palette::default()
        };
        palette.input = palette.background.blend(accent, 9);
        palette.selection = palette.background.blend(accent, 23);
        themes.push(Theme {
            id: id.into(),
            name: name.into(),
            palette,
        });
    }
    themes
}

#[derive(Clone, Debug)]
pub struct Catalog {
    pub themes: Vec<Theme>,
    pub selected: String,
    pub warnings: Vec<String>,
}
impl Catalog {
    pub fn find(&self, id: &str) -> Option<&Theme> {
        self.themes.iter().find(|theme| theme.id == id).or_else(|| {
            self.themes
                .iter()
                .find(|theme| !theme.id.starts_with("custom:") && theme.id.eq_ignore_ascii_case(id))
        })
    }
    pub fn selected(&self) -> Theme {
        self.find(&self.selected).cloned().unwrap_or_default()
    }
}

/// A picker previews immutable values; cancellation restores the exact original
/// palette, even when a custom theme file changed during the preview.
pub struct Preview {
    themes: Vec<Theme>,
    original: Theme,
    selected: usize,
}
impl Preview {
    pub fn new(themes: Vec<Theme>, original: Theme) -> Self {
        let selected = themes
            .iter()
            .position(|theme| theme.id == original.id)
            .unwrap_or(0);
        Self {
            themes,
            original,
            selected,
        }
    }
    pub fn themes(&self) -> &[Theme] {
        &self.themes
    }
    pub fn selected(&self) -> usize {
        self.selected
    }
    pub fn current(&self) -> Theme {
        self.themes
            .get(self.selected)
            .cloned()
            .unwrap_or_else(|| self.original.clone())
    }
    pub fn original(&self) -> Theme {
        self.original.clone()
    }
    pub fn replace_original(&mut self, value: Theme) {
        self.original = value;
    }
    pub fn navigate(&mut self, older: bool) {
        self.selected = if older {
            self.selected.saturating_sub(1)
        } else {
            self.selected
                .saturating_add(1)
                .min(self.themes.len().saturating_sub(1))
        };
    }
}

#[derive(Clone)]
pub struct Store {
    directory: PathBuf,
    caches: Vec<PathBuf>,
}
impl Store {
    pub fn new(home: &Path, directory: &Path) -> Self {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".cache"));
        Self::with_cache(directory, &cache)
    }
    pub fn with_cache(directory: &Path, cache: &Path) -> Self {
        Self {
            directory: directory.into(),
            caches: ["dms", "DankMaterialShell"]
                .map(|name| cache.join(name).join("dms-colors.json"))
                .to_vec(),
        }
    }
    pub fn load(&self, stop: &Cancel) -> Result<Catalog> {
        let mut result = Catalog {
            themes: builtin(),
            selected: "carbon".into(),
            warnings: Vec::new(),
        };
        if stop.cancelled() {
            return Err("theme_cancelled");
        }
        match read(&self.directory.join("ui.json"), stop) {
            Ok(Some(bytes)) => match serde_json::from_slice::<Preferences>(&bytes) {
                Ok(prefs) if prefs.schema_version == 1 && selector(&prefs.theme) => {
                    result.selected = prefs.theme
                }
                _ => result
                    .warnings
                    .push("Invalid ui.json. Choose a theme again".into()),
            },
            Ok(None) => {}
            Err("theme_cancelled") => return Err("theme_cancelled"),
            Err(_) => result.warnings.push("Could not read ui.json".into()),
        }
        if let Ok(theme) = self.dynamic(stop) {
            result.themes.push(theme);
        }
        let directory = self.directory.join("themes");
        match ansa_linux::fs::open_nofollow(
            &directory,
            ansa_linux::local::read_flags() | ansa_linux::local::ReadFlags::DIRECTORY,
        ) {
            Ok(guard) => {
                let entries =
                    ansa_linux::local::names(&guard, 128).map_err(|_| "theme_directory")?;
                let mut names = Vec::new();
                if entries.1 {
                    result
                        .warnings
                        .push("Theme directory exceeds the 128-file limit".into());
                }
                for entry in entries.0 {
                    if stop.cancelled() {
                        return Err("theme_cancelled");
                    }
                    let Some(name) = entry.to_str().map(str::to_owned) else {
                        continue;
                    };
                    let Some(stem) = name.strip_suffix(".json") else {
                        continue;
                    };
                    if identifier(stem) {
                        names.push(name);
                    }
                }
                names.sort();
                if names.len() > THEME_LIMIT {
                    result
                        .warnings
                        .push("Showing the first 32 custom themes".into());
                    names.truncate(THEME_LIMIT);
                }
                for name in names {
                    if stop.cancelled() {
                        return Err("theme_cancelled");
                    }
                    match ansa_linux::local::read_regular(&guard, &name, FILE_LIMIT)
                        .map_err(|_| "theme_file")
                        .and_then(|bytes| {
                            custom(name.trim_end_matches(".json"), &bytes.ok_or("theme_file")?)
                        }) {
                        Ok(theme) => result.themes.push(theme),
                        Err("theme_cancelled") => return Err("theme_cancelled"),
                        Err(_) => result.warnings.push(format!("Could not load theme {name}")),
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => result
                .warnings
                .push("Custom theme directory unavailable".into()),
        }
        if result.find(&result.selected).is_none() {
            result
                .warnings
                .push("Saved theme unavailable. Using Carbon".into());
            result.selected = "carbon".into();
        }
        if stop.cancelled() {
            return Err("theme_cancelled");
        }
        Ok(result)
    }
    pub fn save(&self, id: &str) -> Result<()> {
        if !selector(id) {
            return Err("theme_selector");
        }
        ansa_linux::fs::create_private(&self.directory)
            .map_err(|_| "theme_preferences_directory")?;
        let directory = ansa_linux::fs::private_directory(&self.directory)
            .map_err(|_| "theme_preferences_directory")?;
        let data = serde_json::json!({"schema_version":1,"theme":id}).to_string();
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "theme_preferences_clock")?
            .as_nanos();
        let suffix = format!(
            "{}-{}-{now}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        ansa_linux::local::replace_ui(&directory, data.as_bytes(), &suffix)
            .map_err(|_| "theme_preferences_write")
    }
    pub fn dynamic(&self, stop: &Cancel) -> Result<Theme> {
        let mut failure = "theme_dms_unavailable";
        for path in &self.caches {
            match read(path, stop) {
                Ok(Some(bytes)) => match dms(&bytes) {
                    Ok(theme) => return Ok(theme),
                    Err(error) => failure = error,
                },
                Ok(None) => {}
                Err("theme_cancelled") => return Err("theme_cancelled"),
                Err(error) => failure = error,
            }
        }
        Err(failure)
    }
}

fn read(path: &Path, stop: &Cancel) -> Result<Option<Vec<u8>>> {
    if stop.cancelled() {
        return Err("theme_cancelled");
    }
    let mut file = match ansa_linux::fs::open_nofollow(path, ansa_linux::local::read_flags()) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("theme_file"),
    };
    if !ansa_linux::fs::regular_single(&file).map_err(|_| "theme_file")? {
        return Err("theme_file");
    }
    let length = file.metadata().map_err(|_| "theme_file")?.len();
    if length == 0 || length > FILE_LIMIT as u64 {
        return Err("theme_size_limit");
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(FILE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "theme_file")?;
    if bytes.len() as u64 != length || bytes.len() > FILE_LIMIT {
        return Err("theme_size_limit");
    }
    if stop.cancelled() {
        return Err("theme_cancelled");
    }
    Ok(Some(bytes))
}
fn dms(bytes: &[u8]) -> Result<Theme> {
    if bytes.len() > FILE_LIMIT {
        return Err("theme_size_limit");
    }
    json_guard(bytes)?;
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| "theme_dms_json")?;
    let mode = value
        .get("mode")
        .and_then(|v| v.as_str())
        .filter(|v| matches!(*v, "dark" | "light"))
        .ok_or("theme_dms_mode")?;
    let colors = value
        .get("colors")
        .and_then(|v| v.get(mode))
        .ok_or("theme_dms_colors")?;
    let color = |name| {
        colors
            .get(name)
            .and_then(|v| v.as_str())
            .ok_or("theme_dms_color")
            .and_then(Color::parse)
    };
    let base = Palette::default();
    let accent = color("primary")?;
    let background = color("surface")?;
    let dank = |name| {
        value
            .get("dank16")
            .and_then(|v| v.get(name))
            .and_then(|v| v.get(mode).or_else(|| v.get("default")))
            .and_then(|v| v.as_str())
            .map(Color::parse)
            .transpose()
    };
    let warning = dank("color3")?.unwrap_or(base.warning);
    let p = Palette {
        accent,
        background,
        foreground: color("on_surface")?,
        muted: color("on_surface_variant")?,
        rule: color("outline_variant")?,
        error: color("error")?,
        input: color("surface_container_high")?,
        selection: colors
            .get("secondary_container")
            .and_then(|v| v.as_str())
            .map(Color::parse)
            .transpose()?
            .unwrap_or_else(|| background.blend(accent, 25)),
        success: dank("color2")?.unwrap_or(base.success),
        warning,
        code: warning,
    };
    Ok(Theme {
        id: "dms".into(),
        name: "Auto DMS".into(),
        palette: p,
    })
}
fn json_guard(bytes: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(bytes).map_err(|_| "theme_json")?;
    crate::protocol::json::Json::parse_with_limits(text, FILE_LIMIT, 16, 2048)
        .map(|_| ())
        .map_err(|_| "theme_json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ansa-theme-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            ansa_linux::fs::create_private(&path).unwrap();
            ansa_linux::fs::create_private(&path.join(".ansa")).unwrap();
            Self(path)
        }
        fn store(&self) -> Store {
            Store::with_cache(&self.0.join(".ansa"), &self.0.join(".cache"))
        }
        fn write(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn dms_fixture(mode: &str, accent: &str) -> String {
        let colors = serde_json::json!({
            "primary":accent, "on_surface":"#EEEEEE", "surface":"#101010",
            "on_surface_variant":"#999999", "outline_variant":"#333333",
            "error":"#FF8888", "surface_container_high":"#222222", "secondary_container":"#223344",
        });
        serde_json::json!({"mode":mode,"colors":{"dark":colors.clone(),"light":colors},
            "dank16":{"color2":{"dark":"#44DD77","light":"#007722","default":"#11BB33"},
                      "color3":{"dark":"#EECC44","light":"#775500","default":"#DDAA33"}}})
        .to_string()
    }
    #[test]
    fn custom_palette_validates_schema_colors_and_terminal_control_names() {
        let theme = custom("rose",br##"{"name":"Rose","colors":{"accent":"#dd99bb","warning":"#abcdef","code":"#123456"}}"##).unwrap();
        assert_eq!(theme.id(), "custom:rose");
        assert_eq!(theme.palette.accent, Color(221, 153, 187));
        assert_eq!(theme.palette.warning, Color(171, 205, 239));
        assert_eq!(theme.palette.code, Color(18, 52, 86));
        let replacements = theme.replacements();
        assert!(replacements.contains(&(Color(232, 189, 117).fg(), Color(171, 205, 239).fg())));
        assert!(replacements.contains(&(Color(229, 193, 137).fg(), Color(18, 52, 86).fg())));
        for text in [
            r##"{"name":"Bad","colors":{"accent":"#bad"}}"##,
            r##"{"name":"Bad","colors":{"accent":"red"}}"##,
            r##"{"name":"Bad","colors":{"unknown":"#aabbcc"}}"##,
            r##"{"name":"Bad","colors":{"accent":"#abcdef","accent":"#123456"}}"##,
            r##"{"name":"\u001b[31mBad","colors":{}}"##,
            r##"{"name":"\u202eBad","colors":{}}"##,
        ] {
            assert!(custom("invalid", text.as_bytes()).is_err(), "{text}");
        }
        assert!(custom("../outside", br#"{"name":"Bad","colors":{}}"#).is_err());
        assert!(custom("large", &vec![b' '; FILE_LIMIT + 1]).is_err());
    }
    #[test]
    fn saved_ui_choice_roundtrips_without_changing_runtime_configuration() {
        let root = Root::new();
        let store = root.store();
        let stop = Cancel::default();
        let before = crate::config::load_agent_settings(&root.0, &root.0)
            .unwrap()
            .effective()
            .unwrap();
        store.save("pink").unwrap();
        let catalog = store.load(&stop).unwrap();
        assert_eq!(catalog.selected().id(), "pink");
        assert!(catalog.warnings.is_empty());
        let after = crate::config::load_agent_settings(&root.0, &root.0)
            .unwrap()
            .effective()
            .unwrap();
        assert_eq!(before, after);
        assert!(store.save("custom:../outside").is_err());
        assert_eq!(store.load(&stop).unwrap().selected().id(), "pink");
    }
    #[test]
    fn preview_cancel_restores_original_palette_without_writing_preferences() {
        let root = Root::new();
        let store = root.store();
        let original = Theme {
            palette: Palette {
                accent: Color(1, 2, 3),
                ..Palette::default()
            },
            ..Theme::default()
        };
        let mut preview = Preview::new(builtin(), original.clone());
        assert_eq!(preview.current().id(), "carbon");
        preview.navigate(false);
        assert_eq!(preview.current().id(), "pink");
        assert_eq!(preview.original(), original);
        assert!(!store.directory.join("ui.json").exists());
        preview.navigate(true);
        assert_eq!(preview.current().id(), "carbon");
        assert_eq!(preview.original().palette.accent, Color(1, 2, 3));
        let committed = preview.current();
        store.save(committed.id()).unwrap();
        assert_eq!(
            store.load(&Cancel::default()).unwrap().selected().id(),
            "carbon"
        );
    }
    #[test]
    fn palette_files_reject_links_oversize_and_cancelled_reads() {
        let root = Root::new();
        let store = root.store();
        let stop = Cancel::default();
        root.write(
            ".ansa/themes/good.json",
            r##"{"name":"Good","colors":{"accent":"#ABCDEF"}}"##,
        );
        std::fs::hard_link(
            root.0.join(".ansa/themes/good.json"),
            root.0.join(".ansa/themes/alias.json"),
        )
        .unwrap();
        symlink(
            root.0.join(".ansa/themes/good.json"),
            root.0.join(".ansa/themes/link.json"),
        )
        .unwrap();
        root.write(".ansa/themes/large.json", &" ".repeat(FILE_LIMIT + 1));
        let catalog = store.load(&stop).unwrap();
        assert!(
            catalog
                .themes
                .iter()
                .all(|v| !v.id().starts_with("custom:"))
        );
        assert_eq!(catalog.warnings.len(), 4);
        let cancelled = Cancel::default();
        cancelled.cancel();
        assert_eq!(store.load(&cancelled).unwrap_err(), "theme_cancelled");
        std::fs::remove_file(root.0.join(".ansa/themes/alias.json")).unwrap();
        let catalog = store.load(&stop).unwrap();
        assert!(catalog.find("custom:good").is_some());
        assert!(catalog.find("custom:link").is_none());
    }
    #[test]
    fn dms_roots_modes_and_updated_palettes_preserve_status_colors() {
        let root = Root::new();
        let store = root.store();
        let stop = Cancel::default();
        root.write(
            ".cache/DankMaterialShell/dms-colors.json",
            &dms_fixture("dark", "#CCAAFF"),
        );
        let old = store.dynamic(&stop).unwrap();
        assert_eq!(old.palette.accent, Color(204, 170, 255));
        assert_eq!(old.palette.success, Color(68, 221, 119));
        assert_eq!(old.palette.warning, Color(238, 204, 68));
        root.write(
            ".cache/dms/dms-colors.json",
            &dms_fixture("light", "#EEAABB"),
        );
        let new = store.dynamic(&stop).unwrap();
        assert!(new.is_dynamic());
        assert_ne!(old, new);
        assert_eq!(new.palette.success, Color(0, 119, 34));
        assert_eq!(new.palette.warning, Color(119, 85, 0));
        root.write(
            ".cache/dms/dms-colors.json",
            &dms_fixture("light", "#AABBCC"),
        );
        assert_eq!(
            store.dynamic(&stop).unwrap().palette.accent,
            Color(170, 187, 204)
        );
        assert!(dms(dms_fixture("unknown", "#123456").as_bytes()).is_err());
    }
    #[test]
    fn invalid_saved_selector_and_custom_json_leave_builtin_themes_available() {
        let root = Root::new();
        let store = root.store();
        let stop = Cancel::default();
        root.write(
            ".ansa/ui.json",
            r#"{"schema_version":1,"theme":"custom:missing"}"#,
        );
        root.write(
            ".ansa/themes/bad.json",
            r#"{"name":"Broken","colors":{"accent":false}}"#,
        );
        let catalog = store.load(&stop).unwrap();
        assert_eq!(catalog.selected().id(), "carbon");
        assert!(catalog.find("red").is_some() && catalog.find("blue").is_some());
        assert_eq!(catalog.warnings.len(), 2);
        store.save("amber").unwrap();
        assert_eq!(store.load(&stop).unwrap().selected().id(), "amber");
    }
    #[test]
    fn custom_selection_overlay_preserves_the_applied_palette() {
        use crate::frontend::carbon::{
            self, Entry, Inspector, InspectorKind, Selection, Workspace,
        };
        let root = Root::new();
        root.write(".ansa/themes/custom.json",r##"{"name":"Custom","colors":{"selection":"#123456","foreground":"#abcdef","accent":"#dc8cd0"}}"##);
        let theme = root
            .store()
            .load(&Cancel::default())
            .unwrap()
            .find("custom:custom")
            .unwrap()
            .clone();
        let mut frame = carbon::layout_workspace(
            &[Entry {
                role: "assistant".into(),
                text: "SELECT this text".into(),
                action: None,
                edit: None,
                output: None,
                status: None,
            }],
            b"draft",
            3,
            "ready",
            0,
            "project",
            (50, 20),
            true,
            &Workspace::default(),
        )
        .unwrap();
        frame.apply_theme(&theme);
        let before = frame.lines.clone();
        frame.apply_theme(&Theme::default());
        assert_eq!(
            frame.lines, before,
            "an already mapped frame was mapped twice"
        );
        let row = frame
            .plain_lines()
            .iter()
            .position(|line| line.starts_with("SELECT"))
            .unwrap();
        let mut selection = Selection::new(frame, 0, row);
        selection.drag(5, row);
        let selected = selection.frame_at((50, 20), true);
        assert!(selected.lines[row].contains("\x1b[48;2;18;52;86m"));
        assert!(selected.lines[row].contains("\x1b[38;2;171;205;239m"));
        for (index, line) in before.iter().enumerate() {
            if index != row {
                assert_eq!(&selected.lines[index], line);
            }
        }
        assert_eq!(selection.finish(5, row).as_deref(), Some("SELECT"));
        for panel in [
            Inspector {
                title: "Source".into(),
                kind: InspectorKind::Source,
                lines: vec!["PLAIN text".into()],
                ..Inspector::default()
            },
            Inspector {
                title: "Preview".into(),
                kind: InspectorKind::Markdown,
                markdown: Some(std::sync::Arc::new(
                    crate::frontend::markdown::Document::parse("`code` PLAIN text").rows(49),
                )),
                ..Inspector::default()
            },
        ] {
            let mut frame = carbon::layout_workspace(
                &[],
                b"",
                0,
                "ready",
                0,
                "project",
                (50, 20),
                true,
                &Workspace {
                    inspector: Some(panel),
                    ..Workspace::default()
                },
            )
            .unwrap();
            frame.apply_theme(&theme);
            let line = frame
                .lines
                .iter()
                .find(|line| line.contains("PLAIN"))
                .unwrap();
            let before = &line[..line.find("PLAIN").unwrap()];
            assert!(
                before.rfind("\x1b[38;2;171;205;239m").unwrap()
                    > before.rfind("\x1b[0m").unwrap_or(0),
                "plain text did not restore theme foreground"
            );
        }
    }
}
