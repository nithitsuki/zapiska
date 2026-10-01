use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

#[derive(utoipa::ToSchema)]
pub struct ApiError {
    pub error: String,
    #[schema(example = "rate_limited")]
    pub code: Option<String>,
}

/// Bearer-token scheme for the protected admin group. Declared as a
/// [`Modify`] modifier because this utoipa version's derive macro cannot emit
/// `components(securitySchemes(...))`. Both `ApiDoc` variants attach it, so
/// the scheme exists whether or not the `webmentions` feature is on, and the
/// admin path macros reference it with `security(("bearerAuth" = []))`.
pub struct AdminBearerAuth;

impl Modify for AdminBearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::new);
        components.add_security_scheme(
            "bearerAuth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("opaque")
                    .build(),
            ),
        );
    }
}

#[cfg(feature = "webmentions")]
#[derive(OpenApi)]
#[openapi(
    info(
        title = "zapiska API",
        description = "zapiska — a note, a comment, a webmention",
        version = env!("CARGO_PKG_VERSION")
    ),
    paths(
        crate::http::healthz,
        crate::http::version,
        crate::http::comment_post::create_comment,
        crate::http::comment_post::delete_comment,
        crate::http::comments_read::list_comments,
        crate::http::feed::feed,
        crate::http::reactions::add_reaction,
        crate::http::reactions::remove_reaction,
        crate::http::webmention_post::receive_webmention,
        crate::http::webmention_post::well_known_webmention,
        crate::http::admin::auth::login,
        crate::http::admin::auth::logout,
        crate::http::admin::comments::list_pending,
        crate::http::admin::comments::list_comments,
        crate::http::admin::comments::get_comment,
        crate::http::admin::moderate::moderate,
        crate::http::admin::moderate::moderate_batch,
        crate::http::admin::lookup::list_paths,
        crate::http::admin::lookup::url_lookup,
        crate::http::admin::lookup::comment_urls,
        crate::http::admin::lookup::author_lookup,
        crate::http::admin::lookup::bulk_context,
        crate::http::admin::data::export,
        crate::http::admin::data::import,
        crate::http::admin::reactions::list_reactions,
        crate::http::admin::reactions::moderate_reaction,
        crate::http::admin::reactions::moderate_reactions_batch,
        crate::http::admin::profile::get_profile,
        crate::http::admin::profile::set_profile,
        crate::http::admin::profile::create_owner_comment,
        crate::http::admin::status::status,
    ),
    components(
        schemas(
            ApiError,
            crate::http::comments_read::CommentsResponse,
            crate::http::comments_read::CommentJson,
            crate::http::feed::FeedQuery,
            crate::http::comment_post::CommentForm,
            crate::http::comment_post::DeleteRequest,
            crate::http::reactions::ReactionBody,
            crate::http::admin::auth::LoginRequest,
            crate::http::admin::comments::PendingResponse,
            crate::http::admin::comments::PendingComment,
            crate::http::admin::comments::CommentDetail,
            crate::http::admin::moderate::ModerateRequest,
            crate::http::admin::moderate::ModerateResponse,
            crate::http::admin::moderate::BatchModerateRequest,
            crate::http::admin::moderate::ModerateAction,
            crate::http::admin::moderate::BatchModerateResponse,
            crate::http::admin::moderate::ModerateResult,
            crate::http::admin::data::ImportFile,
            crate::http::admin::lookup::BulkContextRequest,
            crate::http::admin::profile::OwnerCommentRequest,
            crate::http::admin::profile::AdminProfileInput,
            crate::http::admin::profile::AdminProfileResponse,
            crate::http::admin::reactions::ModerateReactionRequest,
            crate::http::admin::reactions::BatchReactionRequest,
            crate::http::webmention_post::WebmentionForm,
        )
    ),
    modifiers(&AdminBearerAuth),
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
        version = env!("CARGO_PKG_VERSION")
    ),
    paths(
        crate::http::healthz,
        crate::http::version,
        crate::http::comment_post::create_comment,
        crate::http::comment_post::delete_comment,
        crate::http::comments_read::list_comments,
        crate::http::feed::feed,
        crate::http::reactions::add_reaction,
        crate::http::reactions::remove_reaction,
        crate::http::admin::auth::login,
        crate::http::admin::auth::logout,
        crate::http::admin::comments::list_pending,
        crate::http::admin::comments::list_comments,
        crate::http::admin::comments::get_comment,
        crate::http::admin::moderate::moderate,
        crate::http::admin::moderate::moderate_batch,
        crate::http::admin::lookup::list_paths,
        crate::http::admin::lookup::url_lookup,
        crate::http::admin::lookup::comment_urls,
        crate::http::admin::lookup::author_lookup,
        crate::http::admin::lookup::bulk_context,
        crate::http::admin::data::export,
        crate::http::admin::data::import,
        crate::http::admin::reactions::list_reactions,
        crate::http::admin::reactions::moderate_reaction,
        crate::http::admin::reactions::moderate_reactions_batch,
        crate::http::admin::profile::get_profile,
        crate::http::admin::profile::set_profile,
        crate::http::admin::profile::create_owner_comment,
        crate::http::admin::status::status,
    ),
    components(
        schemas(
            ApiError,
            crate::http::comments_read::CommentsResponse,
            crate::http::comments_read::CommentJson,
            crate::http::feed::FeedQuery,
            crate::http::comment_post::CommentForm,
            crate::http::comment_post::DeleteRequest,
            crate::http::reactions::ReactionBody,
            crate::http::admin::auth::LoginRequest,
            crate::http::admin::comments::PendingResponse,
            crate::http::admin::comments::PendingComment,
            crate::http::admin::comments::CommentDetail,
            crate::http::admin::moderate::ModerateRequest,
            crate::http::admin::moderate::ModerateResponse,
            crate::http::admin::moderate::BatchModerateRequest,
            crate::http::admin::moderate::ModerateAction,
            crate::http::admin::moderate::BatchModerateResponse,
            crate::http::admin::moderate::ModerateResult,
            crate::http::admin::data::ImportFile,
            crate::http::admin::lookup::BulkContextRequest,
            crate::http::admin::profile::OwnerCommentRequest,
            crate::http::admin::profile::AdminProfileInput,
            crate::http::admin::profile::AdminProfileResponse,
            crate::http::admin::reactions::ModerateReactionRequest,
            crate::http::admin::reactions::BatchReactionRequest,
        )
    ),
    modifiers(&AdminBearerAuth),
    tags(
        (name = "comments", description = "Public comment endpoints"),
        (name = "admin", description = "Admin moderation endpoints"),
    ),
)]
pub struct ApiDoc;
