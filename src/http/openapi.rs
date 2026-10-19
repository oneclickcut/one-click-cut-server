//! OpenAPI 文档聚合与 Swagger UI 挂载。
//!
//! 引入的目的很实际：agent 上传通道与播放器接入都要反复手工试，有一个能直接
//! 填参数发请求的页面比翻文档写 curl 快得多。
//!
//! 一条纪律：**注解与实现同 commit 提交**，不允许「先写接口，文档回头补」。
//! 路由用 [`utoipa_axum::router::OpenApiRouter`] 注册，router 与文档同一次登记，
//! 漏写注解在编译期就会暴露，不靠人工检查。

use utoipa::Modify;
use utoipa::OpenApi;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

/// 顶层文档。各子模块的 path 由 `OpenApiRouter::routes` 自动并入，
/// 这里只声明元信息、安全方案与全局 schema。
#[derive(OpenApi)]
#[openapi(
    info(
        title = "一键成片文件服务",
        description = "以项目组织制作画布与公共资料的文件服务。\n\n\
                       写入面按用户 JWT / Agent Token / 管理账户三类主体鉴权；\
                       `/f/*` 读取路径一律公开，无需凭证。",
        version = env!("CARGO_PKG_VERSION"),
    ),
    modifiers(&SecurityAddon),
    // 这两条的 axum 路由用多段通配 `{*path}`，与 OpenAPI 的 `{path}` 写法不同，
    // 所以没法走 `routes!` 宏自动登记，只能在这里手工挂上。详见 file_api::router。
    paths(
        crate::http::file_api::put_file,
        crate::http::file_api::delete_file,
        crate::http::resources_api::put_file,
        crate::http::resources_api::delete_file,
        crate::http::static_api::serve_file,
        crate::http::upload_api::put_part,
        crate::http::upload_api::batch_upload,
    ),
    components(schemas(crate::http::error::ErrorBody)),
    tags(
        (name = "system", description = "健康检查与运行状态"),
        (name = "auth", description = "注册、单 JWT 登录与当前身份"),
        (name = "tree", description = "文件夹与项目的树结构"),
        (name = "projects", description = "项目与制作画布"),
        (name = "gallery", description = "匿名画廊、自动共享偏好、作品开关与 Worker 完成通知"),
        (name = "constraints", description = "项目公共资料的文件与目录"),
        (name = "files", description = "画布内文件与 manifest"),
        (name = "dirs", description = "画布内目录。与画布外的「文件夹」是两个不同的东西"),
        (name = "static", description = "公开的静态资源分发，无需凭证"),
        (name = "agent", description = "agent token、分块上传、tar 批量上传"),
        (name = "admin", description = "管理接口，仅 role=admin"),
        (name = "tasks", description = "公共任务队列：登录用户提交，配置名单内用户领取并删除"),
    ),
)]
pub struct ApiDoc;

/// 声明两种安全方案。
///
/// **读接口一律不标安全方案**——这样「公开可读」这条设计在文档上一眼就能看出来，
/// 而不是要去读说明文字。
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .as_mut()
            .expect("派生宏总会生成 components");

        components.add_security_scheme(
            "bearer_jwt",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some(
                        "用户或管理员的 access token，由 `/api/auth/login` 签发。",
                    ))
                    .build(),
            ),
        );

        components.add_security_scheme(
            "agent_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "形如 `at_<32 字符>` 的项目级 agent token，按权限操作绑定项目内资源。",
                    ))
                    .build(),
            ),
        );
    }
}
