use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub healthy: bool,
    pub latency_ms: Option<u128>,
    pub last_check: Option<std::time::Instant>,
    pub error: Option<String>,
}

pub type HealthMap = HashMap<String, HealthStatus>;
