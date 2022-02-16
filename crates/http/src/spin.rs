use crate::spin_http::{Method, SpinHttp};
use crate::HttpExecutor;
use crate::{ExecutionContext, RuntimeContext};
use anyhow::Result;
use async_trait::async_trait;
use http::Uri;
use hyper::{Body, Request, Response};
use std::{net::SocketAddr, str::FromStr};
use tracing::log;
use wasmtime::{Instance, Store};

#[derive(Clone)]
pub struct SpinHttpExecutor;

#[async_trait]
impl HttpExecutor for SpinHttpExecutor {
    async fn execute(
        engine: &ExecutionContext,
        component: &str,
        base: &str,
        raw_route: &str,
        req: Request<Body>,
        _client_addr: SocketAddr,
    ) -> Result<Response<Body>> {
        log::trace!(
            "Executing request using the Spin executor for component {}",
            component
        );
        let (store, instance) = engine.prepare_component(component, None, None, None)?;
        let res = Self::execute_impl(store, instance, base, raw_route, req).await?;
        log::info!(
            "Request finished, sending response with status code {}",
            res.status()
        );
        Ok(res)
    }
}

impl SpinHttpExecutor {
    pub async fn execute_impl(
        mut store: Store<RuntimeContext>,
        instance: Instance,
        base: &str,
        raw_route: &str,
        req: Request<Body>,
    ) -> Result<Response<Body>> {
        let headers;
        let mut req = req;
        {
            headers = Self::headers(&mut req, raw_route, base)?;
        }

        let headers: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let engine = SpinHttp::new(&mut store, &instance, |host| {
            &mut host.data.as_mut().unwrap().0
        })?;
        let (parts, bytes) = req.into_parts();
        let bytes = hyper::body::to_bytes(bytes).await?.to_vec();
        let body = Some(&bytes[..]);

        let method = Self::method(&parts.method);
        let params = &Self::params(&parts.uri)?;
        let params: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let req = crate::spin_http_interceptor::RequestParam {
            // FIXME: DELETE
            method: unsafe { std::mem::transmute(method) },
            uri: &parts.uri.path().to_string(),
            headers: &headers,
            params: &params,
            body,
        };

        use crate::spin_http_interceptor::{InterceptRequestResult, SpinHttpInterceptor};
        let interceptor = SpinHttpInterceptor::new(&mut store, &instance, |host| {
            &mut host.data.as_mut().unwrap().1
        })?;

        let res = match interceptor.intercept_request(&mut store, req)? {
            InterceptRequestResult::Continue(ireq) => {
                let req = crate::spin_http::Request {
                    // FIXME: DELETE
                    method: unsafe { std::mem::transmute(ireq.method) },
                    uri: &ireq.uri,
                    headers: &mapping_strings_to_strs(&ireq.headers),
                    params: &mapping_strings_to_strs(&ireq.params),
                    body: ireq.body.as_deref(),
                };
                engine.handler(&mut store, req)?
            }
            InterceptRequestResult::Return(ret) => ret.into(),
        };

        let mut response = http::Response::builder().status(res.status);
        Self::append_headers(response.headers_mut().unwrap(), res.headers)?;

        let body = match res.body {
            Some(b) => Body::from(b),
            None => Body::empty(),
        };

        Ok(response.body(body)?)
    }

    fn method(m: &http::Method) -> Method {
        match *m {
            http::Method::GET => Method::Get,
            http::Method::POST => Method::Post,
            http::Method::PUT => Method::Put,
            http::Method::DELETE => Method::Delete,
            http::Method::PATCH => Method::Patch,
            http::Method::HEAD => Method::Head,
            _ => todo!(),
        }
    }

    fn headers(req: &mut Request<Body>, raw: &str, base: &str) -> Result<Vec<(String, String)>> {
        let mut res = Vec::new();
        for (name, value) in req
            .headers()
            .iter()
            .map(|(name, value)| (name.to_string(), std::str::from_utf8(value.as_bytes())))
        {
            let value = value?.to_string();
            res.push((name, value));
        }

        // TODO
        // Is there any scenario where the server doesn't populate the host header?
        let default_host = http::HeaderValue::from_str("localhost")?;
        let host = std::str::from_utf8(
            req.headers()
                .get("host")
                .unwrap_or(&default_host)
                .as_bytes(),
        )?;

        // Add the default headers.
        for pair in crate::default_headers(req.uri(), raw, base, host)? {
            res.push(pair);
        }

        Ok(res)
    }

    fn append_headers(res: &mut http::HeaderMap, src: Option<Vec<(String, String)>>) -> Result<()> {
        if let Some(src) = src {
            for (k, v) in src.iter() {
                res.insert(
                    http::header::HeaderName::from_str(k)?,
                    http::header::HeaderValue::from_str(v)?,
                );
            }
        };

        Ok(())
    }

    fn params(uri: &Uri) -> Result<Vec<(String, String)>> {
        match uri.query() {
            Some(q) => Ok(url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect::<Vec<_>>()),
            None => Ok(vec![]),
        }
    }
}

fn mapping_strings_to_strs(v: &Vec<(String, String)>) -> Vec<(&str, &str)> {
    v.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

impl<'a> From<&'a crate::spin_http::Request<'a>>
    for crate::spin_http_interceptor::RequestParam<'a>
{
    fn from(req: &'a crate::spin_http::Request) -> crate::spin_http_interceptor::RequestParam<'a> {
        crate::spin_http_interceptor::RequestParam {
            // FIXME(lann): DELETE THIS OBVIOUSLY
            method: unsafe { std::mem::transmute(req.method) },
            uri: &req.uri,
            headers: req.headers,
            params: &req.params,
            body: req.body,
        }
    }
}

impl From<crate::spin_http_interceptor::ResponseResult> for crate::spin_http::Response {
    fn from(resp: crate::spin_http_interceptor::ResponseResult) -> Self {
        crate::spin_http::Response {
            status: resp.status,
            headers: resp.headers,
            body: resp.body,
        }
    }
}
