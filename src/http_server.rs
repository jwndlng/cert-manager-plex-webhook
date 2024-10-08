use anyhow::{Error, anyhow};
use warp::Filter;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;
use rcgen::generate_simple_self_signed;
use tokio::{task, sync::Mutex};
use serde_json::Value;
use crate::plesk_api::PleskAPI;
use crate::settings::Settings;

// Define constants for actions
const ACTION_PRESENT: &str = "Present";
const ACTION_CLEANUP: &str = "CleanUp";

#[derive(Debug, Deserialize)]
struct ChallengeRequest {
    request: ChallengeRequestBody
}

#[derive(Debug, Deserialize)]
struct ChallengeRequestBody {
    uid: Option<String>,
    action: String,
    #[serde(rename = "type")]
    type_: String,
    #[serde(rename = "dnsName")]
    dns_name: String,
    key: String,
    #[serde(rename = "resolvedFQDN")]
    resolved_fqdn: String,
    #[serde(rename = "resolvedZone")]
    resolved_zone: String,
    #[serde(rename = "resourceNamespace")]
    resource_namespace: String,
    #[serde(rename = "allowAmbientCredentials")]
    allow_ambient_credentials: bool,
    config: Option<Value>,
}

#[derive(Debug, Serialize)]
struct ChallengeResponse {
    response: ChallengeResponseBody
}

#[derive(Debug, Serialize)]
struct ChallengeResponseBody {
    #[serde(rename = "uid")]
    uid: String,
    #[serde(rename = "success")]
    success: bool,
    #[serde(rename = "status", skip_serializing_if = "Option::is_none")]
    status: Option<ErrorResponse>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    message: String,
    reason: String,
    code: i32,
}

pub struct HttpServer {
    plesk_api: Arc<PleskAPI>,
    group_name: String,
    solver_name: String,
    solver_version: String,
}


impl HttpServer {
    pub fn new(settings: &Settings) -> Self {
        let plesk_api = PleskAPI::new(
            settings.plesk.url.clone(),
            settings.plesk.siteid.clone(),
            settings.plesk.username.clone(),
            settings.plesk.password.clone(),
        );
        HttpServer {
            plesk_api: Arc::new(plesk_api),
            group_name: settings.common.groupname.clone(),
            solver_name: settings.common.solvername.clone(),
            solver_version: settings.common.solverversion.clone()
        }
    }

    pub async fn start(&mut self) -> Result<(), Error> {

        let cached_record = Arc::new(Mutex::new(Option::<String>::None));
        let plesk_api_clone_present = self.plesk_api.clone();

        // Base path called /apis/<group_name>/<solver_version> by cert-manager
        let url_base_path = warp::path("apis")
            .and(warp::path(self.group_name.clone()))
            .and(warp::path(self.solver_version.clone()));

        let post_route = url_base_path.clone()
        .and(warp::path(self.solver_name.clone()))
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body| {
            let plesk_api = plesk_api_clone_present.clone();
            let cache = cached_record.clone();
            handle_post(body, plesk_api, cache)
        });

        let get_route = url_base_path.clone()
            .and(warp::get())
            .and_then(handle_get);

        // OpenAPI routes are not implemented yet, but will see if we need them

        // Combine the routes
        let routes = post_route.or(get_route)
            .with(warp::log::custom(|info| {
                tracing::info!(
                    "Request: {} {} from {}. Status: {}",
                    info.method(),
                    info.path(),
                    info.remote_addr().map(|addr| addr.to_string()).unwrap_or_else(|| "unknown".to_string()),
                    info.status().as_u16()
                );
            }));

        // SAN; todo make this configurable
        let subject_alt_names = vec![
            "localhost".into(),
            format!("{}.cert-manager.svc.cluster.local", self.solver_name),
        ];
        // Generate an in-memory self-signed certificate
        let cert_key = generate_simple_self_signed(subject_alt_names)?;

        // Serialize the certificate and the private key to PEM format
        let priv_key_pem = cert_key.key_pair.serialize_pem();
        let cert_pem = cert_key.cert.pem();


        // Clone the routes for both HTTP and HTTPS
        let routes_http = routes.clone();
        let routes_https = routes;

        // Serve non-TLS on port 8000
        let http_server = task::spawn(async move {
            warp::serve(routes_http)
                .run(([0, 0, 0, 0], 8080))
                .await;
        });

        // Serve TLS on port 443
        let https_server = task::spawn(async move {
            warp::serve(routes_https)
                .tls()
                .key(priv_key_pem.as_bytes())
                .cert(cert_pem.as_bytes())
                .run(([0, 0, 0, 0], 8443))
                .await;
        });

        // Run both servers concurrently
        tokio::try_join!(http_server, https_server)?;

        Ok(())
    }   
}


// Handler for the /cleanup endpoint
async fn handle_post(
    body: Value,
    plesk_api: Arc<PleskAPI>,
    cache: Arc<Mutex<Option<String>>>
) -> Result<impl warp::Reply, warp::Rejection> {
    
    info!("Received POST request with the following payload: {:?}", &body);

    let request: ChallengeRequest = serde_json::from_value(body).unwrap();
    let body = request.request;

    let mut response_body = ChallengeResponseBody {
        uid: "".to_string(),
        success: false,
        status: None,
    };
    let challenge_id = body.key;
    let action = body.action;
    let mut cached_record = cache.lock().await;

    let result = match action.as_str() {
        ACTION_PRESENT => {
            if let Some(cached_record_id) = cached_record.clone() {
                info!("Challenge already present in cache");
                Ok(cached_record_id)
            } else {
                info!("Adding DNS challenge");
                let record_id = plesk_api.add_challenge(challenge_id).await.unwrap();
                let _ = cached_record.insert(record_id.clone());
                Ok(record_id)
            }
        },
        ACTION_CLEANUP => {
            if let Some(cached_record_id) = cached_record.take() {
                info!("Removing DNS challenge");
                plesk_api.remove_challenge(cached_record_id).await
            } else {
                info!("Record ID not found in cache, returning no success");
                let error_resp = ErrorResponse {
                    message: "Record ID not found in cache".to_string(),
                    reason: "Record ID not found in cache".to_string(),
                    code: 404,
                };
                response_body.status = Some(error_resp);
                return Ok(warp::reply::json(&ChallengeResponse {
                    response: response_body
                }));
            }
        },
        _ => { Err(anyhow!("Invalid action")) }
    };

    if result.is_err() {
        let error_msg = result.err().unwrap().to_string();
        let error_resp = ErrorResponse {
            message: error_msg.clone(),
            reason: error_msg,
            code: 404,
        };
        response_body.status = Some(error_resp);
        return Ok(warp::reply::json(&ChallengeResponse {
            response: response_body,
        }));
    }

    response_body.success = true;
    Ok(warp::reply::json(&ChallengeResponse {
        response: response_body,
    }))
}

async fn handle_get() -> Result<impl warp::Reply, warp::Rejection> {
    Ok(warp::reply::json(&ChallengeResponse {
        response: ChallengeResponseBody {
            uid: "1".to_string(),
            success: false,
            status: Some(ErrorResponse {
                message: "Not implemented".to_string(),
                reason: "Not implemented".to_string(),
                code: 501,
            }),
        }
    }))
}
