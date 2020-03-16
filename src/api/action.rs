//! Contains application actions.

use super::data::{
    CheckEventInitializeData, CheckEventResponseData, CheckQueryParameter, CheckResponseFormat,
    ListJsonResponse, ListQueryParameter, ListResponseFormat,
};
use crate::{
    data::{HomoService, Provider},
    repository::{Repositories, AvatarRepository, User, UserRepository},
    service::homo::{attach_avatar_resolver, fetch_avatar, request_service},
};
use std::{convert::Infallible, iter::repeat, str::FromStr, sync::Arc, time::Duration};

use futures::future::join_all;
use log::{error, warn};
use reqwest::{redirect::Policy as RedirectPolicy, Client};
use serde_json::Value as JsonValue;
use tokio::{spawn, sync::mpsc::channel as tokio_channel};
use url::Url;
use warp::{
    filters::sse::ServerSentEvent,
    http::{StatusCode, Uri},
    redirect, reply, sse, Reply,
};

/// Entrypoint of `GET /check`.
pub async fn check_all(
    query: CheckQueryParameter,
    repo: impl Repositories + 'static,
) -> Result<Box<dyn Reply>, Infallible> {
    let users = match repo.user().fetch_all().await {
        Ok(users) => users,
        Err(e) => {
            let message = format!("Failed to fetch users: {}", e);
            error!("{}", message);
            return Ok(Box::new(reply::with_status(
                message,
                StatusCode::INTERNAL_SERVER_ERROR,
            )));
        }
    };

    check_services(repo.avatar(), users.iter(), query).await
}

/// Entrypoint of `GET /check/:user`.
pub async fn check_user(
    screen_name: String,
    query: CheckQueryParameter,
    repo: impl Repositories + 'static,
) -> Result<Box<dyn Reply>, Infallible> {
    // TODO: screen_name のバリデーション
    let users = match repo.user().fetch_by_screen_name(&screen_name).await {
        Ok(users) => users,
        Err(e) => {
            let message = format!("Failed to fetch users: {}", e);
            error!("{}", message);
            return Ok(Box::new(reply::with_status(
                message,
                StatusCode::INTERNAL_SERVER_ERROR,
            )));
        }
    };

    check_services(repo.avatar(), users.iter(), query).await
}

/// Separates the `GET /check` process by query parameter.
async fn check_services(
    url_repo: impl AvatarRepository + 'static,
    users: impl IntoIterator<Item = &User>,
    query: CheckQueryParameter,
) -> Result<Box<dyn Reply>, Infallible> {
    let services: Vec<_> = users
        .into_iter()
        .map(|r| match HomoService::from_user(r) {
            Ok(hs) => Some(hs),
            Err(e) => {
                warn!("Failed to construct HomoService: {}", e);
                None
            }
        })
        .flatten()
        .collect();

    if services.is_empty() {
        // 0 件のときは 404 として扱う
        return Ok(Box::new(reply::with_status(
            "No such user",
            StatusCode::NOT_FOUND,
        )));
    }

    let client = Client::builder()
        .redirect(RedirectPolicy::custom(|attempt| {
            // HTTPS ドメインへのリダイレクトだけ飛ぶ
            let prev = &attempt.previous()[0];
            let next = attempt.url();
            if prev.host_str() == next.host_str() {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    match query.format {
        Some(CheckResponseFormat::ServerSentEvent) | None => {
            check_services_sse(url_repo, client, services).await
        }
        Some(CheckResponseFormat::Json) => check_services_json(url_repo, client, services).await,
    }
}

/// Checks given services and make SSE response.
async fn check_services_sse(
    url_repo: impl AvatarRepository + 'static,
    client: Client,
    services: Vec<HomoService>,
) -> Result<Box<dyn Reply>, Infallible> {
    let (tx, rx) = tokio_channel(64);
    let (service_sets, avatar_resolvers) = attach_avatar_resolver(services);

    // avatar_url 解決
    for (provider, tx) in avatar_resolvers {
        let client = client.clone();
        let repo = url_repo.clone();
        spawn(async move {
            let avatar = fetch_avatar(repo.clone(), client, Arc::new(provider)).await;
            match tx.send(avatar) {
                Ok(_) => (),
                Err(e) => {
                    warn!("Failed to notify avatar URL: {:?}", e);
                }
            }
        });
    }

    // initialize 送信
    let init_message: Result<_, Infallible> = Ok((
        sse::event("initialize"),
        sse::json(CheckEventInitializeData {
            count: service_sets.len(),
        })
        .into_a(),
    ));
    tx.clone().send(init_message).await.unwrap_or_else(|_| {
        unreachable!("Failed to send `initialize` event: Receiver already dropped");
    });

    // response 送信
    for (service, resolver) in service_sets {
        let service = Arc::new(service);
        let cl = client.clone();
        let srv = service.clone();
        let sender = tx.clone();
        spawn(async move {
            let response = request_service(cl.clone(), srv.clone(), resolver).await;
            let message = match response {
                Ok(r) => (
                    sse::event("response"),
                    sse::json(CheckEventResponseData::build(&srv, &r))
                        .into_a()
                        .into_b(),
                ),
                Err(e) => (sse::event("error"), sse::data(e).into_b().into_b()),
            };
            // rx が drop してたら何もやることはない
            match sender.clone().send(Ok(message)).await {
                Ok(()) => (),
                Err(_) => return,
            }
        });
    }

    Ok(Box::new(sse::reply(rx)))
}

/// Checks given services and make SSE response.
async fn check_services_json(
    url_repo: impl AvatarRepository + 'static,
    client: Client,
    services: Vec<HomoService>,
) -> Result<Box<dyn Reply>, Infallible> {
    let clients = repeat(client.clone());
    let (service_sets, avatar_resolvers) = attach_avatar_resolver(services);

    // avatar_url 解決
    for (provider, tx) in avatar_resolvers {
        let client = client.clone();
        let repo = url_repo.clone();
        spawn(async move {
            let avatar = fetch_avatar(repo, client, Arc::new(provider)).await;
            match tx.send(avatar) {
                Ok(_) => (),
                Err(e) => {
                    warn!("Failed to notify avatar URL: {:?}", e);
                }
            }
        });
    }

    // 一斉送信
    let result_futures = service_sets
        .into_iter()
        .zip(clients)
        .map(|((s, rx), c)| async {
            let service = Arc::new(s);
            match request_service(c, service.clone(), rx).await {
                Ok(response) => Some(CheckEventResponseData::build(&service, &response)),
                Err(_) => None,
            }
        });
    let results: Vec<_> = join_all(result_futures)
        .await
        .into_iter()
        .flatten()
        .collect();

    Ok(Box::new(reply::json(&results)))
}

/// Entrypoint of `GET /list`.
pub async fn list_all(
    query: ListQueryParameter,
    repo: impl Repositories,
) -> Result<Box<dyn Reply>, Infallible> {
    let users = match repo.user().fetch_all().await {
        Ok(users) => users,
        Err(e) => {
            let message = format!("Failed to fetch users: {}", e);
            error!("{}", message);
            return Ok(Box::new(reply::with_status(
                message,
                StatusCode::INTERNAL_SERVER_ERROR,
            )));
        }
    };

    list_services(users.iter(), query).await
}

/// Entrypoint of `GET /check/:user`.
pub async fn list_user(
    screen_name: String,
    query: ListQueryParameter,
    repo: impl Repositories,
) -> Result<Box<dyn Reply>, Infallible> {
    // TODO: screen_name のバリデーション
    let users = match repo.user().fetch_by_screen_name(&screen_name).await {
        Ok(users) => users,
        Err(e) => {
            let message = format!("Failed to fetch users: {}", e);
            error!("{}", message);
            return Ok(Box::new(reply::with_status(
                message,
                StatusCode::INTERNAL_SERVER_ERROR,
            )));
        }
    };

    list_services(users.iter(), query).await
}

/// Lists given services in specific format.
async fn list_services(
    users: impl IntoIterator<Item = &User>,
    query: ListQueryParameter,
) -> Result<Box<dyn Reply>, Infallible> {
    let services: Vec<_> = users
        .into_iter()
        .map(|r| match HomoService::from_user(r) {
            Ok(hs) => Some(hs),
            Err(e) => {
                warn!("Failed to construct HomoService: {}", e);
                None
            }
        })
        .flatten()
        .collect();

    if services.is_empty() {
        // 0 件のときは 404 として扱う
        return Ok(Box::new(reply::with_status(
            "No such user",
            StatusCode::NOT_FOUND,
        )));
    }

    match query.format {
        Some(ListResponseFormat::Json) | None => {
            let json: Vec<_> = services.iter().map(ListJsonResponse::build).collect();
            Ok(Box::new(reply::json(&json)))
        }
        Some(ListResponseFormat::Sql) => {
            let mut sql = String::with_capacity(16384);
            for service in services {
                // TODO: chitoku-k/HomoChecker は MySQL のクエリを返している
                //       PostgreSQL のものも返せるようにしたい
                let (sn, us) = match service.provider {
                    Provider::Twitter(sn) => (sn, "twitter"),
                    Provider::Mastodon { .. } => (service.provider.to_entity_string(), "mastodon"),
                };
                sql.push_str(&format!("INSERT INTO `users` (`screen_name`, `service`, `url`) VALUES ('{}', '{}', '{}');\n", sn, us, service.service_url));
            }
            Ok(Box::new(sql))
        }
    }
}

pub async fn redirect_badge(
    _query: JsonValue,
    repo: impl Repositories,
) -> Result<Box<dyn Reply>, Infallible> {
    let count = match repo.user().count_all().await {
        Ok(c) => c,
        Err(e) => {
            let message = format!("Failed to fetch users count: {}", e);
            error!("{}", message);
            return Ok(Box::new(reply::with_status(
                message,
                StatusCode::INTERNAL_SERVER_ERROR,
            )));
        }
    };

    let mut url = Url::parse("https://img.shields.io").unwrap();
    url.set_path(&format!("/badge/homo-{}%20registered-7a6544.svg", count));

    let uri = Uri::from_str(&url[..]).unwrap();
    Ok(Box::new(redirect::redirect(uri)))
}
