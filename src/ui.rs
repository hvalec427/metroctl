//! Driving the app's UI for agents: the element tree (with React Native
//! testIDs), taps, swipes, typing. iOS (simulators; devices later) goes
//! through WebDriverAgent, the XCTest runner Maestro and Appium use
//! underneath; Android through adb.
//!
//! Coordinates are what the backend taps in: points on iOS, pixels on Android.
//! `elements()` reports them in the same space, so a tree read is tappable.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

/// WebDriverAgent release metroctl builds (override with METROCTL_WDA_TAG).
/// Newer ones embed `lib_TestingInterop.dylib`, which Xcode 26.2 lacks.
pub const WDA_TAG: &str = "v16.12.10";

#[derive(Debug, Clone)]
pub struct Element {
    pub kind: String,
    pub id: Option<String>, // testID (iOS accessibilityIdentifier, Android resource-id)
    pub label: Option<String>,
    pub value: Option<String>,
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

impl Element {
    pub fn center(&self) -> (i64, i64) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    /// `Button login_guest_button "Continue as guest" @201,761`
    pub fn line(&self) -> String {
        let mut s = self.kind.clone();
        if let Some(id) = &self.id {
            s.push_str(&format!(" #{id}"));
        }
        if let Some(l) = self.label.as_ref().filter(|l| Some(*l) != self.id.as_ref()) {
            s.push_str(&format!(" \"{l}\""));
        }
        if let Some(v) = self.value.as_ref().filter(|v| !v.is_empty()) {
            s.push_str(&format!(" value=\"{v}\""));
        }
        let (cx, cy) = self.center();
        s.push_str(&format!(" @{cx},{cy}"));
        s
    }
}

pub enum Backend {
    Wda { port: u16 },
    Adb { serial: String },
}

/// The shell command for the dashboard's WebDriverAgent tab: clone and build
/// it once per Xcode version (serialized across sessions with a lock dir),
/// then run it on `udid`, serving on `port`.
pub fn wda_command(udid: &str, port: u16) -> String {
    let tag = std::env::var("METROCTL_WDA_TAG").unwrap_or_else(|_| WDA_TAG.into());
    format!(
        r#"set -e
D="$HOME/.cache/metroctl"; mkdir -p "$D"
L="$D/wda.lock"
# A lock older than 20 minutes is left over from a killed build.
find "$L" -maxdepth 0 -mmin +20 -exec rmdir {{}} \; 2>/dev/null || true
while ! mkdir "$L" 2>/dev/null; do echo "waiting for another WebDriverAgent build…"; sleep 3; done
trap 'rmdir "$L" 2>/dev/null' EXIT
[ -d "$D/WebDriverAgent/.git" ] || git clone -q https://github.com/appium/WebDriverAgent "$D/WebDriverAgent"
cd "$D/WebDriverAgent"
git rev-parse -q --verify "refs/tags/{tag}" >/dev/null || git fetch -q --tags
git checkout -q "{tag}"
B="$D/wda-build-{tag}-$(xcodebuild -version | head -1 | tr -d ' ')"
if ! ls "$B"/Build/Products/*.xctestrun >/dev/null 2>&1; then
  echo "building WebDriverAgent {tag} (once per Xcode version)…"
  xcodebuild build-for-testing -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner \
    -destination 'generic/platform=iOS Simulator' -derivedDataPath "$B" CODE_SIGNING_ALLOWED=NO -quiet
fi
rmdir "$L"; trap - EXIT
echo "starting WebDriverAgent on :{port}…"
TEST_RUNNER_USE_PORT={port} exec xcodebuild test-without-building -xctestrun "$(ls "$B"/Build/Products/*.xctestrun | head -1)" -destination id={udid}"#
    )
}

pub fn wda_alive(port: u16) -> bool {
    http().get(format!("http://127.0.0.1:{port}/status")).timeout(Duration::from_secs(2)).send().is_ok_and(|r| r.status().is_success())
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder().timeout(Duration::from_secs(60)).build().expect("http client")
}

impl Backend {
    fn wda(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let Backend::Wda { port } = self else { unreachable!() };
        let url = format!("http://127.0.0.1:{port}{path}");
        let req = match method {
            "GET" => http().get(&url),
            _ => http().post(&url).json(&body.unwrap_or(json!({}))),
        };
        let v: Value = req.send().context("WebDriverAgent isn't answering")?.json()?;
        if let Some(e) = v["value"]["error"].as_str() {
            bail!("WebDriverAgent: {e}: {}", v["value"]["message"].as_str().unwrap_or(""));
        }
        Ok(v)
    }

    /// WDA actions need a session; one is cheap, so open one per call.
    fn wda_session(&self) -> Result<String> {
        let v = self.wda("POST", "/session", Some(json!({ "capabilities": { "alwaysMatch": {} } })))?;
        v["sessionId"].as_str().or(v["value"]["sessionId"].as_str()).map(String::from).ok_or_else(|| anyhow!("WebDriverAgent gave no session"))
    }

    fn adb(&self, args: &[&str]) -> Result<String> {
        let Backend::Adb { serial } = self else { unreachable!() };
        let out = Command::new("adb").arg("-s").arg(serial).args(args).output().context("running adb")?;
        if !out.status.success() {
            bail!("adb {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub fn elements(&self) -> Result<Vec<Element>> {
        match self {
            Backend::Wda { .. } => {
                let src = self.wda("GET", "/source?format=json", None)?;
                let mut out = Vec::new();
                wda_walk(&src["value"], &mut out);
                Ok(out)
            }
            Backend::Adb { .. } => {
                // uiautomator can't write to stdout reliably; dump to a file and read it back.
                self.adb(&["shell", "uiautomator", "dump", "/sdcard/metroctl-ui.xml"])?;
                let xml = self.adb(&["exec-out", "cat", "/sdcard/metroctl-ui.xml"])?;
                Ok(parse_uiautomator(&xml))
            }
        }
    }

    pub fn tap(&self, x: i64, y: i64) -> Result<()> {
        match self {
            Backend::Wda { .. } => {
                let sid = self.wda_session()?;
                self.wda("POST", &format!("/session/{sid}/wda/tap"), Some(json!({ "x": x, "y": y }))).map(|_| ())
            }
            Backend::Adb { .. } => self.adb(&["shell", "input", "tap", &x.to_string(), &y.to_string()]).map(|_| ()),
        }
    }

    pub fn swipe(&self, from: (i64, i64), to: (i64, i64), ms: u64) -> Result<()> {
        match self {
            Backend::Wda { .. } => {
                let sid = self.wda_session()?;
                let body = json!({ "fromX": from.0, "fromY": from.1, "toX": to.0, "toY": to.1, "duration": ms as f64 / 1000.0 });
                self.wda("POST", &format!("/session/{sid}/wda/dragfromtoforduration"), Some(body)).map(|_| ())
            }
            Backend::Adb { .. } => {
                let a: Vec<String> = [from.0, from.1, to.0, to.1].iter().map(|n| n.to_string()).collect();
                self.adb(&["shell", "input", "swipe", &a[0], &a[1], &a[2], &a[3], &ms.to_string()]).map(|_| ())
            }
        }
    }

    /// Type into the focused field.
    pub fn type_text(&self, text: &str) -> Result<()> {
        match self {
            Backend::Wda { .. } => {
                let sid = self.wda_session()?;
                let chars: Vec<String> = text.chars().map(String::from).collect();
                self.wda("POST", &format!("/session/{sid}/wda/keys"), Some(json!({ "value": chars }))).map(|_| ())
            }
            Backend::Adb { .. } => {
                // `input text` takes %s for spaces and needs shell metacharacters escaped.
                let escaped: String = text.chars().map(|c| match c {
                    ' ' => "%s".to_string(),
                    c if "()<>|;&*\\~\"'`$".contains(c) => format!("\\{c}"),
                    c => c.to_string(),
                }).collect();
                self.adb(&["shell", "input", "text", &escaped]).map(|_| ())
            }
        }
    }

    /// `home`, `back` (Android), `enter`.
    pub fn press(&self, button: &str) -> Result<()> {
        match (self, button) {
            (Backend::Wda { .. }, "home") => self.wda("POST", "/wda/homescreen", None).map(|_| ()),
            (Backend::Wda { .. }, "enter") => self.type_text("\n"),
            (Backend::Wda { .. }, "back") => bail!("iOS has no back button: tap the screen's back control (often #back) or swipe from the left edge"),
            (Backend::Adb { .. }, b) => {
                let code = match b {
                    "home" => "3",
                    "back" => "4",
                    "enter" => "66",
                    _ => bail!("unknown button {b} (home, back, enter)"),
                };
                self.adb(&["shell", "input", "keyevent", code]).map(|_| ())
            }
            (_, b) => bail!("unknown button {b} (home, back, enter)"),
        }
    }

    /// Screen size in tap coordinates.
    pub fn size(&self) -> Result<(i64, i64)> {
        match self {
            Backend::Wda { .. } => {
                let sid = self.wda_session()?;
                let v = self.wda("GET", &format!("/session/{sid}/window/size"), None)?;
                Ok((v["value"]["width"].as_i64().unwrap_or(390), v["value"]["height"].as_i64().unwrap_or(844)))
            }
            Backend::Adb { .. } => {
                // "Physical size: 1080x2400" (an "Override size" line wins if present).
                let out = self.adb(&["shell", "wm", "size"])?;
                let dims = out.lines().filter_map(|l| l.rsplit(' ').next()).filter_map(|d| d.split_once('x')).filter_map(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?))).last();
                dims.ok_or_else(|| anyhow!("couldn't read the screen size"))
            }
        }
    }
}

/// React Native's placeholder when a view has no accessibility label.
fn meaningful(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty() && *s != "MISSING LABEL").map(String::from)
}

fn wda_walk(n: &Value, out: &mut Vec<Element>) {
    let id = meaningful(n["rawIdentifier"].as_str());
    let label = meaningful(n["label"].as_str());
    let value = meaningful(n["value"].as_str());
    let visible = n["isVisible"].as_str().map_or(n["isVisible"].as_bool().unwrap_or(true), |v| v == "1");
    let r = &n["rect"];
    let (w, h) = (r["width"].as_i64().unwrap_or(0), r["height"].as_i64().unwrap_or(0));
    let kind = n["type"].as_str().unwrap_or("").trim_start_matches("XCUIElementType").to_string();
    if visible && w > 0 && h > 0 && kind != "Application" && (id.is_some() || label.is_some() || value.is_some()) {
        out.push(Element { kind, id, label, value, x: r["x"].as_i64().unwrap_or(0), y: r["y"].as_i64().unwrap_or(0), w, h });
    }
    for c in n["children"].as_array().into_iter().flatten() {
        wda_walk(c, out);
    }
}

fn attr<'a>(node: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = node.find(&key)? + key.len();
    let end = node[start..].find('"')?;
    Some(&node[start..start + end])
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&#10;", " ").replace("&amp;", "&")
}

/// Elements from `uiautomator dump` XML: those with a resource-id, text or
/// content-desc. Bounds are `[x1,y1][x2,y2]` in pixels.
pub fn parse_uiautomator(xml: &str) -> Vec<Element> {
    let mut out = Vec::new();
    for node in xml.split("<node ").skip(1) {
        let node = node.split('>').next().unwrap_or("");
        let get = |k: &str| attr(node, k).map(unescape).filter(|s| !s.is_empty());
        // React Native puts testID in resource-id (sometimes prefixed with the package).
        let id = get("resource-id").map(|r| r.rsplit_once(":id/").map(|(_, i)| i.to_string()).unwrap_or(r));
        let label = get("content-desc").or_else(|| get("text"));
        let Some(b) = attr(node, "bounds") else { continue };
        let nums: Vec<i64> = b.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        if nums.len() != 4 || (id.is_none() && label.is_none()) {
            continue;
        }
        let kind = get("class").map(|c| c.rsplit('.').next().unwrap_or("").to_string()).unwrap_or_default();
        out.push(Element { kind, id, label, value: None, x: nums[0], y: nums[1], w: nums[2] - nums[0], h: nums[3] - nums[1] });
    }
    out
}

/// The element a tap/type targets: exact testID, else exact label, else a
/// label containing `text` (case-insensitive).
pub fn find<'a>(els: &'a [Element], id: Option<&str>, label: Option<&str>) -> Result<&'a Element> {
    if let Some(id) = id {
        return els.iter().find(|e| e.id.as_deref() == Some(id)).ok_or_else(|| anyhow!("no element with testID {id:?} on screen (see `ui`)"));
    }
    if let Some(l) = label {
        let ll = l.to_lowercase();
        return els
            .iter()
            .find(|e| e.label.as_deref() == Some(l))
            .or_else(|| els.iter().find(|e| e.label.as_ref().is_some_and(|x| x.to_lowercase().contains(&ll))))
            .ok_or_else(|| anyhow!("no element labelled {l:?} on screen (see `ui`)"));
    }
    bail!("pass id, label or x/y")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uiautomator_nodes() {
        let xml = r#"<hierarchy><node index="0" text="" resource-id="" class="android.widget.FrameLayout" content-desc="" bounds="[0,0][1080,2400]"><node index="1" text="Continue as guest" resource-id="com.app:id/login_guest_button" class="android.view.ViewGroup" content-desc="" bounds="[48,2100][1032,2265]" /><node text="" resource-id="price_row" class="android.view.View" content-desc="Prices &amp; fees" bounds="[0,10][100,30]"/></node></hierarchy>"#;
        let els = parse_uiautomator(xml);
        assert_eq!(els.len(), 2);
        assert_eq!(els[0].id.as_deref(), Some("login_guest_button"));
        assert_eq!(els[0].center(), (540, 2182));
        assert_eq!(els[1].label.as_deref(), Some("Prices & fees"));
        assert_eq!(find(&els, None, Some("guest")).unwrap().id.as_deref(), Some("login_guest_button"));
    }

    #[test]
    fn walks_wda_source() {
        let src = json!({ "type": "XCUIElementTypeApplication", "label": "App", "isVisible": "1", "rect": {"x":0,"y":0,"width":402,"height":874}, "children": [
            { "type": "XCUIElementTypeOther", "rawIdentifier": "login_guest_button", "label": "login_guest_button", "isVisible": "1", "rect": {"x":16,"y":734,"width":370,"height":55} },
            { "type": "XCUIElementTypeOther", "rawIdentifier": "MISSING LABEL", "label": "4.7 rating", "isVisible": "1", "rect": {"x":25,"y":382,"width":352,"height":29} },
            { "type": "XCUIElementTypeOther", "label": "", "isVisible": "1", "rect": {"x":0,"y":0,"width":10,"height":10} },
        ]});
        let mut els = Vec::new();
        wda_walk(&src, &mut els);
        assert_eq!(els.len(), 2);
        assert_eq!(els[0].line(), "Other #login_guest_button @201,761");
        assert_eq!(els[1].line(), "Other \"4.7 rating\" @201,396");
    }
}
