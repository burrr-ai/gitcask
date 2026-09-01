//! Compile-time OpenAPI document and the offline Scalar reference UI.

use std::sync::OnceLock;

use axum::{Json, response::Html};
use serde::Serialize;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, ToSchema};

const SCALAR_TEMPLATE: &str = r#"<!doctype html>
<html>
<head>
  <title>gitcask API reference</title>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
</head>
<body>
  <div id="app"></div>
  <script id="api-reference" type="application/json">$spec</script>
  <script>__SCALAR_BUNDLE__</script>
  <script>
    Scalar.createApiReference(
      '#app',
      JSON.parse(document.getElementById('api-reference').textContent)
    )
  </script>
</body>
</html>
"#;

// @scalar/api-reference 1.67.0, pinned and vendored for internal/offline use.
const SCALAR_BUNDLE: &str = include_str!("../../assets/scalar-api-reference-1.67.0.js");

#[derive(Serialize, ToSchema)]
pub(crate) struct HealthResponse {
    status: String,
    version: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ReadyResponse {
    status: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    running: Option<usize>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct TasksResponse {
    hostname: String,
    running: Vec<gitcask_wal::TaskRecord>,
    recent: Vec<gitcask_wal::TaskRecord>,
}

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_default();
        components.add_security_scheme(
            "jwt_bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("EdDSA JWT")
                    .description(Some("Repository-scoped JWT; Git protocol clients send the same token as the Basic password."))
                    .build(),
            ),
        );
    }
}

#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "gitcask API",
        version = "1",
        description = "Repository administration, browsing, and operations for comwit. The browser lane at `/{owner}/{repo}/api-browser` exposes the same handlers with CORS. gitcask also serves Git smart HTTP and LFS, which are protocol endpoints and intentionally excluded from OpenAPI."
    ),
    paths(
        crate::admin::create,
        crate::admin::delete,
        crate::web::api::handlers::refs,
        crate::web::api::handlers::ref_list,
        crate::web::api::write::put_branch,
        crate::web::api::write::delete_branch,
        crate::web::api::write::put_lightweight_tag,
        crate::web::api::write::delete_tag,
        crate::web::api::write::create_annotated_tag,
        crate::web::api::commit::create_commit,
        crate::web::api::commit::merge,
        crate::web::api::archive::archive,
        crate::web::api::handlers::resolve_root,
        crate::web::api::handlers::resolve,
        crate::web::api::handlers::tree,
        crate::web::api::handlers::blob,
        crate::web::api::handlers::commits,
        crate::web::api::handlers::commit_detail,
        crate::web::api::handlers::compare,
        crate::web::status::overview,
        crate::web::status::ops_list,
        crate::web::status::tasks_list,
        crate::web::status::task_stream,
        crate::health::healthz,
        crate::health::readyz
    ),
    modifiers(&SecurityAddon),
    security(("jwt_bearer" = [])),
    tags(
        (name = "repositories", description = "Repository lifecycle"),
        (name = "browsing", description = "Read-only repository browsing"),
        (name = "writes", description = "Deterministic ref and object mutations"),
        (name = "operations", description = "Repository status and task inspection"),
        (name = "health", description = "Open process probes")
    )
)]
pub(crate) struct ApiDoc;

pub(crate) async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(<ApiDoc as utoipa::OpenApi>::openapi())
}

pub(crate) async fn scalar_docs() -> Html<&'static str> {
    static HTML: OnceLock<String> = OnceLock::new();
    Html(
        HTML.get_or_init(|| {
            utoipa_scalar::Scalar::new(<ApiDoc as utoipa::OpenApi>::openapi())
                .custom_html(SCALAR_TEMPLATE)
                .to_html()
                .replace("__SCALAR_BUNDLE__", SCALAR_BUNDLE)
        })
        .as_str(),
    )
}
