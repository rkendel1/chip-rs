//! The writer follows the Zig commit protocol; every interrupted state resolves to the prior or the
//! proposed position, and nothing outside the format is created.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chip_session::{
    Boundary, IdSource, Identifier, Kind, NewEvent, SessionError, SessionWriter, encode_authority,
    encode_watermark, read_session,
};

/// Deterministic identifiers so failures reproduce.
struct SeqIds(u128);

impl IdSource for SeqIds {
    fn next_id(&mut self) -> std::io::Result<Identifier> {
        self.0 += 1;
        Ok(Identifier(self.0.to_be_bytes()))
    }
}

fn ids(start: u128) -> Box<dyn IdSource> {
    Box::new(SeqIds(start))
}

const STARTED: &str = r#"{"id":"s1","created_at_ms":1,"origin_workspace_root":"/w","workspace_root":"/w","conversation_language":"und-Latn","preferences":{"model":"m","effort":"auto","fast_mode":false}}"#;

fn started() -> NewEvent {
    NewEvent::new(Kind::SessionStarted, 1, STARTED).unwrap()
}

fn prefs(ts: i64) -> NewEvent {
    NewEvent::new(Kind::PreferencesChanged, ts, r#"{"effort":"high"}"#).unwrap()
}

fn sessions() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("sessions");
    (temp, dir)
}

fn make(sessions_dir: &Path, n_extra: usize) -> PathBuf {
    let mut w = SessionWriter::create(sessions_dir, "s1", started(), ids(1000), None).unwrap();
    for i in 0..n_extra {
        w.append(vec![prefs(10 + i as i64)]).unwrap();
    }
    sessions_dir.join("s1")
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn create_and_append_produce_a_session_the_reader_verifies() {
    let (_t, root) = sessions();
    let dir = make(&root, 3);
    let log = read_session(&dir).unwrap();
    assert_eq!(log.frames.len(), 4);
    assert_eq!(log.frames[0].kind, Kind::SessionStarted);
    assert_eq!(log.position.through_seq, 4);
    assert_eq!(log.uncommitted_tail_bytes, 0);
    assert_eq!(mode(&dir), 0o700);
    for name in [
        "events.jsonl",
        "authority.json",
        "session.lock",
        "commit.lock",
    ] {
        assert_eq!(mode(&dir.join(name)), 0o600, "{name}");
    }
    let listing = names(&dir);
    assert!(
        !listing
            .iter()
            .any(|n| n.contains(".tmp.") || n.ends_with(".pending.json")),
        "{listing:?}"
    );
}

#[test]
fn files_have_exactly_the_bytes_zig_writes_for_the_same_position() {
    let (_t, root) = sessions();
    let dir = make(&root, 1);
    let log = read_session(&dir).unwrap();
    let watermark = fs::read(dir.join(chip_session::watermark_file_name(
        log.position.log_generation,
    )))
    .unwrap();
    assert_eq!(watermark, encode_watermark("s1", &log.position));
    let authority = fs::read(dir.join("authority.json")).unwrap();
    assert_eq!(
        authority,
        encode_authority("s1", log.authority.authority_id)
    );
}

#[test]
fn the_writer_reproduces_zig_watermark_and_authority_bytes_for_zig_sessions() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/zig");
    for scenario in ["single_text", "tool_round_trip", "resumed_two_turns"] {
        let src = fixtures.join(scenario);
        let id = serde_json::from_slice::<serde_json::Value>(
            &fs::read(src.join("PROVENANCE.json")).unwrap(),
        )
        .unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(&id);
        fs::create_dir(&dir).unwrap();
        for e in fs::read_dir(&src).unwrap() {
            let e = e.unwrap();
            fs::copy(e.path(), dir.join(e.file_name())).unwrap();
        }
        let log = read_session(&dir).unwrap();
        let name = chip_session::watermark_file_name(log.position.log_generation);
        assert_eq!(
            fs::read(dir.join(&name)).unwrap(),
            encode_watermark(&id, &log.position),
            "{scenario}"
        );
        assert_eq!(
            fs::read(dir.join("authority.json")).unwrap(),
            encode_authority(&id, log.authority.authority_id),
            "{scenario}"
        );
    }
}

#[test]
fn reopening_continues_the_same_log() {
    let (_t, root) = sessions();
    let dir = make(&root, 2);
    let mut w = SessionWriter::open(&dir, ids(5000), None).unwrap();
    assert_eq!(w.position().through_seq, 3);
    w.append(vec![prefs(50), prefs(51)]).unwrap();
    let log = read_session(&dir).unwrap();
    assert_eq!(log.frames.len(), 5);
    assert_eq!(
        log.frames.iter().map(|f| f.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
}

#[test]
fn creating_over_an_existing_session_is_refused_and_touches_nothing() {
    let (_t, root) = sessions();
    let dir = make(&root, 1);
    let before = fs::read(dir.join("events.jsonl")).unwrap();
    let err = SessionWriter::create(&root, "s1", started(), ids(9), None)
        .err()
        .unwrap();
    assert!(matches!(err, SessionError::AlreadyExists), "{err}");
    assert_eq!(fs::read(dir.join("events.jsonl")).unwrap(), before);
}

#[test]
fn a_second_writer_is_refused_while_the_first_holds_the_session() {
    let (_t, root) = sessions();
    let dir = make(&root, 0);
    let _first = SessionWriter::open(&dir, ids(1), None).unwrap();
    let err = SessionWriter::open(&dir, ids(2), None).err().unwrap();
    assert!(matches!(err, SessionError::Locked), "{err}");
}

#[test]
fn non_private_state_is_refused() {
    let (_t, root) = sessions();
    let dir = make(&root, 0);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        SessionWriter::open(&dir, ids(1), None).err().unwrap(),
        SessionError::PrivateStatePermissions(_)
    ));
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(dir.join("events.jsonl"), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        SessionWriter::open(&dir, ids(1), None).err().unwrap(),
        SessionError::PrivateStatePermissions(_)
    ));
}

fn crash_during_append(boundary: Boundary) -> (tempfile::TempDir, PathBuf) {
    let (t, root) = sessions();
    let dir = make(&root, 2);
    let mut w = SessionWriter::open(&dir, ids(7000), Some(boundary)).unwrap();
    let err = w.append(vec![prefs(99)]).unwrap_err();
    assert!(
        matches!(err, SessionError::SimulatedCrash(_)),
        "{boundary:?}: {err}"
    );
    drop(w);
    (t, dir)
}

#[test]
fn a_crash_at_every_append_boundary_resolves_to_the_prior_or_the_proposed_position() {
    let cases = [
        (Boundary::AfterEventAppend, 3u64),
        (Boundary::AfterEventSync, 3),
        (Boundary::AfterIntentSync, 3),
        (Boundary::AfterWatermarkRename, 4),
        (Boundary::AfterIntentRemove, 4),
    ];
    for (boundary, expected_seq) in cases {
        let (_t, dir) = crash_during_append(boundary);

        // Without opening for write, the reader reports only what the watermark commits.
        let committed = read_session(&dir).unwrap();
        assert_eq!(
            committed.position.through_seq, expected_seq,
            "{boundary:?}: reader"
        );
        assert_eq!(committed.frames.len() as u64, expected_seq, "{boundary:?}");

        // Opening for write resolves the interruption and removes the intent.
        let mut w = SessionWriter::open(&dir, ids(8000), None)
            .unwrap_or_else(|e| panic!("{boundary:?}: {e}"));
        assert_eq!(
            w.position().through_seq,
            expected_seq,
            "{boundary:?}: writer"
        );
        assert!(
            !dir.join("commit.pending.json").exists(),
            "{boundary:?}: intent removed"
        );
        // A pre-intent crash leaves an unpublished tail; like Zig, the next publication replaces it.
        let log = read_session(&dir).unwrap();
        assert_eq!(
            log.frames.len() as u64,
            expected_seq,
            "{boundary:?}: the tail is not history"
        );

        // And the session keeps working: the next append is contiguous.
        w.append(vec![prefs(100)]).unwrap();
        let log = read_session(&dir).unwrap();
        assert_eq!(log.position.through_seq, expected_seq + 1, "{boundary:?}");
        assert_eq!(
            log.frames.iter().map(|f| f.seq).collect::<Vec<_>>(),
            (1..=expected_seq + 1).collect::<Vec<_>>()
        );
        assert_eq!(log.uncommitted_tail_bytes, 0);
    }
}

#[test]
fn a_published_watermark_over_a_torn_tail_rolls_back_to_the_prior_position() {
    let (_t, dir) = crash_during_append(Boundary::AfterWatermarkRename);
    let log = dir.join("events.jsonl");
    let bytes = fs::read(&log).unwrap();
    fs::write(&log, &bytes[..bytes.len() - 7]).unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();

    let w = SessionWriter::open(&dir, ids(8000), None).unwrap();
    assert_eq!(
        w.position().through_seq,
        3,
        "the invalid proposed position is abandoned"
    );
    drop(w);
    let committed = read_session(&dir).unwrap();
    assert_eq!(committed.position.through_seq, 3);
    assert_eq!(committed.uncommitted_tail_bytes, 0);
    assert!(!dir.join("commit.pending.json").exists());
}

#[test]
fn a_stale_tail_without_an_intent_is_replaced_by_the_next_append() {
    let (_t, root) = sessions();
    let dir = make(&root, 1);
    let log = dir.join("events.jsonl");
    let mut bytes = fs::read(&log).unwrap();
    let committed_len = bytes.len();
    bytes.extend_from_slice(b"{\"schema_version\":1,\"torn tail that was never committed");
    fs::write(&log, &bytes).unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();

    assert!(read_session(&dir).unwrap().uncommitted_tail_bytes > 0);
    let mut w = SessionWriter::open(&dir, ids(3), None).unwrap();
    w.append(vec![prefs(7)]).unwrap();
    let after = read_session(&dir).unwrap();
    assert_eq!(after.uncommitted_tail_bytes, 0);
    assert_eq!(after.frames.len(), 3);
    assert!(fs::read(&log).unwrap().len() > committed_len);
}

#[test]
fn a_malformed_or_foreign_intent_is_refused_not_guessed_at() {
    for intent in [
        "not json".to_string(),
        r#"{"schema_version":1,"session_id":"s1","operation_id":"00000000000000000000000000000001","kind":"watermark_advance","prior":{"log_generation":"00000000000000000000000000000001","through_seq":1,"through_event_id":"00000000000000000000000000000001","through_event_log_bytes":1},"extra":1}"#.to_string(),
    ] {
        let (_t, root) = sessions();
        let dir = make(&root, 1);
        fs::write(dir.join("commit.pending.json"), intent).unwrap();
        fs::set_permissions(dir.join("commit.pending.json"), fs::Permissions::from_mode(0o600)).unwrap();
        let err = SessionWriter::open(&dir, ids(1), None).err().unwrap();
        assert!(matches!(err, SessionError::InvalidFormat { .. }), "{err}");
        assert!(dir.join("commit.pending.json").exists(), "an unreadable intent is left in place");
    }
}

#[test]
fn a_watermark_that_is_neither_prior_nor_proposed_is_indeterminate() {
    let (_t, dir) = crash_during_append(Boundary::AfterIntentSync);
    // Move the watermark somewhere the intent does not describe.
    let log = read_session(&dir).unwrap();
    let mut other = log.position;
    other.through_seq -= 1;
    let name = chip_session::watermark_file_name(log.position.log_generation);
    fs::write(dir.join(&name), encode_watermark("s1", &other)).unwrap();
    fs::set_permissions(dir.join(&name), fs::Permissions::from_mode(0o600)).unwrap();
    let err = SessionWriter::open(&dir, ids(1), None).err().unwrap();
    assert!(
        matches!(
            err,
            SessionError::CommitIndeterminate(_) | SessionError::WatermarkMismatch(_)
        ),
        "{err}"
    );
}

#[test]
fn a_crash_while_creating_resolves_without_a_half_session() {
    let cases = [
        (Boundary::AfterEventAppend, false),
        (Boundary::AfterEventSync, false),
        (Boundary::AfterWatermarkRename, false),
        (Boundary::AfterAuthorityIntentSync, false),
        (Boundary::AfterAuthorityRename, true),
        (Boundary::AfterIntentRemove, true),
    ];
    for (boundary, opens) in cases {
        let (_t, root) = sessions();
        let err = SessionWriter::create(&root, "s1", started(), ids(1000), Some(boundary))
            .err()
            .unwrap();
        assert!(
            matches!(err, SessionError::SimulatedCrash(_)),
            "{boundary:?}: {err}"
        );
        let dir = root.join("s1");
        let opened = SessionWriter::open(&dir, ids(2000), None);
        match (opened, opens) {
            (Ok(w), true) => {
                assert_eq!(w.position().through_seq, 1, "{boundary:?}");
                assert!(
                    !dir.join("authority.pending.json").exists(),
                    "{boundary:?}: intent removed"
                );
            }
            (Err(SessionError::Missing(_)), false) => {}
            (other, _) => panic!("{boundary:?}: unexpected {:?}", other.map(|w| w.position())),
        }
    }
}

#[test]
fn a_session_created_without_an_authority_marker_is_never_treated_as_complete() {
    let (_t, root) = sessions();
    let _ = SessionWriter::create(
        &root,
        "s1",
        started(),
        ids(1000),
        Some(Boundary::AfterWatermarkRename),
    );
    assert!(matches!(
        read_session(&root.join("s1")).err().unwrap(),
        SessionError::Missing(_)
    ));
}

#[test]
fn the_first_event_must_be_session_started() {
    let (_t, root) = sessions();
    let err = SessionWriter::create(&root, "s1", prefs(1), ids(1), None)
        .err()
        .unwrap();
    assert!(matches!(err, SessionError::InvalidFrame { .. }), "{err}");
    assert!(
        !root.join("s1").exists(),
        "nothing is created for an invalid first event"
    );
}
