//! The contract's operations, as one table. The router is built from it,
//! and `tests/contract.rs` compares it with `docs/api/openapi.json`, so the
//! document and the code cannot drift apart unnoticed.

/// One operation of the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// The OpenAPI `operationId`.
    pub operation: &'static str,
    /// The HTTP method, upper case.
    pub method: &'static str,
    /// The path, with `{parameters}` as OpenAPI and `axum` both write them.
    pub path: &'static str,
}

const fn route(operation: &'static str, method: &'static str, path: &'static str) -> Route {
    Route {
        operation,
        method,
        path,
    }
}

/// Every operation the server serves.
pub const ROUTES: &[Route] = &[
    route("healthz", "GET", "/healthz"),
    route("readyz", "GET", "/readyz"),
    route("getViewer", "GET", "/v1/viewer"),
    route("getWorkspace", "GET", "/v1/workspace"),
    route("probeWrite", "POST", "/v1/workspace/probe"),
    route("cleanStaging", "POST", "/v1/workspace/clean-staging"),
    route("listItems", "GET", "/v1/items"),
    route("listItemIds", "GET", "/v1/item-ids"),
    route("resolveItem", "GET", "/v1/items/resolve"),
    route("findByContentKey", "GET", "/v1/content-keys/{contentKey}"),
    route("getItem", "GET", "/v1/items/{id}"),
    route("deleteItem", "DELETE", "/v1/items/{id}"),
    route("getItemContent", "GET", "/v1/items/{id}/content"),
    route("beginUpload", "POST", "/v1/uploads"),
    route("putUploadContent", "PUT", "/v1/uploads/{uploadId}/content"),
    route("commitUpload", "POST", "/v1/uploads/{uploadId}/commit"),
    route("abortUpload", "DELETE", "/v1/uploads/{uploadId}"),
    route("enableEncryption", "PUT", "/v1/workspace/encryption"),
    route("freshStart", "POST", "/v1/workspace/encryption/fresh-start"),
    route("replaceHeader", "PUT", "/v1/workspace/encryption/header"),
    route("getRewrite", "GET", "/v1/rewrite"),
    route("beginRewrite", "POST", "/v1/rewrite"),
    route("heartbeatRewrite", "POST", "/v1/rewrite/heartbeat"),
    route("takeOverRewrite", "POST", "/v1/rewrite/take-over"),
    route("commitRewrite", "POST", "/v1/rewrite/commit"),
    route("abortRewrite", "POST", "/v1/rewrite/abort"),
];

/// The operation a request is, for its log line.
pub fn operation_of(method: &str, matched_path: &str) -> Option<&'static str> {
    ROUTES
        .iter()
        .find(|route| route.method == method && route.path == matched_path)
        .map(|route| route.operation)
}
