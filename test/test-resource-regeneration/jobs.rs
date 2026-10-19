#[path = "../common/mod.rs"]
mod common;
use common::TestApp;
use one_click_cut_server::{
    domain::jobs,
    storage::{
        Batch, Cf,
        keys::{CanvasId, ProjectId},
    },
};
use serde_json::{Value, json};

struct Fixture {
    app: TestApp,
    owner: String,
    worker: String,
    other: String,
    pid: String,
    cid: String,
    rid: String,
    hash: String,
    board: Value,
}
async fn fixture() -> Fixture {
    let app = common::spawn_app_with(|c| {
        c.task_queue.reader_usernames = vec!["worker".into()];
        c.plans.basic.monthly_push_limit = 10;
    })
    .await;
    let owner = app.signup("owner", "password123").await;
    let worker = app.signup("worker", "password123").await;
    let other = app.signup("other", "password123").await;
    let p: Value = app
        .post_json_auth("/api/projects", json!({"name":"资源测试"}), &owner)
        .await
        .json()
        .await
        .unwrap();
    let pid = p["id"].as_str().unwrap().to_string();
    let cid = p["canvases"][0]["id"].as_str().unwrap().to_string();
    let rid = p["resources"]["id"].as_str().unwrap().to_string();
    let base = p["canvases"][0]["base_url"].as_str().unwrap();
    let scene = json!({"image_prompt":{"subjects":"人物","environment":"公园","composition":"近景"},"references":[],"narration":"一段旁白","image_url":format!("https://api.example.test{base}old.png"),"audio_url":format!("https://api.example.test{base}old.mp3"),"audio_duration_seconds":12.0});
    let board = json!({"title":"保留创作字段","scenes":[scene.clone(),scene]});
    let r = app
        .put_bytes_auth(
            &format!("/api/canvases/{cid}/files/storyboard.json"),
            serde_json::to_vec(&board).unwrap(),
            &owner,
        )
        .await;
    assert_eq!(r.status(), 200);
    let f: Value = r.json().await.unwrap();
    let hash = f["hash"].as_str().unwrap().to_string();
    let r=app.put_bytes_auth(&format!("/api/canvases/{rid}/files/constraints.json"),serde_json::to_vec(&json!({"style":{"mode":"text","aspect_ratio":"1:1","resolution":"1k"},"narration_voice":{"voice_id":"voice"}})).unwrap(),&owner).await;
    assert_eq!(r.status(), 200);
    Fixture {
        app,
        owner,
        worker,
        other,
        pid,
        cid,
        rid,
        hash,
        board,
    }
}
impl Fixture {
    fn request(&self, key: &str, kind: &str) -> Value {
        json!({"request_id":key,"resource_type":kind,"scene_index":1,"expected_storyboard_hash":self.hash})
    }
    async fn create(&self, key: &str, kind: &str) -> Value {
        let r = self
            .app
            .post_json_auth(
                &format!("/api/canvases/{}/resource-jobs", self.cid),
                self.request(key, kind),
                &self.owner,
            )
            .await;
        assert_eq!(r.status(), 201, "{}", r.text().await.unwrap_or_default());
        self.app
            .get_auth(&format!("/api/jobs?canvas_id={}", self.cid), &self.owner)
            .await
            .json::<Value>()
            .await
            .unwrap()["jobs"][0]
            .clone()
    }
    async fn claim(&self) -> Value {
        self.app
            .post_json_auth(
                "/api/internal/jobs/claim",
                json!({"accepted_types":["resource.regenerate"]}),
                &self.worker,
            )
            .await
            .json::<Value>()
            .await
            .unwrap()["job"]
            .clone()
    }
    async fn op(&self, j: &Value, name: &str, fields: Value) -> reqwest::Response {
        let mut r = json!({"lease_token":j["lease"]["token"],"lease_epoch":j["lease"]["epoch"]});
        for (k, v) in fields.as_object().unwrap() {
            r[k] = v.clone();
        }
        self.app
            .post_json_auth(
                &format!(
                    "/api/internal/jobs/{}/{name}",
                    j["job_id"].as_str().unwrap()
                ),
                r,
                &self.worker,
            )
            .await
    }
    async fn upload(&self, j: &Value, audio: bool) -> Value {
        let (ext, bytes) = if audio {
            ("mp3", b"ID3\x04\0\0\0\0\0\0test-audio-bytes".to_vec())
        } else {
            ("png",hex::decode("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000b49444154789c636000020000050001a5f645400000000049454e44ae426082").unwrap())
        };
        let url = format!(
            "/api/internal/jobs/{}/candidate?extension={ext}{}",
            j["job_id"].as_str().unwrap(),
            if audio {
                "&audio_duration_seconds=13.2"
            } else {
                ""
            }
        );
        let r = self
            .app
            .client
            .put(self.app.url(&url))
            .bearer_auth(&self.worker)
            .header("x-job-lease", j["lease"]["token"].as_str().unwrap())
            .header("x-job-epoch", j["lease"]["epoch"].as_u64().unwrap())
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
        jobs::get(&self.app.store, j["job_id"].as_str().unwrap())
            .await
            .unwrap()
            .candidate
            .unwrap()
    }
    async fn expire(&self, j: &Value) {
        let mut saved = jobs::get(&self.app.store, j["job_id"].as_str().unwrap())
            .await
            .unwrap();
        saved.lease.as_mut().unwrap().expires_at = 0;
        let mut b = Batch::new();
        jobs::stage_job(&mut b, &saved).unwrap();
        self.app.store.write(b).await.unwrap();
        jobs::sweep(&self.app.store, one_click_cut_server::util::now_secs())
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn 幂等额度隔离权限和原子音频替换() {
    let f = fixture().await;
    let first = f.create("request-one", "audio").await;
    let url = format!("/api/canvases/{}/resource-jobs", f.cid);
    let duplicate = f
        .app
        .post_json_auth(&url, f.request("request-one", "audio"), &f.owner)
        .await;
    assert_eq!(duplicate.status(), 200);
    assert_eq!(
        duplicate.json::<Value>().await.unwrap()["job_id"],
        first["job_id"]
    );
    assert_eq!(
        f.app
            .post_json_auth(&url, f.request("request-one", "image"), &f.owner)
            .await
            .status(),
        409
    );
    assert_eq!(
        f.app
            .post_json_auth(&url, f.request("request-two", "audio"), &f.owner)
            .await
            .status(),
        409
    );
    assert_eq!(
        f.app
            .post_json_auth(&url, f.request("request-three", "audio"), &f.other)
            .await
            .status(),
        404
    );
    let old = f
        .app
        .post_json_auth("/api/tasks/pop", json!({}), &f.worker)
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(old["tasks"], json!([]));
    let me = f
        .app
        .get_auth("/api/me", &f.owner)
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(me["push_used"], 1);
    assert_eq!(
        f.app
            .get_auth(
                &format!("/api/jobs/{}", first["job_id"].as_str().unwrap()),
                &f.other
            )
            .await
            .status(),
        404
    );
    let j = f.claim().await;
    assert_eq!(j["job_id"], first["job_id"]);
    assert!(f.claim().await.is_null());
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
    assert_eq!(
        f.app
            .put_bytes_auth(
                &format!("/api/canvases/{}/files/constraints.json", f.rid),
                b"{}".to_vec(),
                &f.owner
            )
            .await
            .status(),
        409
    );
    assert_eq!(
        f.op(&j, "checkpoint", json!({"stage":"generating"}))
            .await
            .status(),
        200
    );
    let candidate = f.upload(&j, true).await;
    let out = f.op(&j, "commit", json!({})).await;
    assert_eq!(out.status(), 200);
    let result: Value = out.json().await.unwrap();
    assert_eq!(result["status"], "succeeded");
    assert_eq!(
        f.op(&j, "commit", json!({}))
            .await
            .json::<Value>()
            .await
            .unwrap(),
        result
    );
    let new = jobs::read_json(
        &f.app.store,
        CanvasId::parse(&f.cid).unwrap(),
        "storyboard.json",
    )
    .await
    .unwrap();
    assert_eq!(new["scenes"][0], f.board["scenes"][0]);
    assert_eq!(
        new["scenes"][1]["image_prompt"],
        f.board["scenes"][1]["image_prompt"]
    );
    assert_eq!(
        new["scenes"][1]["image_url"],
        f.board["scenes"][1]["image_url"]
    );
    assert_eq!(new["scenes"][1]["audio_duration_seconds"], 13.2);
    assert!(
        new["scenes"][1]["audio_url"]
            .as_str()
            .unwrap()
            .contains(candidate["path"].as_str().unwrap())
    );
    let public = f
        .app
        .get_auth(
            &format!("/api/jobs/{}", first["job_id"].as_str().unwrap()),
            &f.owner,
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert!(public.get("lease").is_none());
    assert!(public.get("input").is_none());
    let manifest = f
        .app
        .get_auth(&format!("/api/canvases/{}/manifest", f.cid), &f.owner)
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(manifest["exports_stale"], true);
    assert_eq!(
        f.app
            .put_bytes_auth(
                &format!("/api/canvases/{}/files/storyboard.json", f.cid),
                serde_json::to_vec(&f.board).unwrap(),
                &f.owner
            )
            .await
            .status(),
        409
    );
    assert_eq!(
        f.app
            .put_bytes_auth(
                &format!(
                    "/api/canvases/{}/files/storyboard.json?expected_hash={}",
                    f.cid, f.hash
                ),
                serde_json::to_vec(&f.board).unwrap(),
                &f.owner
            )
            .await
            .status(),
        409
    );
}
#[tokio::test]
async fn 崩溃恢复区分未调用已调用和已上传并阻止旧租约() {
    let f = fixture().await;
    f.create("recovery-one", "image").await;
    let j = f.claim().await;
    f.expire(&j).await;
    let second = f.claim().await;
    assert_eq!(j["job_id"], second["job_id"]);
    assert_eq!(second["lease"]["epoch"], 2);
    assert_eq!(
        f.op(&j, "checkpoint", json!({"stage":"generating"}))
            .await
            .status(),
        409
    );
    assert_eq!(
        f.op(&second, "checkpoint", json!({"stage":"generating"}))
            .await
            .status(),
        200
    );
    f.upload(&second, false).await;
    f.expire(&second).await;
    let third = f.claim().await;
    assert_eq!(third["stage"], "uploaded");
    assert!(!third["candidate"].is_null());
    assert_eq!(f.op(&third, "commit", json!({})).await.status(), 200);
    let current = jobs::get(&f.app.store, third["job_id"].as_str().unwrap())
        .await
        .unwrap();
    let mut req = f.request("ambiguous-two", "audio");
    req["expected_storyboard_hash"] = current.result.unwrap()["storyboard_hash"].clone();
    assert_eq!(
        f.app
            .post_json_auth(
                &format!("/api/canvases/{}/resource-jobs", f.cid),
                req,
                &f.owner
            )
            .await
            .status(),
        201
    );
    let ambiguous = f.claim().await;
    assert_eq!(
        f.op(&ambiguous, "checkpoint", json!({"stage":"generating"}))
            .await
            .status(),
        200
    );
    f.expire(&ambiguous).await;
    assert_eq!(
        jobs::get(&f.app.store, ambiguous["job_id"].as_str().unwrap())
            .await
            .unwrap()
            .status,
        "needs_review"
    );
    assert!(f.claim().await.is_null());
}
#[tokio::test]
async fn 输入冲突不覆盖成果且旧整集共享执行锁() {
    let f = fixture().await;
    let t: Value = f
        .app
        .post_json_auth(
            &format!("/api/projects/{}/agent-tokens", f.pid),
            json!({"canvas_files_write":true,"constraints_write":true}),
            &f.owner,
        )
        .await
        .json()
        .await
        .unwrap();
    let tid = one_click_cut_server::storage::keys::TaskId::new().to_string();
    let route = format!("/api/internal/projects/{}/execution-lease", f.pid);
    let legacy: Value = f
        .app
        .post_json_auth(
            &route,
            json!({"task_id":tid,"agent_token":t["token"]}),
            &f.worker,
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(legacy["token"].is_string());
    f.create("legacy-lock-test", "image").await;
    assert!(f.claim().await.is_null());
    assert_eq!(
        f.app
            .put_bytes_auth(
                &format!("/api/canvases/{}/files/other.txt", f.cid),
                b"legacy writer".to_vec(),
                t["token"].as_str().unwrap()
            )
            .await
            .status(),
        200
    );
    let release = json!({"task_id":tid,"lease_token":legacy["token"]});
    assert_eq!(
        f.app
            .post_json_auth(&format!("{route}/release"), release, &f.worker)
            .await
            .status(),
        200
    );
    assert_eq!(
        f.app
            .put_bytes_auth(
                &format!("/api/canvases/{}/files/late.txt", f.cid),
                b"late writer".to_vec(),
                t["token"].as_str().unwrap()
            )
            .await
            .status(),
        409
    );
    let j = f.claim().await;
    assert!(j["job_id"].is_string());
    assert_eq!(
        f.op(&j, "checkpoint", json!({"stage":"generating"}))
            .await
            .status(),
        200
    );
    f.upload(&j, false).await;
    // Emulate an administrative/storage writer that bypassed HTTP. CAS still protects the result.
    let cid = CanvasId::parse(&f.cid).unwrap();
    let saved = jobs::get(&f.app.store, j["job_id"].as_str().unwrap())
        .await
        .unwrap();
    let mut changed = f.board.clone();
    changed["title"] = json!("外部修改");
    one_click_cut_server::domain::canvas::put_file(
        &f.app.store,
        saved.owner_uid,
        cid,
        "storyboard.json",
        "application/json",
        &serde_json::to_vec(&changed).unwrap(),
        1,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(f.op(&j, "commit", json!({})).await.status(), 409);
    assert_eq!(
        jobs::read_json(&f.app.store, cid, "storyboard.json")
            .await
            .unwrap(),
        changed
    );
    assert_eq!(
        f.op(&j, "fail", json!({"error":"input_version_changed"}))
            .await
            .status(),
        200
    );
    assert!(
        jobs::execution(&f.app.store, ProjectId::parse(&f.pid).unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.app
            .store
            .scan_prefix(Cf::TaskQueue, b"")
            .await
            .unwrap()
            .len(),
        0
    );
}
#[tokio::test]
async fn 排队后新增输入会冲突且待核对回执不泄露给普通用户() {
    let f = fixture().await;
    let first = f.create("new-input-test", "image").await;
    assert_eq!(
        f.app
            .put_bytes_auth(
                &format!("/api/canvases/{}/files/reference-assets.json", f.rid),
                b"[]".to_vec(),
                &f.owner
            )
            .await
            .status(),
        200
    );
    assert!(f.claim().await.is_null());
    assert_eq!(
        jobs::get(&f.app.store, first["job_id"].as_str().unwrap())
            .await
            .unwrap()
            .status,
        "conflict"
    );
    f.create("receipt-test", "audio").await;
    let j = f.claim().await;
    assert_eq!(
        f.op(&j, "checkpoint", json!({"stage":"generating"}))
            .await
            .status(),
        200
    );
    assert_eq!(
        f.op(
            &j,
            "fail",
            json!({"error":"result_unknown","provider_receipt":{"api_key":"do-not-store"}})
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        f.op(
            &j,
            "fail",
            json!({"error":"result_unknown","provider_receipt":{"task_id":"provider-9"}})
        )
        .await
        .status(),
        200
    );
    let stored = jobs::get(&f.app.store, j["job_id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(stored.status, "needs_review");
    assert_eq!(stored.input["provider_receipt"]["task_id"], "provider-9");
    let public = f
        .app
        .get_auth(
            &format!("/api/jobs/{}", j["job_id"].as_str().unwrap()),
            &f.owner,
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert!(public.get("provider_receipt").is_none());
}
