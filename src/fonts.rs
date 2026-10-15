//! Only these six owner-reviewed, unchanged OFL font files are embedded.
//! Copyright/license notices ship under web/assets/fonts/licenses.
use axum::{http::header, routing::get, Router};
const ASSETS: [(&str, &[u8]); 6] = [
    (
        "/assets/fonts/inter-400.woff2",
        include_bytes!("../web/assets/fonts/inter-400.woff2"),
    ),
    (
        "/assets/fonts/inter-500.woff2",
        include_bytes!("../web/assets/fonts/inter-500.woff2"),
    ),
    (
        "/assets/fonts/inter-600.woff2",
        include_bytes!("../web/assets/fonts/inter-600.woff2"),
    ),
    (
        "/assets/fonts/poppins-500.woff2",
        include_bytes!("../web/assets/fonts/poppins-500.woff2"),
    ),
    (
        "/assets/fonts/poppins-600.woff2",
        include_bytes!("../web/assets/fonts/poppins-600.woff2"),
    ),
    (
        "/assets/fonts/spline-sans-mono-400.woff2",
        include_bytes!("../web/assets/fonts/spline-sans-mono-400.woff2"),
    ),
];
const NOTICES: [(&str, &str); 3] = [
    (
        "/assets/fonts/licenses/Inter-OFL.txt",
        include_str!("../web/assets/fonts/licenses/Inter-OFL.txt"),
    ),
    (
        "/assets/fonts/licenses/Poppins-OFL.txt",
        include_str!("../web/assets/fonts/licenses/Poppins-OFL.txt"),
    ),
    (
        "/assets/fonts/licenses/SplineSansMono-OFL.txt",
        include_str!("../web/assets/fonts/licenses/SplineSansMono-OFL.txt"),
    ),
];
pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    let mut router = Router::new();
    for (path, bytes) in ASSETS {
        router = router.route(
            path,
            get(move || async move { ([(header::CONTENT_TYPE, "font/woff2")], bytes) }),
        );
    }
    for (path, text) in NOTICES {
        router =
            router.route(
                path,
                get(move || async move {
                    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text)
                }),
            );
    }
    router
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn six_fixed_woff_assets() {
        assert_eq!(ASSETS.len(), 6);
        for (path, data) in ASSETS {
            assert!(path.starts_with("/assets/fonts/"));
            assert!(data.starts_with(b"wOF2"));
            assert!(data.len() > 1000);
        }
    }
    #[test]
    fn three_fixed_license_notices() {
        assert_eq!(NOTICES.len(), 3);
        for (path, text) in NOTICES {
            assert!(path.starts_with("/assets/fonts/licenses/"));
            assert!(text.contains("SIL OPEN FONT LICENSE"));
        }
    }
}
