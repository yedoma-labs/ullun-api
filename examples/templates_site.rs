//! Scenario: server-rendered site with templates, forms and compression
//! (feature `compression`)
//!
//! - built-in `Templates` engine (partials, loops, conditionals, escaping)
//! - form posts parsed with `Request::form`, validated, Post/Redirect/Get
//! - gzip compression for the HTML and JSON responses
//! - the same data as JSON under `/entries.json`
//!
//! Port: set PORT (default 3000)
//! Run:  cargo run --example templates_site --features compression   then open http://localhost:3000

use std::sync::{Arc, Mutex};
use ullun::compression::Compression;
use ullun::prelude::*;

#[derive(Clone, Serialize)]
struct Entry {
    name: String,
    message: String,
}

#[derive(Deserialize)]
struct SignForm {
    name: String,
    message: String,
}

fn templates() -> Templates {
    let mut t = Templates::new();
    t.add(
        "layout.html",
        "<!doctype html><meta charset=\"utf-8\"><title>{{title}}</title>\
         <body style=\"font-family:system-ui;max-width:40rem;margin:2rem auto\">\
         <h1>{{title}}</h1>",
    )
    .and_then(|t| {
        t.add(
            "index.html",
            "{{> layout.html}}\
             {{#if error}}<p style=\"color:#b00\">{{error}}</p>{{/if}}\
             <form method=\"post\" action=\"/sign\">\
               <input name=\"name\" placeholder=\"name\" value=\"{{draft.name}}\"> \
               <input name=\"message\" placeholder=\"message\" value=\"{{draft.message}}\"> \
               <button>Sign</button></form>\
             {{#if entries}}<ul>{{#each entries}}<li>#{{@index}} <b>{{name}}</b>: {{message}}</li>{{/each}}</ul>\
             {{else}}<p>No entries yet.</p>{{/if}}",
        )
    })
    .expect("templates parse");
    t
}

fn render(templates: &Templates, ctx: serde_json::Value) -> Result<Response> {
    Ok(Response::html(templates.render("index.html", &ctx)?))
}

#[tokio::main]
async fn main() {
    let addr = format!(
        "127.0.0.1:{}",
        std::env::var("PORT").unwrap_or_else(|_| "3000".into())
    );
    let templates = Arc::new(templates());
    let entries: Arc<Mutex<Vec<Entry>>> = Arc::default();

    let (t, e) = (templates.clone(), entries.clone());
    let index = move |_req: Request| {
        let (t, e) = (t.clone(), e.clone());
        async move {
            let entries = e.lock().expect("entries lock").clone();
            render(
                &t,
                serde_json::json!({ "title": "Guestbook", "entries": entries }),
            )
        }
    };

    let (t, e) = (templates, entries.clone());
    let sign = move |req: Request| {
        let (t, e) = (t.clone(), e.clone());
        async move {
            let form: SignForm = req.form()?;
            let (name, message) = (form.name.trim(), form.message.trim());
            if name.is_empty() || message.is_empty() || message.len() > 280 {
                // re-render with the draft instead of losing the input
                let entries = e.lock().expect("entries lock").clone();
                let mut res = render(
                    &t,
                    serde_json::json!({
                        "title": "Guestbook",
                        "entries": entries,
                        "error": "Name and message (max. 280 characters) are required",
                        "draft": { "name": name, "message": message },
                    }),
                )?;
                res.status = http::StatusCode::UNPROCESSABLE_ENTITY;
                return Ok(res);
            }
            e.lock().expect("entries lock").push(Entry {
                name: name.to_string(),
                message: message.to_string(),
            });
            Ok(Response::redirect("/"))
        }
    };

    App::new()
        .max_body_size(Some(16 * 1024))
        .middleware(Compression::new().min_size(512))
        .get("/", index)
        .post("/sign", sign)
        .get("/entries.json", move |_req: Request| {
            let entries = entries.clone();
            async move {
                Ok(Response::json(
                    entries.lock().expect("entries lock").clone(),
                ))
            }
        })
        .run(&addr)
        .await
        .unwrap();
}
