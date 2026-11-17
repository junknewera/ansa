#[allow(dead_code)]
mod support {
    pub mod runtime_fixture;
}
use ansa::{
    frontend::bridge::Bridge, protocol::json::Json, provider::native, runtime::*,
    session::journal_v1::Point, tools::editor_context::EditorHub,
};
use std::sync::{Arc, atomic::AtomicUsize};
use support::runtime_fixture as f;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Release(Arc<f::Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn editor_cancel_bypasses_its_held_send_and_full_ordinary_admission() {
    let root = f::Root::new();
    let provider = Arc::new(f::Gate::default());
    let journal = Arc::new(f::Gate::default());
    let _release_provider = Release(provider.clone());
    let _release_journal = Release(journal.clone());
    let held_provider = provider.clone();
    let held_journal = journal.clone();
    let mut runtime = f::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        Arc::new(move |r, stop, _| {
            held_provider.hold(stop);
            native::load(&f::saved_response(r, Some("done"), 0))
        }),
        Limits {
            commands: 1,
            ..Limits::default()
        },
        Options {
            directory: Some(root.journal()),
            journal: Some(Arc::new(move |point, kind| {
                if point == Point::BeforeSync && kind == "queued" {
                    held_journal.hold(&Cancel::default());
                }
                Ok(())
            })),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let hub = EditorHub::new(root.0.clone(), vec![]);
    let mut bridge =
        Bridge::start_with_editor(&root.0.join("bridge"), handle.clone(), Some(hub)).unwrap();
    let stream = tokio::net::UnixStream::connect(bridge.socket())
        .await
        .unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    writer
        .write_all(b"{\"version\":1,\"role\":\"editor\",\"client\":\"cancel_test\"}\n")
        .await
        .unwrap();
    async fn frame(
        runtime: &mut Runtime,
        bridge: &mut Bridge,
        reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        id: &str,
    ) -> Json {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut bytes = String::new();
            loop {
                tokio::select! {
                    event = runtime.next_event() => bridge.publish(&event.unwrap()),
                    count = reader.read_line(&mut bytes) => {
                        assert!(count.unwrap()>0,"closed connection");
                        let value=Json::parse(&bytes).unwrap();
                        bytes.clear();
                        if value.text_field("id").ok()==Some(id) || value.text_field("type").unwrap()==id { return value; }
                    }
                }
            }
        }).await.expect("workspace response")
    }
    frame(&mut runtime, &mut bridge, &mut reader, "welcome").await;
    f::post(
        &handle,
        "view",
        CommandPayload::GetSnapshot(0),
        "read",
        PostStatus::Accepted,
    )
    .await;
    frame(&mut runtime, &mut bridge, &mut reader, "snapshot").await;
    // A pending initial loading snapshot can precede the requested ready view.
    f::post(
        &handle,
        "ready",
        CommandPayload::GetSnapshot(0),
        "read",
        PostStatus::Accepted,
    )
    .await;
    loop {
        let event = runtime.next_event().await.unwrap();
        let ready =
            matches!(&event.payload,EventPayload::FrontendSnapshot(s) if s.state!="loading");
        bridge.publish(&event);
        if ready {
            break;
        }
    }
    let wire = |id: &str, kind: &str, extra: &str| {
        format!(
            "{{\"version\":1,\"session\":\"{}\",\"incarnation\":\"{}\",\"id\":\"{id}\",\"kind\":\"{kind}\",{extra}}}\n",
            handle.session().0,
            handle.incarnation().0
        )
    };
    writer
        .write_all(
            wire(
                "m_1",
                "submit_message",
                r#""text":"A","turn":"m_1","target":null,"expected_mode":"basic""#,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(
        frame(&mut runtime, &mut bridge, &mut reader, "m_1")
            .await
            .text_field("status")
            .unwrap(),
        "Accepted"
    );
    let waiting = provider.clone();
    tokio::task::spawn_blocking(move || waiting.wait())
        .await
        .unwrap();
    writer
        .write_all(wire("lookup", "message_lookup", r#""message":"m_1""#).as_bytes())
        .await
        .unwrap();
    let found = frame(&mut runtime, &mut bridge, &mut reader, "lookup").await;
    let info = found.field("receipt").unwrap();
    let turn = info.text_field("turn").unwrap();
    let target = info.text_field("request").unwrap();
    writer
        .write_all(
            wire(
                "m_2",
                "submit_message",
                &format!(
                    r#""text":"B","turn":"{turn}","target":"{target}","expected_mode":"basic""#
                ),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let waiting = journal.clone();
    tokio::task::spawn_blocking(move || waiting.wait())
        .await
        .unwrap();
    f::post(
        &handle,
        "full",
        CommandPayload::Steer(Steer {
            target: target.into(),
            text: "C".into(),
        }),
        turn,
        PostStatus::QueueFull,
    )
    .await;
    writer
        .write_all(
            wire(
                "cancel",
                "cancel_message",
                &format!(r#""message":"m_1","turn":"{turn}","target":"{target}""#),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(
        frame(&mut runtime, &mut bridge, &mut reader, "cancel")
            .await
            .text_field("status")
            .unwrap(),
        "Accepted"
    );
    assert!(
        provider.stopped(),
        "Cancel waited for the normal send's journal fsync"
    );
    provider.release();
    journal.release();
    bridge.shutdown().await;
    f::finish(&mut runtime).await;
}
