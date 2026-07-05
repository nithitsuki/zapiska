use utoipa::OpenApi;

#[derive(utoipa::ToSchema)]
pub struct ApiError {
    pub error: String,
    #[schema(example = "rate_limited")]
    pub code: Option<String>,
}

#[cfg(feature = "webmentions")]
#[derive(OpenApi)]
#[openapi(
    info(
        title = "zapiska API",
        description = "zapiska — a note, a comment, a webmention",
        version = "0.1.0"
    ),
    paths(
        crate::http::healthz,
        crate::http::comment_post::create_comment,
        crate::http::webmention_post::receive_webmention,
        crate::http::comments_read::list_comments,
        crate::http::admin::list_pending,
        crate::http::admin::moderate,
    ),
    components(
        schemas(
            ApiError,
            crate::http::comments_read::CommentsResponse,
            crate::http::comments_read::CommentJson,
            crate::http::admin::PendingResponse,
            crate::http::admin::PendingComment,
            crate::http::admin::ModerateRequest,
            crate::http::admin::ModerateResponse,
            crate::http::comment_post::CommentForm,
            crate::http::webmention_post::WebmentionForm,
        )
    ),
    tags(
        (name = "comments", description = "Public comment endpoints"),
        (name = "webmention", description = "Webmention (W3C) endpoint"),
        (name = "admin", description = "Admin moderation endpoints"),
    ),
)]
pub struct ApiDoc;

#[cfg(not(feature = "webmentions"))]
#[derive(OpenApi)]
#[openapi(
    info(
        title = "zapiska API",
        description = "zapiska — a note, a comment, a webmention",
        version = "0.1.0"
    ),
    paths(
        crate::http::healthz,
        crate::http::comment_post::create_comment,
        crate::http::comments_read::list_comments,
        crate::http::admin::list_pending,
        crate::http::admin::moderate,
    ),
    components(
        schemas(
            ApiError,
            crate::http::comments_read::CommentsResponse,
            crate::http::comments_read::CommentJson,
            crate::http::admin::PendingResponse,
            crate::http::admin::PendingComment,
            crate::http::admin::ModerateRequest,
            crate::http::admin::ModerateResponse,
            crate::http::comment_post::CommentForm,
        )
    ),
    tags(
        (name = "comments", description = "Public comment endpoints"),
        (name = "admin", description = "Admin moderation endpoints"),
    ),
)]
pub struct ApiDoc;
