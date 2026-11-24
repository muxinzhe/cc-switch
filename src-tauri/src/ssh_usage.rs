pub(crate) mod attribution;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const LOG_COLUMNS: &str = "request_id,provider_id,app_type,model,request_model,pricing_model,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,input_token_semantics,input_cost_usd,output_cost_usd,cache_read_cost_usd,cache_creation_cost_usd,total_cost_usd,latency_ms,first_token_ms,duration_ms,status_code,error_message,session_id,provider_type,is_streaming,cost_multiplier,created_at,data_source";
pub(crate) const ROLLUP_COLUMNS: &str = "provider_id,date,app_type,model,request_model,pricing_model,request_count,success_count,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,input_token_semantics,total_cost_usd,avg_latency_ms";

#[derive(Deserialize, Serialize)]
pub(crate) struct Rows {
    pub logs: Vec<Vec<Value>>,
    pub rollups: Vec<Vec<Value>>,
    /// Provider id, application type and display name. Configuration and credentials stay on the host.
    pub providers: Vec<(String, String, String)>,
}
