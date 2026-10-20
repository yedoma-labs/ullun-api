//! File serving: ranges, conditional requests, streaming and the static handler

mod common;

use common::{spawn, Server};
use std::path::PathBuf;
use ullun::files::{http_date, serve_file, FileOptions};
use ullun::prelude::*;

const BIG: usize = 200_000; // above the in-memory threshold, so it is streamed

struct Fixture {
    dir: PathBuf,
    server: Server,
    big: Vec<u8>,
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

async fn fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("ullun-files-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join("small.txt"), "hello world").unwrap();
    std::fs::write(dir.join("big.bin"), pattern(BIG)).unwrap();
    std::fs::write(dir.join("my file.txt"), "spaces").unwrap();
    std::fs::write(dir.join(".env"), "SECRET=1").unwrap();
    std::fs::write(dir.join(".git/config"), "[core]").unwrap();
    std::fs::write(dir.join("sub/index.html"), "<h1>sub</h1>").unwrap();

    let (small, big_path) = (dir.join("small.txt"), dir.join("big.bin"));
    let app = App::new()
        .get("/dl/small", move |req: Request| {
            let p = small.clone();
            async move {
                serve_file(
                    &req,
                    p,
                    &FileOptions::new()
                        .attachment("Übersicht.txt")
                        .cache_control("no-cache"),
                )
                .await
            }
        })
        .get("/dl/big", move |req: Request| {
            let p = big_path.clone();
            async move {
                serve_file(
                    &req,
                    p,
                    &FileOptions::new().content_type("application/x-custom"),
                )
                .await
            }
        })
        .get("/dl/missing", |req: Request| async move {
            serve_file(&req, "/definitely/not/here.bin", &FileOptions::new()).await
        })
        .serve_static("/static", dir.to_str().unwrap());
    Fixture {
        server: spawn(app).await,
        dir,
        big: pattern(BIG),
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn mtime(path: &PathBuf) -> u64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
async fn small_and_large_files_are_served_with_metadata() {
    let f = fixture("serve").await;

    let small = f.server.get("/dl/small").send().await;
    assert_eq!(small.status, 200);
    assert_eq!(small.text(), "hello world");
    assert_eq!(small.header("content-type"), "text/plain; charset=utf-8");
    assert_eq!(small.header("accept-ranges"), "bytes");
    assert_eq!(small.header("cache-control"), "no-cache");
    assert_eq!(
        small.header("last-modified"),
        http_date(mtime(&f.dir.join("small.txt")))
    );
    assert!(small.header("etag").starts_with("W/\""));
    assert_eq!(
        small.header("content-disposition"),
        "attachment; filename=\"_bersicht.txt\"; filename*=UTF-8''%C3%9Cbersicht.txt"
    );

    // streamed in chunks with a known length
    let big = f.server.get("/dl/big").send().await;
    assert_eq!(big.status, 200);
    assert_eq!(big.header("content-length"), BIG.to_string());
    assert_eq!(big.header("content-type"), "application/x-custom");
    assert_eq!(big.body, f.big);

    // HEAD: same headers, no body, same length
    let head = f.server.request("HEAD", "/dl/big").send().await;
    assert_eq!((head.status, head.body.len()), (200, 0));
    assert_eq!(head.header("content-length"), BIG.to_string());
    assert_eq!(head.header("etag"), big.header("etag"));

    let missing = f.server.get("/dl/missing").send().await;
    assert_eq!(missing.status, 404);
}

#[tokio::test]
async fn byte_ranges_on_buffered_and_streamed_files() {
    let f = fixture("range").await;
    let range = |path: &'static str, spec: &'static str| {
        let s = f.server;
        async move { s.get(path).header("range", spec).send().await }
    };

    let first = range("/dl/big", "bytes=0-99").await;
    assert_eq!(first.status, 206);
    assert_eq!(first.header("content-range"), format!("bytes 0-99/{BIG}"));
    assert_eq!(first.header("content-length"), "100");
    assert_eq!(first.body, f.big[..100]);

    // a range spanning several 64 KiB chunks
    let middle = range("/dl/big", "bytes=60000-140000").await;
    assert_eq!(middle.status, 206);
    assert_eq!(middle.body, f.big[60_000..=140_000]);

    let tail = range("/dl/big", "bytes=-1000").await;
    assert_eq!(tail.body, f.big[BIG - 1000..]);
    assert_eq!(
        tail.header("content-range"),
        format!("bytes {}-{}/{}", BIG - 1000, BIG - 1, BIG)
    );

    let open_ended = range("/dl/big", "bytes=199990-").await;
    assert_eq!(open_ended.body, f.big[199_990..]);

    // small (buffered) files too
    let small = range("/dl/small", "bytes=6-").await;
    assert_eq!((small.status, small.text().as_str()), (206, "world"));
    assert_eq!(small.header("content-range"), "bytes 6-10/11");

    // not satisfiable
    let beyond = range("/dl/big", "bytes=200000-").await;
    assert_eq!(beyond.status, 416);
    assert_eq!(beyond.header("content-range"), format!("bytes */{BIG}"));
    assert_eq!(range("/dl/small", "bytes=-0").await.status, 416);

    // several ranges or garbage: the whole file
    assert_eq!(
        range("/dl/small", "bytes=0-1,4-5").await.text(),
        "hello world"
    );
    assert_eq!(range("/dl/small", "bytes=abc").await.status, 200);
    assert_eq!(range("/dl/small", "pages=1-2").await.status, 200);

    // HEAD with a range reports the partial length
    let head = f
        .server
        .request("HEAD", "/dl/big")
        .header("range", "bytes=0-9")
        .send()
        .await;
    assert_eq!(
        (head.status, head.header("content-length"), head.body.len()),
        (206, "10", 0)
    );
}

#[tokio::test]
async fn conditional_requests_and_if_range() {
    let f = fixture("cond").await;
    let first = f.server.get("/dl/big").send().await;
    let (etag, modified) = (
        first.header("etag").to_string(),
        first.header("last-modified").to_string(),
    );

    let by_tag = f
        .server
        .get("/dl/big")
        .header("if-none-match", &etag)
        .send()
        .await;
    assert_eq!((by_tag.status, by_tag.body.len()), (304, 0));
    assert_eq!(by_tag.header("etag"), etag);
    assert_eq!(
        f.server
            .get("/dl/big")
            .header("if-none-match", "\"other\"")
            .send()
            .await
            .status,
        200
    );

    let by_date = f
        .server
        .get("/dl/big")
        .header("if-modified-since", &modified)
        .send()
        .await;
    assert_eq!(by_date.status, 304);
    let old = http_date(mtime(&f.dir.join("big.bin")) - 3600);
    assert_eq!(
        f.server
            .get("/dl/big")
            .header("if-modified-since", &old)
            .send()
            .await
            .status,
        200
    );
    assert_eq!(
        f.server
            .get("/dl/big")
            .header("if-modified-since", "garbage")
            .send()
            .await
            .status,
        200
    );
    // If-None-Match wins over If-Modified-Since
    let both = f
        .server
        .get("/dl/big")
        .header("if-none-match", "\"other\"")
        .header("if-modified-since", &modified)
        .send()
        .await;
    assert_eq!(both.status, 200);

    // If-Range: a matching date keeps the range, a stale one resends everything
    let ok = f
        .server
        .get("/dl/big")
        .header("range", "bytes=0-9")
        .header("if-range", &modified)
        .send()
        .await;
    assert_eq!(ok.status, 206);
    let stale = f
        .server
        .get("/dl/big")
        .header("range", "bytes=0-9")
        .header("if-range", &old)
        .send()
        .await;
    assert_eq!((stale.status, stale.body.len()), (200, BIG));
    // weak validators never satisfy If-Range
    let weak = f
        .server
        .get("/dl/big")
        .header("range", "bytes=0-9")
        .header("if-range", &etag)
        .send()
        .await;
    assert_eq!(weak.status, 200);
}

#[tokio::test]
async fn static_route_serves_ranges_index_and_encoded_names() {
    let f = fixture("static").await;

    let big = f
        .server
        .get("/static/big.bin")
        .header("range", "bytes=100-199")
        .send()
        .await;
    assert_eq!((big.status, big.body.as_slice()), (206, &f.big[100..200]));
    assert_eq!(big.header("cache-control"), "public, max-age=3600");

    assert_eq!(
        f.server.get("/static/my%20file.txt").send().await.text(),
        "spaces"
    );
    assert_eq!(
        f.server.get("/static/sub").send().await.text(),
        "<h1>sub</h1>"
    );
    assert_eq!(
        f.server.get("/static/sub/").send().await.text(),
        "<h1>sub</h1>"
    );
    assert_eq!(
        f.server
            .request("HEAD", "/static/small.txt")
            .send()
            .await
            .header("content-length"),
        "11"
    );
}

#[tokio::test]
async fn static_route_hides_dotfiles_and_refuses_escapes() {
    let f = fixture("hidden").await;
    let outside = f
        .dir
        .parent()
        .unwrap()
        .join(format!("ullun-outside-{}.txt", std::process::id()));
    std::fs::write(&outside, "outside").unwrap();
    let outside_name = outside.file_name().unwrap().to_str().unwrap().to_string();

    for path in [
        "/static/.env".to_string(),
        "/static/%2eenv".to_string(),
        "/static/.git/config".to_string(),
        "/static/sub/../.env".to_string(),
        format!("/static/../{outside_name}"),
        format!("/static/..%2F{outside_name}"),
        format!("/static/%2e%2e/{outside_name}"),
        format!("/static/%2e%2e%2f{outside_name}"),
        "/static/small.txt%00.png".to_string(),
        "/static/sub%5C..%5C..%5C.env".to_string(),
        "/static/C:/Windows/win.ini".to_string(),
    ] {
        let res = f.server.get(&path).send().await;
        assert_eq!(res.status, 404, "{path}");
        assert!(
            !res.text().contains("SECRET") && !res.text().contains("outside"),
            "{path}"
        );
    }
    // a directory without index.html is not listed
    assert_eq!(f.server.get("/static/.git").send().await.status, 404);
    std::fs::remove_file(outside).ok();
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_pointing_outside_the_root_are_not_followed() {
    let f = fixture("symlink").await;
    let target = f
        .dir
        .parent()
        .unwrap()
        .join(format!("ullun-link-target-{}.txt", std::process::id()));
    std::fs::write(&target, "linked secret").unwrap();
    std::os::unix::fs::symlink(&target, f.dir.join("link.txt")).unwrap();

    assert_eq!(f.server.get("/static/link.txt").send().await.status, 404);
    std::fs::remove_file(target).ok();
}
