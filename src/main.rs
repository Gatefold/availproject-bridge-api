use alloy_primitives::{hex, B256, U256};
use axum::{
    extract::{Json, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use chrono::{Duration, TimeDelta, Timelike, Utc};
use jsonrpsee::core::Error;
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};
use sp_core::Decode;
use sp_io::hashing::twox_128;
use std::env;
use std::sync::Arc;
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;
use tokio::join;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};
use tracing_subscriber::prelude::*;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

struct AppState {
    avail_client: HttpClient,
    ethereum_client: HttpClient,
    request_client: Client,
    succinct_base_url: String,
    beaconchain_base_url: String,
    avail_chain_name: String,
    contract_chain_id: String,
    contract_address: String,
}

#[derive(Deserialize)]
struct IndexStruct {
    index: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KateQueryDataProofV2Response {
    data_proof: DataProof,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataProof {
    data_root: B256,
    blob_root: B256,
    bridge_root: B256,
    proof: Vec<B256>,
    leaf_index: u32,
    leaf: B256,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountStorageProofResponse {
    account_proof: Vec<String>,
    storage_proof: Vec<StorageProof>,
}

#[derive(Deserialize)]
struct StorageProof {
    proof: Vec<String>,
}

#[derive(Deserialize)]
struct SuccinctAPIResponse {
    data: Option<SuccinctAPIData>,
    error: Option<String>,
    success: Option<bool>,
}

#[derive(Deserialize)]
struct BeaconAPIResponse {
    status: String,
    data: BeaconAPIResponseData,
}

#[derive(Deserialize, Serialize)]
struct BeaconAPIResponseData {
    blockroot: B256,
    exec_block_number: u32,
    epoch: u32,
    slot: u32,
    exec_state_root: B256,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SuccinctAPIData {
    range_hash: B256,
    data_commitment: B256,
    merkle_branch: Vec<B256>,
    index: u8,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AggregatedResponse {
    data_root_proof: Vec<B256>,
    leaf_proof: Vec<B256>,
    range_hash: B256,
    data_root_index: u8,
    leaf: B256,
    leaf_index: u32,
    data_root: B256,
    blob_root: B256,
    bridge_root: B256,
    data_root_commitment: B256,
    block_hash: B256,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EthProofResponse {
    account_proof: Vec<String>,
    storage_proof: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HeadResponse {
    pub slot: u64,
    pub eth_block_number: u32,
    pub timestamp: u64,
    pub timestamp_diff: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RangeBlocks {
    start: u32,
    end: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RangeBlocksAPIResponse {
    data: RangeBlocks,
}

async fn alive() -> Result<Json<Value>, StatusCode> {
    Ok(Json(json!({ "name": "Avail Bridge API" })))
}

#[inline(always)]
async fn get_eth_proof(
    Path(block_hash): Path<B256>,
    Query(index_struct): Query<IndexStruct>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let cloned_state = state.clone();
    let data_proof_response_fut = tokio::spawn(async move {
        cloned_state
            .avail_client
            .request(
                "kate_queryDataProofV2",
                rpc_params![index_struct.index, &block_hash],
            )
            .await
    });
    let succinct_response_fut = tokio::spawn(async move {
        let url = format!(
            "{}?chainName={}&contractChainId={}&contractAddress={}&blockHash={}",
            state.succinct_base_url,
            state.avail_chain_name,
            state.contract_chain_id,
            state.contract_address,
            block_hash
        );

        let succinct_response = state.request_client.get(url).send().await;
        match succinct_response {
            Ok(resp) => resp.json::<SuccinctAPIResponse>().await,
            Err(err) => Err(err),
        }
    });
    let (data_proof, succinct_response) = join!(data_proof_response_fut, succinct_response_fut);
    let data_proof_res: KateQueryDataProofV2Response = match data_proof {
        Ok(resp) => match resp {
            Ok(data) => data,
            Err(err) => {
                tracing::error!("❌ {:?}", err);
                return (
                    StatusCode::BAD_REQUEST,
                    [("Cache-Control", "max-age=300, must-revalidate")],
                    Json(json!({ "error": err.to_string()})),
                );
            }
        },
        Err(err) => {
            tracing::error!("❌ {:?}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("Cache-Control", "max-age=300, must-revalidate")],
                Json(json!({ "error": err.to_string()})),
            );
        }
    };
    let succinct_data = match succinct_response {
        Ok(data) => match data {
            Ok(SuccinctAPIResponse {
                data: Some(data), ..
            }) => data,
            Ok(SuccinctAPIResponse {
                success: Some(false),
                error: Some(data),
                ..
            }) => {
                tracing::error!("❌ Succinct API returned unsuccessfully");
                return (
                    StatusCode::NOT_FOUND,
                    [("Cache-Control", "max-age=300, must-revalidate")],
                    Json(json!({ "error": data })),
                );
            }
            Err(err) => {
                tracing::error!("❌ {:?}", err);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [("Cache-Control", "max-age=300, must-revalidate")],
                    Json(json!({ "error": err.to_string()})),
                );
            }
            _ => {
                tracing::error!("❌ Succinct API returned no data");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [("Cache-Control", "max-age=300, must-revalidate")],
                    Json(json!({ "error": "Succinct API returned no data"})),
                );
            }
        },
        Err(err) => {
            tracing::error!("❌ {:?}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("Cache-Control", "max-age=300, must-revalidate")],
                Json(json!({ "error": err.to_string()})),
            );
        }
    };

    (
        StatusCode::OK,
        [("Cache-Control", "public, max-age=31536000")],
        Json(json!(AggregatedResponse {
            data_root_proof: succinct_data.merkle_branch,
            leaf_proof: data_proof_res.data_proof.proof,
            range_hash: succinct_data.range_hash,
            data_root_index: succinct_data.index,
            leaf: data_proof_res.data_proof.leaf,
            leaf_index: data_proof_res.data_proof.leaf_index,
            data_root: data_proof_res.data_proof.data_root,
            blob_root: data_proof_res.data_proof.blob_root,
            bridge_root: data_proof_res.data_proof.bridge_root,
            data_root_commitment: succinct_data.data_commitment,
            block_hash,
        })),
    )
}

#[inline(always)]
async fn get_avl_proof(
    Path(message_id): Path<U256>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mut hasher = Keccak256::new();
    hasher.update(
        [
            message_id.to_be_bytes_vec(),
            U256::from(1).to_be_bytes_vec(),
        ]
        .concat(),
    );
    let result = hasher.finalize();
    let proof: Result<AccountStorageProofResponse, jsonrpsee::core::Error> = state
        .ethereum_client
        .request(
            "eth_getProof",
            rpc_params![
                "0x8F8d47bF15953E26c622F36F3366e43e26B9b78b",
                [B256::from_slice(&result[..]).to_string()],
                "finalized"
            ],
        )
        .await;
    match proof {
        Ok(mut resp) => (
            StatusCode::OK,
            [("Cache-Control", "public, max-age=31536000")],
            Json(json!(EthProofResponse {
                account_proof: resp.account_proof,
                storage_proof: resp.storage_proof.swap_remove(0).proof,
            })),
        ),
        Err(err) => {
            tracing::error!("❌ {:?}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("Cache-Control", "max-age=300, must-revalidate")],
                Json(json!({ "error": err.to_string()})),
            )
        }
    }
}

/// Creates a request to the beaconcha service for mapping slot to the block number.
#[inline(always)]
async fn get_beacon_slot(
    Path(slot): Path<U256>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let resp = state
        .request_client
        .get(format!("{}/{}", state.beaconchain_base_url, slot))
        .send()
        .await;

    match resp {
        Ok(ok) => {
            let response_data = ok.json::<BeaconAPIResponse>().await;
            match response_data {
                Ok(rsp_data) => {
                    if rsp_data.status == "OK" {
                        (
                            StatusCode::OK,
                            [("Cache-Control", "public, max-age=31536000")],
                            Json(json!({
                                "blockNumber": rsp_data.data.exec_block_number
                            })),
                        )
                    } else {
                        tracing::error!("❌ {:?}", rsp_data.status);
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            [("Cache-Control", "max-age=300, must-revalidate")],
                            Json(json!({ "error": "Cannot fetch slot data"})),
                        )
                    }
                }
                Err(err) => {
                    tracing::error!("❌ {:?}", err);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        [("Cache-Control", "max-age=300, must-revalidate")],
                        Json(json!({ "error": err.to_string()})),
                    )
                }
            }
        }
        Err(err) => {
            tracing::error!("❌ {:?}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("Cache-Control", "max-age=300, must-revalidate")],
                Json(json!({ "error": err.to_string()})),
            )
        }
    }
}

/// get_eth_head returns Ethereum head with the latest slot/block that is stored and a time.
#[inline(always)]
async fn get_eth_head(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let pallet = "Succinct";
    let head = "Head";
    let timestamp = "Timestamps";

    let head_key = format!(
        "0x{}{}",
        hex::encode(twox_128(pallet.as_bytes())),
        hex::encode(twox_128(head.as_bytes()))
    );

    let head_response: Result<String, Error> = state
        .avail_client
        .request("state_getStorage", rpc_params![head_key])
        .await;

    match head_response {
        Ok(slot_storage_response) => {
            let timestamp_key = format!(
                "0x{}{}{}",
                hex::encode(twox_128(pallet.as_bytes())),
                hex::encode(twox_128(timestamp.as_bytes())),
                &slot_storage_response[2..].to_string()
            );
            let timestamp_response: Result<String, Error> = state
                .avail_client
                .request("state_getStorage", rpc_params![timestamp_key])
                .await;
            match timestamp_response {
                Ok(timestamp_storage_response) => {
                    // decode response from storage into readable values
                    let slot_from_hex =
                        sp_core::bytes::from_hex(slot_storage_response.as_str()).unwrap();
                    let slot_input = &mut slot_from_hex.as_slice();
                    let slot: u64 = Decode::decode(slot_input).unwrap();
                    let timestamp_from_hex =
                        sp_core::bytes::from_hex(timestamp_storage_response.as_str()).unwrap();
                    let timestamp_input = &mut timestamp_from_hex.as_slice();
                    let timestamp: u64 = Decode::decode(timestamp_input).unwrap();

                    let url = format!("{}/{}", state.beaconchain_base_url, slot);
                    let resp = state.request_client.get(url).send().await;

                    match resp {
                        Ok(ok) => {
                            if let Ok(response_data) = ok.json::<BeaconAPIResponse>().await {
                                let now = Utc::now().timestamp() as u64;
                                (
                                    StatusCode::OK,
                                    [("Cache-Control", "public, max-age=31536000")],
                                    Json(json!(HeadResponse {
                                        slot,
                                        timestamp,
                                        timestamp_diff: (now - timestamp),
                                        eth_block_number: response_data.data.exec_block_number,
                                    })),
                                )
                            } else {
                                tracing::error!("Cannot get beacon api response.");
                                (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    [("Cache-Control", "max-age=300, must-revalidate")],
                                    Json(json!({ "error": "Cannot get beacon api response"})),
                                )
                            }
                        }
                        Err(err) => {
                            tracing::error!("Cannot get beacon api response: {:?}.", err);
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                [("Cache-Control", "max-age=300, must-revalidate")],
                                Json(json!({ "error": err.to_string()})),
                            )
                        }
                    }
                }

                Err(err) => {
                    tracing::error!("Cannot get timestamp storage: {:?}", err);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        [("Cache-Control", "max-age=300, must-revalidate")],
                        Json(json!({ "error": err.to_string()})),
                    )
                }
            }
        }
        Err(err) => {
            tracing::error!("Cannot get head storage: {:?}", err.to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("Cache-Control", "max-age=300, must-revalidate")],
                Json(json!({ "error": err.to_string()})),
            )
        }
    }
}

/// get_avl_head returns start and end blocks which the contract has commitments
#[inline(always)]
async fn get_avl_head(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let url = format!(
        "{}/{}/?contractChainId={}&contractAddress={}",
        state.succinct_base_url,
        "range".to_string(),
        state.contract_chain_id,
        state.contract_address
    );
    let response = state.request_client.get(url).send().await;
    match response {
        Ok(ok) => {
            let range_response = ok.json::<RangeBlocksAPIResponse>().await;
            match range_response {
                Ok(range_blocks) => (
                    StatusCode::OK,
                    [("Cache-Control", "public, max-age=31536000")],
                    Json(json!(range_blocks)),
                ),
                Err(err) => {
                    tracing::error!("Cannot parse range blocks: {:?}", err.to_string());
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        [("Cache-Control", "max-age=300, must-revalidate")],
                        Json(json!({ "error": err.to_string()})),
                    )
                }
            }
        }
        Err(err) => {
            tracing::error!("Cannot get avl head: {:?}", err.to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("Cache-Control", "max-age=300, must-revalidate")],
                Json(json!({ "error": err.to_string()})),
            )
        }
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "bridge_api=debug,tower_http=debug,axum::rejection=trace".into()
            }),
        )
        .init();

    let shared_state = Arc::new(AppState {
        avail_client: HttpClientBuilder::default()
            .build(
                env::var("AVAIL_CLIENT_URL")
                    .unwrap_or("https://goldberg.avail.tools/api".to_owned()),
            )
            .unwrap(),
        ethereum_client: HttpClientBuilder::default()
            .build(
                env::var("ETHEREUM_CLIENT_URL")
                    .unwrap_or("https://ethereum-sepolia.publicnode.com".to_owned()),
            )
            .unwrap(),
        request_client: Client::builder().brotli(true).build().unwrap(),
        succinct_base_url: env::var("SUCCINCT_URL")
            .unwrap_or("https://beaconapi.succinct.xyz/api/integrations/vectorx".to_owned()),
        beaconchain_base_url: env::var("BEACONCHAIN_URL")
            .unwrap_or("https://sepolia.beaconcha.in/api/v1/slot".to_owned()),
        contract_address: env::var("CONTRACT_ADDRESS")
            .unwrap_or("0x169e50f09A50F3509777cEf63EC59Eeb2aAcd201".to_owned()),
        contract_chain_id: env::var("CONTRACT_CHAIN_ID").unwrap_or("11155111".to_owned()),
        avail_chain_name: env::var("AVAIL_CHAIN_NAME").unwrap_or("goldberg".to_owned()),
    });

    let app = Router::new()
        .route("/", get(alive))
        .route("/eth/proof/:block_hash", get(get_eth_proof))
        .route("/eth/head", get(get_eth_head))
        .route("/avl/head", get(get_avl_head))
        .route("/avl/proof/:message_id", get(get_avl_proof))
        .route("/beacon/slot/:slot_number", get(get_beacon_slot))
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .with_state(shared_state);

    let host = env::var("HOST").unwrap_or("0.0.0.0".to_owned());
    let port = env::var("PORT").unwrap_or("8080".to_owned());
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", host, port))
        .await
        .unwrap();
    tracing::info!("🚀 Listening on {} port {}", host, port);
    axum::serve(listener, app).await.unwrap();
}
