use crate::*;
pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(
            "/assets/smith-shell.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css")],
                    include_str!("../web/smith-shell.css"),
                )
            }),
        )
        .route(
            "/assets/smith-shell.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    include_str!("../web/smith-shell.js"),
                )
            }),
        )
        .route(
            "/assets/legacy-panel.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    include_str!("../web/legacy-panel.js"),
                )
            }),
        )
        .layer(middleware::from_fn(guard))
}
