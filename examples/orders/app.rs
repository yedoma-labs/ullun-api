//! Checkout API: application logic, shared by `main.rs` and the tests

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use ullun::cors::Cors;
use ullun::idempotency::Idempotency;
use ullun::limits::{ConcurrencyLimit, Timeout};
use ullun::prelude::*;
use ullun::security::constant_time_eq;
use ullun::security_headers::{Csp, SecurityHeaders};
use ullun::validate::Validator;

const MAX_LINES: usize = 20;
const MAX_QUANTITY: u32 = 50;

pub struct Config {
    pub shop_origin: String,
    /// bearer token -> customer name
    pub tokens: Vec<(String, String)>,
    /// Requests handled at once before the server answers 503
    pub max_in_flight: usize,
    pub request_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        let tokens = std::env::var("TOKENS")
            .unwrap_or_else(|_| "demo-token:alice".into())
            .split(',')
            .filter_map(|pair| pair.split_once(':'))
            .map(|(t, c)| (t.trim().to_string(), c.trim().to_string()))
            .collect();
        Self {
            shop_origin: std::env::var("SHOP_ORIGIN")
                .unwrap_or_else(|_| "http://localhost:5173".into()),
            tokens,
            max_in_flight: 256,
            request_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Serialize)]
struct Product {
    sku: &'static str,
    name: &'static str,
    price_cents: u64,
}

const CATALOGUE: &[Product] = &[
    Product {
        sku: "mug",
        name: "Coffee mug",
        price_cents: 1200,
    },
    Product {
        sku: "shirt",
        name: "T-shirt",
        price_cents: 2500,
    },
    Product {
        sku: "sticker",
        name: "Sticker",
        price_cents: 300,
    },
];

#[derive(Deserialize)]
struct NewOrder {
    items: Vec<Line>,
}

#[derive(Deserialize, Clone, Serialize)]
struct Line {
    sku: String,
    quantity: u32,
}

#[derive(Clone, Serialize)]
struct Order {
    id: u64,
    customer: String,
    lines: Vec<Line>,
    total_cents: u64,
}

struct Shop {
    stock: Mutex<HashMap<&'static str, u32>>,
    orders: Mutex<Vec<Order>>,
    tokens: Vec<(String, String)>,
}

fn customer(req: &Request) -> Result<String> {
    let shop = req.state::<Shop>()?;
    let token = req
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| Error::unauthorized("Bearer token required"))?;
    shop.tokens
        .iter()
        .find(|(t, _)| constant_time_eq(t.as_bytes(), token.as_bytes()))
        .map(|(_, c)| c.clone())
        .ok_or_else(|| Error::unauthorized("Unknown token"))
}

async fn products(req: Request) -> Result<Response> {
    let shop = req.state::<Shop>()?;
    let stock = shop.stock.lock().unwrap();
    let list: Vec<_> = CATALOGUE
        .iter()
        .map(|p| {
            serde_json::json!({
                "sku": p.sku, "name": p.name, "price_cents": p.price_cents,
                "in_stock": stock.get(p.sku).copied().unwrap_or(0),
            })
        })
        .collect();
    Ok(Response::json(list))
}

async fn place_order(req: Request) -> Result<Response> {
    let who = customer(&req)?;
    let body: NewOrder = req.json()?;
    let mut v = Validator::new();
    v.check(!body.items.is_empty(), "items", "at least one item");
    v.check(body.items.len() <= MAX_LINES, "items", "too many lines");
    for (i, line) in body.items.iter().enumerate() {
        let field = format!("items[{i}]");
        v.check(
            CATALOGUE.iter().any(|p| p.sku == line.sku),
            &field,
            "unknown sku",
        );
        v.check(
            (1..=MAX_QUANTITY).contains(&line.quantity),
            &field,
            format!("quantity must be 1-{MAX_QUANTITY}"),
        );
    }
    v.finish()?;

    let shop = req.state::<Shop>()?;
    // check and reserve under one lock: concurrent orders cannot oversell
    let mut stock = shop.stock.lock().unwrap();
    let mut wanted: HashMap<&str, u32> = HashMap::new();
    for line in &body.items {
        *wanted.entry(line.sku.as_str()).or_default() += line.quantity;
    }
    for (sku, qty) in &wanted {
        let available = CATALOGUE
            .iter()
            .find(|p| p.sku == *sku)
            .and_then(|p| stock.get(p.sku))
            .copied()
            .unwrap_or(0);
        if available < *qty {
            return Err(Error::custom(409, format!("Not enough stock for {sku}")));
        }
    }
    let mut total = 0;
    for line in &body.items {
        let product = CATALOGUE.iter().find(|p| p.sku == line.sku).unwrap();
        total += product.price_cents * u64::from(line.quantity);
        *stock.get_mut(product.sku).unwrap() -= line.quantity;
    }
    drop(stock);

    let mut orders = shop.orders.lock().unwrap();
    let order = Order {
        id: orders.len() as u64 + 1,
        customer: who,
        lines: body.items,
        total_cents: total,
    };
    orders.push(order.clone());
    Ok(Response::json(&order)
        .with_status(http::StatusCode::CREATED)
        .with_header("location", format!("/api/orders/{}", order.id)))
}

async fn get_order(req: Request) -> Result<Response> {
    let who = customer(&req)?;
    let id: u64 = req.params.parse("id")?;
    let shop = req.state::<Shop>()?;
    let orders = shop.orders.lock().unwrap();
    // someone else's order looks like a missing one
    match orders.iter().find(|o| o.id == id && o.customer == who) {
        Some(order) => Ok(Response::json(order)),
        None => Err(Error::not_found("Order not found")),
    }
}

pub fn build(config: Config) -> App {
    let stock: HashMap<&'static str, u32> = [("mug", 5), ("shirt", 3), ("sticker", 100)]
        .into_iter()
        .collect();
    let shop = Shop {
        stock: Mutex::new(stock),
        orders: Mutex::new(Vec::new()),
        tokens: config.tokens,
    };

    App::new()
        .state(shop)
        .header_read_timeout(Duration::from_secs(10))
        .max_body_size(Some(16 * 1024))
        .middleware(SecurityHeaders::new().csp(Csp::api()))
        .middleware(
            Cors::new()
                .allow_origin(config.shop_origin)
                .allow_credentials()
                .expose_headers(["location", "idempotency-replayed"]),
        )
        .middleware(Timeout::new(config.request_timeout))
        .middleware(ConcurrencyLimit::new(config.max_in_flight))
        .get("/api/products", products)
        .group("/api/orders", |g| {
            g.middleware(Idempotency::new().required())
                .post("", place_order)
        })
        .get("/api/orders/{id}", get_order)
}
