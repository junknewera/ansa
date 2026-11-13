use super::legacy_compact::CompactedContext;
use crate::{
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::Epoch,
    session::{Result, num},
};
pub fn compact_epoch(
    base: &Epoch,
    generation: usize,
    compacted: &CompactedContext,
) -> Result<Epoch> {
    let hash = sha256(compacted.native().dump().as_bytes());
    Ok(Epoch {
        id: format!("compact_{generation}_{}", &hash[..16]),
        instructions: base.instructions.clone(),
        exact_state: Json::object(vec![
            ("base", base.exact_state.clone()),
            ("base_id", Json::string(&base.id)?),
            ("compaction_generation", num(generation)?),
            ("compaction_digest", Json::string(&hash)?),
        ])?,
    })
}
