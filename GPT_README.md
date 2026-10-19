<div align="center">

# One Click Cut · Server

**The API, storage, and job coordination service behind One Click Cut.**

### 🌐 Live website: [https://oneclickcut.com/](https://oneclickcut.com/)

**The website is live and ready to use, with a free allowance available.**

`Rust` · `Axum` · `RocksDB` · `OpenAPI`

[Explore features](#what-it-does) · [Get started](#quick-start) · [Configure](#configuration) · [API](#api-and-authentication) · [Operations](#operations)

</div>

---

## What it does

One Click Cut organizes video production around **projects** and **canvases**. This repository provides the backend that stores their files, manages access, tracks quotas, and coordinates generation and export jobs with authenticated workers.

| Area | Capabilities |
| --- | --- |
| Workspace | Create projects, canvases, folders, and project-wide resource directories. |
| Files | Upload and manage canvas files, use resumable multipart uploads or tar batches, and retrieve playback manifests. |
| Media delivery | Serve public canvas assets with HTTP range requests, ETags, and cache controls. |
| Processing | Submit tasks and manage resource-generation and MP4 export job lifecycles for workers. |
| Sharing | Publish projects to a public gallery, manage sharing preferences, and browse shared work. |
| Access | Use user and admin JWTs or scoped project Agent Tokens; enforce storage and monthly submission limits. |
| Operations | Inspect health and storage, run garbage collection, create database checkpoints, and verify backups. |

Metadata and file contents are stored in RocksDB under one configurable data directory. The HTTP API and optional Swagger UI are generated from the server's route definitions.

## Quick start

### Requirements

- A Rust toolchain that supports **edition 2024**.
- A C/C++ build toolchain and the native build dependencies required by the `rocksdb` crate. Depending on your platform, its bindings may also need `libclang`.
- `openssl` for the secret-generation command below; `curl` for the health check.

From the repository root:

```bash
cp config.example.toml config.toml
```

For local development, set `data_dir = "./data"` in `config.toml`. The example file uses `/data`, which usually requires a separately prepared, writable directory. Leave `jwt.secret` empty in the file and supply a secret through the environment:

```bash
export OCC_JWT_SECRET="$(openssl rand -base64 48)"
cargo run --bin one-click-cut-server
```

Keep the same secret across restarts: changing it invalidates previously issued JWTs. In another terminal, check the local instance:

```bash
curl http://127.0.0.1:3823/healthz
```

On the **first** start, the server creates the configured administrator and prints its one-time initial password in the server log. Save it immediately; restarting does not generate another password. With documentation enabled, open [http://127.0.0.1:3823/swagger-ui](http://127.0.0.1:3823/swagger-ui).

> [!TIP]
> `config.toml` is ignored by version control. For repeatable deployments, inject `OCC_JWT_SECRET` from a secret manager or service environment instead of storing it in the file.

## Configuration

Configuration is applied in this order: **built-in defaults → TOML file → supported environment variables**. The server chooses its file from a positional argument, then `OCC_CONFIG`, then `./config.toml`. An explicitly selected missing file is an error; without an explicit path, a missing `./config.toml` falls back to defaults. A valid JWT secret is still required to start.

```bash
# Inspect the selected configuration without opening the database or listening.
cargo run --bin one-click-cut-server -- config.toml --check-config

# Start with an explicitly selected configuration file.
cargo run --bin one-click-cut-server -- /path/to/config.toml
```

`--check-config` prints selected non-secret settings, including the worker allowlists. It does not print the JWT secret.

### Core settings

| TOML key | Environment override | Example default | Purpose |
| --- | --- | --- | --- |
| `listen` | `OCC_LISTEN` | `127.0.0.1:3823` | Bind address and port. |
| `data_dir` | `OCC_DATA_DIR` | `/data` | Parent of `rocksdb/` and `backup/`; use `./data` locally. |
| `jwt.secret` | `OCC_JWT_SECRET` | Empty | JWT signing secret; **at least 32 bytes**. |
| `admin.username` | `OCC_ADMIN_USERNAME` | `admin` | Administrator created on the first start. |
| `docs.enabled` | `OCC_DOCS_ENABLED` | `true` | Serve Swagger UI and OpenAPI JSON. |
| `log.filter` | `OCC_LOG_FILTER`, `RUST_LOG` | `info,one_click_cut_server=debug` | Log filter; `RUST_LOG` takes priority. |
| `log.json` | `OCC_LOG_JSON` | `false` | Emit structured JSON logs when enabled. |
| `registration.daily_limit` | `OCC_REGISTRATION_DAILY_LIMIT` | `100` | Daily public registration cap; `0` disables public registration. |

The registration cap resets at midnight in Beijing time (UTC+8). Boolean environment values accept `true`/`false`, `1`/`0`, `yes`/`no`, or `on`/`off`.

### Uploads and worker permissions

| TOML key | Environment override | Default | Purpose |
| --- | --- | --- | --- |
| `upload.max_single_bytes` | `OCC_UPLOAD_MAX_SINGLE_MB` | 32 MiB | Maximum single-file PUT body; the override is in MiB. |
| `upload.max_part_bytes` | — | 50 MiB | Maximum multipart chunk or tar batch body. |
| `task_queue.reader_usernames` | `OCC_TASK_QUEUE_READER_USERNAMES` | `[]` | Accounts allowed to list and claim queued tasks. |
| `task_queue.max_payload_bytes` | `OCC_TASK_QUEUE_MAX_PAYLOAD_BYTES` | 65,536 | Maximum task submission JSON body. |
| `gallery.publisher_usernames` | `OCC_GALLERY_PUBLISHER_USERNAMES` | `[]` | Workers allowed to submit cross-user generation-completion notices. |

The two username lists grant **separate permissions**. An empty reader list blocks task claims, including claims by an administrator. An empty publisher list blocks generation-completion publishing. Environment overrides for these lists must be JSON string arrays:

```bash
export OCC_TASK_QUEUE_READER_USERNAMES='["worker1"]'
export OCC_GALLERY_PUBLISHER_USERNAMES='["worker1"]'
```

Restart the server after changing either list.

### Plans and free allowance

New public accounts start on the **Basic** plan. The repository's default limits are configurable and do not prescribe the current offer on the hosted website.

| Plan | Default storage | Default monthly task submissions |
| --- | ---: | ---: |
| Basic | 5 GB | 5 |
| Pro | 50 GB | 50 |
| Max | 200 GB | 300 |

Storage uses decimal bytes (`1 GB = 1,000,000,000 bytes`). Edit `[plans.basic]`, `[plans.pro]`, and `[plans.max]` in `config.toml`, or override each plan with `OCC_PLANS_<PLAN>_STORAGE_BYTES` and `OCC_PLANS_<PLAN>_MONTHLY_PUSH_LIMIT`, where `<PLAN>` is `BASIC`, `PRO`, or `MAX`. Restart to apply new storage limits to existing users.

See [config.example.toml](config.example.toml) for the complete annotated configuration.

## API and authentication

When `docs.enabled = true`, browse `/swagger-ui` or fetch `/api-docs/openapi.json` for request and response schemas. Useful entry points include:

| Endpoint | Use |
| --- | --- |
| `GET /healthz` | Check that the process responds; this is not a storage integrity check. |
| `POST /api/auth/register` | Create a public account, subject to the daily cap. |
| `POST /api/auth/login` | Receive a user JWT. |
| `GET /api/me` | Inspect the authenticated account and its limits. |
| `/api/projects`, `/api/tree` | Manage projects and the workspace tree. |
| `/api/canvases/{id}/files`, `/api/canvases/{id}/uploads` | Manage files and resumable uploads. |
| `/api/tasks`, `/api/jobs` | Work with the task queue and processing jobs. |
| `/api/gallery/projects` | Browse publicly shared projects. |
| `/f/*` | Read public canvas assets. |

Send a user JWT as `Authorization: Bearer <token>`. Project Agent Tokens use the same header but are limited to their project's granted operations. The public gallery and `/f/*` asset reads do not require a token.

## Operations

### Production build

```bash
bash scripts/build-release.sh
```

The script builds and installs both `./one-click-cut-server` and `./reset_admin_password` so their database formats stay in sync. Run the server from the directory containing your chosen configuration, or pass an explicit path:

```bash
OCC_JWT_SECRET="<secret-from-your-secret-manager>" ./one-click-cut-server /path/to/config.toml
```

For an internet-facing deployment, keep `listen` on a loopback address behind a trusted reverse proxy or tunnel, set `docs.enabled = false`, and use a persistent writable `data_dir`. Terminate TLS at the proxy or tunnel.

### Backups and recovery checks

```bash
# Use the same data directory as the running server.
OCC_DATA_DIR=./data \
OCC_BASE_URL=http://127.0.0.1:3823 \
OCC_ADMIN_TOKEN="<admin-jwt>" \
bash scripts/backup.sh

OCC_DATA_DIR=./data bash scripts/restore-drill.sh
```

With `OCC_BASE_URL` and `OCC_ADMIN_TOKEN`, the backup script requests a consistent RocksDB checkpoint through the admin API. Without them, it falls back to a filesystem hard-link snapshot, which should be taken while writes are stopped. It keeps four local snapshots by default; set `OCC_BACKUP_KEEP` to change that and `OCC_BACKUP_REMOTE` to sync them to an off-machine destination with `rsync`. The restore drill starts a temporary server against a copy of a snapshot and verifies `/healthz`.

### Administrator password recovery

Stop the server before running the reset utility, because it must open the same RocksDB directory exclusively. Use the same configuration and `OCC_DATA_DIR` as the server:

```bash
./reset_admin_password admin '<new-password>' /path/to/config.toml
```

Restart the server after resetting the password. Previously issued JWTs are not revoked by a password reset; disable the account if they must stop working immediately.

---

<div align="center">

**Ready to try it?** [Open the live One Click Cut website](https://oneclickcut.com/) · Free allowance available

</div>
