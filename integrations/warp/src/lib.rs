//! Async-graphql integration with Warp

#![warn(missing_docs)]
#![allow(clippy::type_complexity)]
#![allow(clippy::needless_doctest_main)]
#![forbid(unsafe_code)]

use async_graphql::http::MultipartOptions;
use async_graphql::{
    resolver_utils::ObjectType, Data, FieldResult, Request, Schema, SubscriptionType,
};
use futures::{future, StreamExt, TryStreamExt};
use hyper::Method;
use std::io::{self, ErrorKind};
use std::sync::Arc;
use warp::filters::ws;
use warp::reject::Reject;
use warp::reply::Response;
use warp::{Buf, Filter, Rejection, Reply};

/// Bad request error
///
/// It's a wrapper of `async_graphql::ParseRequestError`.
pub struct BadRequest(pub anyhow::Error);

impl std::fmt::Debug for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Reject for BadRequest {}

/// GraphQL request filter
///
/// It outputs a tuple containing the `async_graphql::Schema` and `async_graphql::Request`.
///
/// # Examples
///
/// *[Full Example](<https://github.com/async-graphql/examples/blob/master/warp/starwars/src/main.rs>)*
///
/// ```no_run
///
/// use async_graphql::*;
/// use async_graphql_warp::*;
/// use warp::Filter;
/// use std::convert::Infallible;
///
/// struct QueryRoot;
///
/// #[Object]
/// impl QueryRoot {
///     #[field]
///     async fn value(&self, ctx: &Context<'_>) -> i32 {
///         unimplemented!()
///     }
/// }
///
/// type MySchema = Schema<QueryRoot, EmptyMutation, EmptySubscription>;
///
/// #[tokio::main]
/// async fn main() {
///     let schema = Schema::new(QueryRoot, EmptyMutation, EmptySubscription);
///     let filter = async_graphql_warp::graphql(schema).
///             and_then(|(schema, request): (MySchema, async_graphql::Request)| async move {
///         Ok::<_, Infallible>(GQLResponse::from(schema.execute(request).await))
///     });
///     warp::serve(filter).run(([0, 0, 0, 0], 8000)).await;
/// }
/// ```
pub fn graphql<Query, Mutation, Subscription>(
    schema: Schema<Query, Mutation, Subscription>,
) -> impl Filter<
    Extract = ((
        Schema<Query, Mutation, Subscription>,
        async_graphql::Request,
    ),),
    Error = Rejection,
> + Clone
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    graphql_opts(schema, Default::default())
}

/// Similar to graphql, but you can set the options `async_graphql::MultipartOptions`.
pub fn graphql_opts<Query, Mutation, Subscription>(
    schema: Schema<Query, Mutation, Subscription>,
    opts: MultipartOptions,
) -> impl Filter<
    Extract = ((
        Schema<Query, Mutation, Subscription>,
        async_graphql::Request,
    ),),
    Error = Rejection,
> + Clone
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    let opts = Arc::new(opts);
    warp::any()
        .and(warp::method())
        .and(warp::query::raw().or(warp::any().map(String::new)).unify())
        .and(warp::header::optional::<String>("content-type"))
        .and(warp::body::stream())
        .and(warp::any().map(move || opts.clone()))
        .and(warp::any().map(move || schema.clone()))
        .and_then(
            |method,
             query: String,
             content_type,
             body,
             opts: Arc<MultipartOptions>,
             schema| async move {
                if method == Method::GET {
                    let request: Request = serde_urlencoded::from_str(&query)
                        .map_err(|err| warp::reject::custom(BadRequest(err.into())))?;
                    Ok::<_, Rejection>((schema, request))
                } else {
                    let request = async_graphql::http::receive_body(
                        content_type,
                        futures::TryStreamExt::map_err(body, |err| io::Error::new(ErrorKind::Other, err))
                            .map_ok(|mut buf| Buf::to_bytes(&mut buf))
                            .into_async_read(),
                        MultipartOptions::clone(&opts),
                    )
                    .await
                    .map_err(|err| warp::reject::custom(BadRequest(err.into())))?;
                    Ok::<_, Rejection>((schema, request))
                }
            },
        )
}

/// GraphQL subscription filter
///
/// # Examples
///
/// ```no_run
/// use async_graphql::*;
/// use async_graphql_warp::*;
/// use warp::Filter;
/// use futures::{Stream, StreamExt};
/// use std::time::Duration;
///
/// struct QueryRoot;
///
/// #[Object]
/// impl QueryRoot {}
///
/// struct SubscriptionRoot;
///
/// #[Subscription]
/// impl SubscriptionRoot {
///     #[field]
///     async fn tick(&self) -> impl Stream<Item = String> {
///         tokio::time::interval(Duration::from_secs(1)).map(|n| format!("{}", n.elapsed().as_secs_f32()))
///     }
/// }
///
/// #[tokio::main]
/// async fn main() {
///     let schema = Schema::new(QueryRoot, EmptyMutation, SubscriptionRoot);
///     let filter = async_graphql_warp::graphql_subscription(schema)
///         .or(warp::any().map(|| "Hello, World!"));
///     warp::serve(filter).run(([0, 0, 0, 0], 8000)).await;
/// }
/// ```
pub fn graphql_subscription<Query, Mutation, Subscription>(
    schema: Schema<Query, Mutation, Subscription>,
) -> impl Filter<Extract = (impl Reply,), Error = Rejection> + Clone
where
    Query: ObjectType + Sync + Send + 'static,
    Mutation: ObjectType + Sync + Send + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    graphql_subscription_with_data::<_, _, _, fn(serde_json::Value) -> FieldResult<Data>>(
        schema, None,
    )
}

/// GraphQL subscription filter
///
/// Specifies that a function converts the init payload to data.
pub fn graphql_subscription_with_data<Query, Mutation, Subscription, F>(
    schema: Schema<Query, Mutation, Subscription>,
    initializer: Option<F>,
) -> impl Filter<Extract = (impl Reply,), Error = Rejection> + Clone
where
    Query: ObjectType + Sync + Send + 'static,
    Mutation: ObjectType + Sync + Send + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
    F: FnOnce(serde_json::Value) -> FieldResult<Data> + Send + Sync + Clone + 'static,
{
    warp::any()
        .and(warp::ws())
        .and(warp::any().map(move || schema.clone()))
        .and(warp::any().map(move || initializer.clone()))
        .map(
            |ws: ws::Ws, schema: Schema<Query, Mutation, Subscription>, initializer: Option<F>| {
                ws.on_upgrade(move |websocket| {
                    let (ws_sender, ws_receiver) = websocket.split();

                    async move {
                        let _ = async_graphql::http::WebSocket::with_data(
                            schema,
                            ws_receiver
                                .take_while(|msg| future::ready(msg.is_ok()))
                                .map(Result::unwrap)
                                .map(ws::Message::into_bytes),
                            initializer,
                        )
                        .map(ws::Message::text)
                        .map(Ok)
                        .forward(ws_sender)
                        .await;
                    }
                })
            },
        )
        .map(|reply| warp::reply::with_header(reply, "Sec-WebSocket-Protocol", "graphql-ws"))
}

/// GraphQL reply
pub struct GQLResponse(async_graphql::Response);

impl From<async_graphql::Response> for GQLResponse {
    fn from(resp: async_graphql::Response) -> Self {
        GQLResponse(resp)
    }
}

fn add_cache_control(http_resp: &mut Response, resp: &async_graphql::Response) {
    if resp.is_ok() {
        if let Some(cache_control) = resp.cache_control.value() {
            if let Ok(value) = cache_control.parse() {
                http_resp.headers_mut().insert("cache-control", value);
            }
        }
    }
}

impl Reply for GQLResponse {
    fn into_response(self) -> Response {
        let mut resp = warp::reply::with_header(
            warp::reply::json(&self.0),
            "content-type",
            "application/json",
        )
        .into_response();
        add_cache_control(&mut resp, &self.0);
        resp
    }
}
