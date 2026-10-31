use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex as StdMutex;

use super::*;

/// `dictate` owns one process-wide recording slot; these tests take turns.
static SERIAL: StdMutex<()> = StdMutex::new(());

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn path_of(dir: &Path) -> OsString {
    dir.as_os_str().to_owned()
}

#[test]
fn config_reads_the_voice_table_and_clamps_seconds() {
    let c = VoiceConfig::parse(
        "[voice]\ncommand = \"stt {wav}\"\nrecorder = \"rec {out}\"\nmodel = \"/m/ggml.bin\"\nseconds = 9999\n",
    );
    assert_eq!(c.command.as_deref(), Some("stt {wav}"));
    assert_eq!(c.recorder.as_deref(), Some("rec {out}"));
    assert_eq!(c.model, Some(PathBuf::from("/m/ggml.bin")));
    assert_eq!(c.seconds, MAX_SECONDS);
    let none = VoiceConfig::parse("[other]\nx = 1\n");
    assert_eq!((none.command, none.seconds), (None, DEFAULT_SECONDS));
    assert_eq!(VoiceConfig::parse("not toml {{").seconds, DEFAULT_SECONDS);
}

#[test]
fn detection_prefers_arecord_then_ffmpeg_then_sox_and_names_what_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = VoiceConfig::parse("");

    let empty = detect(&cfg, &path_of(tmp.path()), None);
    assert_eq!((empty.recorder.clone(), empty.transcriber.clone()), (None, None));
    let help = empty.missing().unwrap();
    for needle in ["arecord", "alsa-utils", "ffmpeg", "sox", "whisper-cli", "ggml", "vosk", "[voice]"] {
        assert!(help.contains(needle), "{needle} missing from: {help}");
    }

    script(tmp.path(), "sox", "true");
    assert!(matches!(detect(&cfg, &path_of(tmp.path()), None).recorder, Some(Recorder::Sox(_))));
    script(tmp.path(), "ffmpeg", "true");
    assert!(matches!(detect(&cfg, &path_of(tmp.path()), None).recorder, Some(Recorder::Ffmpeg(_))));
    script(tmp.path(), "arecord", "true");
    assert!(matches!(detect(&cfg, &path_of(tmp.path()), None).recorder, Some(Recorder::Arecord(_))));
}

#[test]
fn whisper_cli_needs_a_model_and_the_reply_says_where_to_get_one() {
    let tmp = tempfile::tempdir().unwrap();
    script(tmp.path(), "whisper-cli", "true");
    let home = tempfile::tempdir().unwrap();
    let found = detect(&VoiceConfig::parse(""), &path_of(tmp.path()), Some(home.path()));
    assert_eq!(found.transcriber, None);
    let help = found.missing().unwrap();
    assert!(help.contains("has no model") && help.contains("ggml-base.en.bin"), "{help}");

    let models = home.path().join(".wizard/models/whisper");
    std::fs::create_dir_all(&models).unwrap();
    std::fs::write(models.join("ggml-base.en.bin"), "m").unwrap();
    let found = detect(&VoiceConfig::parse(""), &path_of(tmp.path()), Some(home.path()));
    assert!(matches!(found.transcriber, Some(Transcriber::WhisperCpp { .. })));
}

#[test]
fn vosk_is_the_fallback_and_a_configured_command_wins() {
    let tmp = tempfile::tempdir().unwrap();
    script(tmp.path(), "vosk-transcriber", "true");
    let found = detect(&VoiceConfig::parse(""), &path_of(tmp.path()), None);
    assert!(matches!(found.transcriber, Some(Transcriber::Vosk(_))));
    let found = detect(
        &VoiceConfig::parse("[voice]\ncommand = \"mine\"\n"),
        &path_of(tmp.path()),
        None,
    );
    assert_eq!(found.transcriber, Some(Transcriber::Custom("mine".into())));
}

#[test]
fn the_recorders_ask_for_16khz_mono_wav() {
    let out = Path::new("/tmp/x.wav");
    let args = |c: Command| {
        let std = c.as_std();
        std::iter::once(std.get_program())
            .chain(std.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let a = args(Recorder::Arecord("arecord".into()).command(out, 7));
    assert!(a.contains("-f S16_LE -r 16000 -c 1 -d 7 /tmp/x.wav"), "{a}");
    let f = args(Recorder::Ffmpeg("ffmpeg".into()).command(out, 7));
    assert!(f.contains("-ac 1 -ar 16000 -t 7 /tmp/x.wav"), "{f}");
    let s = args(Recorder::Sox("sox".into()).command(out, 7));
    assert!(s.contains("-r 16000 -c 1 -b 16 /tmp/x.wav trim 0 7"), "{s}");
    let c = args(Recorder::Custom("rec {out} {seconds}".into()).command(out, 7));
    assert!(c.contains("rec '/tmp/x.wav' 7"), "{c}");
}

#[test]
fn the_transcript_is_one_clean_line() {
    assert_eq!(clean("\n [BLANK_AUDIO]\n hello there \n\n  world\n"), "hello there world");
    assert_eq!(clean("(silence)"), "");
}

fn plan(recorder: &str, transcriber: &str) -> Plan {
    Plan {
        recorder: Recorder::Custom(recorder.into()),
        transcriber: Transcriber::Custom(transcriber.into()),
    }
}

#[tokio::test]
async fn a_fake_recorder_and_a_fake_transcriber_make_text() {
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let stt = script(tmp.path(), "stt", "test -s \"$1\" && echo ' open the pull request '");
    let text = dictate(
        &plan("head -c 200 /dev/zero > {out}", &format!("{} {{wav}}", stt.display())),
        3,
    )
    .await
    .unwrap();
    assert_eq!(text, "open the pull request");
    assert!(!is_recording());
}

#[tokio::test]
async fn a_recorder_that_records_nothing_is_reported_with_its_stderr() {
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let err = dictate(&plan("echo no such device >&2", "echo x"), 2).await.unwrap_err();
    assert!(err.contains("recorded no audio") && err.contains("no such device"), "{err}");
}

#[tokio::test]
async fn a_failing_or_silent_transcriber_is_an_error_not_empty_text() {
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rec = "head -c 200 /dev/zero > {out}";
    let err = dictate(&plan(rec, "echo broken model >&2; exit 3"), 2).await.unwrap_err();
    assert!(err.contains("failed") && err.contains("broken model"), "{err}");
    let err = dictate(&plan(rec, ": {wav}; echo '[BLANK_AUDIO]'"), 2).await.unwrap_err();
    assert_eq!(err, "heard nothing");
}

#[tokio::test]
async fn stop_ends_the_recording_early_and_the_audio_is_still_transcribed() {
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The recorder says when its trap is in place, so the test never signals
    // a shell that has not got that far.
    let tmp = tempfile::tempdir().unwrap();
    let ready = tmp.path().join("ready");
    let recorder = format!(
        "trap 'head -c 200 /dev/zero > {{out}}; exit 0' INT; touch {}; sleep 5 & wait",
        ready.display()
    );
    let p = plan(&recorder, ": {wav}; echo heard it");
    let task = tokio::spawn(async move { dictate(&p, 60).await });
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while !ready.exists() && std::time::Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready.exists() && is_recording());
    let started = std::time::Instant::now();
    assert!(stop());
    let text = task.await.unwrap().unwrap();
    assert_eq!(text, "heard it");
    assert!(started.elapsed() < Duration::from_secs(3), "stop did not cut the recording short");
    assert!(!stop(), "nothing is recording any more");
}
