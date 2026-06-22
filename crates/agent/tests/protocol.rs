//! End-to-end test of the *actual compiled agent binary*: spawn it on a temp build tree and
//! drive a real Hello/Plan/Bye exchange over its stdin/stdout. This exercises the deployed
//! artifact's protocol loop + reconciliation engine together — the one seam the unit tests can't
//! reach individually (and the closest we can get to the remote without an SSH host).

use std::process::{Command, Stdio};

use rustle_agent::proto::{self, FileEntry, LinkEntry, Request, Response};

#[test]
fn agent_process_reconciles_over_stdio() {
    let root = std::env::temp_dir().join(format!("cra-agent-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("old.rs"), b"x").unwrap(); // size differs from manifest → upload
    std::fs::write(root.join("stale.rs"), b"y").unwrap(); // absent in manifest → pruned
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("target/keep"), b"big").unwrap(); // excluded → never pruned

    let mut child = Command::new(env!("CARGO_BIN_EXE_rustle-agent"))
        .arg(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn agent");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // Handshake.
    proto::write_frame(&mut stdin, &Request::Hello { proto: proto::PROTOCOL_VERSION }.encode())
        .unwrap();
    let hello = Response::decode(&proto::read_frame(&mut stdout).unwrap()).unwrap();
    assert!(matches!(hello, Response::HelloOk { .. }));

    // Plan: one changed file, one new (nested) file, one symlink; prune on.
    let plan = Request::Plan {
        include_hidden: false,
        prune: true,
        excludes: vec!["target".to_string(), ".*".to_string()],
        files: vec![
            FileEntry { rel: "old.rs".into(), size: 999, mtime: 1 },
            FileEntry { rel: "pkg/new.rs".into(), size: 1, mtime: 1 },
        ],
        links: vec![LinkEntry { rel: "l.rs".into(), target: "old.rs".into() }],
    };
    proto::write_frame(&mut stdin, &plan.encode()).unwrap();
    let resp = Response::decode(&proto::read_frame(&mut stdout).unwrap()).unwrap();

    match resp {
        Response::Worklist { uploads, pruned, symlinks, .. } => {
            assert!(uploads.contains(&"old.rs".to_string()));
            assert!(uploads.contains(&"pkg/new.rs".to_string()));
            assert_eq!(pruned, 1, "stale.rs pruned, target/ untouched");
            assert_eq!(symlinks, 1);
        }
        other => panic!("expected worklist, got {other:?}"),
    }

    proto::write_frame(&mut stdin, &Request::Bye.encode()).unwrap();
    drop(stdin);
    child.wait().unwrap();

    // The agent's filesystem effects actually happened on the remote tree.
    assert!(!root.join("stale.rs").exists(), "stale file pruned");
    assert!(root.join("target/keep").exists(), "excluded subtree preserved");
    assert!(root.join("pkg").is_dir(), "parent dir pre-created for the upload");
    assert_eq!(std::fs::read_link(root.join("l.rs")).unwrap().to_str(), Some("old.rs"));

    let _ = std::fs::remove_dir_all(&root);
}
