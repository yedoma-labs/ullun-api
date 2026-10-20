//! Test helpers: start an `App` on an ephemeral port and talk to it over real HTTP

#![allow(dead_code)]

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use ullun::prelude::App;

/// A running app
#[derive(Clone, Copy)]
pub struct Server {
    pub addr: SocketAddr,
}

pub async fn spawn(app: App) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(app.serve(listener));
    Server { addr }
}

impl Server {
    pub fn request(&self, method: &str, path: &str) -> Call {
        Call {
            addr: self.addr,
            method: method.to_string(),
            path: path.to_string(),
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }

    pub fn get(&self, path: &str) -> Call {
        self.request("GET", path)
    }
    pub fn post(&self, path: &str) -> Call {
        self.request("POST", path)
    }
    pub fn put(&self, path: &str) -> Call {
        self.request("PUT", path)
    }
    pub fn delete(&self, path: &str) -> Call {
        self.request("DELETE", path)
    }
    pub fn patch(&self, path: &str) -> Call {
        self.request("PATCH", path)
    }
}

/// A form field for [`Call::multipart`]
pub enum Field {
    Text(&'static str, String),
    File {
        name: &'static str,
        filename: String,
        content_type: &'static str,
        data: Vec<u8>,
    },
}

impl Field {
    pub fn text(name: &'static str, value: &str) -> Self {
        Field::Text(name, value.to_string())
    }

    pub fn file(name: &'static str, filename: &str, data: impl Into<Vec<u8>>) -> Self {
        Field::File {
            name,
            filename: filename.to_string(),
            content_type: "application/octet-stream",
            data: data.into(),
        }
    }
}

/// Request under construction
pub struct Call {
    addr: SocketAddr,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Bytes,
}

impl Call {
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    pub fn json(mut self, value: &serde_json::Value) -> Self {
        self.headers
            .push(("content-type".into(), "application/json".into()));
        self.body = Bytes::from(serde_json::to_vec(value).unwrap());
        self
    }

    pub fn form(mut self, body: &str) -> Self {
        self.headers.push((
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ));
        self.body = Bytes::from(body.to_string());
        self
    }

    pub fn text(mut self, body: &str) -> Self {
        self.headers
            .push(("content-type".into(), "text/plain; charset=utf-8".into()));
        self.body = Bytes::from(body.to_string());
        self
    }

    /// `multipart/form-data` body, as a browser would send it
    pub fn multipart(mut self, fields: &[Field]) -> Self {
        let boundary = "----ullunTestBoundary7MA4YWxkTrZu0gW";
        let mut body = Vec::new();
        for field in fields {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match field {
                Field::Text(name, value) => {
                    body.extend_from_slice(
                        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                            .as_bytes(),
                    );
                    body.extend_from_slice(value.as_bytes());
                }
                Field::File {
                    name,
                    filename,
                    content_type,
                    data,
                } => {
                    body.extend_from_slice(
                        format!(
                            "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                    body.extend_from_slice(data);
                }
            }
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        self.headers.push((
            "content-type".into(),
            format!("multipart/form-data; boundary={boundary}"),
        ));
        self.body = Bytes::from(body);
        self
    }

    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = Bytes::from(body);
        self
    }

    pub async fn send(self) -> Reply {
        let stream = TcpStream::connect(self.addr).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);

        let mut builder = hyper::Request::builder()
            .method(self.method.as_str())
            .uri(&self.path)
            .header("host", "localhost");
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        let res = sender
            .send_request(builder.body(Full::new(self.body)).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }
}

/// Response of a test call
pub struct Reply {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}
