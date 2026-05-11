//! Integration tests for `cluster::gossip::ping_all_alive`.
//!
//! Spins up small in-process axum mock servers that answer
//! `v3/cluster.ping` over JSON-RPC, seeds a [`SharedPeerTable`] with two
//! Alive peers pointing at those servers, and asserts the fan-out
//! refreshes both peers' `last_seen` (and bumps `miss_count` on a peer
//! whose URL points nowhere).
//!
//! These tests are what verifies the gossip-flapping fix: the previous
//! `tick()` pinged a single random Alive peer per tick, so in an N-peer
//! cluster each peer's `last_seen` only refreshed ~1/N of the time and
//! peers regularly aged past `suspect_timeout`.  After the refactor
//! every Alive peer is pinged in parallel each tick.

use bdslib::cluster::gossip::{ping_all_alive, PingFanOutResult};
use bdslib::cluster::peer_table::{Peer, PeerState, PeerTable, SharedPeerTable};
use parking_lot::RwLock;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Build a one-route axum app that always answers `v3/cluster.ping`
/// with a successful JSON-RPC envelope reporting `node_id` and a fixed
/// timestamp.  HMAC verification is intentionally *not* done by the
/// mock — the test exercises the client-side fan-out, not the
/// server-side auth path.
fn mock_app(node_id: String) -> axum::Router {
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};

    Router::new().route(
        "/",
        post(move |body: Json<Value>| {
            let nid = node_id.clone();
            async move {
                let id = body.0.get("id").cloned().unwrap_or(json!(1));
                Json(json!({
                    "jsonrpc": "2.0",
                    "id":      id,
                    "result":  { "node_id": nid, "ts": 1_700_000_000_u64 },
                }))
            }
        }),
    )
}

/// Bind an ephemeral port, spawn the mock, return its base URL plus a
/// JoinHandle the caller can `abort()` to shut the server down.
async fn spawn_mock(node_id: String) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    let app = mock_app(node_id);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Give the listener a moment to start accepting (axum::serve enters
    // the accept loop on the first poll).
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), handle)
}

/// Allocate a TCP port and immediately drop the listener.  reqwest will
/// see "connection refused" on the next call — a deterministic failure
/// without depending on a reserved port number.
async fn dead_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    format!("http://{addr}")
}

fn seed_table(peers: Vec<(Uuid, String)>) -> SharedPeerTable {
    let table = Arc::new(RwLock::new(PeerTable::new(Uuid::now_v7())));
    {
        let mut t = table.write();
        for (id, url) in peers {
            let mut p = Peer::new(id, url);
            // Mark Alive but with a deliberately stale last_seen=0 so
            // we can detect refresh.  miss_count=0 lets us see bumps.
            p.state      = PeerState::Alive;
            p.last_seen  = 0;
            p.miss_count = 0;
            t.upsert(p);
        }
    }
    table
}

#[tokio::test]
async fn ping_fan_out_refreshes_all_alive_peers() {
    let id_a = Uuid::now_v7();
    let id_b = Uuid::now_v7();
    let (url_a, h_a) = spawn_mock(id_a.to_string()).await;
    let (url_b, h_b) = spawn_mock(id_b.to_string()).await;

    let table = seed_table(vec![(id_a, url_a.clone()), (id_b, url_b.clone())]);
    let http  = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("reqwest client");

    let PingFanOutResult { succeeded, last_ok_url } =
        ping_all_alive(&table, &http, "test-secret", Duration::from_secs(2)).await;

    assert_eq!(succeeded, 2, "both mock peers should answer");
    let ok = last_ok_url.expect("at least one peer answered");
    assert!(ok == url_a || ok == url_b, "last_ok_url should be one of the two mocks, got {ok}");

    let snap = table.read().snapshot();
    assert_eq!(snap.len(), 2);
    for p in &snap {
        assert_eq!(p.state, PeerState::Alive, "peer {} should still be Alive", p.url);
        assert!(p.last_seen > 0, "peer {} last_seen should be refreshed", p.url);
        assert_eq!(p.miss_count, 0, "peer {} miss_count should reset", p.url);
    }

    h_a.abort();
    h_b.abort();
}

#[tokio::test]
async fn ping_fan_out_records_miss_on_failure() {
    let id_good = Uuid::now_v7();
    let id_bad  = Uuid::now_v7();
    let (url_good, h_good) = spawn_mock(id_good.to_string()).await;
    let url_bad = dead_url().await;

    let table = seed_table(vec![
        (id_good, url_good.clone()),
        (id_bad,  url_bad.clone()),
    ]);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("reqwest client");

    let PingFanOutResult { succeeded, last_ok_url } =
        ping_all_alive(&table, &http, "test-secret", Duration::from_secs(2)).await;

    assert_eq!(succeeded, 1, "only the running mock should answer");
    assert_eq!(last_ok_url.as_deref(), Some(url_good.as_str()),
        "last_ok_url should be the reachable mock");

    let snap = table.read().snapshot();
    let good = snap.iter().find(|p| p.node_id == id_good).expect("good peer present");
    let bad  = snap.iter().find(|p| p.node_id == id_bad ).expect("bad peer present");

    assert!(good.last_seen > 0, "good peer last_seen should be refreshed");
    assert_eq!(good.miss_count, 0, "good peer miss_count should reset");

    assert_eq!(bad.last_seen, 0, "bad peer last_seen should stay stale");
    assert_eq!(bad.miss_count, 1, "bad peer miss_count should bump by 1");

    h_good.abort();
}
