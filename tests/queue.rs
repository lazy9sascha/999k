use radio999k::{AckResult, Envelope, Error, Queue};
use std::{
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};
fn envelope(to: &str) -> Envelope {
    Envelope {
        sender: "a".into(),
        recipient: to.into(),
        body: "hello".into(),
        correlation_id: Some("request-1".into()),
        in_reply_to: None,
        idempotency_key: Some("send-1".into()),
    }
}
#[test]
fn durable_reopen_isolation_and_repeated_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let sent = Queue::open(&path).unwrap().send(envelope("b")).unwrap();
    let mut q = Queue::open(&path).unwrap();
    assert!(q.receive("c", Duration::from_secs(60)).unwrap().is_none());
    let delivery = q.receive("b", Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!(delivery.message, sent.message);
    assert_eq!(delivery.attempt, 1);
    drop(q);
    let mut q = Queue::open(&path).unwrap();
    assert!(q.receive("b", Duration::from_secs(60)).unwrap().is_none());
    assert!(matches!(
        q.ack("c", &sent.message.id, &delivery.receipt),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        q.ack("b", &sent.message.id, "wrong"),
        Err(Error::StaleReceipt)
    ));
    assert_eq!(
        q.ack("b", &sent.message.id, &delivery.receipt).unwrap(),
        AckResult::Acknowledged
    );
    drop(q);
    let mut q = Queue::open(&path).unwrap();
    assert_eq!(
        q.ack("b", &sent.message.id, &delivery.receipt).unwrap(),
        AckResult::AlreadyAcknowledged
    );
    assert!(q.receive("b", Duration::from_secs(60)).unwrap().is_none());
    assert!(q.send(envelope("b")).unwrap().duplicate);
}
#[test]
fn idempotency_conflict_and_scope() {
    let mut q = Queue::open(":memory:").unwrap();
    let e = envelope("b");
    let first = q.send(e.clone()).unwrap();
    let replay = q.send(e.clone()).unwrap();
    assert!(replay.duplicate);
    assert_eq!(first.message, replay.message);
    for changed in [
        Envelope {
            body: "changed".into(),
            ..e.clone()
        },
        Envelope {
            correlation_id: Some("different".into()),
            ..e.clone()
        },
        Envelope {
            in_reply_to: Some("other".into()),
            ..e.clone()
        },
    ] {
        assert!(matches!(q.send(changed), Err(Error::Conflict)));
    }
    assert!(!q.send(envelope("c")).unwrap().duplicate);
    assert!(
        !q.send(Envelope {
            sender: "another-sender".into(),
            ..e.clone()
        })
        .unwrap()
        .duplicate
    );
    let mut unkeyed = e;
    unkeyed.idempotency_key = None;
    assert_ne!(
        q.send(unkeyed.clone()).unwrap().message.id,
        q.send(unkeyed).unwrap().message.id
    );
}
#[test]
fn request_reply_is_independent_of_ack() {
    let mut q = Queue::open(":memory:").unwrap();
    let request = q.send(envelope("b")).unwrap().message;
    let reply = q
        .send(Envelope {
            sender: "b".into(),
            recipient: "a".into(),
            body: "result".into(),
            correlation_id: request.envelope.correlation_id.clone(),
            in_reply_to: Some(request.id.clone()),
            idempotency_key: Some("reply-1".into()),
        })
        .unwrap()
        .message;
    let got = q.receive("a", Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!(got.message, reply);
    assert_eq!(got.message.envelope.in_reply_to, Some(request.id));
    assert!(q.receive("b", Duration::from_secs(60)).unwrap().is_some());
}
#[test]
fn concurrent_connections_claim_each_message_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let mut q = Queue::open(&path).unwrap();
    for i in 0..16 {
        let mut e = envelope("b");
        e.idempotency_key = Some(format!("key-{i}"));
        q.send(e).unwrap();
    }
    let barrier = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let mut q = Queue::open(path).unwrap();
                barrier.wait();
                let mut ids = Vec::new();
                while let Some(delivery) = q.receive("b", Duration::from_secs(60)).unwrap() {
                    ids.push(delivery.message.id);
                }
                ids
            })
        })
        .collect();
    let ids: Vec<_> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    assert_eq!(ids.len(), 16);
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 16);
}
#[test]
fn rejects_invalid_input_and_unrelated_database() {
    let mut q = Queue::open(":memory:").unwrap();
    assert!(q.receive("b", Duration::ZERO).is_err());
    let mut e = envelope("b");
    e.sender = " ".into();
    assert!(q.send(e).is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other.db");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute("CREATE TABLE user_data(value TEXT)", [])
        .unwrap();
    drop(connection);
    assert!(Queue::open(&path).is_err());
    let connection = rusqlite::Connection::open(path).unwrap();
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='messages'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn concurrent_keyed_sends_return_one_durable_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    Queue::open(&path).unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let mut q = Queue::open(path).unwrap();
                barrier.wait();
                q.send(envelope("b")).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| !r.duplicate).count(), 1);
    assert!(results.iter().all(|r| r.message == results[0].message));
    let mut q = Queue::open(path).unwrap();
    assert!(q.receive("b", Duration::from_secs(60)).unwrap().is_some());
    assert!(q.receive("b", Duration::from_secs(60)).unwrap().is_none());
}
