#![cfg(feature = "dynamic-peers")]

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_inbox::http::{self, Response, SenderIdentity};
use agent_inbox::mesh_pair::{HostSecret, MeshRequest, ReplayGuard};

#[test]
fn signed_mesh_post_reaches_handler_once_and_replay_fails() {
    let root = std::env::temp_dir().join(format!(
        "agent-inbox-mesh-auth-{}",
        agent_inbox::state::id().unwrap()
    ));
    let alpha = HostSecret::generate(&root.join("alpha.key")).unwrap();
    let beta = HostSecret::generate(&root.join("beta.key")).unwrap();
    let sender = alpha
        .derive_keys("team", "alpha", "beta", beta.public_key(), 1)
        .unwrap();
    let receiver = beta
        .derive_keys("team", "beta", "alpha", alpha.public_key(), 1)
        .unwrap();
    let replay = Arc::new(ReplayGuard::open(root.join("replay.json")).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let running = Arc::new(AtomicBool::new(true));
    let worker_running = Arc::clone(&running);
    let auth = Arc::new(
        move |header: &[u8], method: &str, path: &str, body: &[u8], from: &str, _| {
            receiver
                .verify_request(
                    header,
                    MeshRequest {
                        method,
                        path,
                        body,
                        from,
                    },
                    1000,
                    &replay,
                )
                .ok()
                .map(|_| SenderIdentity::HostPrefix("alpha".into()))
        },
    );
    let handler = Arc::new(|path: &str, text: &str, from: Option<&str>, _| {
        assert_eq!(path, "/mesh/directory");
        assert_eq!(text, "{}");
        assert_eq!(from, Some("alpha.agent"));
        Response::new(202, "saved")
    });
    let worker = std::thread::spawn(move || {
        http::serve_mesh_authenticated(listener, worker_running, auth, handler).unwrap()
    });
    let auth = sender
        .sign_request(
            MeshRequest {
                method: "POST",
                path: "/mesh/directory",
                body: b"{}",
                from: "alpha.agent",
            },
            1000,
        )
        .unwrap();
    let send = || {
        http::post_mesh_diagnostic(
            &addr,
            "/mesh/directory",
            &auth,
            "alpha.agent",
            "{}",
            Duration::from_secs(3),
        )
        .unwrap()
        .0
    };
    assert_eq!(send(), 202);
    assert_eq!(send(), 401);
    assert_eq!(
        http::post_mesh_diagnostic(
            &addr,
            "/mesh/directory",
            &auth,
            "alpha.agent",
            "changed",
            Duration::from_secs(3),
        )
        .unwrap()
        .0,
        401
    );
    running.store(false, Ordering::Release);
    worker.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
