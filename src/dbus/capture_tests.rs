use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use kora_capture_client::engine::{PATH, Recording};

use super::*;
use crate::dbus::private_bus::PrivateBus;

/// Plays the engine: the recordings it lists, and the stops it answers as the
/// real one does.
#[derive(Clone, Default)]
struct StandIn(Arc<Mutex<Vec<Recording>>>);

impl StandIn {
    fn recording(id: &str) -> Self {
        let engine = Self::default();
        engine.0.lock().unwrap().push(Recording {
            id: id.to_owned(),
            source: "toplevel".to_owned(),
            path: format!("/captures/{id}.mp4"),
            started: 1_700_000_000,
        });
        engine
    }

    /// Lose every recording without a word, as a stalled stop did.
    fn drop_silently(&self) {
        self.0.lock().unwrap().clear();
    }
}

#[zbus::interface(name = "inc.kora.CaptureEngine1")]
impl StandIn {
    fn stop_recording(&self, id: String) -> zbus::fdo::Result<String> {
        if id == "stalled" {
            return Err(zbus::fdo::Error::Failed("the recording stalled".into()));
        }
        let mut listed = self.0.lock().unwrap();
        let at = listed
            .iter()
            .position(|recording| recording.id == id)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("no recording {id}")))?;
        Ok(listed.remove(at).path)
    }

    fn list_recordings(&self) -> Vec<Recording> {
        self.0.lock().unwrap().clone()
    }
}

async fn serve(bus: &PrivateBus, engine: &StandIn) -> zbus::Connection {
    zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .serve_at(PATH, engine.clone())
        .unwrap()
        .name(BUS_NAME)
        .unwrap()
        .build()
        .await
        .unwrap()
}

/// Follow the engine on `bus` from a thread of its own; what it hands on, and
/// where to say a recording was lost.
fn follow(
    bus: &PrivateBus,
) -> (
    impl Fn() -> Mirrored,
    futures_channel::mpsc::UnboundedSender<String>,
) {
    let client = zbus::block_on(bus.connect());
    let (forget, forgotten) = futures_channel::mpsc::unbounded();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        zbus::block_on(follow_recordings(&client, forgotten, |mirrored| {
            tx.send(mirrored).is_ok()
        }))
    });
    let next = move || {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the follower said nothing")
    };
    (next, forget)
}

fn listing(ids: &[&str]) -> Option<(u64, Vec<String>)> {
    let since = if ids.is_empty() { 0 } else { 1_700_000_000 };
    Some((since, ids.iter().map(|&id| id.to_owned()).collect()))
}

// Only the engine's InvalidArgs means "no such recording"; anything else is a
// real failure and must still say so.
#[test]
fn only_the_engine_s_unknown_id_answer_is_no_such_recording() {
    let Some(bus) = PrivateBus::start() else {
        eprintln!("no dbus-daemon; skipping");
        return;
    };
    zbus::block_on(async {
        let client = bus.connect().await;
        assert!(
            matches!(stop(&client, "rec-1").await, Err(StopFailure::Failed(_))),
            "no engine at all is a failure, not an ending"
        );
        let _engine = serve(&bus, &StandIn::recording("rec-1")).await;
        assert_eq!(
            stop(&client, "rec-1").await,
            Ok("/captures/rec-1.mp4".into())
        );
        assert_eq!(
            stop(&client, "rec-1").await,
            Err(StopFailure::NoSuchRecording)
        );
        let Err(StopFailure::Failed(error)) = stop(&client, "stalled").await else {
            panic!("a stalled stop is a failure");
        };
        assert!(error.contains("the recording stalled"), "{error}");
    });
}

// The report's case: the engine dropped a recording without RecordingStopped,
// so nothing refreshed game mode's list until a stop learned it was gone.
#[test]
fn a_recording_the_engine_lost_leaves_the_list_without_a_failure() {
    let Some(bus) = PrivateBus::start() else {
        eprintln!("no dbus-daemon; skipping");
        return;
    };
    let engine = StandIn::recording("rec-1");
    let _service = zbus::block_on(serve(&bus, &engine));
    let (next, forget) = follow(&bus);
    assert_eq!(
        next(),
        Mirrored {
            listed: listing(&["rec-1"]),
            ended: Vec::new(),
        }
    );

    engine.drop_silently();
    forget.unbounded_send("rec-1".into()).unwrap();
    assert_eq!(
        next(),
        Mirrored {
            listed: listing(&[]),
            ended: Vec::new(),
        },
        "gone from the list, and no RecordingEnded failure on top of the stop's answer"
    );
}

#[test]
fn an_engine_that_goes_away_ends_every_recording_it_listed() {
    let Some(bus) = PrivateBus::start() else {
        eprintln!("no dbus-daemon; skipping");
        return;
    };
    let service = zbus::block_on(serve(&bus, &StandIn::recording("rec-1")));
    let (next, _forget) = follow(&bus);
    assert_eq!(next().listed, listing(&["rec-1"]));

    // Closed, not released: what a crash looks like from the bus.
    zbus::block_on(service.close()).unwrap();
    assert_eq!(
        next(),
        Mirrored {
            listed: listing(&[]),
            ended: vec![(
                "rec-1".into(),
                "/captures/rec-1.mp4".into(),
                "the recorder stopped".into()
            )],
        }
    );

    // A restarted engine starts empty; nothing is ended twice.
    let _restarted = zbus::block_on(serve(&bus, &StandIn::default()));
    assert_eq!(
        next(),
        Mirrored {
            listed: listing(&[]),
            ended: Vec::new(),
        }
    );
}
