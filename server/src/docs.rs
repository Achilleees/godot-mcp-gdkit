//! Godot API reference — offline, version-exact, cached.
//!
//! The reference is generated from the *installed* engine rather than fetched from the web, so it
//! always matches the binary the project actually runs and works with no network. The source is
//! `godot --headless --dump-extension-api-with-docs`, which emits one JSON document carrying the
//! full API **and** its prose. (`--doctool` is the other offline route and is deliberately not
//! used, for two reasons. An official build dumps signatures with *empty* descriptions, because
//! the prose lives in the engine's source XML and is merged *in* at doctool time, not carried back
//! out. And doctool deletes the editor's own doc cache — `editor_doc_cache-<ver>.res` in the user
//! data dir — which the next editor-mode launch, such as `reimport`'s `--import`, then has to
//! rebuild.)
//!
//! The dump costs several seconds and ~12 MB, so it is written once per engine version into the
//! data dir and reused. The parsed index is held for the life of the process — tens of MB
//! resident, paid only by a session that actually looks something up.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// File the engine writes into its working directory.
pub const DUMP_FILE: &str = "extension_api.json";

/// Cache root inside the data dir; one subdirectory per engine version.
pub const CACHE_SUBDIR: &str = "api";

/// Max entries listed per section when searching.
const SEARCH_LIMIT: usize = 12;

// ---- the slice of extension_api.json gdkit reads ----------------------------------------------
// Unknown fields are ignored by serde, so an engine update that adds keys stays readable.

#[derive(Debug, Deserialize)]
struct Api {
    header: Header,
    #[serde(default)]
    classes: Vec<Class>,
    #[serde(default)]
    builtin_classes: Vec<Class>,
    #[serde(default)]
    global_enums: Vec<Enum>,
    #[serde(default)]
    utility_functions: Vec<Method>,
    #[serde(default)]
    singletons: Vec<Singleton>,
}

#[derive(Debug, Deserialize)]
struct Header {
    #[serde(default)]
    version_full_name: String,
}

#[derive(Debug, Deserialize)]
struct Singleton {
    name: String,
    #[serde(default)]
    r#type: String,
}

#[derive(Debug, Default, Deserialize)]
struct Class {
    name: String,
    #[serde(default)]
    inherits: Option<String>,
    #[serde(default)]
    api_type: Option<String>,
    #[serde(default)]
    brief_description: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    methods: Vec<Method>,
    /// Engine classes expose properties (setter/getter pairs)…
    #[serde(default)]
    properties: Vec<Property>,
    /// …while the built-in Variant types expose plain members.
    #[serde(default)]
    members: Vec<Member>,
    #[serde(default)]
    signals: Vec<Signal>,
    #[serde(default)]
    constants: Vec<Constant>,
    #[serde(default)]
    enums: Vec<Enum>,
}

#[derive(Debug, Default, Deserialize)]
struct Method {
    #[serde(default)]
    name: String,
    #[serde(default)]
    is_const: bool,
    #[serde(default)]
    is_static: bool,
    #[serde(default)]
    is_virtual: bool,
    #[serde(default)]
    is_vararg: bool,
    /// Engine-class methods carry a structured return value…
    #[serde(default)]
    return_value: Option<ReturnValue>,
    /// …built-in methods and utility functions carry a bare type name.
    #[serde(default)]
    return_type: Option<String>,
    #[serde(default)]
    arguments: Vec<Argument>,
    #[serde(default)]
    description: String,
    /// Utility functions only.
    #[serde(default)]
    category: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ReturnValue {
    #[serde(default)]
    r#type: String,
}

#[derive(Debug, Default, Deserialize)]
struct Argument {
    #[serde(default)]
    name: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    default_value: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Property {
    #[serde(default)]
    name: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    setter: Option<String>,
    #[serde(default)]
    getter: Option<String>,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Default, Deserialize)]
struct Member {
    #[serde(default)]
    name: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Default, Deserialize)]
struct Signal {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: Vec<Argument>,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Default, Deserialize)]
struct Constant {
    #[serde(default)]
    name: String,
    /// An integer for engine classes, a constructor expression for built-ins — kept raw.
    #[serde(default)]
    value: serde_json::Value,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Default, Deserialize)]
struct Enum {
    #[serde(default)]
    name: String,
    #[serde(default)]
    is_bitfield: bool,
    #[serde(default)]
    values: Vec<Constant>,
}

// ---- the index ---------------------------------------------------------------------------------

/// A parsed, queryable API reference.
pub struct ApiIndex {
    /// The engine version the dump came from.
    pub version: String,
    classes: BTreeMap<String, Class>,
    /// Lowercased class name -> canonical name, so lookups are case-insensitive.
    by_lower: BTreeMap<String, String>,
    global_enums: Vec<Enum>,
    utility: Vec<Method>,
    singletons: Vec<Singleton>,
}

impl ApiIndex {
    /// Parse a dump. Fails only when the JSON is unreadable or not an extension-api document.
    pub fn parse(json: &str) -> Result<Self, String> {
        let api: Api = serde_json::from_str(json).map_err(|e| format!("bad API dump: {e}"))?;
        let mut classes: BTreeMap<String, Class> = BTreeMap::new();
        for c in api.classes.into_iter().chain(api.builtin_classes) {
            classes.insert(c.name.clone(), c);
        }
        if classes.is_empty() {
            return Err("API dump contains no classes".to_string());
        }
        let by_lower = classes
            .keys()
            .map(|k| (k.to_lowercase(), k.clone()))
            .collect();
        Ok(Self {
            version: api.header.version_full_name,
            classes,
            by_lower,
            global_enums: api.global_enums,
            utility: api.utility_functions,
            singletons: api.singletons,
        })
    }

    /// Read and parse a cached dump from disk.
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    /// Answer one query. `Class`, `Class.member`, or a free-text search term.
    pub fn query(&self, q: &str) -> String {
        let q = q.trim();
        if q.is_empty() {
            return self.overview();
        }
        if let Some((head, tail)) = q.split_once('.') {
            if let Some(class) = self.resolve(head) {
                return self.render_member(class, tail.trim());
            }
        }
        if let Some(class) = self.resolve(q) {
            return self.render_class(class);
        }
        self.search(q)
    }

    /// Canonical class name for a case-insensitive query.
    fn resolve(&self, name: &str) -> Option<&str> {
        self.by_lower
            .get(&name.trim().to_lowercase())
            .map(|s| s.as_str())
    }

    fn get(&self, name: &str) -> Option<&Class> {
        self.classes.get(name)
    }

    fn overview(&self) -> String {
        format!(
            "Godot API reference {} — {} classes, {} global enums, {} utility functions, {} \
             singletons.\nQuery a class (`Node2D`), a member (`Sprite2D.frame_coords`), or any \
             substring to search.",
            self.version,
            self.class_count(),
            self.global_enums.len(),
            self.utility.len(),
            self.singletons.len()
        )
    }

    /// `Class < Parent < Grandparent` — the chain, cycle-guarded.
    fn inheritance_chain(&self, name: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = self.get(name);
        while let Some(c) = cur {
            chain.push(c.name.clone());
            if chain.len() > 64 {
                break;
            }
            cur = c.inherits.as_deref().and_then(|p| self.get(p));
        }
        chain
    }

    fn render_class(&self, name: &str) -> String {
        let Some(c) = self.get(name) else {
            return format!("no class {name:?}");
        };
        let chain = self.inheritance_chain(name);
        let mut out = format!("{} ({})", c.name, chain.join(" < "));
        if let Some(api) = &c.api_type {
            out.push_str(&format!("  [{api}]"));
        }
        // Autoloaded singletons are reachable by name from any script, which is worth saying
        // before listing members the caller would otherwise think they must instantiate.
        if let Some(s) = self.singletons.iter().find(|s| s.r#type == c.name) {
            out.push_str(&format!("  [singleton: {}]", s.name));
        }
        out.push('\n');
        if !c.brief_description.trim().is_empty() {
            out.push_str(&format!("{}\n", bbcode_to_text(&c.brief_description)));
        }
        if !c.description.trim().is_empty() {
            out.push_str(&format!("\n{}\n", bbcode_to_text(&c.description)));
        }

        let props = property_lines(c);
        if !props.is_empty() {
            out.push_str(&format!("\nproperties ({}):\n", props.len()));
            for p in &props {
                out.push_str(&format!("  {p}\n"));
            }
        }
        if !c.methods.is_empty() {
            out.push_str(&format!("\nmethods ({}):\n", c.methods.len()));
            for m in &c.methods {
                out.push_str(&format!("  {}\n", signature(m)));
            }
        }
        if !c.signals.is_empty() {
            out.push_str(&format!("\nsignals ({}):\n", c.signals.len()));
            for s in &c.signals {
                out.push_str(&format!("  {}({})\n", s.name, arg_list(&s.arguments)));
            }
        }
        if !c.enums.is_empty() {
            out.push_str(&format!("\nenums ({}):\n", c.enums.len()));
            for e in &c.enums {
                let values: Vec<String> = e.values.iter().map(|v| v.name.clone()).collect();
                out.push_str(&format!("  {} {{ {} }}\n", e.name, values.join(", ")));
            }
        }
        if !c.constants.is_empty() {
            out.push_str(&format!("\nconstants ({}):\n", c.constants.len()));
            for k in &c.constants {
                out.push_str(&format!("  {} = {}\n", k.name, render_value(&k.value)));
            }
        }
        out.push_str(&format!(
            "\nAsk for `{}.<member>` to get one member's full description.\n",
            c.name
        ));
        out
    }

    /// Find `member` on `class` or anywhere up its inheritance chain.
    fn render_member(&self, class: &str, member: &str) -> String {
        if member.is_empty() {
            return self.render_class(class);
        }
        for owner in self.inheritance_chain(class) {
            let Some(c) = self.get(&owner) else { continue };
            if let Some(text) = render_member_of(c, member) {
                let mut out = text;
                if owner != class {
                    out.push_str(&format!("\n(inherited by {class} from {owner})\n"));
                }
                return out;
            }
        }
        let chain = self.inheritance_chain(class).join(" < ");
        format!(
            "{class} has no member {member:?} (searched {chain}).\nAsk for `{class}` to list its \
             members."
        )
    }

    /// Free-text search across class names, member names, utility functions and enum values.
    fn search(&self, q: &str) -> String {
        let needle = q.to_lowercase();
        let mut class_hits: Vec<&str> = Vec::new();
        let mut member_hits: Vec<String> = Vec::new();

        for (name, c) in &self.classes {
            if name.to_lowercase().contains(&needle) {
                class_hits.push(name);
            }
            if member_hits.len() >= SEARCH_LIMIT {
                continue;
            }
            for m in &c.methods {
                if m.name.to_lowercase().contains(&needle) {
                    member_hits.push(format!("{name}.{} (method)", m.name));
                }
            }
            for p in &c.properties {
                if p.name.to_lowercase().contains(&needle) {
                    member_hits.push(format!("{name}.{} (property)", p.name));
                }
            }
            for m in &c.members {
                if m.name.to_lowercase().contains(&needle) {
                    member_hits.push(format!("{name}.{} (member)", m.name));
                }
            }
            for s in &c.signals {
                if s.name.to_lowercase().contains(&needle) {
                    member_hits.push(format!("{name}.{} (signal)", s.name));
                }
            }
        }

        let util_hits: Vec<String> = self
            .utility
            .iter()
            .filter(|u| u.name.to_lowercase().contains(&needle))
            .take(SEARCH_LIMIT)
            .map(signature)
            .collect();

        let enum_hits: Vec<String> = self
            .global_enums
            .iter()
            .flat_map(|e| {
                e.values
                    .iter()
                    .filter(|v| v.name.to_lowercase().contains(&needle))
                    .map(move |v| format!("{}.{} = {}", e.name, v.name, render_value(&v.value)))
            })
            .take(SEARCH_LIMIT)
            .collect();

        if class_hits.is_empty()
            && member_hits.is_empty()
            && util_hits.is_empty()
            && enum_hits.is_empty()
        {
            return format!("nothing in the {} API matches {q:?}", self.version);
        }

        let mut out = format!("no exact class named {q:?} — matches:\n");
        section(
            &mut out,
            "classes",
            class_hits.iter().map(|s| s.to_string()),
        );
        section(&mut out, "members", member_hits.into_iter());
        section(&mut out, "utility functions", util_hits.into_iter());
        section(&mut out, "global enum values", enum_hits.into_iter());
        out
    }
}

/// Append a capped, labelled list to `out` when it has entries.
fn section<I: Iterator<Item = String>>(out: &mut String, label: &str, items: I) {
    let items: Vec<String> = items.take(SEARCH_LIMIT + 1).collect();
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("{label}:\n"));
    for i in items.iter().take(SEARCH_LIMIT) {
        out.push_str(&format!("  {i}\n"));
    }
    if items.len() > SEARCH_LIMIT {
        out.push_str("  …\n");
    }
}

/// One member's full entry, or `None` when this class does not declare it.
fn render_member_of(c: &Class, member: &str) -> Option<String> {
    let want = member.to_lowercase();

    if let Some(m) = c.methods.iter().find(|m| m.name.to_lowercase() == want) {
        return Some(format!(
            "{}.{} (method)\n{}\n\n{}\n",
            c.name,
            m.name,
            signature(m),
            bbcode_to_text(&m.description)
        ));
    }
    if let Some(p) = c.properties.iter().find(|p| p.name.to_lowercase() == want) {
        let accessors = match (&p.setter, &p.getter) {
            (Some(s), Some(g)) if !s.is_empty() && !g.is_empty() => {
                format!("  [set {s} / get {g}]")
            }
            (_, Some(g)) if !g.is_empty() => format!("  [get {g}, read-only]"),
            _ => String::new(),
        };
        return Some(format!(
            "{}.{} (property)\n{}: {}{}\n\n{}\n",
            c.name,
            p.name,
            p.name,
            p.r#type,
            accessors,
            bbcode_to_text(&p.description)
        ));
    }
    if let Some(m) = c.members.iter().find(|m| m.name.to_lowercase() == want) {
        return Some(format!(
            "{}.{} (member)\n{}: {}\n\n{}\n",
            c.name,
            m.name,
            m.name,
            m.r#type,
            bbcode_to_text(&m.description)
        ));
    }
    if let Some(s) = c.signals.iter().find(|s| s.name.to_lowercase() == want) {
        return Some(format!(
            "{}.{} (signal)\n{}({})\n\n{}\n",
            c.name,
            s.name,
            s.name,
            arg_list(&s.arguments),
            bbcode_to_text(&s.description)
        ));
    }
    if let Some(k) = c.constants.iter().find(|k| k.name.to_lowercase() == want) {
        return Some(format!(
            "{}.{} (constant)\n{} = {}\n\n{}\n",
            c.name,
            k.name,
            k.name,
            render_value(&k.value),
            bbcode_to_text(&k.description)
        ));
    }
    if let Some(e) = c.enums.iter().find(|e| e.name.to_lowercase() == want) {
        let mut out = format!(
            "{}.{} (enum{})\n",
            c.name,
            e.name,
            if e.is_bitfield { ", bitfield" } else { "" }
        );
        for v in &e.values {
            out.push_str(&format!("  {} = {}", v.name, render_value(&v.value)));
            let d = bbcode_to_text(&v.description);
            if !d.trim().is_empty() {
                out.push_str(&format!("  — {}", d.replace('\n', " ")));
            }
            out.push('\n');
        }
        return Some(out);
    }
    // An enum value addressed directly, e.g. `Control.FOCUS_ALL`.
    for e in &c.enums {
        if let Some(v) = e.values.iter().find(|v| v.name.to_lowercase() == want) {
            return Some(format!(
                "{}.{} (value of enum {})\n{} = {}\n\n{}\n",
                c.name,
                v.name,
                e.name,
                v.name,
                render_value(&v.value),
                bbcode_to_text(&v.description)
            ));
        }
    }
    None
}

/// Property/member one-liners, whichever shape this class uses.
fn property_lines(c: &Class) -> Vec<String> {
    c.properties
        .iter()
        .map(|p| format!("{}: {}", p.name, p.r#type))
        .chain(
            c.members
                .iter()
                .map(|m| format!("{}: {}", m.name, m.r#type)),
        )
        .collect()
}

/// `name(arg: Type = default, …) -> Return  [qualifiers]`
fn signature(m: &Method) -> String {
    let ret = m
        .return_value
        .as_ref()
        .map(|r| r.r#type.clone())
        .or_else(|| m.return_type.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "void".to_string());

    let mut quals: Vec<&str> = Vec::new();
    if m.is_const {
        quals.push("const");
    }
    if m.is_static {
        quals.push("static");
    }
    if m.is_virtual {
        quals.push("virtual");
    }
    if m.is_vararg {
        quals.push("vararg");
    }
    let qual = if quals.is_empty() {
        String::new()
    } else {
        format!("  [{}]", quals.join(" "))
    };
    let cat = match &m.category {
        Some(c) if !c.is_empty() => format!("  ({c})"),
        _ => String::new(),
    };

    format!("{}({}) -> {ret}{qual}{cat}", m.name, arg_list(&m.arguments))
}

fn arg_list(args: &[Argument]) -> String {
    args.iter()
        .map(|a| match &a.default_value {
            Some(d) if !d.is_empty() => format!("{}: {} = {d}", a.name, a.r#type),
            _ => format!("{}: {}", a.name, a.r#type),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Constant values arrive as integers (engine classes) or strings (built-ins).
fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "?".to_string(),
        other => other.to_string(),
    }
}

/// Flatten Godot's documentation BBCode into plain text.
///
/// Reference tags (`[param x]`, `[method Node.free]`, `[Sprite2D]`) become their target, `[code]`
/// spans become backticks, block tags become newlines, and unknown tags are dropped rather than
/// printed raw — an LLM reading this wants the words, not the markup.
pub fn bbcode_to_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            // An unmatched bracket is literal text.
            out.push_str(&rest[open..]);
            return out;
        };
        let tag = &after[..close];
        rest = &after[close + 1..];

        let (head, arg) = match tag.split_once([' ', '=']) {
            Some((h, a)) => (h, Some(a)),
            None => (tag, None),
        };

        match head {
            "code" | "/code" => out.push('`'),
            "br" => out.push('\n'),
            "codeblock" | "codeblocks" | "gdscript" | "csharp" => out.push('\n'),
            "/codeblock" | "/codeblocks" | "/gdscript" | "/csharp" => out.push('\n'),
            // Pure formatting — drop the markup, keep the text between the tags.
            "b" | "i" | "u" | "s" | "center" | "kbd" | "color" | "font" | "img" | "url" => {}
            _ if head.starts_with('/') => {}
            // Reference tags name their target after a space: [param pos], [method Node.free].
            "param" | "member" | "method" | "constant" | "enum" | "signal" | "theme_item"
            | "annotation" | "constructor" | "operator" => {
                if let Some(a) = arg {
                    out.push_str(a);
                }
            }
            // A bare tag is a class reference: [Sprite2D].
            _ if arg.is_none() && !head.is_empty() => out.push_str(head),
            _ => {}
        }
    }
    out.push_str(rest);
    out
}

/// Directory holding the cached dump for one engine version.
pub fn cache_dir(data_dir: &Path, version: &str) -> PathBuf {
    data_dir.join(CACHE_SUBDIR).join(sanitize_version(version))
}

/// Path of the cached dump for one engine version.
pub fn cache_path(data_dir: &Path, version: &str) -> PathBuf {
    cache_dir(data_dir, version).join(DUMP_FILE)
}

/// Reduce a version string to something safe as a single path segment.
pub fn sanitize_version(version: &str) -> String {
    let cleaned: String = version
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// Pull the version out of `godot --version` output, ignoring any banner lines around it.
pub fn parse_version_output(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .map(|l| l.trim())
        .find(|l| {
            let mut chars = l.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_digit()) && l.contains('.')
        })
        .map(|l| l.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miniature dump with the same shape as the engine's, covering both class flavours.
    const MINI: &str = r#"{
      "header": { "version_full_name": "Godot Engine v4.7.1.stable.official" },
      "classes": [
        {
          "name": "Node",
          "is_refcounted": false,
          "api_type": "core",
          "brief_description": "Base class for scene objects.",
          "description": "The [b]base[/b] class. See [method add_child].",
          "methods": [
            { "name": "add_child", "is_const": false, "is_static": false, "is_virtual": false,
              "is_vararg": false, "return_value": { "type": "void" },
              "arguments": [ { "name": "node", "type": "Node" },
                             { "name": "force_readable_name", "type": "bool", "default_value": "false" } ],
              "description": "Adds [param node] as a child." }
          ],
          "properties": [
            { "type": "String", "name": "name", "setter": "set_name", "getter": "get_name",
              "description": "The node's name." }
          ],
          "signals": [ { "name": "ready", "description": "Emitted when ready." } ],
          "constants": [ { "name": "NOTIFICATION_ENTER_TREE", "value": 10 } ],
          "enums": [ { "name": "ProcessMode", "is_bitfield": false,
                       "values": [ { "name": "PROCESS_MODE_INHERIT", "value": 0, "description": "Inherit." } ] } ]
        },
        {
          "name": "Sprite2D",
          "inherits": "Node",
          "api_type": "core",
          "brief_description": "General-purpose sprite node.",
          "description": "Displays a 2D texture.",
          "methods": [
            { "name": "is_pixel_opaque", "is_const": true, "return_value": { "type": "bool" },
              "arguments": [ { "name": "pos", "type": "Vector2" } ],
              "description": "Returns [code]true[/code] if opaque." }
          ],
          "properties": [
            { "type": "bool", "name": "centered", "setter": "set_centered", "getter": "is_centered",
              "description": "Whether the sprite is centered." }
          ]
        }
      ],
      "builtin_classes": [
        {
          "name": "Vector2",
          "brief_description": "A 2D vector.",
          "members": [ { "name": "x", "type": "float", "description": "The X component." } ],
          "methods": [ { "name": "angle", "return_type": "float", "is_const": true,
                         "description": "Returns the angle." } ],
          "constants": [ { "name": "ZERO", "type": "Vector2", "value": "Vector2(0, 0)",
                           "description": "Zero vector." } ]
        }
      ],
      "global_enums": [
        { "name": "Side", "is_bitfield": false,
          "values": [ { "name": "SIDE_LEFT", "value": 0, "description": "Left side." } ] }
      ],
      "utility_functions": [
        { "name": "sin", "return_type": "float", "category": "math",
          "arguments": [ { "name": "angle_rad", "type": "float" } ],
          "description": "Returns the sine." }
      ],
      "singletons": [ { "name": "Performance", "type": "Performance" } ]
    }"#;

    fn index() -> ApiIndex {
        ApiIndex::parse(MINI).expect("mini dump parses")
    }

    #[test]
    fn parses_both_class_flavours() {
        let idx = index();
        assert_eq!(idx.version, "Godot Engine v4.7.1.stable.official");
        assert_eq!(idx.class_count(), 3, "2 engine classes + 1 built-in");
    }

    #[test]
    fn rejects_a_document_that_is_not_an_api_dump() {
        assert!(ApiIndex::parse("{}").is_err());
        assert!(ApiIndex::parse("nonsense").is_err());
    }

    #[test]
    fn a_class_query_shows_the_inheritance_chain_and_members() {
        let out = index().query("Sprite2D");
        assert!(out.starts_with("Sprite2D (Sprite2D < Node)"), "{out}");
        assert!(out.contains("centered: bool"), "{out}");
        assert!(
            out.contains("is_pixel_opaque(pos: Vector2) -> bool  [const]"),
            "{out}"
        );
    }

    #[test]
    fn class_lookup_is_case_insensitive() {
        assert!(index().query("sprite2d").starts_with("Sprite2D"));
    }

    #[test]
    fn a_singleton_class_says_so() {
        let mini = MINI.replace(r#""name": "Node","#, r#""name": "Performance","#);
        let idx = ApiIndex::parse(&mini).unwrap();
        let out = idx.query("Performance");
        assert!(out.contains("[singleton: Performance]"), "{out}");
        assert!(!index().query("Sprite2D").contains("singleton"));
    }

    #[test]
    fn a_method_query_shows_the_signature_and_defaults() {
        let out = index().query("Node.add_child");
        assert!(out.contains("(method)"), "{out}");
        assert!(
            out.contains("add_child(node: Node, force_readable_name: bool = false) -> void"),
            "{out}"
        );
        assert!(out.contains("Adds node as a child."), "{out}");
    }

    #[test]
    fn a_property_query_shows_its_accessors() {
        let out = index().query("Sprite2D.centered");
        assert!(out.contains("centered: bool"), "{out}");
        assert!(out.contains("set_centered"), "{out}");
    }

    #[test]
    fn an_inherited_member_is_found_and_attributed() {
        let out = index().query("Sprite2D.name");
        assert!(out.contains("Node.name (property)"), "{out}");
        assert!(out.contains("inherited by Sprite2D from Node"), "{out}");
    }

    #[test]
    fn signals_constants_enums_and_enum_values_all_resolve() {
        let idx = index();
        assert!(idx.query("Node.ready").contains("(signal)"));
        assert!(idx
            .query("Node.NOTIFICATION_ENTER_TREE")
            .contains("NOTIFICATION_ENTER_TREE = 10"));
        assert!(idx
            .query("Node.ProcessMode")
            .contains("PROCESS_MODE_INHERIT = 0"));
        assert!(idx
            .query("Node.PROCESS_MODE_INHERIT")
            .contains("value of enum ProcessMode"));
    }

    #[test]
    fn builtin_members_and_string_constants_render() {
        let idx = index();
        let out = idx.query("Vector2");
        assert!(out.contains("x: float"), "{out}");
        assert!(out.contains("ZERO = Vector2(0, 0)"), "{out}");
        assert!(idx
            .query("Vector2.angle")
            .contains("angle() -> float  [const]"));
    }

    #[test]
    fn an_unknown_member_names_what_was_searched() {
        let out = index().query("Sprite2D.nope");
        assert!(out.contains("no member \"nope\""), "{out}");
        assert!(out.contains("Sprite2D < Node"), "{out}");
    }

    #[test]
    fn a_free_text_query_searches_members_utilities_and_enums() {
        let idx = index();
        let out = idx.query("pixel");
        assert!(out.contains("Sprite2D.is_pixel_opaque (method)"), "{out}");

        let out = idx.query("sin");
        assert!(out.contains("sin(angle_rad: float) -> float"), "{out}");
        assert!(out.contains("(math)"), "{out}");

        let out = idx.query("SIDE_LEFT");
        assert!(out.contains("Side.SIDE_LEFT = 0"), "{out}");
    }

    #[test]
    fn a_query_matching_nothing_says_so() {
        let out = index().query("zzzznotathing");
        assert!(out.contains("nothing in the"), "{out}");
    }

    #[test]
    fn an_empty_query_returns_the_overview() {
        let out = index().query("  ");
        assert!(out.contains("3 classes"), "{out}");
        assert!(out.contains("1 utility functions"), "{out}");
    }

    #[test]
    fn bbcode_becomes_readable_text() {
        assert_eq!(bbcode_to_text("[b]bold[/b] text"), "bold text");
        assert_eq!(
            bbcode_to_text("returns [code]true[/code]"),
            "returns `true`"
        );
        assert_eq!(bbcode_to_text("uses [param pos] now"), "uses pos now");
        assert_eq!(bbcode_to_text("see [method Node.free]"), "see Node.free");
        assert_eq!(bbcode_to_text("a [Sprite2D] node"), "a Sprite2D node");
        assert_eq!(bbcode_to_text("line[br]break"), "line\nbreak");
        assert_eq!(bbcode_to_text("[url=http://x]text[/url]"), "text");
        // An unmatched bracket must survive rather than eating the rest of the line.
        assert_eq!(bbcode_to_text("array[0 is fine"), "array[0 is fine");
        assert_eq!(bbcode_to_text("plain"), "plain");
    }

    #[test]
    fn version_strings_become_safe_path_segments() {
        assert_eq!(
            sanitize_version("4.7.1.stable.official"),
            "4.7.1.stable.official"
        );
        assert_eq!(sanitize_version("4.7.1 stable/x"), "4.7.1-stable-x");
        assert_eq!(sanitize_version("   "), "unknown");
    }

    #[test]
    fn the_version_line_is_picked_out_of_banner_noise() {
        let lines = vec![
            "Godot Engine v4.7.1.stable - https://godotengine.org".to_string(),
            "4.7.1.stable.official.a13da4feb".to_string(),
        ];
        assert_eq!(
            parse_version_output(&lines).as_deref(),
            Some("4.7.1.stable.official.a13da4feb")
        );
        assert_eq!(parse_version_output(&["banner only".to_string()]), None);
    }

    /// The whole `docs` tool rests on one engine contract: `--dump-extension-api-with-docs`
    /// writes a JSON reference *including prose* into its working directory. This drives the real
    /// engine and queries the result, so a change in that contract fails here rather than in a
    /// session.
    #[tokio::test]
    async fn dumps_and_queries_the_installed_engine_api() {
        let Some((godot, _engine_guard)) =
            crate::testutil::godot_or_skip("dumps_the_engine_api").await
        else {
            return;
        };
        let dir = crate::testutil::temp_dir("apidump");

        let ver = crate::process::run_oneshot(
            &godot,
            &["--version".to_string()],
            std::time::Duration::from_secs(30),
            None,
        )
        .await
        .expect("godot --version");
        let lines: Vec<String> = ver.lines.iter().map(|l| l.text.clone()).collect();
        let version = parse_version_output(&lines).expect("a version line");
        assert!(
            version.starts_with('4'),
            "gdkit targets Godot 4.x: {version}"
        );

        crate::process::run_oneshot(
            &godot,
            &[
                "--headless".to_string(),
                "--dump-extension-api-with-docs".to_string(),
            ],
            std::time::Duration::from_secs(300),
            Some(&dir),
        )
        .await
        .expect("dump the API");

        let dump = dir.join(DUMP_FILE);
        assert!(
            dump.is_file(),
            "the engine wrote no {DUMP_FILE} into {}",
            dir.display()
        );

        let idx = ApiIndex::load(&dump).expect("parse the real dump");
        assert!(
            idx.class_count() > 500,
            "only {} classes",
            idx.class_count()
        );

        // A class, an inherited member, and a member the engine documents in prose — the dump is
        // only worth using if the descriptions actually came along.
        let out = idx.query("Sprite2D");
        assert!(out.starts_with("Sprite2D (Sprite2D < Node2D"), "{out}");
        assert!(out.contains("frame_coords"), "{out}");

        let out = idx.query("Sprite2D.position");
        assert!(out.contains("inherited by Sprite2D from Node2D"), "{out}");

        let out = idx.query("Sprite2D.is_pixel_opaque");
        assert!(out.contains("(pos: Vector2) -> bool"), "{out}");
        assert!(
            out.to_lowercase().contains("opaque"),
            "descriptions must survive the dump: {out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_paths_are_versioned() {
        let p = cache_path(Path::new("D:/data"), "4.7.1.stable");
        assert!(p.ends_with("extension_api.json"));
        assert!(p.to_string_lossy().contains("4.7.1.stable"));
        assert_eq!(
            cache_dir(Path::new("D:/data"), "x")
                .parent()
                .unwrap()
                .file_name()
                .unwrap(),
            "api"
        );
    }
}
