//! A lightweight crate that aids in implementing short-circuiting ratelimits for projects that
//! utilize the `axum` crate.

use axum::{
    body::{Body, BoxBody},
    extract::FromRequest,
    http::{header::{HeaderName, HeaderValue}, Request},
    response::Response,
};
use tower::{Layer, Service};
use std::{
    // TODO: DashMap could be considered over HashMap
    collections::HashMap,
    hash::Hash,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use std::time::SystemTime;

/// A bucket assigned to a ratelimiting scope, such as a user or IP address.
#[derive(Clone, Debug, Hash)]
pub struct Bucket(
    /// The number of rqeuests remaining.
    ///
    /// # See Also
    /// [Self::remaining](#method.remaining)
    pub u32,
    /// The time at which the bucket will be refilled.
    ///
    /// # See Also
    /// [Self::reset](#method.reset)
    pub Instant,
);

impl Bucket {
    /// Returns the number of requests remaining that can be made until [`duration`](Self::duration).
    pub fn remaining(&self) -> u32 {
        self.0
    }

    /// Returns the [`Instant`](std::time::Instant) of when [`remaining`](Self::remaining) will be reset.
    pub fn reset(&self) -> Instant {
        self.1
    }

    /// Returns the [`Duration`](std::time::Duration) until [`reset`](Self::reset).
    pub fn retry_after(&self) -> Duration {
        self.1.duration_since(Instant::now())
    }
}

/// Holds information and the buckets about a ratelimiting middleware service.
///
/// `B` is the bucket-type, or scope, of which how each request is assigned a bucket.
/// It must implement [`axum::extract::FromRequest`], [`Eq`](std::cmp::Eq), and [`Hash`](std::hash::Hash).
#[derive(Debug)]
pub struct RatelimitProvider<B, ReqBody = Body>
where
    B: FromRequest<ReqBody> + Eq + Hash,
{
    /// The amount of requests allowed per [`per`] seconds.
    pub rate: u32,

    /// The duration until each bucket refills.
    pub per: Duration,

    /// A HashMap mapping of ratelimit scopes to [`Bucket`]s.
    pub buckets: HashMap<B, Bucket>,

    _marker: PhantomData<ReqBody>,
}

impl<B, Body> RatelimitProvider<B, Body>
where
    B: FromRequest<Body> + Eq + Hash,
{
    /// Creates a new [`RatelimitProvider`] with the given rate and duration.
    pub fn new(rate: u32, per: Duration) -> Self {
        Self {
            rate,
            per,
            buckets: HashMap::new(),
            _marker: PhantomData,
        }
    }
}

/// A trait used as an implementor to handle ratelimits.
pub trait RatelimitHandler<S, B, ReqBody = Body, ResBody = BoxBody>
where
    B: FromRequest<ReqBody> + Eq + Hash,
{
    /// Returns the [`RatelimitProvider`] that provides ratelimiting information for this handler.
    fn provider(self) -> RatelimitProvider<B, ReqBody>;

    /// Returns the inner service request.
    fn service(self) -> S;

    /// Returns a mutable reference to the handler's buckets.
    ///
    /// This should not be implemented manually as the default implementation simply pulls
    /// this from [`Self::provider`]. Modify that instead of this.
    fn buckets(&mut self) -> &mut HashMap<B, Bucket> {
        &mut self.provider().buckets
    }

    /// Appends ratelimit information headers to the response.
    ///
    /// The default implementation of this appends the `X-Ratelimit-Limit` header with the value
    /// of [`RatelimitProvider::rate`].
    ///
    /// Because implementations of these headers vary across different specifications,
    /// this method may be overridden to provide custom headers.
    ///
    /// # Note
    /// This is called before the ratelimit is checked, hence the lack of a `bucket` parameter.
    fn append_ratelimit_info_headers(&self, response: &mut Response<ResBody>) {
        let headers = response.headers_mut();

        headers.insert(
            HeaderName::from_static("X-Ratelimit-Limit"),
            HeaderValue::from_str(&self.provider().rate.to_string()).unwrap(),
        );
    }

    /// Appends the limited ratelimit headers to the response.
    ///
    /// The default implementation of this appends the following headers:
    ///
    /// - `X-Ratelimit-Remaining` with the value of [`Bucket::remaining`](Bucket::remaining).
    /// - `X-Ratelimit-Reset` with the value of [`Bucket::reset`](Bucket::reset) as a Unix timestamp.
    /// - `Retry-After` with the value of [`Bucket::retry_after`](Bucket::retry_after) in seconds.
    ///
    /// Because implementations of these headers vary across different specifications,
    /// this method may be overridden to provide custom headers.
    ///
    /// # Note
    /// This is only called if a ratelimit is encountered.
    fn append_ratelimit_limited_headers(&self, bucket: &Bucket, response: &mut Response<ResBody>) {
        let headers = response.headers_mut();

        headers.insert(
            HeaderName::from_static("X-Ratelimit-Remaining"),
            HeaderValue::from_str(&*bucket.remaining().to_string()).unwrap(),
        );

        headers.insert(
            HeaderName::from_static("X-Ratelimit-Reset"),
            HeaderValue::from_str(
                &*(SystemTime::now() + bucket.retry_after())
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    .to_string()
            )
            .unwrap(),
        );

        headers.insert(
            HeaderName::from_static("Retry-After"),
            HeaderValue::from_str(&*bucket.retry_after().as_secs_f64().to_string()).unwrap(),
        );
    }

    /// Handles a ratelimited request.
    ///
    /// This should return a [`Result`](std::result::Result) where the `Ok` variant is a unit
    /// and the `Err` variant is a [`Response`](axum::response::Response).
    ///
    /// If the request should be short-circuited, you can call [`.into_response()`](axum::response::IntoResponse) on any response,
    /// favorably one with a [`TOO_MANY_REQUESTS`](axum::http::StatusCode::TOO_MANY_REQUESTS) status code, and
    /// then wrap it in an `Err`.
    ///
    /// If you would like the request to continue - in other words ignore the ratelimit, simply
    /// return `Ok(())`.
    fn handle_ratelimit(
        &mut self,
        request: &Request<ReqBody>,
        bucket: &mut Bucket,
    ) -> Result<(), Response<ResBody>>;

    /// Handles a request that could potentially be ratelimited.
    ///
    /// This should not be implemented manually; the default implementation
    /// already implements necessary logic.
    ///
    /// See [`Self::handle_ratelimit`] if you would like to handle a ratelimited request instead.
    fn handle_request(
        &mut self,
        request: &Request<ReqBody>,
        key: B,
    ) -> Result<(), Response<ResBody>> {
        let bucket = self
            .buckets()
            .entry(key)
            .or_insert_with(|| Bucket(self.provider().rate, Instant::now()));

        if bucket.1 > Instant::now() {
            let response = self.handle_ratelimit(request, bucket);

            if let Err(mut response) = response {
                self.append_ratelimit_limited_headers(bucket, &mut response);
                return Err(response);
            }

            return Ok(());
        }

        bucket.0 -= 1;

        if bucket.0 == 0 {
            bucket.0 = self.provider().rate;
            bucket.1 = Instant::now() + self.provider().per.clone();
        }

        Ok(())
    }
}

impl<S, B, ReqBody, ResBody> Service<Request<ReqBody>> for RatelimitHandler<S, B, ReqBody, ResBody>
where
    B: FromRequest<ReqBody> + Eq + Hash,
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        (&mut self.service()).poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        todo!()
    }
}
