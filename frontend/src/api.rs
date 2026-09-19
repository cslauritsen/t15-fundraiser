use gloo_net::http::Request;
use shared::{CatalogResponse, CheckoutRequest, CheckoutResponse, ErrorResponse, OrderStatusResponse};

/// Either a structured error from our backend or a transport problem.
#[derive(Debug, Clone)]
pub enum ApiError {
    Server(ErrorResponse),
    Network,
}

impl ApiError {
    pub fn message(&self) -> String {
        match self {
            ApiError::Server(e) => e.message.clone(),
            ApiError::Network => "We couldn't reach the server. Check your connection and try again.".into(),
        }
    }
}

async fn read<T: serde::de::DeserializeOwned>(resp: gloo_net::http::Response) -> Result<T, ApiError> {
    if resp.ok() {
        resp.json::<T>().await.map_err(|_| ApiError::Network)
    } else {
        match resp.json::<ErrorResponse>().await {
            Ok(e) => Err(ApiError::Server(e)),
            Err(_) => Err(ApiError::Network),
        }
    }
}

pub async fn fetch_catalog() -> Result<CatalogResponse, ApiError> {
    let resp = Request::get("/api/catalog").send().await.map_err(|_| ApiError::Network)?;
    read(resp).await
}

pub async fn checkout(req: &CheckoutRequest) -> Result<CheckoutResponse, ApiError> {
    let resp = Request::post("/api/checkout")
        .json(req)
        .map_err(|_| ApiError::Network)?
        .send()
        .await
        .map_err(|_| ApiError::Network)?;
    read(resp).await
}

pub async fn order_status(order_id: &str, session_id: &str) -> Result<OrderStatusResponse, ApiError> {
    let resp = Request::get(&format!("/api/orders/{order_id}/status"))
        .query([("session_id", session_id)])
        .send()
        .await
        .map_err(|_| ApiError::Network)?;
    read(resp).await
}
