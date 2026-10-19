//! 第二层：业务规则。**不认识 HTTP**——这里不出现状态码、请求头、路由。

pub mod agent;
pub mod canvas;
pub mod canvas_dir;
pub mod gallery;
pub mod gallery_policy;
pub mod identity;
pub mod maintenance;
pub mod plans;
pub mod quota;
pub mod space;
pub mod task_queue;
pub mod upload;

pub mod jobs;

pub mod mp4_jobs;
