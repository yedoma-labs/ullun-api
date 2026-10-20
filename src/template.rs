//! Server-side HTML templates
//!
//! [`TemplateEngine`] is the integration point: implement it for any engine
//! (minijinja, tera, handlebars, ...) and render with
//! `Response::html(engine.render("page.html", &ctx)?)`.
//!
//! [`Templates`] is a small built-in engine without dependencies. Syntax
//! (Mustache/Handlebars subset):
//!
//! | Tag | Meaning |
//! |-----|---------|
//! | `{{ user.name }}` | value, HTML-escaped; missing values render as empty |
//! | `{{{ html }}}` or `{{& html }}` | value, **not** escaped |
//! | `{{#if x}}..{{else}}..{{/if}}` | conditional; falsy: missing, `null`, `false`, `0`, `""`, `[]`, `{}` |
//! | `{{#unless x}}..{{/unless}}` | inverted conditional |
//! | `{{#each items}}..{{/each}}` | loop; `{{this}}`, `{{@index}}`, `{{@first}}`, `{{@last}}`, `{{@key}}` (objects) |
//! | `{{> partial.html}}` | include another template with the current context |
//! | `{{! comment }}` | ignored |
//!
//! Names resolve against the innermost scope first, then outward, so loop
//! bodies can still read top-level values.
//!
//! ```
//! use ullun::template::Templates;
//!
//! let mut t = Templates::new();
//! t.add("hello.html", "Hello {{name}}!{{#each tags}} #{{this}}{{/each}}").unwrap();
//! let out = t
//!     .render("hello.html", &serde_json::json!({ "name": "<Ann>", "tags": ["a", "b"] }))
//!     .unwrap();
//! assert_eq!(out, "Hello &lt;Ann&gt;! #a #b");
//! ```

use crate::error::{Error, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

const MAX_PARTIAL_DEPTH: usize = 16;

/// Anything that can turn a named template plus data into text
pub trait TemplateEngine: Send + Sync + 'static {
    fn render(&self, name: &str, context: &Value) -> Result<String>;
}

/// Built-in template engine, see the [module docs](self) for the syntax
#[derive(Default)]
pub struct Templates {
    templates: HashMap<String, Vec<Node>>,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Var {
        path: String,
        escape: bool,
    },
    If {
        path: String,
        negate: bool,
        then: Vec<Node>,
        otherwise: Vec<Node>,
    },
    Each {
        path: String,
        body: Vec<Node>,
    },
    Partial(String),
}

impl Templates {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse and register a template under `name`
    pub fn add(&mut self, name: &str, source: &str) -> Result<&mut Self> {
        let nodes =
            parse(source).map_err(|e| Error::internal(format!("Template '{}': {}", name, e)))?;
        self.templates.insert(name.to_string(), nodes);
        Ok(self)
    }

    /// Load every `.html`, `.htm`, `.txt` and `.xml` file below `dir`
    /// (recursively); the name is the path relative to `dir`, with `/`
    /// separators, e.g. `layouts/base.html`.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let mut templates = Templates::new();
        templates.load_dir(dir.as_ref(), dir.as_ref())?;
        Ok(templates)
    }

    fn load_dir(&mut self, root: &Path, dir: &Path) -> Result<()> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| Error::internal(format!("Template dir {}: {}", dir.display(), e)))?;
        for entry in entries {
            let path = entry
                .map_err(|e| Error::internal(format!("Template dir: {}", e)))?
                .path();
            if path.is_dir() {
                self.load_dir(root, &path)?;
                continue;
            }
            let known = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("html" | "htm" | "txt" | "xml")
            );
            if !known {
                continue;
            }
            let name = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let source = std::fs::read_to_string(&path)
                .map_err(|e| Error::internal(format!("Template {}: {}", path.display(), e)))?;
            self.add(&name, &source)?;
        }
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.templates.contains_key(name)
    }

    /// Render with any serializable context
    pub fn render<T: Serialize>(&self, name: &str, context: &T) -> Result<String> {
        let ctx = serde_json::to_value(context)
            .map_err(|e| Error::internal(format!("Template context: {}", e)))?;
        TemplateEngine::render(self, name, &ctx)
    }
}

impl TemplateEngine for Templates {
    fn render(&self, name: &str, context: &Value) -> Result<String> {
        let mut out = String::new();
        let mut scope = vec![Frame {
            value: context,
            index: None,
            key: None,
            last: false,
        }];
        self.render_nodes(name, &mut scope, &mut out, 0)?;
        Ok(out)
    }
}

struct Frame<'a> {
    value: &'a Value,
    index: Option<usize>,
    key: Option<&'a str>,
    last: bool,
}

impl Templates {
    fn render_nodes<'a>(
        &'a self,
        name: &str,
        scope: &mut Vec<Frame<'a>>,
        out: &mut String,
        depth: usize,
    ) -> Result<()> {
        let nodes = self
            .templates
            .get(name)
            .ok_or_else(|| Error::internal(format!("Template '{}' not found", name)))?;
        self.render_list(nodes, scope, out, depth)
    }

    fn render_list<'a>(
        &'a self,
        nodes: &'a [Node],
        scope: &mut Vec<Frame<'a>>,
        out: &mut String,
        depth: usize,
    ) -> Result<()> {
        for node in nodes {
            match node {
                Node::Text(t) => out.push_str(t),
                Node::Var { path, escape } => {
                    let text = match special(path, scope) {
                        Some(s) => s,
                        None => lookup(path, scope).map(display).unwrap_or_default(),
                    };
                    if *escape {
                        escape_html(&text, out);
                    } else {
                        out.push_str(&text);
                    }
                }
                Node::If {
                    path,
                    negate,
                    then,
                    otherwise,
                } => {
                    let truthy = match special(path, scope) {
                        Some(s) => s == "true",
                        None => lookup(path, scope).map(is_truthy).unwrap_or(false),
                    };
                    let branch = if truthy != *negate { then } else { otherwise };
                    self.render_list(branch, scope, out, depth)?;
                }
                Node::Each { path, body } => {
                    // The looked-up value borrows from the render context ('a),
                    // not from the scope stack, so it can be pushed as a frame.
                    let items = lookup(path, scope);
                    match items {
                        Some(Value::Array(list)) => {
                            for (i, item) in list.iter().enumerate() {
                                scope.push(Frame {
                                    value: item,
                                    index: Some(i),
                                    key: None,
                                    last: i + 1 == list.len(),
                                });
                                let r = self.render_list(body, scope, out, depth);
                                scope.pop();
                                r?;
                            }
                        }
                        Some(Value::Object(map)) => {
                            let len = map.len();
                            for (i, (k, v)) in map.iter().enumerate() {
                                scope.push(Frame {
                                    value: v,
                                    index: Some(i),
                                    key: Some(k.as_str()),
                                    last: i + 1 == len,
                                });
                                let r = self.render_list(body, scope, out, depth);
                                scope.pop();
                                r?;
                            }
                        }
                        _ => {}
                    }
                }
                Node::Partial(name) => {
                    if depth >= MAX_PARTIAL_DEPTH {
                        return Err(Error::internal(format!(
                            "Partial '{}' nested deeper than {} levels",
                            name, MAX_PARTIAL_DEPTH
                        )));
                    }
                    self.render_nodes(name, scope, out, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

/// `@index`, `@first`, `@last`, `@key` of the innermost loop frame
fn special(path: &str, scope: &[Frame<'_>]) -> Option<String> {
    let name = path.strip_prefix('@')?;
    let frame = scope.iter().rev().find(|f| f.index.is_some())?;
    match name {
        "index" => frame.index.map(|i| i.to_string()),
        "first" => frame.index.map(|i| (i == 0).to_string()),
        "last" => Some(frame.last.to_string()),
        "key" => Some(frame.key.unwrap_or("").to_string()),
        _ => None,
    }
}

fn lookup<'a>(path: &str, scope: &[Frame<'a>]) -> Option<&'a Value> {
    if path == "this" || path == "." {
        return scope.last().map(|f| f.value);
    }
    let mut parts = path.split('.');
    let first = parts.next()?;
    let mut current = scope.iter().rev().find_map(|f| member(f.value, first))?;
    for part in parts {
        current = member(current, part)?;
    }
    Some(current)
}

fn member<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map.get(key),
        Value::Array(list) => key.parse::<usize>().ok().and_then(|i| list.get(i)),
        _ => None,
    }
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn escape_html(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

// ---- parser ----

fn parse(source: &str) -> std::result::Result<Vec<Node>, String> {
    enum Open {
        If {
            path: String,
            negate: bool,
            then: Vec<Node>,
            in_else: bool,
            otherwise: Vec<Node>,
        },
        Each {
            path: String,
        },
    }
    // (kind, nodes collected for the open block's current branch)
    let mut stack: Vec<(Open, Vec<Node>)> = Vec::new();
    let mut current: Vec<Node> = Vec::new();
    let mut rest = source;

    while let Some(start) = rest.find("{{") {
        if start > 0 {
            current.push(Node::Text(rest[..start].to_string()));
        }
        let after = &rest[start + 2..];
        let (tag, consumed) = if let Some(inner) = after.strip_prefix('{') {
            let end = inner.find("}}}").ok_or("unclosed '{{{'")?;
            (format!("{{{}", inner[..end].trim()), 1 + end + 3)
        } else {
            let end = after.find("}}").ok_or("unclosed '{{'")?;
            (after[..end].trim().to_string(), end + 2)
        };
        rest = &after[consumed..];

        if let Some(raw) = tag.strip_prefix('{') {
            current.push(Node::Var {
                path: raw.trim().to_string(),
                escape: false,
            });
        } else if tag.starts_with('!') {
            // comment
        } else if let Some(raw) = tag.strip_prefix('&') {
            current.push(Node::Var {
                path: raw.trim().to_string(),
                escape: false,
            });
        } else if let Some(name) = tag.strip_prefix('>') {
            current.push(Node::Partial(name.trim().to_string()));
        } else if let Some(path) = tag.strip_prefix("#if ") {
            stack.push((
                Open::If {
                    path: path.trim().to_string(),
                    negate: false,
                    then: Vec::new(),
                    in_else: false,
                    otherwise: Vec::new(),
                },
                std::mem::take(&mut current),
            ));
        } else if let Some(path) = tag.strip_prefix("#unless ") {
            stack.push((
                Open::If {
                    path: path.trim().to_string(),
                    negate: true,
                    then: Vec::new(),
                    in_else: false,
                    otherwise: Vec::new(),
                },
                std::mem::take(&mut current),
            ));
        } else if let Some(path) = tag.strip_prefix("#each ") {
            stack.push((
                Open::Each {
                    path: path.trim().to_string(),
                },
                std::mem::take(&mut current),
            ));
        } else if tag == "else" {
            match stack.last_mut() {
                Some((Open::If { then, in_else, .. }, _)) if !*in_else => {
                    *then = std::mem::take(&mut current);
                    *in_else = true;
                }
                _ => return Err("'{{else}}' outside of #if/#unless".into()),
            }
        } else if tag == "/if" || tag == "/unless" || tag == "/each" {
            let (open, parent) = stack
                .pop()
                .ok_or_else(|| format!("unexpected '{{{{{}}}}}'", tag))?;
            let body = std::mem::replace(&mut current, parent);
            let node = match (open, tag.as_str()) {
                (
                    Open::If {
                        path,
                        negate,
                        then,
                        in_else,
                        mut otherwise,
                    },
                    "/if" | "/unless",
                ) => {
                    let (then, otherwise) = if in_else {
                        otherwise = body;
                        (then, otherwise)
                    } else {
                        (body, otherwise)
                    };
                    Node::If {
                        path,
                        negate,
                        then,
                        otherwise,
                    }
                }
                (Open::Each { path }, "/each") => Node::Each { path, body },
                _ => return Err(format!("mismatched closing tag '{{{{{}}}}}'", tag)),
            };
            current.push(node);
        } else if tag.is_empty() {
            return Err("empty tag '{{}}'".into());
        } else if tag.starts_with('#') || tag.starts_with('/') {
            return Err(format!("unknown block tag '{{{{{}}}}}'", tag));
        } else {
            current.push(Node::Var {
                path: tag,
                escape: true,
            });
        }
    }
    if !rest.is_empty() {
        current.push(Node::Text(rest.to_string()));
    }
    if !stack.is_empty() {
        return Err("unclosed block (missing '{{/if}}', '{{/unless}}' or '{{/each}}')".into());
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(src: &str, ctx: Value) -> String {
        let mut t = Templates::new();
        t.add("t", src).unwrap();
        t.render("t", &ctx).unwrap()
    }

    #[test]
    fn variables_are_escaped_unless_raw() {
        let ctx = json!({ "x": "<b>\"a\" & 'b'</b>", "n": 3, "ok": true, "nothing": null });
        assert_eq!(
            render("{{x}}", ctx.clone()),
            "&lt;b&gt;&quot;a&quot; &amp; &#39;b&#39;&lt;/b&gt;"
        );
        assert_eq!(
            render("{{{x}}}|{{& x}}", ctx.clone()),
            "<b>\"a\" & 'b'</b>|<b>\"a\" & 'b'</b>"
        );
        assert_eq!(
            render("{{n}} {{ok}} [{{nothing}}] [{{missing}}]", ctx),
            "3 true [] []"
        );
    }

    #[test]
    fn nested_paths_and_array_index() {
        let ctx = json!({ "user": { "name": "Ann", "tags": ["a", "b"] } });
        assert_eq!(
            render("{{user.name}} {{user.tags.1}} [{{user.nope.deep}}]", ctx),
            "Ann b []"
        );
    }

    #[test]
    fn conditionals() {
        let t = "{{#if a}}A{{else}}no A{{/if}}|{{#unless a}}U{{/unless}}|{{#if b}}B{{/if}}";
        assert_eq!(render(t, json!({ "a": "x", "b": 0 })), "A||");
        assert_eq!(render(t, json!({ "a": [], "b": [1] })), "no A|U|B");
        assert_eq!(render(t, json!({})), "no A|U|");
    }

    #[test]
    fn each_over_arrays_with_specials_and_outer_scope() {
        let ctx = json!({ "site": "S", "items": [{ "n": "x" }, { "n": "y" }, { "n": "z" }] });
        let t = "{{#each items}}{{@index}}:{{n}}@{{site}}{{#if @first}}!{{/if}}{{#if @last}}.{{/if}};{{/each}}";
        assert_eq!(render(t, ctx), "0:x@S!;1:y@S;2:z@S.;");
        assert_eq!(
            render("{{#each l}}[{{this}}]{{/each}}", json!({ "l": [1, "a"] })),
            "[1][a]"
        );
        assert_eq!(render("{{#each l}}x{{/each}}", json!({ "l": [] })), "");
    }

    #[test]
    fn each_over_objects_exposes_key() {
        let out = render(
            "{{#each m}}{{@key}}={{this}},{{/each}}",
            json!({ "m": { "a": 1, "b": 2 } }),
        );
        assert_eq!(out, "a=1,b=2,");
    }

    #[test]
    fn nested_blocks_and_else_inside_each() {
        let t = "{{#each u}}{{#if admin}}{{name}}*{{else}}{{name}}{{/if}} {{/each}}";
        let ctx = json!({ "u": [{ "name": "a", "admin": true }, { "name": "b" }] });
        assert_eq!(render(t, ctx), "a* b ");
    }

    #[test]
    fn partials_and_comments() {
        let mut t = Templates::new();
        t.add("p", "<{{name}}>").unwrap();
        t.add("page", "{{! hidden }}A{{> p}}B").unwrap();
        assert_eq!(t.render("page", &json!({ "name": "n" })).unwrap(), "A<n>B");
    }

    #[test]
    fn recursive_partials_are_bounded() {
        let mut t = Templates::new();
        t.add("loop", "{{> loop}}").unwrap();
        let err = t.render("loop", &json!({})).unwrap_err();
        assert_eq!(err.status_code(), 500);
        assert!(err.message().contains("nested deeper"));
    }

    #[test]
    fn errors_are_reported_with_template_name() {
        let mut t = Templates::new();
        for bad in [
            "{{#if a}}",
            "{{/if}}",
            "{{x",
            "{{{x}}",
            "{{}}",
            "{{#foo a}}{{/foo}}",
            "{{else}}",
            "{{#each a}}{{/if}}",
        ] {
            let e = t
                .add("bad", bad)
                .err()
                .unwrap_or_else(|| panic!("accepted {bad}"));
            assert!(
                e.message().contains("Template 'bad'"),
                "{bad}: {}",
                e.message()
            );
        }
        assert!(t.render("missing", &json!({})).is_err());
        assert!(!t.contains("missing"));
    }

    #[test]
    fn text_only_and_serializable_contexts() {
        #[derive(Serialize)]
        struct Ctx {
            title: &'static str,
        }
        let mut t = Templates::new();
        t.add("a", "plain {text} with } braces").unwrap();
        t.add("b", "{{title}}").unwrap();
        assert_eq!(
            t.render("a", &json!({})).unwrap(),
            "plain {text} with } braces"
        );
        assert_eq!(t.render("b", &Ctx { title: "T" }).unwrap(), "T");
    }

    #[test]
    fn from_dir_loads_nested_files() {
        let dir = std::env::temp_dir().join(format!("ullun-tpl-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("layouts")).unwrap();
        std::fs::write(dir.join("index.html"), "{{> layouts/base.html}}!").unwrap();
        std::fs::write(dir.join("layouts/base.html"), "<h1>{{t}}</h1>").unwrap();
        std::fs::write(dir.join("ignored.bin"), "x").unwrap();

        let t = Templates::from_dir(&dir).unwrap();
        assert!(t.contains("layouts/base.html") && !t.contains("ignored.bin"));
        assert_eq!(
            t.render("index.html", &json!({ "t": "Hi" })).unwrap(),
            "<h1>Hi</h1>!"
        );
        std::fs::remove_dir_all(dir).ok();
        assert!(Templates::from_dir("/definitely/not/here").is_err());
    }
}
