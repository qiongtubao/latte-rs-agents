//! Process-wide suppression for provider credentials that have been rejected.
//!
//! A model-local cooldown is insufficient because each workflow step and
//! delegate builds a fresh `Agent`. Without this registry, every new agent
//! retries the same invalid key. Blocks are keyed by provider + endpoint + a
//! non-reversible key fingerprint, so rotating the key naturally creates a
//! fresh identity. HTTP 401 blocks the whole account; HTTP 403 is kept
//! model-specific because some providers use it for per-model authorization.

use std::collections::HashMap;
use std::sync::OnceLock;

use latte_ai::error::AiError;
use latte_ai::models::Model;
use parking_lot::Mutex;

#[derive(Debug, Clone)]
struct Entry {
    provider: String,
    model_id: String,
    message: String,
}

fn registry() -> &'static Mutex<HashMap<String, Entry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key_fingerprint(key: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn account_key(model: &Model) -> String {
    format!(
        "{}|{}|{:016x}",
        model.provider,
        model.base_url.trim_end_matches('/'),
        key_fingerprint(&model.api_key)
    )
}

fn account_block_key(model: &Model) -> String {
    format!("account|{}", account_key(model))
}

fn model_block_key(model: &Model) -> String {
    format!("model|{}|{}", account_key(model), model.id)
}

fn brief(message: &str) -> String {
    let text: String = message.chars().take(160).collect();
    if message.chars().count() > 160 {
        format!("{text}…")
    } else {
        text
    }
}

/// Whether this provider failure is a stable authentication/authorization
/// rejection that should move immediately to the next configured candidate.
pub fn is_auth_error(error: &AiError) -> bool {
    matches!(
        error,
        AiError::Auth(_)
            | AiError::Api {
                status: 401 | 403,
                ..
            }
    )
}

/// Record a rejected credential. Billing errors must be handled before this
/// function because some providers report exhausted quota as HTTP 403.
pub fn mark(model: &Model, error: &AiError) {
    if !is_auth_error(error) {
        return;
    }
    let key = match error {
        AiError::Api { status: 403, .. } => model_block_key(model),
        _ => account_block_key(model),
    };
    registry().lock().insert(
        key,
        Entry {
            provider: model.provider.clone(),
            model_id: model.id.clone(),
            message: brief(&error.to_string()),
        },
    );
}

/// Return the rejection summary when either this account or this specific
/// model authorization is blocked.
pub fn blocked(model: &Model) -> Option<String> {
    let registry = registry().lock();
    registry
        .get(&account_block_key(model))
        .or_else(|| registry.get(&model_block_key(model)))
        .map(|entry| entry.message.clone())
}

/// Human-readable blocked entries for the model-unavailable pause prompt.
pub fn blocked_summary() -> Vec<String> {
    let mut entries: Vec<String> = registry()
        .lock()
        .values()
        .map(|entry| {
            format!(
                "{}（{}：{}）",
                entry.provider, entry.model_id, entry.message
            )
        })
        .collect();
    entries.sort();
    entries.dedup();
    entries
}

/// Manual resume explicitly requests a fresh probe. A rotated key does not
/// require this because its fingerprint already differs.
pub fn clear_all() -> usize {
    let mut registry = registry().lock();
    let count = registry.len();
    registry.clear();
    count
}

pub const CREDENTIAL_FAILURE_PREFIX: &str = "鉴权失败（已摘除，不自动重试）";
