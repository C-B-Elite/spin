// Import the HTTP objects from the generated bindings.
use spin_http::{Request, Response};

// Generate Rust bindings for interface defined in spin-http.wit file
wit_bindgen_rust::export!("spin-http.wit");

struct SpinHttp {}
impl spin_http::SpinHttp for SpinHttp {
    // Implement the `handler` entrypoint for Spin HTTP components.
    fn handle_http_request(req: Request) -> Response {
        let path = req.uri;

        if path.contains("test-placement") {
            match std::fs::read_to_string("/test.txt") {
                Ok(text) =>
                    Response {
                        status: 200,
                        headers: None,
                        body: Some(text.as_bytes().to_vec()),
                    },
                Err(e) =>
                    Response {
                        status: 500,
                        headers: None,
                        body: Some(format!("ERROR! {:?}", e).as_bytes().to_vec()),
                    },
            }
        } else {
            Response {
                status: 200,
                headers: None,
                body: Some("I'm a teapot".as_bytes().to_vec()),
            }
        }
    }
}