use super::*;

fn session(dir: &Path, id: &str, cwd: &str) {
    std::fs::write(
        dir.join(format!("{id}.jsonl")),
        format!(
            "{{\"timestamp\":\"2026-10-31T10:00:00Z\",\"cwd\":\"{cwd}\",\"version\":1}}\n{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}\n"
        ),
    )
    .unwrap();
}

#[test]
fn a_session_round_trips_to_another_machine_and_points_at_the_local_directory() {
    let src = tempfile::tempdir().unwrap();
    session(src.path(), "2026-10-31T10-00-00", "/home/a/project");
    let bytes = pack(src.path(), "2026-10-31T10-00-00").unwrap();

    let envelope = unpack(&bytes).unwrap();
    assert_eq!(envelope.id, "2026-10-31T10-00-00");
    assert_eq!(envelope.cwd, "/home/a/project");

    let dst = tempfile::tempdir().unwrap();
    let id = import(&envelope, dst.path(), Path::new("/srv/work")).unwrap();
    assert_eq!(id, "2026-10-31T10-00-00");
    let text = std::fs::read_to_string(dst.path().join(format!("{id}.jsonl"))).unwrap();
    assert!(text.lines().next().unwrap().contains("/srv/work"), "{text}");
    assert!(text.contains("\"text\":\"hi\""));
    assert_eq!(header_cwd(&dst.path().join(format!("{id}.jsonl"))).as_deref(), Some("/srv/work"));
}

#[test]
fn importing_never_overwrites_an_existing_session() {
    let src = tempfile::tempdir().unwrap();
    session(src.path(), "s1", "/a");
    let envelope = unpack(&pack(src.path(), "s1").unwrap()).unwrap();
    let dst = tempfile::tempdir().unwrap();
    assert_eq!(import(&envelope, dst.path(), Path::new("/b")).unwrap(), "s1");
    assert_eq!(import(&envelope, dst.path(), Path::new("/b")).unwrap(), "s1-tp2");
    assert_eq!(import(&envelope, dst.path(), Path::new("/b")).unwrap(), "s1-tp3");
}

#[test]
fn bad_envelopes_are_refused_before_anything_is_written() {
    assert!(unpack(b"plain text").is_err());
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(br#"{"teleport":1,"id":"../evil","host":"h","cwd":"","packed_at":"","session":""}"#).unwrap();
    let err = unpack(&gz.finish().unwrap()).unwrap_err();
    assert!(err.to_string().contains("session id"), "{err}");

    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(br#"{"teleport":1,"id":"ok","host":"h","cwd":"","packed_at":"","session":"{}\nnot json\n"}"#).unwrap();
    let err = unpack(&gz.finish().unwrap()).unwrap_err();
    assert!(err.to_string().contains("not JSON"), "{err}");

    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(br#"{"teleport":9,"id":"ok","host":"h","cwd":"","packed_at":"","session":""}"#).unwrap();
    assert!(unpack(&gz.finish().unwrap()).unwrap_err().to_string().contains("version"));
}

#[test]
fn pack_rejects_ids_that_are_paths_and_missing_sessions() {
    let dir = tempfile::tempdir().unwrap();
    assert!(pack(dir.path(), "../x").is_err());
    assert!(pack(dir.path(), "").is_err());
    assert!(pack(dir.path(), "nope").unwrap_err().to_string().contains("no session"));
}

#[test]
fn latest_prefers_the_directory_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    session(dir.path(), "2026-01-01T00-00-00", "/a");
    session(dir.path(), "2026-02-01T00-00-00", "/b");
    assert_eq!(latest(dir.path(), None).as_deref(), Some("2026-02-01T00-00-00"));
    assert_eq!(latest(dir.path(), Some(Path::new("/a"))).as_deref(), Some("2026-01-01T00-00-00"));
    assert_eq!(latest(dir.path(), Some(Path::new("/zzz"))), None);
}

#[test]
fn sources_are_told_apart() {
    assert_eq!(parse_source("https://x.test/s.gz"), Source::Url("https://x.test/s.gz".into()));
    assert_eq!(parse_source("/tmp/s.gz"), Source::File("/tmp/s.gz".into()));
    assert_eq!(parse_source("s.gz"), Source::File("s.gz".into()));
    assert_eq!(
        parse_source("me@box"),
        Source::Ssh { host: "me@box".into(), id: "latest".into() }
    );
    assert_eq!(
        parse_source("me@box:2026-10-31T10-00-00"),
        Source::Ssh { host: "me@box".into(), id: "2026-10-31T10-00-00".into() }
    );
}

#[tokio::test]
async fn a_file_source_is_read_and_a_dash_host_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    session(dir.path(), "s1", "/a");
    let file = dir.path().join("s1.gz");
    std::fs::write(&file, pack(dir.path(), "s1").unwrap()).unwrap();
    let bytes = fetch(&Source::File(file)).await.unwrap();
    assert_eq!(unpack(&bytes).unwrap().id, "s1");
    let err = fetch(&Source::Ssh { host: "-oProxyCommand=x".into(), id: "latest".into() })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a host"), "{err}");
}
