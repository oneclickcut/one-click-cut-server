#[path = "../common/mod.rs"]
mod common;
use common::TestApp;
use one_click_cut_server::{
    domain::{jobs, mp4_jobs},
    storage::{
        Cf,
        keys::{self, CanvasId, Uid},
    },
};
use serde_json::{Value, json};

struct F {
    app: TestApp,
    owner: String,
    worker: String,
    other: String,
    cid: String,
    hash: String,
}
impl F {
    async fn new() -> Self {
        let app = common::spawn_app_with(|c| {
            c.task_queue.reader_usernames = vec!["worker".into()];
            c.plans.basic.monthly_push_limit = 1;
        })
        .await;
        let owner = app.signup("owner", "password123").await;
        let worker = app.signup("worker", "password123").await;
        let other = app.signup("other", "password123").await;
        let p: Value = app
            .post_json_auth("/api/projects", json!({"name":"Manual export"}), &owner)
            .await
            .json()
            .await
            .unwrap();
        let cid = p["canvases"][0]["id"].as_str().unwrap().to_string();
        let board = json!({"scenes":[{"image_url":"https://assets.test/image.png","audio_url":"https://assets.test/audio.mp3","narration":"Test","audio_duration_seconds":2.0}]});
        let r: Value = app
            .put_bytes_auth(
                &format!("/api/canvases/{cid}/files/storyboard.json"),
                serde_json::to_vec(&board).unwrap(),
                &owner,
            )
            .await
            .json()
            .await
            .unwrap();
        for path in ["story.mp4", "story-no-subtitles.mp4"] {
            assert_eq!(
                app.put_bytes_auth(
                    &format!("/api/canvases/{cid}/files/{path}"),
                    b"old-video".to_vec(),
                    &owner
                )
                .await
                .status(),
                200
            );
        }
        assert_eq!(
            app.put_bytes_auth(
                &format!("/api/canvases/{cid}/files/progress.json"),
                br#"{"status":"completed","percent":100,"mp4_completed":true}"#.to_vec(),
                &owner
            )
            .await
            .status(),
            200
        );
        let f = Self {
            app,
            owner,
            worker,
            other,
            cid,
            hash: r["hash"].as_str().unwrap().into(),
        };
        f.app
            .store
            .put(
                Cf::Meta,
                keys::resource_version_key(CanvasId::parse(&f.cid).unwrap()),
                serde_json::to_vec(&json!({"storyboard_hash":f.hash,"exports_stale":true}))
                    .unwrap(),
            )
            .await
            .unwrap();
        f
    }
    async fn manifest(&self) -> Value {
        self.app
            .get_auth(&format!("/api/canvases/{}/manifest", self.cid), &self.owner)
            .await
            .json()
            .await
            .unwrap()
    }
    async fn create(&self, id: &str) -> reqwest::Response {
        self.app
            .post_json_auth(
                &format!("/api/canvases/{}/mp4-jobs", self.cid),
                json!({"request_id":id,"expected_storyboard_hash":self.hash}),
                &self.owner,
            )
            .await
    }
    async fn claim(&self) -> Value {
        self.app
            .post_json_auth(
                "/api/internal/jobs/claim",
                json!({"accepted_types":["mp4.export"]}),
                &self.worker,
            )
            .await
            .json::<Value>()
            .await
            .unwrap()["job"]
            .clone()
    }
    async fn op(&self, j: &Value, op: &str, extra: Value) -> reqwest::Response {
        let mut body = json!({"lease_token":j["lease"]["token"],"lease_epoch":j["lease"]["epoch"]});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        self.app
            .post_json_auth(
                &format!("/api/internal/jobs/{}/{op}", j["job_id"].as_str().unwrap()),
                body,
                &self.worker,
            )
            .await
    }
    async fn upload(&self, j: &Value, kind: &str, bytes: Vec<u8>) -> Value {
        let base = format!("/api/internal/jobs/{}", j["job_id"].as_str().unwrap());
        let req = |path: &str| {
            self.app
                .client
                .post(self.app.url(path))
                .bearer_auth(&self.worker)
                .header("x-job-lease", j["lease"]["token"].as_str().unwrap())
                .header("x-job-epoch", j["lease"]["epoch"].to_string())
        };
        let a: Value = req(&format!("{base}/export-upload"))
            .json(&json!({"variant":kind}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let b: Value = req(&format!("{base}/export-upload"))
            .json(&json!({"variant":kind}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(a, b);
        for (part, chunk) in bytes.chunks(16 * 1024 * 1024).enumerate() {
            let r = self
                .app
                .client
                .put(self.app.url(&format!("{base}/export-parts/{kind}/{part}")))
                .bearer_auth(&self.worker)
                .header("x-job-lease", j["lease"]["token"].as_str().unwrap())
                .header("x-job-epoch", j["lease"]["epoch"].to_string())
                .body(chunk.to_vec())
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
        }
        let r = req(&format!("{base}/export-complete"))
            .json(&json!({"variant":kind}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
        req(&format!("{base}/export-complete"))
            .json(&json!({"variant":kind}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}
fn video(n: usize, fill: u8) -> Vec<u8> {
    let mut b = vec![fill; n];
    b[..12].copy_from_slice(b"\0\0\0\x18ftypisom");
    b
}
fn file<'a>(m: &'a Value, path: &str) -> &'a Value {
    m["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .unwrap()
}

#[tokio::test]
async fn manual_export_is_idempotent_owner_only_and_publishes_both_files_atomically() {
    let f = F::new().await;
    let before = f.manifest().await;
    assert_eq!(before["exports_stale"], true);
    let r = f.create("export-request-one").await;
    assert_eq!(r.status(), 201);
    let created: Value = r.json().await.unwrap();
    assert_eq!(f.create("export-request-one").await.status(), 200);
    assert_eq!(f.create("export-request-other").await.status(), 409);
    assert_eq!(
        f.app
            .post_json_auth(
                &format!("/api/canvases/{}/mp4-jobs", f.cid),
                json!({"request_id":"another-user-request","expected_storyboard_hash":f.hash}),
                &f.other
            )
            .await
            .status(),
        404
    );
    let resource: Value = f
        .app
        .get_auth(
            &format!("/api/jobs?canvas_id={}&type=resource.regenerate", f.cid),
            &f.owner,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(resource["jobs"], json!([]));
    let ignored: Value = f
        .app
        .post_json_auth(
            "/api/internal/jobs/claim",
            json!({"accepted_types":["resource.regenerate"]}),
            &f.worker,
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(ignored["job"].is_null());
    let job = f.claim().await;
    assert_eq!(job["job_id"], created["job_id"]);
    assert_eq!(
        f.op(
            &job,
            "export-progress",
            json!({"progress":{"mp4":{"status":"encoding","percent":80}}})
        )
        .await
        .status(),
        200
    );
    let a = f.upload(&job, "mp4", video(16 * 1024 * 1024 + 32, 5)).await;
    assert_eq!(f.op(&job, "commit", json!({})).await.status(), 400);
    let mid = f.manifest().await;
    assert_eq!(file(&before, "story.mp4"), file(&mid, "story.mp4"));
    assert_eq!(
        file(&before, "story-no-subtitles.mp4"),
        file(&mid, "story-no-subtitles.mp4")
    );
    let b = f.upload(&job, "mp4_no_subtitles", video(1024, 6)).await;
    let r = f.op(&job, "commit", json!({})).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert_eq!(f.op(&job, "commit", json!({})).await.status(), 200);
    let after = f.manifest().await;
    let old_hash =
        keys::ContentHash::parse_hex(file(&before, "story.mp4")["hash"].as_str().unwrap()).unwrap();
    assert_eq!(
        f.app
            .store
            .counter(Cf::BlobMeta, keys::blob_ref_key(old_hash))
            .await
            .unwrap(),
        0
    );
    assert!(
        !f.app
            .store
            .scan_prefix(Cf::Gc, vec![])
            .await
            .unwrap()
            .is_empty()
    );

    assert_eq!(file(&after, "story.mp4")["hash"], a["hash"]);
    assert_eq!(file(&after, "story-no-subtitles.mp4")["hash"], b["hash"]);
    assert_eq!(
        file(&before, "progress.json"),
        file(&after, "progress.json")
    );
    assert_eq!(after["mp4_export"]["storyboard_hash"], f.hash);
    assert_ne!(after["exports_stale"], true);
    assert!(
        !after["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["path"].as_str().unwrap().starts_with("exports/"))
    );
    let me: Value = f
        .app
        .get_auth("/api/me", &f.owner)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["push_used"], 0);
    let uid = Uid::parse(me["uid"].as_str().unwrap()).unwrap();
    let canvas = one_click_cut_server::domain::space::get_canvas(
        &f.app.store,
        uid,
        CanvasId::parse(&f.cid).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        canvas.file_count as usize,
        after["files"].as_array().unwrap().len()
    );
    assert_eq!(
        canvas.bytes,
        after["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["size"].as_u64().unwrap())
            .sum::<u64>()
    );
    assert!(
        f.app
            .store
            .get(
                Cf::Meta,
                keys::resource_version_key(CanvasId::parse(&f.cid).unwrap())
            )
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        f.app
            .put_bytes_auth(
                &format!("/api/canvases/{}/files/storyboard.json", f.cid),
                b"{}".to_vec(),
                &f.owner
            )
            .await
            .status(),
        409
    );
    // A later resource update marks the exported version stale again without touching either video.
    let r = f
        .app
        .put_bytes_auth(
            &format!(
                "/api/canvases/{}/files/storyboard.json?expected_hash={}",
                f.cid, f.hash
            ),
            b"{\"changed\":true}".to_vec(),
            &f.owner,
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(f.manifest().await["exports_stale"], true);
}
#[tokio::test]
async fn failure_and_lost_lease_preserve_old_exports_and_cleanup_candidates() {
    let f = F::new().await;
    let before = f.manifest().await;
    assert_eq!(f.create("failed-export-one").await.status(), 201);
    let job = f.claim().await;
    f.upload(&job, "mp4", video(128, 7)).await;
    assert_eq!(
        f.op(&job, "fail", json!({"error":"mp4_recording_failed"}))
            .await
            .status(),
        200
    );
    assert_eq!(f.op(&job, "commit", json!({})).await.status(), 409);
    let after = f.manifest().await;
    assert_eq!(file(&before, "story.mp4"), file(&after, "story.mp4"));
    assert!(
        !after["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["path"].as_str().unwrap().starts_with("exports/"))
    );
    assert_eq!(f.create("failed-export-two").await.status(), 201);
    let job = f.claim().await;
    f.upload(&job, "mp4", video(128, 8)).await;
    let expires = job["lease"]["expires_at"].as_u64().unwrap();
    jobs::sweep(&f.app.store, expires + 1).await.unwrap();
    let persisted = jobs::get(&f.app.store, job["job_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(persisted.status, "failed");
    assert_eq!(
        persisted.error.as_deref(),
        Some("export_worker_interrupted")
    );
    assert!(f.claim().await.is_null());
    assert_eq!(
        mp4_jobs::state(&f.app.store, CanvasId::parse(&f.cid).unwrap())
            .await
            .unwrap()["exports_stale"],
        true
    );
    assert_eq!(
        file(&before, "story.mp4"),
        file(&f.manifest().await, "story.mp4")
    );
}

#[tokio::test]
async fn queued_export_rejects_a_changed_storyboard_without_touching_videos() {
    let f = F::new().await;
    let before = f.manifest().await;
    assert_eq!(f.create("changed-input-export").await.status(), 201);
    let response = f
        .app
        .put_bytes_auth(
            &format!(
                "/api/canvases/{}/files/storyboard.json?expected_hash={}",
                f.cid, f.hash
            ),
            b"{}".to_vec(),
            &f.owner,
        )
        .await;
    assert_eq!(response.status(), 200);
    assert!(f.claim().await.is_null());
    let jobs: Value = f
        .app
        .get_auth(
            &format!("/api/jobs?canvas_id={}&type=mp4.export", f.cid),
            &f.owner,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(jobs["jobs"][0]["status"], "conflict");
    assert_eq!(
        file(&before, "story.mp4"),
        file(&f.manifest().await, "story.mp4")
    );
}
