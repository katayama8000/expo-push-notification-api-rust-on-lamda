use aws_config::BehaviorVersion;
use aws_sdk_ssm::Client as SsmClient;
use expo_push_notification_client::{Expo, ExpoClientOptions, ExpoPushMessage};
use futures::future::join_all;
use http::{header::HeaderValue, StatusCode};
use lambda_http::{Body, Error, Request, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use supabase_rs::SupabaseClient;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ApiError {
    #[error("Failed to load secrets from SSM")]
    SsmError,
    #[error("Missing expected secret: {0}")]
    MissingSecret(String),
    #[error("Missing environment variable: {0}")]
    MissingEnvVar(String),
    #[error("Failed to initialize Supabase client")]
    SupabaseInitialization,
    #[error("Failed to fetch tokens from Supabase")]
    SupabaseFetch,
    #[error("Invalid API Key")]
    InvalidApiKey,
    #[error("Invalid request body")]
    InvalidBody,
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Failed to build push message")]
    PushMessageBuild,
}

/// SSM Parameter Storeから設定を一括で取得します。（ページネーションなし）
pub async fn get_secrets() -> Result<HashMap<String, String>, ApiError> {
    let ssm_parameter_path = env::var("SSM_PARAMETER_PATH")
        .map_err(|_| ApiError::MissingEnvVar("SSM_PARAMETER_PATH".into()))?;

    let config = aws_config::load_defaults(BehaviorVersion::v2025_08_07()).await;
    let ssm_client = SsmClient::new(&config);

    println!("Fetching parameters from SSM path: {}", ssm_parameter_path);

    let mut secrets = HashMap::new();

    // ページネーションを削除し、一度のリクエストで取得
    let response = ssm_client
        .get_parameters_by_path()
        .path(ssm_parameter_path.clone())
        .with_decryption(true)
        .send()
        .await
        .map_err(|e| {
            eprintln!("Failed to get parameters from SSM: {:?}", e);
            ApiError::SsmError
        })?;

    if let Some(params) = response.parameters {
        for param in params {
            if let (Some(name), Some(value)) = (param.name, param.value) {
                println!("Fetched parameter: {}, value: {}", name, value);
                // パスからキー名のみを抽出 (e.g., /expo-push-api/supabase-key -> supabase-key)
                if let Some(key) = name.split('/').last() {
                    secrets.insert(key.to_string(), value);
                }
            }
        }
    }

    println!("Successfully fetched secrets from SSM.");
    Ok(secrets)
}

pub fn initialize_supabase_client(
    secrets: &HashMap<String, String>,
) -> Result<SupabaseClient, ApiError> {
    let supabase_url = secrets
        .get("supabase-url")
        .ok_or_else(|| ApiError::MissingSecret("supabase-url".into()))?;
    let supabase_key = secrets
        .get("supabase-key")
        .ok_or_else(|| ApiError::MissingSecret("supabase-key".into()))?;

    SupabaseClient::new(supabase_url.to_string(), supabase_key.to_string())
        .map_err(|_| ApiError::SupabaseInitialization)
}

pub async fn fetch_expo_push_tokens(client: &SupabaseClient) -> Result<Vec<String>, ApiError> {
    let response = client.select("users").execute().await.map_err(|e| {
        eprintln!("Error fetching expo push tokens: {:?}", e);
        ApiError::SupabaseFetch
    })?;

    let tokens = response
        .iter()
        .filter_map(|row| row["expo_push_token"].as_str().map(|s| s.to_string()))
        .collect::<Vec<String>>();
    println!("fetched expo push tokens from supabase {:?}", tokens);
    Ok(tokens)
}

pub async fn extract_body(req: &Request) -> Result<Value, ApiError> {
    let body_str = match req.body() {
        Body::Text(s) => s.to_string(),
        Body::Binary(b) => String::from_utf8(b.to_vec()).map_err(|_| ApiError::InvalidBody)?,
        _ => return Err(ApiError::InvalidBody),
    };

    serde_json::from_str(&body_str).map_err(|_| ApiError::InvalidBody)
}

pub fn create_error_response(
    status_code: StatusCode,
    message: &str,
) -> Result<Response<Body>, Error> {
    Ok(Response::builder()
        .status(status_code)
        .header("Content-Type", "application/json")
        .body(json!({ "error": message }).to_string().into())?)
}

pub async fn function_handler(event: Request) -> Result<Response<Body>, Error> {
    // 1. APIキーの検証
    let expected_key = env::var("API_KEY").expect("API_KEY not set");
    let expected_key_value =
        HeaderValue::from_str(&expected_key).map_err(|_| ApiError::InvalidApiKey)?;
    if event.headers().get("x-api-key") != Some(&expected_key_value) {
        return create_error_response(StatusCode::FORBIDDEN, "Forbidden: Invalid API Key");
    }

    println!(
        "Expo push notification API ver: {}",
        env!("CARGO_PKG_VERSION")
    );

    // 2. SSMから設定情報を取得
    let secrets = get_secrets().await?;
    let expo_access_token = secrets
        .get("expo-access-token")
        .ok_or_else(|| ApiError::MissingSecret("expo-access-token".into()))?;

    let expo = Expo::new(ExpoClientOptions {
        access_token: Some(expo_access_token.clone()),
    });

    let title;
    let body;
    let mut expo_push_tokens = vec![];

    // 3. メソッドに応じて処理を分岐
    match event.method().as_str() {
        "GET" => {
            title = "25日だよ".to_string();
            body = "パートナーに請求しよう".to_string();
            let supabase_client = initialize_supabase_client(&secrets)?;
            expo_push_tokens = fetch_expo_push_tokens(&supabase_client).await?;
        }
        "POST" => {
            let json_body = extract_body(&event).await?;
            title = json_body["title"]
                .as_str()
                .ok_or_else(|| ApiError::BadRequest("Title is required".into()))?
                .to_string();
            body = json_body["body"]
                .as_str()
                .ok_or_else(|| ApiError::BadRequest("Body is required".into()))?
                .to_string();

            let token = json_body["expo_push_token"]
                .as_str()
                .ok_or_else(|| ApiError::BadRequest("expo_push_token is required".into()))?;

            if Expo::is_expo_push_token(token) {
                expo_push_tokens.push(token.to_string());
            } else {
                return create_error_response(StatusCode::BAD_REQUEST, "Invalid expo push token");
            }
        }
        _ => return create_error_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed"),
    }

    if expo_push_tokens.is_empty() {
        println!("No push tokens found, skipping notification.");
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(
                json!({ "message": "No push tokens found." })
                    .to_string()
                    .into(),
            )?);
    }

    // 4. プッシュ通知メッセージの構築
    println!(
        "Building push notification for tokens: {:?}",
        expo_push_tokens
    );
    let messages = expo_push_tokens
        .into_iter()
        .map(|token| {
            ExpoPushMessage::builder(vec![token])
                .title(title.clone())
                .body(body.clone())
                .build()
                .map_err(|_| ApiError::PushMessageBuild)
        })
        .collect::<Result<Vec<_>, _>>()?;

    // 5. プッシュ通知の送信
    println!("Sending push notifications...");
    let send_futures = messages
        .into_iter()
        .map(|msg| expo.send_push_notifications(msg))
        .collect::<Vec<_>>();

    let results = join_all(send_futures).await;

    let has_error = results.iter().any(|r| r.is_err());

    if has_error {
        eprintln!("Failed to send some push notifications: {:?}", results);
        create_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to send some push notifications",
        )
    } else {
        println!("Push notifications sent successfully.");
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(
                json!({ "message": "Push notifications sent successfully" })
                    .to_string()
                    .into(),
            )?)
    }
}
