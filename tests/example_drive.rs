//! The file drive example, tested over real HTTP with real files on disk

mod common;

#[allow(dead_code)]
#[path = "../examples/drive/app.rs"]
mod drive;

use common::{spawn, Field, Server};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const ALICE: &str = "alice-test-key-0123456789";
const BOB: &str = "bob-test-key-0123456789ab";

struct Fixture {
    server: Server,
    dir: PathBuf,
    clock: Arc<AtomicU64>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn config(dir: &Path, clock: Arc<AtomicU64>) -> drive::Config {
    drive::Config {
        users: vec![
            drive::UserConfig {
                name: "alice".into(),
                api_key: ALICE.into(),
                quota_bytes: 1_000_000,
            },
            drive::UserConfig {
                name: "bob".into(),
                api_key: BOB.into(),
                quota_bytes: 1_000,
            },
        ],
        data_dir: dir.to_path_buf(),
        base_url: "https://drive.example".into(),
        max_file_bytes: 300_000,
        max_files_per_upload: 3,
        upload_rate_limit: (1000, Duration::from_secs(60)),
        cleanup_every: Duration::from_secs(3600),
        clock: Arc::new(move || clock.load(Ordering::SeqCst)),
    }
}

async fn fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("ullun-drive-{}-{}", name, std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let clock = Arc::new(AtomicU64::new(10_000));
    let app = drive::build(config(&dir, clock.clone())).await.unwrap();
    Fixture {
        server: spawn(app).await,
        dir,
        clock,
    }
}

fn blobs(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join("blobs"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 253) as u8).collect()
}

impl Fixture {
    async fn upload(&self, key: &str, fields: &[Field]) -> common::Reply {
        self.server
            .post("/api/upload")
            .header("x-api-key", key)
            .multipart(fields)
            .send()
            .await
    }

    /// Upload one file, return its id
    async fn put(&self, key: &str, filename: &str, bytes: Vec<u8>) -> String {
        let res = self
            .upload(key, &[Field::file("file", filename, bytes)])
            .await;
        assert_eq!(res.status, 201, "{}", res.text());
        res.json()["files"][0]["id"].as_str().unwrap().to_string()
    }

    async fn share(&self, key: &str, id: &str, body: Value) -> common::Reply {
        self.server
            .post(&format!("/api/files/{id}/share"))
            .header("x-api-key", key)
            .json(&body)
            .send()
            .await
    }
}

#[tokio::test]
async fn upload_store_list_and_download() {
    let f = fixture("basic").await;
    let content = data(120_000);

    let res = f
        .upload(
            ALICE,
            &[
                Field::text("description", "  quarterly numbers "),
                Field::file("report", "Q1 report.csv", content.clone()),
                Field::file("notes", "notes.txt", b"hello".to_vec()),
            ],
        )
        .await;
    assert_eq!(res.status, 201, "{}", res.text());
    let files = res.json()["files"].as_array().unwrap().clone();
    assert_eq!(files.len(), 2);
    assert_eq!(
        (files[0]["name"].as_str(), files[0]["size"].as_u64()),
        (Some("Q1 report.csv"), Some(120_000))
    );
    assert_eq!(files[0]["content_type"], "text/csv; charset=utf-8");
    assert_eq!(files[0]["description"], "quarterly numbers");
    assert_eq!(files[1]["content_type"], "text/plain; charset=utf-8");

    // stored under generated names, nothing else on disk
    let id = files[0]["id"].as_str().unwrap();
    assert_eq!(id.len(), 16);
    assert_eq!(blobs(&f.dir).len(), 2);
    assert!(blobs(&f.dir).contains(&id.to_string()));
    assert_eq!(
        std::fs::read(f.dir.join("blobs").join(id)).unwrap(),
        content
    );

    // download (csv is not inline-safe: attachment, sandboxed)
    let dl = f
        .server
        .get(&format!("/api/files/{id}/content"))
        .header("x-api-key", ALICE)
        .send()
        .await;
    assert_eq!(dl.status, 200);
    assert_eq!(dl.body, content);
    assert!(dl
        .header("content-disposition")
        .starts_with("attachment; filename=\"Q1 report.csv\""));
    assert_eq!(dl.header("x-content-type-options"), "nosniff");
    assert!(dl.header("content-security-policy").contains("sandbox"));

    // list, filter, details, usage
    let list = f
        .server
        .get("/api/files")
        .header("x-api-key", ALICE)
        .send()
        .await
        .json();
    assert_eq!(list["total"], 2);
    let filtered = f
        .server
        .get("/api/files?q=REPORT")
        .header("x-api-key", ALICE)
        .send()
        .await
        .json();
    assert_eq!(filtered["total"], 1);
    let info = f
        .server
        .get(&format!("/api/files/{id}"))
        .header("x-api-key", ALICE)
        .send()
        .await
        .json();
    assert_eq!(
        info["download_url"],
        format!("https://drive.example/api/files/{id}/content")
    );
    let usage = f
        .server
        .get("/api/usage")
        .header("x-api-key", ALICE)
        .send()
        .await
        .json();
    assert_eq!(
        (usage["used_bytes"].as_u64(), usage["files"].as_u64()),
        (Some(120_005), Some(2))
    );
}

#[tokio::test]
async fn downloads_support_ranges_and_conditional_requests() {
    let f = fixture("ranges").await;
    let content = data(200_000);
    let id = f.put(ALICE, "movie.mp4", content.clone()).await;
    let url = format!("/api/files/{id}/content");

    let part = f
        .server
        .get(&url)
        .header("x-api-key", ALICE)
        .header("range", "bytes=100000-100999")
        .send()
        .await;
    assert_eq!(part.status, 206);
    assert_eq!(part.body, content[100_000..101_000]);
    // mp4 is safe to play inline
    assert_eq!(part.header("content-disposition"), "");
    assert_eq!(part.header("content-type"), "video/mp4");

    let full = f.server.get(&url).header("x-api-key", ALICE).send().await;
    let cached = f
        .server
        .get(&url)
        .header("x-api-key", ALICE)
        .header("if-none-match", full.header("etag"))
        .send()
        .await;
    assert_eq!(cached.status, 304);
    // ?download=1 forces a download even for inline-safe types
    let forced = f
        .server
        .get(&format!("{url}?download=1"))
        .header("x-api-key", ALICE)
        .send()
        .await;
    assert!(forced
        .header("content-disposition")
        .starts_with("attachment;"));
}

#[tokio::test]
async fn hostile_file_names_never_reach_the_file_system() {
    let f = fixture("names").await;
    let before = std::fs::read_dir(std::env::temp_dir()).unwrap().count();

    let res = f
        .upload(
            ALICE,
            &[
                Field::file("a", "../../../../tmp/pwned.txt", b"1".to_vec()),
                Field::file("b", "C:\\Windows\\System32\\evil.exe", b"2".to_vec()),
                Field::file("c", "..", b"3".to_vec()),
            ],
        )
        .await;
    assert_eq!(res.status, 201, "{}", res.text());
    let names: Vec<String> = res.json()["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["pwned.txt", "evil.exe", "file"]);

    // three blobs with generated ids, nothing escaped the blob directory
    let stored = blobs(&f.dir);
    assert_eq!(stored.len(), 3);
    assert!(stored
        .iter()
        .all(|n| n.len() == 16 && n.chars().all(|c| c.is_ascii_alphanumeric())));
    assert!(!std::path::Path::new("/tmp/pwned.txt").exists());
    assert!(std::fs::read_dir(std::env::temp_dir()).unwrap().count() <= before + 2);
}

#[tokio::test]
async fn risky_types_are_downloads_with_a_sandbox() {
    let f = fixture("risky").await;
    for (name, mime) in [
        ("page.html", "text/html; charset=utf-8"),
        ("logo.svg", "image/svg+xml"),
        ("app.js", "application/javascript; charset=utf-8"),
        ("doc.xml", "application/xml"),
    ] {
        let id = f
            .put(ALICE, name, b"<script>alert(1)</script>".to_vec())
            .await;
        let dl = f
            .server
            .get(&format!("/api/files/{id}/content"))
            .header("x-api-key", ALICE)
            .send()
            .await;
        assert_eq!(dl.header("content-type"), mime, "{name}");
        assert!(
            dl.header("content-disposition").starts_with("attachment;"),
            "{name}"
        );
        assert_eq!(dl.header("x-content-type-options"), "nosniff");
        assert!(dl
            .header("content-security-policy")
            .contains("default-src 'none'"));
    }
    // a lying client content type changes nothing: the name decides
    let res = f
        .upload(
            ALICE,
            &[Field::File {
                name: "f",
                filename: "evil.html".into(),
                content_type: "image/png",
                data: b"<b>".to_vec(),
            }],
        )
        .await;
    assert_eq!(
        res.json()["files"][0]["content_type"],
        "text/html; charset=utf-8"
    );
}

#[tokio::test]
async fn upload_validation() {
    let f = fixture("validate").await;

    assert_eq!(
        f.server
            .post("/api/upload")
            .header("x-api-key", ALICE)
            .json(&json!({}))
            .send()
            .await
            .status,
        415
    );
    assert_eq!(
        f.upload(ALICE, &[Field::text("description", "no file")])
            .await
            .status,
        400
    );
    assert_eq!(f.upload(ALICE, &[]).await.status, 400);
    let empty = f
        .upload(ALICE, &[Field::file("f", "empty.txt", Vec::new())])
        .await;
    assert_eq!(empty.status, 422);
    assert!(empty.json()["fields"]["files[0]"].is_array());
    let long_description = f
        .upload(
            ALICE,
            &[
                Field::text("description", &"d".repeat(501)),
                Field::file("f", "a.txt", b"x".to_vec()),
            ],
        )
        .await;
    assert_eq!(long_description.status, 422);

    let big = f
        .upload(ALICE, &[Field::file("f", "big.bin", data(300_001))])
        .await;
    assert_eq!(big.status, 413);
    assert!(big.text().contains("big.bin"));
    let many: Vec<Field> = (0..4)
        .map(|i| Field::file("f", &format!("{i}.txt"), b"x".to_vec()))
        .collect();
    assert_eq!(f.upload(ALICE, &many).await.status, 413);
    // exactly at the limits works
    assert_eq!(
        f.upload(ALICE, &[Field::file("f", "max.bin", data(300_000))])
            .await
            .status,
        201
    );

    // truncated multipart body
    let broken = f
        .server
        .post("/api/upload")
        .header("x-api-key", ALICE)
        .header("content-type", "multipart/form-data; boundary=zz")
        .body(
            b"--zz\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a\"\r\n\r\ndata"
                .to_vec(),
        )
        .send()
        .await;
    assert_eq!(broken.status, 400);

    // only the one valid upload left something behind
    assert_eq!(blobs(&f.dir).len(), 1);
}

#[tokio::test]
async fn the_body_limit_for_uploads_is_separate_from_the_api_limit() {
    let f = fixture("limits").await;
    // JSON API calls stay small
    let big_json = f
        .server
        .post("/api/files/x/share")
        .header("x-api-key", ALICE)
        .header("content-type", "application/json")
        .body(vec![b' '; 17 * 1024])
        .send()
        .await;
    assert_eq!(big_json.status, 413);
    // while an upload of the same size is fine
    assert_eq!(
        f.upload(ALICE, &[Field::file("f", "ok.bin", data(50_000))])
            .await
            .status,
        201
    );
    // far beyond the whole-request budget
    let huge = f
        .server
        .post("/api/upload")
        .header("x-api-key", ALICE)
        .multipart(&[Field::file("f", "huge.bin", data(1_300_000))])
        .send()
        .await;
    assert_eq!(huge.status, 413);
}

#[tokio::test]
async fn quota_is_all_or_nothing_and_deleting_frees_it() {
    let f = fixture("quota").await;

    assert_eq!(
        f.upload(BOB, &[Field::file("f", "a.bin", data(600))])
            .await
            .status,
        201
    );
    // 600 + 500 + 100 > 1000: neither file of the second upload is stored
    let over = f
        .upload(
            BOB,
            &[
                Field::file("f", "b.bin", data(500)),
                Field::file("f", "c.bin", data(100)),
            ],
        )
        .await;
    assert_eq!(over.status, 507);
    assert!(over.text().contains("quota"));
    assert_eq!(blobs(&f.dir).len(), 1);
    assert_eq!(
        f.server
            .get("/api/usage")
            .header("x-api-key", BOB)
            .send()
            .await
            .json()["used_bytes"],
        600
    );

    // quotas are per user
    assert_eq!(
        f.upload(ALICE, &[Field::file("f", "big.bin", data(200_000))])
            .await
            .status,
        201
    );

    let id = f
        .server
        .get("/api/files")
        .header("x-api-key", BOB)
        .send()
        .await
        .json()["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        f.server
            .delete(&format!("/api/files/{id}"))
            .header("x-api-key", BOB)
            .send()
            .await
            .status,
        204
    );
    assert!(!f.dir.join("blobs").join(&id).exists());
    assert_eq!(
        f.upload(BOB, &[Field::file("f", "b.bin", data(900))])
            .await
            .status,
        201
    );
}

#[tokio::test]
async fn users_only_see_their_own_files() {
    let f = fixture("isolation").await;
    let id = f.put(ALICE, "private.txt", b"alice only".to_vec()).await;

    for (method, path) in [
        ("GET", format!("/api/files/{id}")),
        ("GET", format!("/api/files/{id}/content")),
        ("DELETE", format!("/api/files/{id}")),
        ("POST", format!("/api/files/{id}/share")),
    ] {
        let bob = f
            .server
            .request(method, &path)
            .header("x-api-key", BOB)
            .send()
            .await;
        assert_eq!(bob.status, 404, "{method} {path}");
    }
    assert_eq!(
        f.server
            .get("/api/files")
            .header("x-api-key", BOB)
            .send()
            .await
            .json()["total"],
        0
    );

    // auth is required everywhere under /api
    for (method, path) in [
        ("GET", "/api/files"),
        ("GET", "/api/usage"),
        ("POST", "/api/upload"),
        ("GET", &format!("/api/files/{id}/content")[..]),
    ] {
        assert_eq!(
            f.server.request(method, path).send().await.status,
            401,
            "{path}"
        );
        assert_eq!(
            f.server
                .request(method, path)
                .header("x-api-key", "nope")
                .send()
                .await
                .status,
            401
        );
        assert_eq!(
            f.server
                .request(method, path)
                .header("x-api-key", &ALICE[..20])
                .send()
                .await
                .status,
            401
        );
    }
    // still there for alice
    assert_eq!(
        f.server
            .get(&format!("/api/files/{id}"))
            .header("x-api-key", ALICE)
            .send()
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn share_links_expire_and_count_downloads() {
    let f = fixture("share").await;
    let content = data(150_000);
    let id = f.put(ALICE, "photo.png", content.clone()).await;

    for (body, field) in [
        (json!({ "ttl_secs": 0 }), "ttl_secs"),
        (json!({ "ttl_secs": 31 * 24 * 3600 }), "ttl_secs"),
        (json!({ "max_downloads": 0 }), "max_downloads"),
    ] {
        let res = f.share(ALICE, &id, body).await;
        assert_eq!(res.status, 422);
        assert!(res.json()["fields"][field].is_array());
    }

    let share = f
        .share(ALICE, &id, json!({ "ttl_secs": 100, "max_downloads": 2 }))
        .await;
    assert_eq!(share.status, 201);
    let share = share.json();
    let token = share["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 48);
    assert_eq!(share["url"], format!("https://drive.example/s/{token}"));
    assert_eq!(share["expires_at"], 10_100);
    let path = format!("/s/{token}");

    // no API key needed; inline-safe image, not attachment
    let first = f.server.get(&path).send().await;
    assert_eq!((first.status, first.body.len()), (200, 150_000));
    assert_eq!(first.header("content-type"), "image/png");
    assert_eq!(first.header("content-disposition"), "");

    // HEAD, 304, and a resumed range do not use up the budget
    assert_eq!(f.server.request("HEAD", &path).send().await.status, 200);
    assert_eq!(
        f.server
            .get(&path)
            .header("if-none-match", first.header("etag"))
            .send()
            .await
            .status,
        304
    );
    let resumed = f
        .server
        .get(&path)
        .header("range", "bytes=100000-")
        .send()
        .await;
    assert_eq!(
        (resumed.status, resumed.body),
        (206, content[100_000..].to_vec())
    );

    // a second full download is the last one
    assert_eq!(f.server.get(&path).send().await.status, 200);
    let used_up = f.server.get(&path).send().await;
    assert_eq!(used_up.status, 410);
    assert_eq!(f.server.get(&path).send().await.status, 404); // and the link is gone

    // time limit
    let timed = f.share(ALICE, &id, json!({ "ttl_secs": 50 })).await.json();
    let timed_path = format!("/s/{}", timed["token"].as_str().unwrap());
    f.clock.store(10_049, Ordering::SeqCst);
    assert_eq!(f.server.get(&timed_path).send().await.status, 200);
    f.clock.store(10_050, Ordering::SeqCst);
    assert_eq!(f.server.get(&timed_path).send().await.status, 410);

    assert_eq!(f.server.get("/s/not-a-token").send().await.status, 404);
}

#[tokio::test]
async fn revoking_and_deleting_invalidate_shares() {
    let f = fixture("revoke").await;
    let id = f.put(ALICE, "doc.pdf", b"%PDF-1.4".to_vec()).await;

    let one = f.share(ALICE, &id, json!({})).await.json();
    let (t1, p1) = (
        one["token"].as_str().unwrap().to_string(),
        format!("/s/{}", one["token"].as_str().unwrap()),
    );
    assert_eq!(f.server.get(&p1).send().await.status, 200);

    // someone else cannot revoke it, the owner can
    assert_eq!(
        f.server
            .delete(&format!("/api/shares/{t1}"))
            .header("x-api-key", BOB)
            .send()
            .await
            .status,
        404
    );
    assert_eq!(f.server.get(&p1).send().await.status, 200);
    assert_eq!(
        f.server
            .delete(&format!("/api/shares/{t1}"))
            .header("x-api-key", ALICE)
            .send()
            .await
            .status,
        204
    );
    assert_eq!(f.server.get(&p1).send().await.status, 404);

    // deleting the file kills its links
    let two = f.share(ALICE, &id, json!({})).await.json();
    let p2 = format!("/s/{}", two["token"].as_str().unwrap());
    f.server
        .delete(&format!("/api/files/{id}"))
        .header("x-api-key", ALICE)
        .send()
        .await;
    assert_eq!(f.server.get(&p2).send().await.status, 404);
}

#[tokio::test]
async fn state_survives_a_restart_and_missing_blobs_are_forgotten() {
    let dir = std::env::temp_dir().join(format!("ullun-drive-restart-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let clock = Arc::new(AtomicU64::new(10_000));

    let first = spawn(drive::build(config(&dir, clock.clone())).await.unwrap()).await;
    let up = |name: &'static str, n: usize| {
        let call = first
            .post("/api/upload")
            .header("x-api-key", ALICE)
            .multipart(&[Field::file("f", name, data(n))]);
        async move {
            call.send().await.json()["files"][0]["id"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let keep = up("keep.txt", 10).await;
    let lose = up("lose.txt", 10).await;
    let token = first
        .post(&format!("/api/files/{keep}/share"))
        .header("x-api-key", ALICE)
        .json(&json!({}))
        .send()
        .await
        .json()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let orphan_token = first
        .post(&format!("/api/files/{lose}/share"))
        .header("x-api-key", ALICE)
        .json(&json!({}))
        .send()
        .await
        .json()["token"]
        .as_str()
        .unwrap()
        .to_string();

    // "restart" after one blob disappeared from disk
    std::fs::remove_file(dir.join("blobs").join(&lose)).unwrap();
    let second = spawn(drive::build(config(&dir, clock)).await.unwrap()).await;

    assert_eq!(second.get(&format!("/s/{token}")).send().await.status, 200);
    assert_eq!(
        second
            .get(&format!("/s/{orphan_token}"))
            .send()
            .await
            .status,
        404
    );
    let list = second
        .get("/api/files")
        .header("x-api-key", ALICE)
        .send()
        .await
        .json();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["id"], keep);
    assert!(!dir.join("index.tmp").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn upload_rate_limit_is_per_user() {
    let dir = std::env::temp_dir().join(format!("ullun-drive-rate-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let clock = Arc::new(AtomicU64::new(1));
    let mut cfg = config(&dir, clock);
    cfg.upload_rate_limit = (2, Duration::from_secs(60));
    let s = spawn(drive::build(cfg).await.unwrap()).await;

    let up = |key: &'static str| {
        let call = s
            .post("/api/upload")
            .header("x-api-key", key)
            .multipart(&[Field::file("f", "a.txt", b"x".to_vec())]);
        async move { call.send().await.status }
    };
    assert_eq!((up(ALICE).await, up(ALICE).await), (201, 201));
    assert_eq!(up(ALICE).await, 429);
    assert_eq!(up(BOB).await, 201);
    // unauthenticated calls cannot burn anyone's budget, and reads are not limited
    assert_eq!(s.post("/api/upload").send().await.status, 401);
    assert_eq!(
        s.get("/api/files")
            .header("x-api-key", ALICE)
            .send()
            .await
            .status,
        200
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn the_cleanup_task_removes_expired_shares_from_the_index() {
    let dir = std::env::temp_dir().join(format!("ullun-drive-clean-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let clock = Arc::new(AtomicU64::new(1_000));
    let mut cfg = config(&dir, clock.clone());
    cfg.cleanup_every = Duration::from_millis(20);
    let s = spawn(drive::build(cfg).await.unwrap()).await;

    let id = s
        .post("/api/upload")
        .header("x-api-key", ALICE)
        .multipart(&[Field::file("f", "a.txt", b"x".to_vec())])
        .send()
        .await
        .json()["files"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    s.post(&format!("/api/files/{id}/share"))
        .header("x-api-key", ALICE)
        .json(&json!({ "ttl_secs": 10 }))
        .send()
        .await;
    let count_shares = || {
        std::fs::read(dir.join("index.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .map(|v| v["shares"].as_object().map(|o| o.len()).unwrap_or(0))
            .unwrap_or(usize::MAX)
    };
    assert_eq!(count_shares(), 1);

    clock.store(2_000, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while count_shares() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "expired share was never cleaned up"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn openapi_documents_the_api() {
    let f = fixture("openapi").await;
    let doc = f.server.get("/openapi.json").send().await.json();
    for path in [
        "/api/upload",
        "/api/files",
        "/api/files/{id}/content",
        "/api/files/{id}/share",
        "/api/shares/{token}",
        "/api/usage",
        "/s/{token}",
    ] {
        assert!(doc["paths"][path].is_object(), "{path}");
    }
    assert_eq!(
        doc["paths"]["/api/upload"]["post"]["responses"]["507"]["description"],
        "Quota exceeded"
    );
    assert!(doc["paths"]["/s/{token}"]["get"].get("security").is_none());
    assert_eq!(f.server.get("/docs").send().await.status, 200);
}
