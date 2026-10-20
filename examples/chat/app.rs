//! Chat: application logic, shared by `main.rs` and the tests

use argon2::password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use ullun::hub::{Delivery, Hub};
use ullun::jwt::{self, Claims, JwtAuth};
use ullun::prelude::*;
use ullun::rate_limit::RateLimiter;
use ullun::security::random_token;
use ullun::validate::Validator;
use ullun::websocket::{Message, WebSocket, WsOptions};

const HISTORY_LEN: usize = 100;
const MAX_ROOMS: usize = 100;
const MAX_TEXT_CHARS: usize = 2000;

pub struct Config {
    /// (name, password)
    pub users: Vec<(String, String)>,
    pub jwt_secret: Vec<u8>,
    pub token_ttl_secs: u64,
    pub ticket_ttl: Duration,
    pub max_connections_per_user: usize,
    /// Messages allowed per window and connection
    pub message_rate: (u32, Duration),
    /// Rate-limit violations before the connection is closed
    pub max_violations: u32,
    pub ws: WsOptions,
    pub fast_hashing: bool,
    pub login_rate: (u32, Duration),
}

impl Config {
    pub fn from_env() -> std::result::Result<Self, String> {
        let dev = std::env::var("DEV").is_ok_and(|v| v == "1");
        let users_spec = match std::env::var("CHAT_USERS") {
            Ok(s) => s,
            Err(_) if dev => "alice:demo-password,bob:demo-password".into(),
            Err(_) => return Err("CHAT_USERS is required (name:password,...) or DEV=1".into()),
        };
        let mut users = Vec::new();
        for entry in users_spec.split(',').filter(|e| !e.trim().is_empty()) {
            let (name, password) = entry
                .trim()
                .split_once(':')
                .ok_or("bad CHAT_USERS entry, expected name:password")?;
            if password.len() < 8 {
                return Err(format!("password of {name} must be at least 8 characters"));
            }
            users.push((name.to_string(), password.to_string()));
        }
        if users.is_empty() {
            return Err("no users configured".into());
        }
        let secret = match std::env::var("JWT_SECRET") {
            Ok(s) if s.len() >= 32 => s,
            Ok(_) => return Err("JWT_SECRET must be at least 32 bytes".into()),
            Err(_) if dev => "dev-only-secret-dev-only-secret-xx".into(),
            Err(_) => return Err("JWT_SECRET is required (or DEV=1)".into()),
        };
        Ok(Self {
            users,
            jwt_secret: secret.into_bytes(),
            token_ttl_secs: 3600,
            ticket_ttl: Duration::from_secs(30),
            max_connections_per_user: 5,
            message_rate: (5, Duration::from_secs(3)),
            max_violations: 3,
            ws: WsOptions {
                max_message_size: 16 * 1024,
                ..Default::default()
            },
            fast_hashing: false,
            login_rate: (10, Duration::from_secs(60)),
        })
    }
}

// ---- protocol ----

/// What the hub carries and the clients receive
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    Message {
        id: u64,
        user: String,
        text: String,
        ts: u64,
    },
    Joined {
        user: String,
    },
    Left {
        user: String,
    },
    Typing {
        user: String,
    },
}

/// What clients send
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Command {
    Say { text: String },
    Typing,
}

#[derive(Clone, Serialize)]
struct StoredMessage {
    id: u64,
    user: String,
    text: String,
    ts: u64,
}

struct Ticket {
    user: String,
    expires: Instant,
}

/// The authenticated user of a WebSocket request, put there by the ticket check
#[derive(Clone)]
struct ChatUser(String);

struct Chat {
    hub: Hub<Event>,
    history: Mutex<HashMap<String, VecDeque<StoredMessage>>>,
    /// room -> user -> open connections (a user may have several tabs)
    presence: Mutex<HashMap<String, HashMap<String, usize>>>,
    connections: Mutex<HashMap<String, usize>>,
    tickets: Mutex<HashMap<String, Ticket>>,
    next_id: AtomicU64,
    users: HashMap<String, String>, // name -> password hash
    dummy_hash: String,
    hasher: Argon2<'static>,
    secret: Vec<u8>,
    token_ttl: u64,
    ticket_ttl: Duration,
    max_connections: usize,
    message_rate: (u32, Duration),
    max_violations: u32,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn valid_room(room: &str) -> bool {
    (1..=32).contains(&room.len())
        && room
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Keep text readable and harmless: no control characters except newline and tab
fn clean_text(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect::<String>()
        .trim()
        .to_string()
}

fn chat(req: &Request) -> Result<&Chat> {
    req.state::<Chat>()
}

fn bearer_user(req: &Request) -> Result<String> {
    req.extensions
        .get::<Claims>()
        .and_then(|c| c.0["sub"].as_str().map(str::to_owned))
        .ok_or_else(|| Error::unauthorized("not logged in"))
}

// ---- REST: login, tickets, rooms ----

#[derive(Deserialize)]
struct Login {
    name: String,
    password: String,
}

async fn login(req: Request) -> Result<Response> {
    let c = chat(&req)?;
    let body: Login = req.json()?;
    // always do the hashing work, whether or not the name exists
    let (hash, known) = match c.users.get(&body.name) {
        Some(h) => (h.clone(), true),
        None => (c.dummy_hash.clone(), false),
    };
    let hasher = c.hasher.clone();
    let password = body.password;
    let ok = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|p| hasher.verify_password(password.as_bytes(), &p).is_ok())
            .unwrap_or(false)
    })
    .await
    .map_err(|_| Error::internal("hashing task failed"))?;
    if !(ok && known) {
        return Err(Error::unauthorized("invalid name or password"));
    }
    let token = jwt::encode(
        &json!({ "sub": body.name, "exp": jwt::expires_in(c.token_ttl) }),
        &c.secret,
    )?;
    Ok(Response::json(
        json!({ "token": token, "expires_in": c.token_ttl }),
    ))
}

/// One-time ticket for opening a WebSocket: browsers cannot send an
/// Authorization header there, and a bearer token in the URL would end up in
/// logs and history.
async fn issue_ticket(req: Request) -> Result<Response> {
    let c = chat(&req)?;
    let user = bearer_user(&req)?;
    let ticket = random_token(16)?;
    {
        let mut tickets = lock(&c.tickets);
        let now = Instant::now();
        tickets.retain(|_, t| t.expires > now); // expired tickets do not accumulate
        if tickets.len() >= 10_000 {
            return Err(Error::custom(503, "too many open tickets"));
        }
        tickets.insert(
            ticket.clone(),
            Ticket {
                user,
                expires: now + c.ticket_ttl,
            },
        );
    }
    Ok(Response::json(
        json!({ "ticket": ticket, "expires_in": c.ticket_ttl.as_secs() }),
    ))
}

async fn list_rooms(req: Request) -> Result<Response> {
    let c = chat(&req)?;
    let history = lock(&c.history);
    let rooms: Vec<Value> = c
        .hub
        .rooms()
        .into_iter()
        .map(|(name, members)| json!({ "room": name, "members": members, "messages": history.get(&name).map_or(0, VecDeque::len) }))
        .collect();
    Ok(Response::json(json!({ "rooms": rooms })))
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
}

async fn room_history(req: Request) -> Result<Response> {
    let c = chat(&req)?;
    let room = req.params.get("room")?.to_string();
    if !valid_room(&room) {
        return Err(Error::bad_request("invalid room name"));
    }
    let q: HistoryQuery = req.query.parse()?;
    let limit = q.limit.unwrap_or(50).clamp(1, HISTORY_LEN);
    let history = lock(&c.history);
    let all = history
        .get(&room)
        .ok_or_else(|| Error::not_found("no such room"))?;
    let skip = all.len().saturating_sub(limit);
    Ok(Response::json(
        json!({ "room": room, "messages": all.iter().skip(skip).collect::<Vec<_>>() }),
    ))
}

// ---- WebSocket ----

/// Runs before the upgrade: consumes the ticket and checks room and capacity,
/// so unauthenticated clients never get a socket.
async fn ws_gate(mut req: Request, next: Next) -> Result<Response> {
    let c = req.state_arc::<Chat>()?; // owned: `req` moves into `next.run`
    let ticket = req.query.get("ticket").unwrap_or("").to_string();
    let room = req.query.get("room").unwrap_or("general").to_string();

    if !valid_room(&room) {
        return Err(Error::bad_request(
            "room names use a-z, 0-9 and '-' (32 characters at most)",
        ));
    }
    let user = {
        let mut tickets = lock(&c.tickets);
        // single use: removed whether or not it turns out to be valid
        match tickets.remove(&ticket) {
            Some(t) if t.expires > Instant::now() => t.user,
            _ => return Err(Error::unauthorized("invalid or expired ticket")),
        }
    };
    if c.hub.members(&room) == 0 && c.hub.rooms().len() >= MAX_ROOMS {
        return Err(Error::custom(503, "too many rooms"));
    }
    {
        let mut conns = lock(&c.connections);
        let n = conns.entry(user.clone()).or_insert(0);
        if *n >= c.max_connections {
            return Err(Error::custom(
                429,
                "too many open connections for this user",
            ));
        }
        *n += 1;
    }
    req.extensions.insert(ChatUser(user.clone()));
    // the connection slot must be given back even if the upgrade fails
    match next.run(req).await {
        Ok(res) if res.status == http::StatusCode::SWITCHING_PROTOCOLS => Ok(res),
        other => {
            release_connection(&c, &user);
            other
        }
    }
}

fn release_connection(c: &Chat, user: &str) {
    let mut conns = lock(&c.connections);
    if let Some(n) = conns.get_mut(user) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            conns.remove(user);
        }
    }
}

/// Fixed-window limiter for one connection
struct Budget {
    limit: u32,
    window: Duration,
    started: Instant,
    used: u32,
}

impl Budget {
    fn allow(&mut self) -> bool {
        if self.started.elapsed() >= self.window {
            self.started = Instant::now();
            self.used = 0;
        }
        self.used += 1;
        self.used <= self.limit
    }
}

async fn session(req: Request, mut socket: WebSocket) {
    let Ok(c) = chat(&req) else { return };
    let Some(ChatUser(user)) = req.extensions.get::<ChatUser>().cloned() else {
        return;
    };
    let room = req.query.get("room").unwrap_or("general").to_string();
    let shutdown = req.shutdown_signal();

    // subscribe first, then read the history: nothing falls into the gap
    let mut sub = c.hub.subscribe(&room);
    let (history, last_in_history) = {
        let h = lock(&c.history);
        let list: Vec<StoredMessage> = h
            .get(&room)
            .map(|d| d.iter().cloned().collect())
            .unwrap_or_default();
        let last = list.last().map_or(0, |m| m.id);
        (list, last)
    };
    let users = {
        let mut p = lock(&c.presence);
        let members = p.entry(room.clone()).or_default();
        *members.entry(user.clone()).or_insert(0) += 1;
        let mut names: Vec<String> = members.keys().cloned().collect();
        names.sort();
        names
    };
    c.hub.publish(&room, Event::Joined { user: user.clone() });
    let _ = socket
        .send_json(&json!({ "type": "welcome", "room": room, "you": user, "users": users, "history": history }))
        .await;

    let mut budget = Budget {
        limit: c.message_rate.0,
        window: c.message_rate.1,
        started: Instant::now(),
        used: 0,
    };
    let mut violations = 0;

    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<Command>(&text) {
                        Ok(Command::Say { text }) => {
                            if !budget.allow() {
                                violations += 1;
                                let _ = socket.send_json(&json!({ "type": "error", "message": "slow down" })).await;
                                if violations >= c.max_violations {
                                    let _ = socket.close_with(1008, "rate limit exceeded").await;
                                    break;
                                }
                                continue;
                            }
                            let text = clean_text(&text);
                            let mut v = Validator::new();
                            v.len_between("text", &text, 1, MAX_TEXT_CHARS);
                            if v.finish().is_err() {
                                let _ = socket.send_json(&json!({ "type": "error", "message": format!("messages need 1 to {MAX_TEXT_CHARS} characters") })).await;
                                continue;
                            }
                            let id = c.next_id.fetch_add(1, Ordering::SeqCst) + 1;
                            let stored = StoredMessage { id, user: user.clone(), text: text.clone(), ts: now() };
                            {
                                let mut h = lock(&c.history);
                                let list = h.entry(room.clone()).or_default();
                                list.push_back(stored);
                                while list.len() > HISTORY_LEN {
                                    list.pop_front();
                                }
                            }
                            c.hub.publish(&room, Event::Message { id, user: user.clone(), text, ts: now() });
                        }
                        Ok(Command::Typing) => {
                            if budget.allow() {
                                c.hub.publish(&room, Event::Typing { user: user.clone() });
                            }
                        }
                        Err(_) => {
                            let _ = socket.send_json(&json!({ "type": "error", "message": "unknown command" })).await;
                        }
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    let _ = socket.send_json(&json!({ "type": "error", "message": "binary messages are not supported" })).await;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close)) | Some(Err(_)) | None => break,
            },
            delivery = sub.recv() => match delivery {
                Some(Delivery::Message(Event::Message { id, .. })) if id <= last_in_history => {}
                Some(Delivery::Message(Event::Typing { user: who })) if who == user => {}
                Some(Delivery::Message(event)) => {
                    if socket.send_json(&event).await.is_err() {
                        break;
                    }
                }
                Some(Delivery::Missed(n)) => {
                    let _ = socket.send_json(&json!({ "type": "missed", "count": n })).await;
                }
                None => break,
            },
            _ = shutdown.wait() => {
                let _ = socket.close_with(1001, "server shutting down").await;
                break;
            }
        }
    }

    // leave: presence first, then tell the others
    drop(sub);
    let still_here = {
        let mut p = lock(&c.presence);
        let mut still = false;
        if let Some(members) = p.get_mut(&room) {
            if let Some(n) = members.get_mut(&user) {
                *n = n.saturating_sub(1);
                still = *n > 0;
                if *n == 0 {
                    members.remove(&user);
                }
            }
            if members.is_empty() {
                p.remove(&room);
            }
        }
        still
    };
    release_connection(c, &user);
    if !still_here {
        c.hub.publish(&room, Event::Left { user });
    }
}

// ---- page ----

const PAGE: &str = r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>ullun chat</title>
<style>
 body{font:16px/1.4 system-ui;max-width:42rem;margin:1rem auto;padding:0 1rem}
 #log{border:1px solid #ccc;height:24rem;overflow:auto;padding:.5rem;list-style:none;margin:0}
 #log li{padding:.1rem 0}#log .sys{color:#666;font-style:italic}.who{font-weight:600}
 #bar{display:flex;gap:.5rem;margin-top:.5rem}#bar input{flex:1}
</style>
<body>
<h1>ullun chat</h1>
<form id="login"><input id="name" placeholder="name" autocomplete="username"> <input id="pw" type="password" placeholder="password" autocomplete="current-password"> <input id="room" value="general" size="12"> <button>Join</button></form>
<p id="status"></p>
<ul id="log" hidden></ul>
<form id="bar" hidden><input id="msg" autocomplete="off" placeholder="Say something"><button>Send</button></form>
<script src="/chat.js"></script>
</body></html>"#;

const CLIENT_JS: &str = r#"
const $ = id => document.getElementById(id);
let ws, typingAt = 0;
function line(cls, parts) {
  const li = document.createElement("li"); li.className = cls;
  for (const p of parts) { const s = document.createElement("span"); s.textContent = p.text; if (p.cls) s.className = p.cls; li.append(s); }
  $("log").append(li); $("log").scrollTop = $("log").scrollHeight;
}
function show(ev) {
  if (ev.type === "message") line("", [{text: ev.user, cls: "who"}, {text: ": " + ev.text}]);
  else if (ev.type === "joined") line("sys", [{text: ev.user + " joined"}]);
  else if (ev.type === "left") line("sys", [{text: ev.user + " left"}]);
  else if (ev.type === "typing") { $("status").textContent = ev.user + " is typing…"; setTimeout(() => $("status").textContent = "", 1500); }
  else if (ev.type === "missed") line("sys", [{text: "missed " + ev.count + " messages"}]);
  else if (ev.type === "error") line("sys", [{text: ev.message}]);
}
$("login").onsubmit = async e => {
  e.preventDefault();
  const post = (url, body, token) => fetch(url, {method: "POST", headers: {"content-type": "application/json", ...(token ? {authorization: "Bearer " + token} : {})}, body: body && JSON.stringify(body)});
  const l = await post("/api/login", {name: $("name").value, password: $("pw").value});
  if (!l.ok) { $("status").textContent = "login failed"; return; }
  const {token} = await l.json();
  const {ticket} = await (await post("/api/ws-ticket", null, token)).json();
  ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws?room=${encodeURIComponent($("room").value)}&ticket=${ticket}`);
  ws.onmessage = m => {
    const ev = JSON.parse(m.data);
    if (ev.type === "welcome") { $("log").hidden = $("bar").hidden = false; $("login").hidden = true; ev.history.forEach(h => show({type: "message", ...h})); line("sys", [{text: "in " + ev.room + ": " + ev.users.join(", ")}]); }
    else show(ev);
  };
  ws.onclose = () => line("sys", [{text: "disconnected"}]);
};
$("bar").onsubmit = e => { e.preventDefault(); if ($("msg").value) ws.send(JSON.stringify({type: "say", text: $("msg").value})); $("msg").value = ""; };
$("msg").oninput = () => { if (Date.now() - typingAt > 1500) { typingAt = Date.now(); ws.send(JSON.stringify({type: "typing"})); } };
"#;

pub fn build(config: Config) -> App {
    let params = if config.fast_hashing {
        Params::new(8, 1, 1, None).expect("params")
    } else {
        Params::default()
    };
    let hasher = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let hash = |pw: &str| {
        hasher
            .hash_password(pw.as_bytes())
            .expect("argon2")
            .to_string()
    };
    let users = config
        .users
        .iter()
        .map(|(n, p)| (n.clone(), hash(p)))
        .collect();
    let dummy_hash = hash("dummy password for timing");

    let chat = Chat {
        hub: Hub::new(64),
        history: Mutex::new(HashMap::new()),
        presence: Mutex::new(HashMap::new()),
        connections: Mutex::new(HashMap::new()),
        tickets: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(0),
        users,
        dummy_hash,
        hasher: hasher.clone(),
        secret: config.jwt_secret.clone(),
        token_ttl: config.token_ttl_secs,
        ticket_ttl: config.ticket_ttl,
        max_connections: config.max_connections_per_user,
        message_rate: config.message_rate,
        max_violations: config.max_violations.max(1),
    };
    let (login_limit, login_window) = config.login_rate;
    let secret = config.jwt_secret;
    let ws_options = config.ws;

    App::new()
        .state(chat)
        .max_body_size(Some(8 * 1024))
        .middleware(|req: Request, next: Next| async move {
            let res = match next.run(req).await {
                Ok(res) => res,
                Err(err) => Response::from_error(err),
            };
            Ok(res
                .with_header("x-content-type-options", "nosniff")
                .with_header("referrer-policy", "no-referrer")
                .with_header(
                    "content-security-policy",
                    "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'",
                ))
        })
        .get("/", |_req: Request| async { Ok(Response::html(PAGE)) })
        .get("/chat.js", |_req: Request| async {
            Ok(Response::ok().with_header("content-type", "application/javascript; charset=utf-8").with_body(CLIENT_JS))
        })
        .group("/api/login", move |g| g.middleware(RateLimiter::new(login_limit, login_window)).post("", login))
        .group("/api", move |g| {
            g.middleware(JwtAuth::new(secret))
                .post("/ws-ticket", issue_ticket)
                .get("/rooms", list_rooms)
                .get("/rooms/{room}/history", room_history)
        })
        .group("/ws", move |g| g.middleware(ws_gate).ws_with("", ws_options, session))
}
