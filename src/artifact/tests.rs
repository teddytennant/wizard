use std::path::Path;

use serde_json::json;

use super::*;
use crate::minihttp::Request;

fn store() -> (tempfile::TempDir, Artifacts) {
    let dir = tempfile::tempdir().unwrap();
    let store = Artifacts::at(dir.path().join("artifacts"));
    (dir, store)
}

fn page(dir: &Path, name: &str, html: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, html).unwrap();
    path.display().to_string()
}

const HTML: &str = "<!doctype html><title>Cost Chart</title><p>hi</p>";

fn slug_in(reply: &str) -> String {
    let url = reply
        .split_whitespace()
        .find(|w| w.starts_with("file://"))
        .unwrap();
    url.trim_end_matches("/index.html")
        .rsplit('/')
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn publish_writes_the_page_and_says_it_is_local() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    let reply = store
        .call(&json!({"file_path": file, "icon": "chart"}), tmp.path())
        .unwrap();
    assert!(reply.contains("not hosted"), "{reply}");
    assert!(reply.contains("wizard artifact serve"), "{reply}");
    let slug = slug_in(&reply);
    assert!(slug.starts_with("cost-chart-"), "{slug}");
    assert_eq!(
        std::fs::read_to_string(store.dir(&slug).join("index.html")).unwrap(),
        HTML
    );
    let meta = store.meta(&slug).unwrap();
    assert_eq!(meta.title, "Cost Chart");
    assert_eq!(meta.version, 1);
    assert_eq!(meta.icon, "chart");
}

#[test]
fn publishing_with_a_url_updates_in_place() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    let first = store.call(&json!({"file_path": file}), tmp.path()).unwrap();
    let slug = slug_in(&first);
    let url = store.file_url(&slug);
    let file = page(tmp.path(), "a.html", "<title>Cost Chart</title><p>v2</p>");
    let second = store
        .call(&json!({"file_path": file, "url": url}), tmp.path())
        .unwrap();
    assert_eq!(slug_in(&second), slug);
    assert_eq!(store.meta(&slug).unwrap().version, 2);
    assert_eq!(store.all().len(), 1);
    assert!(
        std::fs::read_to_string(store.dir(&slug).join("index.html"))
            .unwrap()
            .contains("v2")
    );
}

#[test]
fn an_update_to_an_unknown_artifact_is_an_error_and_a_claude_link_is_explained() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    let err = store
        .call(&json!({"file_path": file, "url": "nope-123456"}), tmp.path())
        .unwrap_err();
    assert!(err.contains("no local artifact"), "{err}");
    let err = store
        .call(
            &json!({"action": "read", "url": "https://claude.ai/artifact/abc"}),
            tmp.path(),
        )
        .unwrap_err();
    assert!(err.contains("hosted claude.ai link"), "{err}");
}

#[test]
fn edits_apply_all_or_none_and_the_source_file_follows() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", "<title>T</title><p>one</p><p>two</p>");
    let err = store
        .call(
            &json!({"file_path": file, "edits": [
                {"old_str": "one", "new_str": "1"},
                {"old_str": "<p>", "new_str": "<b>"}
            ]}),
            tmp.path(),
        )
        .unwrap_err();
    assert!(err.contains("occurs 2 times"), "{err}");
    assert!(store.all().is_empty(), "nothing is published on a failed edit");
    assert!(std::fs::read_to_string(&file).unwrap().contains("one"));

    let reply = store
        .call(
            &json!({"file_path": file, "edits": [{"old_str": "one", "new_str": "1"}]}),
            tmp.path(),
        )
        .unwrap();
    let slug = slug_in(&reply);
    assert!(std::fs::read_to_string(&file).unwrap().contains("<p>1</p>"));
    assert!(
        std::fs::read_to_string(store.dir(&slug).join("index.html"))
            .unwrap()
            .contains("<p>1</p>")
    );
}

#[test]
fn supporting_files_are_copied_replaced_and_removed() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    std::fs::write(tmp.path().join("style.css"), "p{}").unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/app.js"), "1").unwrap();
    let reply = store
        .call(
            &json!({"file_path": file, "files": {
                "css/style.css": "style.css",
                "app.js": {"from": "src/app.js"}
            }}),
            tmp.path(),
        )
        .unwrap();
    let slug = slug_in(&reply);
    assert!(store.dir(&slug).join("css/style.css").is_file());
    assert!(store.dir(&slug).join("app.js").is_file());

    store
        .call(
            &json!({"file_path": file, "url": slug, "files": {"app.js": null}}),
            tmp.path(),
        )
        .unwrap();
    assert!(!store.dir(&slug).join("app.js").exists());
    assert!(store.dir(&slug).join("css/style.css").is_file());

    // The array form publishes each file at its own spelling.
    let reply = store
        .call(
            &json!({"file_path": file, "url": slug, "files": [{"path": "style.css"}]}),
            tmp.path(),
        )
        .unwrap();
    assert!(reply.contains("version 3"), "{reply}");
    assert!(store.dir(&slug).join("style.css").is_file());
}

#[test]
fn a_file_can_come_from_another_local_artifact() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    std::fs::write(tmp.path().join("font.txt"), "f").unwrap();
    let a = slug_in(
        &store
            .call(
                &json!({"file_path": file, "files": {"font.txt": "font.txt"}}),
                tmp.path(),
            )
            .unwrap(),
    );
    let b = slug_in(
        &store
            .call(
                &json!({"file_path": file, "files": {
                    "copy.txt": {"artifact": store.file_url(&a), "path": "font.txt"}
                }}),
                tmp.path(),
            )
            .unwrap(),
    );
    assert_eq!(std::fs::read_to_string(store.dir(&b).join("copy.txt")).unwrap(), "f");
}

#[test]
fn published_paths_cannot_escape_or_shadow_the_page() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    std::fs::write(tmp.path().join("x"), "x").unwrap();
    for bad in ["../x", "/etc/x", ".artifact.json"] {
        let err = store
            .call(
                &json!({"file_path": file, "files": {bad: "x"}}),
                tmp.path(),
            )
            .unwrap_err();
        assert!(
            err.contains("relative") || err.contains("reserved"),
            "{bad}: {err}"
        );
    }
    let err = store
        .call(
            &json!({"file_path": file, "files": {"index.html": "x"}}),
            tmp.path(),
        )
        .unwrap_err();
    assert!(err.contains("the page itself"), "{err}");
}

#[test]
fn the_documented_size_limits_hold() {
    let (tmp, store) = store();
    let big = "x".repeat(MAX_TEXT_BYTES as usize + 1);
    let file = page(tmp.path(), "big.html", &big);
    let err = store.call(&json!({"file_path": file}), tmp.path()).unwrap_err();
    assert!(err.contains("at most"), "{err}");

    let file = page(tmp.path(), "a.html", HTML);
    let files: serde_json::Map<String, serde_json::Value> = (0..255)
        .map(|i| (format!("f{i}.txt"), json!("a.html")))
        .collect();
    let err = store
        .call(&json!({"file_path": file, "files": files}), tmp.path())
        .unwrap_err();
    assert!(err.contains("at most 255 files"), "{err}");

    let blob = tmp.path().join("blob.bin");
    std::fs::write(&blob, vec![0xff; MAX_BINARY_BYTES as usize + 1]).unwrap();
    let err = store
        .call(
            &json!({"file_path": file, "files": {"blob.bin": blob.display().to_string()}}),
            tmp.path(),
        )
        .unwrap_err();
    assert!(err.contains("binary file"), "{err}");
}

#[test]
fn list_read_open_pin_and_delete() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    let slug = slug_in(&store.call(&json!({"file_path": file}), tmp.path()).unwrap());
    let other = page(tmp.path(), "b.html", "<title>Other</title>");
    let other_slug = slug_in(&store.call(&json!({"file_path": other}), tmp.path()).unwrap());

    let list = store.call(&json!({"action": "list"}), tmp.path()).unwrap();
    assert!(list.contains("Cost Chart") && list.contains("Other"), "{list}");
    assert!(list.contains("file://"), "{list}");

    let read = store
        .call(&json!({"action": "read", "url": slug}), tmp.path())
        .unwrap();
    assert!(read.contains("<p>hi</p>") && read.contains("version 1"), "{read}");

    let opened = store
        .call(&json!({"action": "open", "url": store.file_url(&slug)}), tmp.path())
        .unwrap();
    assert!(opened.contains("Open this in a browser"), "{opened}");

    store
        .call(&json!({"action": "pin", "url": other_slug}), tmp.path())
        .unwrap();
    assert_eq!(store.all()[0].slug, other_slug, "pinned sorts first");
    store
        .call(&json!({"action": "unpin", "url": other_slug}), tmp.path())
        .unwrap();
    assert!(!store.meta(&other_slug).unwrap().pinned);

    let files = store
        .call(&json!({"action": "list", "scope": "files", "url": slug}), tmp.path())
        .unwrap();
    assert!(files.contains("index.html"), "{files}");

    let gone = store
        .call(&json!({"action": "delete", "url": slug}), tmp.path())
        .unwrap();
    assert!(gone.contains("Deleted local artifact"), "{gone}");
    assert!(!store.dir(&slug).exists());
    assert!(
        store
            .call(&json!({"action": "read", "url": slug}), tmp.path())
            .is_err()
    );
}

#[test]
fn assets_upload_list_read_copy_and_delete() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    let slug = slug_in(&store.call(&json!({"file_path": file}), tmp.path()).unwrap());
    std::fs::write(tmp.path().join("logo.png"), [1, 2, 3]).unwrap();
    let reply = store
        .call(
            &json!({"action": "publish", "asset": true, "url": slug,
                    "file_paths": [tmp.path().join("logo.png").display().to_string()]}),
            tmp.path(),
        )
        .unwrap();
    assert!(reply.contains("url assets/"), "{reply}");
    let assets = store
        .call(&json!({"action": "list", "scope": "assets", "url": slug}), tmp.path())
        .unwrap();
    let id = assets.split_whitespace().next().unwrap().to_string();
    assert_eq!(id.len(), 32);

    let other = slug_in(
        &store
            .call(&json!({"file_path": page(tmp.path(), "b.html", "<title>B</title>")}), tmp.path())
            .unwrap(),
    );
    let copied = store
        .call(
            &json!({"action": "publish", "asset": true, "url": other, "from_url": slug, "asset_ids": [id]}),
            tmp.path(),
        )
        .unwrap();
    assert!(copied.contains("copied as"), "{copied}");
    assert_eq!(list_len(&store, &other), 1);

    store
        .call(&json!({"action": "delete", "url": slug, "path": id}), tmp.path())
        .unwrap();
    assert_eq!(list_len(&store, &slug), 0);
}

fn list_len(store: &Artifacts, slug: &str) -> usize {
    super::publish::list_assets(store, slug).len()
}

#[test]
fn what_needs_the_hosted_service_is_named_not_dropped() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    let reply = store
        .call(
            &json!({"file_path": file, "capabilities": {"db": {}}}),
            tmp.path(),
        )
        .unwrap();
    assert!(reply.contains("Not applied locally: capabilities"), "{reply}");
    let err = store
        .call(&json!({"file_path": file, "type_url": "x"}), tmp.path())
        .unwrap_err();
    assert!(err.contains("claude.ai Artifact type"), "{err}");
    let q = store.call(&json!({"action": "quickstart"}), tmp.path()).unwrap();
    assert!(q.contains("local counterpart"), "{q}");
    assert!(store.call(&json!({"action": "bogus"}), tmp.path()).is_err());
}

fn get(path: &str) -> Request {
    Request {
        method: "GET".into(),
        path: path.into(),
        query: String::new(),
        headers: Default::default(),
        body: Vec::new(),
    }
}

#[test]
fn the_static_server_serves_pages_and_refuses_the_rest() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    std::fs::write(tmp.path().join("s.css"), "p{}").unwrap();
    let slug = slug_in(
        &store
            .call(&json!({"file_path": file, "files": {"s.css": "s.css"}}), tmp.path())
            .unwrap(),
    );
    let (status, kind, body) = serve::route(&store, &get(&format!("/{slug}/")));
    assert_eq!((status, kind.as_str()), (200, "text/html; charset=utf-8"));
    assert_eq!(body, HTML.as_bytes());
    let (status, kind, _) = serve::route(&store, &get(&format!("/{slug}/s.css")));
    assert_eq!((status, kind.as_str()), (200, "text/css; charset=utf-8"));
    assert_eq!(serve::route(&store, &get(&format!("/{slug}/.artifact.json"))).0, 403);
    assert_eq!(serve::route(&store, &get(&format!("/{slug}/%2e%2e/x"))).0, 403);
    assert_eq!(serve::route(&store, &get("/missing-000000/")).0, 404);
    let (status, _, body) = serve::route(&store, &get("/"));
    assert_eq!(status, 200);
    assert!(String::from_utf8(body).unwrap().contains("Cost Chart"));
    let mut post = get("/");
    post.method = "POST".into();
    assert_eq!(serve::route(&store, &post).0, 405);
}

#[test]
fn claude_code_2_1_314s_action_names_mean_the_scope_and_asset_forms() {
    let (tmp, store) = store();
    let file = page(tmp.path(), "a.html", HTML);
    let slug = slug_in(&store.call(&json!({"file_path": file}), tmp.path()).unwrap());
    let files = store
        .call(&json!({"action": "list_files", "url": slug}), tmp.path())
        .unwrap();
    assert!(files.contains("index.html"), "{files}");
    let page = store
        .call(
            &json!({"action": "read_file", "url": slug, "path": "index.html"}),
            tmp.path(),
        )
        .unwrap();
    assert!(page.contains("<p>hi</p>"), "{page}");
    std::fs::write(tmp.path().join("i.png"), [1u8, 2]).unwrap();
    let image = tmp.path().join("i.png").display().to_string();
    let up = store
        .call(
            &json!({"action": "upload_asset", "url": slug, "file_path": image}),
            tmp.path(),
        )
        .unwrap();
    assert!(up.contains("url assets/"), "{up}");
    let assets = store
        .call(&json!({"action": "list_assets", "url": slug}), tmp.path())
        .unwrap();
    assert!(assets.contains("2 bytes"), "{assets}");
    let types = store.call(&json!({"action": "list_types"}), tmp.path()).unwrap();
    assert!(types.contains("No artifact types"), "{types}");
    let watch = store
        .call(&json!({"action": "watch", "url": slug}), tmp.path())
        .unwrap();
    assert!(watch.contains("Nothing to watch"), "{watch}");
}
